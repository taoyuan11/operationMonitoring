use std::{
    collections::{BTreeMap, HashMap, VecDeque},
    env,
    ffi::{OsStr, OsString},
    fs,
    io::{self, Read, Write},
    path::{Component, Path, PathBuf},
    process::{ExitStatus, Stdio},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc as std_mpsc,
    },
    thread,
    time::Duration,
};

use base64::{Engine as _, engine::general_purpose::STANDARD};
use portable_pty::{CommandBuilder, PtySize, native_pty_system};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tokio::{
    io::{AsyncRead, AsyncReadExt},
    process::Command,
    sync::{Semaphore, mpsc},
    task::JoinHandle,
};

use crate::{
    activity::ActivityTracker,
    config::AgentConfig,
    identity::identity_path,
    lifecycle::docker_protected_paths,
    models::{
        AgentInbound, DockerComposeAction, DockerComposeConfigSummary, DockerComposeServiceSummary,
        DockerComposeTarget, DockerComposeValidation, DockerContainerAction,
        DockerContainerCreateSpec, DockerError, DockerErrorCode, DockerMountKind,
        DockerNetworkCreateSpec, DockerPortProtocol, DockerRequest, DockerResponse,
        DockerRestartPolicy, DockerStatus, DockerStatusState, DockerVolumeCreateSpec,
    },
    outbound::AgentEventSender,
    pty_io::PtyInputWriter,
    time::now_ts,
    update::docker_protected_update_root,
};

pub const CAPABILITY: &str = "docker_manager_v1";

const PROBE_TIMEOUT: Duration = Duration::from_secs(5);
const READ_TIMEOUT: Duration = Duration::from_secs(30);
const LIFECYCLE_TIMEOUT: Duration = Duration::from_secs(120);
const LONG_TIMEOUT: Duration = Duration::from_secs(15 * 60);
const MAX_CONTROL_OUTPUT: usize = 4 * 1024 * 1024;
const MAX_ERROR_OUTPUT: usize = 64 * 1024;
const MAX_ERROR_MESSAGE: usize = 2 * 1024;
const LOG_CHUNK_SIZE: usize = 16 * 1024;
const MAX_EXEC_INPUT_BYTES: usize = 64 * 1024;
const EXEC_CONTROL_QUEUE_CAPACITY: usize = 128;
const EXEC_WRITE_QUEUE_CAPACITY: usize = 16;
const MIN_DOCKER_MAJOR: u64 = 20;
const MIN_DOCKER_MINOR: u64 = 10;

pub fn supported() -> bool {
    cfg!(target_os = "linux")
}

pub struct DockerManager {
    outbound: AgentEventSender,
    stream_outbound: AgentEventSender,
    activity: ActivityTracker,
    runner: DockerRunner,
    status: Arc<Mutex<Option<DockerStatus>>>,
    probe_task: Option<JoinHandle<()>>,
    read_slots: Arc<Semaphore>,
    mutation_slots: Arc<Semaphore>,
    stream_slots: Arc<Semaphore>,
    requests: HashMap<String, JoinHandle<()>>,
    logs: HashMap<String, JoinHandle<()>>,
    exec: DockerExecManager,
}

impl DockerManager {
    pub fn new(
        config: &AgentConfig,
        outbound: AgentEventSender,
        stream_outbound: AgentEventSender,
        activity: ActivityTracker,
    ) -> Self {
        let stream_slots = Arc::new(Semaphore::new(2));
        let snapshot_dir = docker_protected_update_root(config)
            .unwrap_or_else(|_| env::temp_dir().join("operation-monitoring-agent"))
            .join("docker-compose");
        let runner = DockerRunner::new("docker", protected_paths(config))
            .with_compose_snapshot_dir(snapshot_dir);
        let exec = DockerExecManager::new(
            runner.binary.clone(),
            stream_outbound.clone(),
            activity.clone(),
            stream_slots.clone(),
        );
        Self {
            outbound,
            stream_outbound,
            activity,
            runner,
            status: Arc::new(Mutex::new(None)),
            probe_task: None,
            read_slots: Arc::new(Semaphore::new(4)),
            mutation_slots: Arc::new(Semaphore::new(1)),
            stream_slots,
            requests: HashMap::new(),
            logs: HashMap::new(),
            exec,
        }
    }

    pub fn status(&self) -> Option<DockerStatus> {
        if !supported() {
            return None;
        }
        self.status.lock().ok().and_then(|status| status.clone())
    }

    pub fn start_probe(&mut self) {
        if !supported() {
            return;
        }
        if self
            .probe_task
            .as_ref()
            .is_some_and(|task| !task.is_finished())
        {
            return;
        }
        self.probe_task.take();
        let Some(activity_guard) = self.activity.try_enter() else {
            return;
        };
        let runner = self.runner.clone();
        let status = self.status.clone();
        self.probe_task = Some(tokio::spawn(async move {
            let next = probe_docker(&runner).await;
            drop(activity_guard);
            if let Ok(mut current) = status.lock() {
                *current = Some(next);
            }
        }));
    }

    pub fn handle_request(&mut self, request_id: String, request: DockerRequest) {
        self.prune();
        self.cancel(&request_id);
        if let Some(error) = self.unavailable_error() {
            send_response(&self.outbound, request_id, DockerResponse::Error { error });
            return;
        }

        let slots = if request.is_mutating() {
            self.mutation_slots.clone()
        } else {
            self.read_slots.clone()
        };
        let Ok(permit) = slots.try_acquire_owned() else {
            send_error(
                &self.outbound,
                request_id,
                DockerError::new(
                    DockerErrorCode::Busy,
                    "Docker operation concurrency limit reached",
                    true,
                ),
            );
            return;
        };
        let Some(activity_guard) = self.activity.try_enter() else {
            send_error(
                &self.outbound,
                request_id,
                DockerError::new(
                    DockerErrorCode::Busy,
                    "Agent update is waiting to install",
                    true,
                ),
            );
            return;
        };

        let task_request_id = request_id.clone();
        let runner = self.runner.clone();
        let outbound = self.outbound.clone();
        let timeout = request.timeout();
        let task = tokio::spawn(async move {
            let _permit = permit;
            let response =
                match tokio::time::timeout(timeout, execute_request(&runner, request)).await {
                    Ok(Ok(response)) => response,
                    Ok(Err(error)) => DockerResponse::Error { error },
                    Err(_) => DockerResponse::Error {
                        error: DockerError::new(
                            DockerErrorCode::Timeout,
                            "Docker operation timed out",
                            true,
                        ),
                    },
                };
            drop(activity_guard);
            send_response(&outbound, task_request_id, response);
        });
        self.requests.insert(request_id, task);
    }

    pub fn cancel(&mut self, request_id: &str) {
        if let Some(task) = self.requests.remove(request_id) {
            if task.is_finished() {
                return;
            }
            task.abort();
            send_error(
                &self.outbound,
                request_id.to_string(),
                DockerError::new(
                    DockerErrorCode::Cancelled,
                    "Docker operation cancelled",
                    false,
                ),
            );
        }
    }

    pub fn start_logs(
        &mut self,
        request_id: String,
        container: String,
        tail: u32,
        follow: bool,
        since: Option<String>,
    ) {
        self.prune();
        self.cancel_logs(&request_id);
        if let Some(error) = self.unavailable_error() {
            send_stream_event(
                &self.stream_outbound,
                AgentInbound::DockerLogClosed {
                    request_id,
                    error: Some(error),
                },
            );
            return;
        }
        let tail = tail.clamp(1, 2000);
        if let Err(error) = validate_identifier(&container, "container")
            .and_then(|_| validate_optional_cursor(since.as_deref()))
        {
            send_stream_event(
                &self.stream_outbound,
                AgentInbound::DockerLogClosed {
                    request_id,
                    error: Some(error),
                },
            );
            return;
        }
        let Ok(permit) = self.stream_slots.clone().try_acquire_owned() else {
            send_stream_event(
                &self.stream_outbound,
                AgentInbound::DockerLogClosed {
                    request_id,
                    error: Some(DockerError::new(
                        DockerErrorCode::Busy,
                        "Docker stream concurrency limit reached",
                        true,
                    )),
                },
            );
            return;
        };
        let Some(activity_guard) = self.activity.try_enter() else {
            send_stream_event(
                &self.stream_outbound,
                AgentInbound::DockerLogClosed {
                    request_id,
                    error: Some(DockerError::new(
                        DockerErrorCode::Busy,
                        "Agent update is waiting to install",
                        true,
                    )),
                },
            );
            return;
        };

        let runner = self.runner.clone();
        let stream_outbound = self.stream_outbound.clone();
        let task_request_id = request_id.clone();
        let task = tokio::spawn(async move {
            let _permit = permit;
            let _activity_guard = activity_guard;
            let result = stream_logs(
                &runner,
                &task_request_id,
                &container,
                tail,
                follow,
                since.as_deref(),
                &stream_outbound,
            )
            .await;
            let _ = stream_outbound
                .send_async(AgentInbound::DockerLogClosed {
                    request_id: task_request_id,
                    error: result.err(),
                })
                .await;
        });
        self.logs.insert(request_id, task);
    }

    pub fn cancel_logs(&mut self, request_id: &str) {
        if let Some(task) = self.logs.remove(request_id) {
            if task.is_finished() {
                return;
            }
            task.abort();
            send_stream_event(
                &self.stream_outbound,
                AgentInbound::DockerLogClosed {
                    request_id: request_id.to_string(),
                    error: None,
                },
            );
        }
    }

    pub fn exec_open(
        &mut self,
        session_id: String,
        container: String,
        shell: String,
        cols: u16,
        rows: u16,
    ) {
        if let Some(error) = self.unavailable_error() {
            send_stream_event(
                &self.stream_outbound,
                AgentInbound::DockerExecClosed {
                    session_id,
                    exit_code: None,
                    reason: Some(error.message),
                },
            );
            return;
        }
        self.exec.open(session_id, container, shell, cols, rows);
    }

    pub fn exec_input(&mut self, session_id: &str, data: &str) {
        self.exec.input(session_id, data);
    }

    pub fn exec_resize(&mut self, session_id: &str, cols: u16, rows: u16) {
        self.exec.resize(session_id, cols, rows);
    }

    pub fn exec_close(&mut self, session_id: &str) {
        self.exec.close(session_id);
    }

    pub fn close_all(&mut self) {
        if let Some(task) = self.probe_task.take() {
            task.abort();
        }
        for (_, task) in self.requests.drain() {
            task.abort();
        }
        for (_, task) in self.logs.drain() {
            task.abort();
        }
        self.exec.close_all();
    }

    pub fn close_streams_for_update(&mut self) {
        for (request_id, task) in self.logs.drain() {
            task.abort();
            send_stream_event(
                &self.stream_outbound,
                AgentInbound::DockerLogClosed {
                    request_id,
                    error: Some(DockerError::new(
                        DockerErrorCode::Cancelled,
                        "Docker log stream closed for Agent update",
                        true,
                    )),
                },
            );
        }
        self.exec
            .close_all_with_reason("Docker terminal closed for Agent update");
    }

    fn prune(&mut self) {
        self.requests.retain(|_, task| !task.is_finished());
        self.logs.retain(|_, task| !task.is_finished());
        self.exec.prune();
    }

    fn unavailable_error(&self) -> Option<DockerError> {
        if !supported() {
            return Some(DockerError::new(
                DockerErrorCode::Unsupported,
                "Docker management is supported only on Linux/OpenWrt agents",
                false,
            ));
        }
        let status = self.status();
        match status.as_ref().map(|status| status.state) {
            Some(DockerStatusState::Ready) => None,
            Some(DockerStatusState::NotInstalled) => Some(DockerError::new(
                DockerErrorCode::NotInstalled,
                status_message(status.as_ref(), "Docker CLI is not installed"),
                false,
            )),
            Some(DockerStatusState::DaemonUnreachable) => Some(DockerError::new(
                DockerErrorCode::DaemonUnavailable,
                status_message(status.as_ref(), "Docker daemon is unavailable"),
                true,
            )),
            Some(DockerStatusState::PermissionDenied) => Some(DockerError::new(
                DockerErrorCode::PermissionDenied,
                status_message(status.as_ref(), "Docker daemon permission denied"),
                false,
            )),
            Some(DockerStatusState::UnsupportedVersion) => Some(DockerError::new(
                DockerErrorCode::UnsupportedVersion,
                status_message(status.as_ref(), "Docker CLI 20.10 or newer is required"),
                false,
            )),
            Some(DockerStatusState::Error) => Some(DockerError::new(
                DockerErrorCode::Internal,
                status_message(status.as_ref(), "Docker status probe failed"),
                true,
            )),
            None => Some(DockerError::new(
                DockerErrorCode::Busy,
                "Docker status probe has not completed",
                true,
            )),
        }
    }
}

fn status_message(status: Option<&DockerStatus>, fallback: &str) -> String {
    status
        .and_then(|status| status.message.clone())
        .unwrap_or_else(|| fallback.to_string())
}

impl DockerError {
    fn new(code: DockerErrorCode, message: impl Into<String>, retryable: bool) -> Self {
        Self {
            code,
            message: truncate_text(&message.into(), MAX_ERROR_MESSAGE),
            retryable,
            exit_code: None,
        }
    }

    fn with_exit_code(mut self, exit_code: Option<i64>) -> Self {
        self.exit_code = exit_code;
        self
    }
}

impl DockerRequest {
    fn is_mutating(&self) -> bool {
        !matches!(
            self,
            Self::ContainerList { .. }
                | Self::ContainerInspect { .. }
                | Self::ContainerStats { .. }
                | Self::ImageList { .. }
                | Self::ImageInspect { .. }
                | Self::NetworkList
                | Self::NetworkInspect { .. }
                | Self::VolumeList
                | Self::VolumeInspect { .. }
                | Self::ComposeList
                | Self::ComposeInspect { .. }
                | Self::ComposeValidate { .. }
                | Self::SystemDf
        )
    }

    fn timeout(&self) -> Duration {
        match self {
            Self::ImagePull { .. }
            | Self::ImagePrune
            | Self::NetworkPrune
            | Self::VolumePrune
            | Self::ComposeDeploy { .. }
            | Self::SystemPrune { .. }
            | Self::ComposeAction {
                action:
                    DockerComposeAction::Pull | DockerComposeAction::Up | DockerComposeAction::Down,
                ..
            } => LONG_TIMEOUT,
            Self::ContainerCreate { .. }
            | Self::ContainerAction { .. }
            | Self::ContainerRename { .. }
            | Self::ContainerRemove { .. }
            | Self::ImageTag { .. }
            | Self::ImageRemove { .. }
            | Self::NetworkCreate { .. }
            | Self::NetworkConnect { .. }
            | Self::NetworkDisconnect { .. }
            | Self::NetworkRemove { .. }
            | Self::VolumeCreate { .. }
            | Self::VolumeRemove { .. }
            | Self::ComposeAction { .. } => LIFECYCLE_TIMEOUT,
            _ => READ_TIMEOUT,
        }
    }
}

#[derive(Clone)]
struct DockerRunner {
    binary: OsString,
    protected_paths: Arc<Vec<PathBuf>>,
    compose_snapshot_dir: Arc<PathBuf>,
}

#[derive(Debug)]
struct CommandOutput {
    status: ExitStatus,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
    stdout_truncated: bool,
    stderr_truncated: bool,
}

impl DockerRunner {
    fn new(binary: impl Into<OsString>, protected_paths: Vec<PathBuf>) -> Self {
        Self {
            binary: binary.into(),
            protected_paths: Arc::new(protected_paths),
            compose_snapshot_dir: Arc::new(
                env::temp_dir().join("operation-monitoring-agent-docker-compose"),
            ),
        }
    }

    fn with_compose_snapshot_dir(mut self, path: PathBuf) -> Self {
        self.compose_snapshot_dir = Arc::new(path);
        self
    }

    async fn ensure_local_unix_daemon(&self) -> Result<String, DockerError> {
        self.ensure_local_unix_daemon_for(
            env::var_os("DOCKER_CONTEXT").filter(|value| !value.is_empty()),
            env::var_os("DOCKER_HOST").filter(|value| !value.is_empty()),
        )
        .await
    }

    async fn ensure_local_unix_daemon_for(
        &self,
        context: Option<OsString>,
        docker_host: Option<OsString>,
    ) -> Result<String, DockerError> {
        let endpoint = if context.is_none()
            && let Some(value) = docker_host
        {
            value.into_string().map_err(|_| {
                invalid("DOCKER_HOST must be valid UTF-8 before bind mounts can be used")
            })?
        } else {
            let mut args = os_args(["context", "inspect"]);
            if let Some(context) = context {
                args.push(context);
            }
            args.extend(os_args(["--format", "{{json .Endpoints.docker.Host}}"]));
            let output = checked_command(self, args, PROBE_TIMEOUT).await?;
            ensure_complete_control_output(&output)?;
            serde_json::from_slice::<String>(&output.stdout).map_err(|_| {
                DockerError::new(
                    DockerErrorCode::CommandFailed,
                    "Unable to determine the active Docker context endpoint",
                    false,
                )
            })?
        };

        if !is_local_unix_docker_endpoint(&endpoint) {
            return Err(DockerError::new(
                DockerErrorCode::Unsupported,
                "Bind mounts are allowed only with a confirmed local Unix Docker daemon",
                false,
            ));
        }
        Ok(endpoint)
    }

    async fn run(
        &self,
        args: Vec<OsString>,
        timeout: Duration,
    ) -> Result<CommandOutput, DockerError> {
        let endpoint_is_pinned = args
            .first()
            .is_some_and(|argument| argument == OsStr::new("--host"));
        let mut command = Command::new(&self.binary);
        command
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        if endpoint_is_pinned {
            command
                .env_remove("DOCKER_CONTEXT")
                .env_remove("DOCKER_HOST");
        }
        configure_docker_command(&mut command);
        let mut child = command.spawn().map_err(spawn_error)?;
        let mut process_group = ProcessGroupGuard::new(child.id());
        let stdout = child.stdout.take().ok_or_else(|| {
            DockerError::new(
                DockerErrorCode::Internal,
                "Docker stdout pipe was not created",
                false,
            )
        })?;
        let stderr = child.stderr.take().ok_or_else(|| {
            DockerError::new(
                DockerErrorCode::Internal,
                "Docker stderr pipe was not created",
                false,
            )
        })?;

        let execution = async {
            let wait = async {
                child.wait().await.map_err(|error| {
                    DockerError::new(
                        DockerErrorCode::Internal,
                        format!("Failed to wait for Docker CLI: {error}"),
                        true,
                    )
                })
            };
            let (status, stdout, stderr) = tokio::try_join!(
                wait,
                read_bounded_tail(stdout, MAX_CONTROL_OUTPUT),
                read_bounded_tail(stderr, MAX_ERROR_OUTPUT)
            )?;
            Result::<_, DockerError>::Ok((status, stdout, stderr))
        };

        let result = match tokio::time::timeout(timeout, execution).await {
            Ok(result) => result,
            Err(_) => {
                terminate_tokio_process(&mut child).await;
                return Err(DockerError::new(
                    DockerErrorCode::Timeout,
                    "Docker operation timed out",
                    true,
                ));
            }
        }?;
        process_group.disarm();
        let (status, (stdout, stdout_truncated), (stderr, stderr_truncated)) = result;
        Ok(CommandOutput {
            status,
            stdout,
            stderr,
            stdout_truncated,
            stderr_truncated,
        })
    }
}

fn is_local_unix_docker_endpoint(endpoint: &str) -> bool {
    endpoint
        .trim()
        .strip_prefix("unix://")
        .map(Path::new)
        .is_some_and(|path| path.is_absolute() && path.parent().is_some())
}

fn pin_docker_endpoint(args: &mut Vec<OsString>, endpoint: String) {
    args.insert(0, endpoint.into());
    args.insert(0, "--host".into());
}

struct ProcessGroupGuard {
    pid: Option<u32>,
}

impl ProcessGroupGuard {
    fn new(pid: Option<u32>) -> Self {
        Self { pid }
    }

    fn disarm(&mut self) {
        self.pid = None;
    }
}

impl Drop for ProcessGroupGuard {
    fn drop(&mut self) {
        if let Some(pid) = self.pid {
            terminate_process_group(pid);
        }
    }
}

#[cfg(unix)]
fn configure_docker_command(command: &mut Command) {
    use std::os::unix::process::CommandExt;

    command.as_std_mut().process_group(0);
}

#[cfg(not(unix))]
fn configure_docker_command(_command: &mut Command) {}

#[cfg(unix)]
fn terminate_process_group(pid: u32) {
    if let Ok(pid) = i32::try_from(pid) {
        unsafe {
            let _ = libc::kill(-pid, libc::SIGKILL);
        }
    }
}

#[cfg(not(unix))]
fn terminate_process_group(_pid: u32) {}

async fn terminate_tokio_process(child: &mut tokio::process::Child) {
    if let Some(pid) = child.id() {
        terminate_process_group(pid);
    }
    let _ = child.kill().await;
    let _ = child.wait().await;
}

async fn read_bounded_tail<R>(mut reader: R, limit: usize) -> Result<(Vec<u8>, bool), DockerError>
where
    R: AsyncRead + Unpin,
{
    let mut output = VecDeque::with_capacity(limit.min(64 * 1024));
    let mut buffer = [0_u8; 16 * 1024];
    let mut truncated = false;
    loop {
        let count = reader.read(&mut buffer).await.map_err(|error| {
            DockerError::new(
                DockerErrorCode::Internal,
                format!("Failed to read Docker output: {error}"),
                true,
            )
        })?;
        if count == 0 {
            break;
        }
        if count >= limit {
            output.clear();
            output.extend(&buffer[count - limit..count]);
            truncated = true;
            continue;
        }
        let overflow = output.len().saturating_add(count).saturating_sub(limit);
        if overflow > 0 {
            output.drain(..overflow);
            truncated = true;
        }
        output.extend(&buffer[..count]);
    }
    Ok((output.into(), truncated))
}

async fn probe_docker(runner: &DockerRunner) -> DockerStatus {
    probe_docker_with_timeout(runner, PROBE_TIMEOUT).await
}

async fn probe_docker_with_timeout(runner: &DockerRunner, timeout: Duration) -> DockerStatus {
    let checked_at = now_ts();
    match tokio::time::timeout(timeout, probe_docker_inner(runner, checked_at)).await {
        Ok(status) => status,
        Err(_) => DockerStatus {
            state: DockerStatusState::Error,
            cli_version: None,
            engine_version: None,
            api_version: None,
            compose_version: None,
            message: Some("Docker status probe timed out".to_string()),
            checked_at,
        },
    }
}

async fn probe_docker_inner(runner: &DockerRunner, checked_at: i64) -> DockerStatus {
    let cli_output = match runner.run(os_args(["--version"]), PROBE_TIMEOUT).await {
        Ok(output) if output.status.success() => output,
        Ok(output) => {
            let error = command_failure(&output);
            return probe_status_from_error(error, None, checked_at);
        }
        Err(error) => return probe_status_from_error(error, None, checked_at),
    };
    let cli_text = String::from_utf8_lossy(&cli_output.stdout);
    let cli_version = parse_cli_version(&cli_text);
    let Some((major, minor, _)) = cli_version.as_deref().and_then(parse_numeric_version) else {
        return DockerStatus {
            state: DockerStatusState::Error,
            cli_version,
            engine_version: None,
            api_version: None,
            compose_version: None,
            message: Some("Unable to determine the Docker CLI version".to_string()),
            checked_at,
        };
    };
    if major < MIN_DOCKER_MAJOR || (major == MIN_DOCKER_MAJOR && minor < MIN_DOCKER_MINOR) {
        return DockerStatus {
            state: DockerStatusState::UnsupportedVersion,
            cli_version,
            engine_version: None,
            api_version: None,
            compose_version: None,
            message: Some("Docker CLI 20.10 or newer is required".to_string()),
            checked_at,
        };
    }

    let version_output = match runner
        .run(
            os_args(["version", "--format", "{{json .}}"]),
            PROBE_TIMEOUT,
        )
        .await
    {
        Ok(output) if output.status.success() => output,
        Ok(output) => {
            return probe_status_from_error(command_failure(&output), cli_version, checked_at);
        }
        Err(error) => return probe_status_from_error(error, cli_version, checked_at),
    };
    let value = match parse_single_json(&version_output.stdout) {
        Ok(value) => value,
        Err(error) => return probe_status_from_error(error, cli_version, checked_at),
    };
    let engine_version = value
        .pointer("/Server/Version")
        .and_then(Value::as_str)
        .map(str::to_string);
    let api_version = value
        .pointer("/Server/ApiVersion")
        .and_then(Value::as_str)
        .map(str::to_string);
    if engine_version.is_none() || api_version.is_none() {
        return DockerStatus {
            state: DockerStatusState::Error,
            cli_version,
            engine_version,
            api_version,
            compose_version: None,
            message: Some("Docker daemon version response is missing Engine or API data".into()),
            checked_at,
        };
    }
    let compose_version = match runner
        .run(os_args(["compose", "version", "--short"]), PROBE_TIMEOUT)
        .await
    {
        Ok(output) if output.status.success() => nonempty_stdout(&output),
        _ => None,
    };
    DockerStatus {
        state: DockerStatusState::Ready,
        cli_version,
        engine_version,
        api_version,
        compose_version,
        message: None,
        checked_at,
    }
}

fn probe_status_from_error(
    error: DockerError,
    cli_version: Option<String>,
    checked_at: i64,
) -> DockerStatus {
    let state = match error.code {
        DockerErrorCode::NotInstalled => DockerStatusState::NotInstalled,
        DockerErrorCode::PermissionDenied => DockerStatusState::PermissionDenied,
        DockerErrorCode::DaemonUnavailable => DockerStatusState::DaemonUnreachable,
        DockerErrorCode::UnsupportedVersion => DockerStatusState::UnsupportedVersion,
        _ => DockerStatusState::Error,
    };
    DockerStatus {
        state,
        cli_version,
        engine_version: None,
        api_version: None,
        compose_version: None,
        message: Some(error.message),
        checked_at,
    }
}

fn parse_cli_version(output: &str) -> Option<String> {
    let marker = "version ";
    let tail = output.split_once(marker)?.1;
    let version = tail.split([',', ' ']).next()?.trim();
    (!version.is_empty()).then(|| version.to_string())
}

fn parse_numeric_version(version: &str) -> Option<(u64, u64, u64)> {
    let mut numbers = version
        .trim_start_matches('v')
        .split(['.', '-', '+'])
        .take(3)
        .map(str::parse::<u64>);
    Some((
        numbers.next()?.ok()?,
        numbers.next()?.ok()?,
        numbers.next().transpose().ok()?.unwrap_or(0),
    ))
}

fn protected_paths(config: &AgentConfig) -> Vec<PathBuf> {
    let mut paths = vec![
        PathBuf::from("/etc/om-agent"),
        PathBuf::from("/etc/operation-monitoring-agent"),
        PathBuf::from("/run/om-agent"),
        PathBuf::from("/run/operation-monitoring-agent"),
        PathBuf::from("/var/run/om-agent"),
        PathBuf::from("/var/run/operation-monitoring-agent"),
        PathBuf::from("/var/lib/om-agent"),
        PathBuf::from("/var/lib/operation-monitoring-agent"),
        PathBuf::from("/var/log/om-agent"),
        PathBuf::from("/var/log/operation-monitoring-agent"),
    ];
    paths.extend(
        [
            config.identity_file.as_ref(),
            config.state_dir.as_ref(),
            config.log_file.as_ref(),
            config.update_dir.as_ref(),
        ]
        .into_iter()
        .flatten()
        .cloned(),
    );
    if let Ok(path) = identity_path(config.identity_file.clone()) {
        paths.push(path);
    }
    if let Ok(runtime_paths) = docker_protected_paths(config) {
        paths.extend(runtime_paths);
    }
    if let Ok(path) = docker_protected_update_root(config) {
        paths.push(path);
    }
    if let Some(path) = env::var_os("DOCKER_CONFIG").map(PathBuf::from) {
        paths.push(path);
    } else if let Some(home) = env::var_os("HOME") {
        paths.push(PathBuf::from(home).join(".docker"));
    }
    if let Some(runtime) = env::var_os("XDG_RUNTIME_DIR") {
        paths.push(PathBuf::from(runtime).join("docker.sock"));
    }
    if let Some(socket) = env::var_os("DOCKER_HOST")
        .and_then(|value| value.into_string().ok())
        .and_then(|value| value.strip_prefix("unix://").map(PathBuf::from))
    {
        paths.push(socket);
    }
    paths.sort();
    paths.dedup();
    paths
}

async fn execute_request(
    runner: &DockerRunner,
    request: DockerRequest,
) -> Result<DockerResponse, DockerError> {
    match request {
        DockerRequest::ContainerList { all } => {
            let mut args = os_args(["container", "ls"]);
            if all {
                args.push("--all".into());
            }
            args.extend(os_args(["--format", "{{json .}}"]));
            data_command(runner, args, READ_TIMEOUT).await
        }
        DockerRequest::ContainerInspect { container } => {
            validate_identifier(&container, "container")?;
            data_command(
                runner,
                os_args(["container", "inspect", &container]),
                READ_TIMEOUT,
            )
            .await
        }
        DockerRequest::ContainerCreate { spec } => {
            let bind_endpoint = if spec
                .mounts
                .iter()
                .any(|mount| mount.kind == DockerMountKind::Bind)
            {
                Some(runner.ensure_local_unix_daemon().await?)
            } else {
                None
            };
            let mut args = container_create_args(runner, spec)?;
            if let Some(endpoint) = bind_endpoint {
                pin_docker_endpoint(&mut args, endpoint);
            }
            let output = checked_command(runner, args, LIFECYCLE_TIMEOUT).await?;
            Ok(operation_response_with_output(
                json!({
                    "id": nonempty_stdout(&output),
                    "completed": true
                }),
                &output,
            ))
        }
        DockerRequest::ContainerAction {
            container,
            action,
            timeout_seconds,
        } => {
            validate_identifier(&container, "container")?;
            let mut args = vec![OsString::from("container")];
            match action {
                DockerContainerAction::Start => args.push("start".into()),
                DockerContainerAction::Stop => {
                    args.push("stop".into());
                    args.push("-t".into());
                    args.push(
                        timeout_seconds
                            .unwrap_or(10)
                            .clamp(1, 120)
                            .to_string()
                            .into(),
                    );
                }
                DockerContainerAction::Restart => {
                    args.push("restart".into());
                    args.push("-t".into());
                    args.push(
                        timeout_seconds
                            .unwrap_or(10)
                            .clamp(1, 120)
                            .to_string()
                            .into(),
                    );
                }
                DockerContainerAction::Kill => args.push("kill".into()),
                DockerContainerAction::Pause => args.push("pause".into()),
                DockerContainerAction::Unpause => args.push("unpause".into()),
            }
            args.push(container.clone().into());
            let output = checked_command(runner, args, LIFECYCLE_TIMEOUT).await?;
            Ok(operation_response_with_output(
                json!({
                    "container": container,
                    "action": serde_json::to_value(action).unwrap_or(Value::Null),
                    "message": nonempty_stdout(&output),
                    "completed": true
                }),
                &output,
            ))
        }
        DockerRequest::ContainerRename { container, name } => {
            validate_identifier(&container, "container")?;
            validate_name(&name, "container name")?;
            let output = checked_command(
                runner,
                os_args(["container", "rename", &container, &name]),
                LIFECYCLE_TIMEOUT,
            )
            .await?;
            Ok(operation_response_with_output(
                json!({
                    "container": container,
                    "name": name,
                    "message": nonempty_stdout(&output),
                    "completed": true
                }),
                &output,
            ))
        }
        DockerRequest::ContainerRemove {
            container,
            force,
            remove_volumes,
        } => {
            validate_identifier(&container, "container")?;
            let mut args = os_args(["container", "rm"]);
            if force {
                args.push("--force".into());
            }
            if remove_volumes {
                args.push("--volumes".into());
            }
            args.push(container.clone().into());
            let output = checked_command(runner, args, LIFECYCLE_TIMEOUT).await?;
            Ok(operation_response_with_output(
                json!({
                    "container": container,
                    "message": nonempty_stdout(&output),
                    "completed": true
                }),
                &output,
            ))
        }
        DockerRequest::ContainerStats { container } => {
            let mut args = os_args(["container", "stats", "--no-stream"]);
            if let Some(container) = container {
                validate_identifier(&container, "container")?;
                args.push(container.into());
            }
            args.extend(os_args(["--format", "{{json .}}"]));
            data_command(runner, args, READ_TIMEOUT).await
        }
        DockerRequest::ImageList { all } => {
            let mut args = os_args(["image", "ls"]);
            if all {
                args.push("--all".into());
            }
            args.extend(os_args(["--format", "{{json .}}"]));
            data_command(runner, args, READ_TIMEOUT).await
        }
        DockerRequest::ImageInspect { image } => {
            validate_identifier(&image, "image")?;
            data_command(runner, os_args(["image", "inspect", &image]), READ_TIMEOUT).await
        }
        DockerRequest::ImagePull { image } => {
            validate_identifier(&image, "image")?;
            let output =
                checked_command(runner, os_args(["image", "pull", &image]), LONG_TIMEOUT).await?;
            Ok(operation_response_with_output(
                json!({
                    "image": image,
                    "message": last_nonempty_line(&output.stdout),
                    "completed": true
                }),
                &output,
            ))
        }
        DockerRequest::ImageTag { source, target } => {
            validate_identifier(&source, "source image")?;
            validate_identifier(&target, "target image")?;
            let output = checked_command(
                runner,
                os_args(["image", "tag", &source, &target]),
                LIFECYCLE_TIMEOUT,
            )
            .await?;
            Ok(operation_response_with_output(
                json!({
                    "source": source,
                    "target": target,
                    "completed": true
                }),
                &output,
            ))
        }
        DockerRequest::ImageRemove { image, force } => {
            validate_identifier(&image, "image")?;
            let mut args = os_args(["image", "rm"]);
            if force {
                args.push("--force".into());
            }
            args.push(image.clone().into());
            let output = checked_command(runner, args, LIFECYCLE_TIMEOUT).await?;
            Ok(operation_response_with_output(
                json!({
                    "image": image,
                    "message": last_nonempty_line(&output.stdout),
                    "completed": true
                }),
                &output,
            ))
        }
        DockerRequest::ImagePrune => prune_command(runner, "image", Vec::new(), LONG_TIMEOUT).await,
        DockerRequest::NetworkList => {
            data_command(
                runner,
                os_args(["network", "ls", "--format", "{{json .}}"]),
                READ_TIMEOUT,
            )
            .await
        }
        DockerRequest::NetworkInspect { network } => {
            validate_identifier(&network, "network")?;
            data_command(
                runner,
                os_args(["network", "inspect", &network]),
                READ_TIMEOUT,
            )
            .await
        }
        DockerRequest::NetworkCreate { spec } => {
            let name = spec.name.clone();
            let args = network_create_args(spec)?;
            let output = checked_command(runner, args, LIFECYCLE_TIMEOUT).await?;
            Ok(operation_response_with_output(
                json!({
                    "network": name,
                    "id": nonempty_stdout(&output),
                    "completed": true
                }),
                &output,
            ))
        }
        DockerRequest::NetworkConnect {
            network,
            container,
            aliases,
        } => {
            validate_identifier(&network, "network")?;
            validate_identifier(&container, "container")?;
            if aliases.len() > 16 {
                return Err(invalid("At most 16 network aliases are allowed"));
            }
            let mut args = os_args(["network", "connect"]);
            for alias in aliases {
                validate_name(&alias, "network alias")?;
                args.push("--alias".into());
                args.push(alias.into());
            }
            args.push(network.clone().into());
            args.push(container.clone().into());
            let output = checked_command(runner, args, LIFECYCLE_TIMEOUT).await?;
            Ok(operation_response_with_output(
                json!({
                    "network": network,
                    "container": container,
                    "completed": true
                }),
                &output,
            ))
        }
        DockerRequest::NetworkDisconnect {
            network,
            container,
            force,
        } => {
            validate_identifier(&network, "network")?;
            validate_identifier(&container, "container")?;
            let mut args = os_args(["network", "disconnect"]);
            if force {
                args.push("--force".into());
            }
            args.push(network.clone().into());
            args.push(container.clone().into());
            let output = checked_command(runner, args, LIFECYCLE_TIMEOUT).await?;
            Ok(operation_response_with_output(
                json!({
                    "network": network,
                    "container": container,
                    "completed": true
                }),
                &output,
            ))
        }
        DockerRequest::NetworkRemove { network } => {
            validate_identifier(&network, "network")?;
            let output = checked_command(
                runner,
                os_args(["network", "rm", &network]),
                LIFECYCLE_TIMEOUT,
            )
            .await?;
            Ok(operation_response_with_output(
                json!({
                    "network": network,
                    "message": nonempty_stdout(&output),
                    "completed": true
                }),
                &output,
            ))
        }
        DockerRequest::NetworkPrune => {
            prune_command(runner, "network", Vec::new(), LONG_TIMEOUT).await
        }
        DockerRequest::VolumeList => {
            data_command(
                runner,
                os_args(["volume", "ls", "--format", "{{json .}}"]),
                READ_TIMEOUT,
            )
            .await
        }
        DockerRequest::VolumeInspect { volume } => {
            validate_identifier(&volume, "volume")?;
            data_command(
                runner,
                os_args(["volume", "inspect", &volume]),
                READ_TIMEOUT,
            )
            .await
        }
        DockerRequest::VolumeCreate { spec } => {
            let name = spec.name.clone();
            let args = volume_create_args(spec)?;
            let output = checked_command(runner, args, LIFECYCLE_TIMEOUT).await?;
            Ok(operation_response_with_output(
                json!({
                    "volume": name,
                    "message": nonempty_stdout(&output),
                    "completed": true
                }),
                &output,
            ))
        }
        DockerRequest::VolumeRemove { volume, force } => {
            validate_identifier(&volume, "volume")?;
            let mut args = os_args(["volume", "rm"]);
            if force {
                args.push("--force".into());
            }
            args.push(volume.clone().into());
            let output = checked_command(runner, args, LIFECYCLE_TIMEOUT).await?;
            Ok(operation_response_with_output(
                json!({
                    "volume": volume,
                    "message": nonempty_stdout(&output),
                    "completed": true
                }),
                &output,
            ))
        }
        DockerRequest::VolumePrune => {
            prune_command(runner, "volume", Vec::new(), LONG_TIMEOUT).await
        }
        DockerRequest::ComposeList => {
            data_command(
                runner,
                os_args(["compose", "ls", "--all", "--format", "json"]),
                READ_TIMEOUT,
            )
            .await
        }
        DockerRequest::ComposeInspect { target } => {
            let mut args = compose_prefix(&target)?;
            args.extend(os_args(["ps", "--all", "--format", "json"]));
            data_command(runner, args, READ_TIMEOUT).await
        }
        DockerRequest::ComposeValidate { target } => {
            let validation = validate_compose(runner, &target).await?;
            Ok(DockerResponse::ComposeValidation { validation })
        }
        DockerRequest::ComposeDeploy {
            target,
            config_digest,
            confirm_high_risk,
        } => {
            let validated = load_compose_config(runner, &target).await?;
            verify_compose_confirmation(&validated.validation, &config_digest, confirm_high_risk)?;
            let bind_endpoint = if validated.uses_bind_mounts {
                Some(runner.ensure_local_unix_daemon().await?)
            } else {
                None
            };
            let mut snapshot = ComposeSnapshot::create(
                &runner.compose_snapshot_dir,
                validated
                    .validation
                    .project
                    .as_deref()
                    .or(target.project.as_deref()),
                &validated.normalized_config,
            )?;
            let snapshot_target = snapshot.target(&target);
            let mut args = compose_prefix(&snapshot_target)?;
            if let Some(endpoint) = bind_endpoint.as_ref() {
                pin_docker_endpoint(&mut args, endpoint.clone());
            }
            args.extend(os_args(["up", "--detach", "--no-build"]));
            append_compose_services(&mut args, &snapshot_target.services)?;
            snapshot.retain();
            let output = checked_command(runner, args, LONG_TIMEOUT).await;
            let preserve = output
                .as_ref()
                .ok()
                .map(|_| vec![snapshot.path.clone()])
                .unwrap_or_default();
            cleanup_compose_snapshots(
                runner,
                &snapshot.project_key,
                bind_endpoint.as_deref(),
                &preserve,
            )
            .await;
            let output = output?;
            Ok(operation_response_with_output(
                json!({
                    "project": validated.validation.project,
                    "services": validated.validation.services,
                    "message": last_nonempty_line(&output.stdout),
                    "completed": true
                }),
                &output,
            ))
        }
        DockerRequest::ComposeAction {
            target,
            action,
            config_digest,
            remove_volumes,
            confirm_high_risk,
        } => {
            execute_compose_action(
                runner,
                target,
                action,
                config_digest,
                remove_volumes,
                confirm_high_risk,
            )
            .await
        }
        DockerRequest::SystemDf => {
            data_command(
                runner,
                os_args(["system", "df", "--format", "{{json .}}"]),
                READ_TIMEOUT,
            )
            .await
        }
        DockerRequest::SystemPrune {
            containers,
            images,
            networks,
            volumes,
            all_images,
        } => execute_system_prune(runner, containers, images, networks, volumes, all_images).await,
    }
}

async fn execute_system_prune(
    runner: &DockerRunner,
    containers: bool,
    images: bool,
    networks: bool,
    volumes: bool,
    all_images: bool,
) -> Result<DockerResponse, DockerError> {
    if !(containers || images || networks || volumes) {
        return Err(invalid("Select at least one Docker resource type to prune"));
    }
    let mut results = Vec::new();
    if containers {
        results.push(prune_phase(runner, "container", Vec::new()).await);
    }
    if images {
        let extra = all_images
            .then(|| OsString::from("--all"))
            .into_iter()
            .collect();
        results.push(prune_phase(runner, "image", extra).await);
    }
    if networks {
        results.push(prune_phase(runner, "network", Vec::new()).await);
    }
    if volumes {
        results.push(prune_phase(runner, "volume", Vec::new()).await);
    }
    let succeeded = results
        .iter()
        .filter(|result| result.get("completed").and_then(Value::as_bool) == Some(true))
        .count();
    let failed = results.len().saturating_sub(succeeded);
    Ok(operation_response(json!({
        "resources": results,
        "completed": failed == 0,
        "partial_success": succeeded > 0 && failed > 0,
        "succeeded_stages": succeeded,
        "failed_stages": failed
    })))
}

async fn checked_command(
    runner: &DockerRunner,
    args: Vec<OsString>,
    timeout: Duration,
) -> Result<CommandOutput, DockerError> {
    let output = runner.run(args, timeout).await?;
    if output.status.success() {
        Ok(output)
    } else {
        Err(command_failure(&output))
    }
}

async fn data_command(
    runner: &DockerRunner,
    args: Vec<OsString>,
    timeout: Duration,
) -> Result<DockerResponse, DockerError> {
    let output = checked_command(runner, args, timeout).await?;
    ensure_complete_control_output(&output)?;
    Ok(DockerResponse::Data {
        data: parse_json_output(&output.stdout)?,
    })
}

fn operation_response(data: Value) -> DockerResponse {
    DockerResponse::OperationComplete { data }
}

fn operation_response_with_output(mut data: Value, output: &CommandOutput) -> DockerResponse {
    if let Value::Object(fields) = &mut data {
        fields.insert(
            "output_truncated".to_string(),
            Value::Bool(output.stdout_truncated || output.stderr_truncated),
        );
    }
    operation_response(data)
}

fn ensure_complete_control_output(output: &CommandOutput) -> Result<(), DockerError> {
    if output.stdout_truncated {
        return Err(DockerError::new(
            DockerErrorCode::OutputTooLarge,
            "Docker response exceeded the 4 MiB control-message limit",
            false,
        ));
    }
    Ok(())
}

async fn prune_command(
    runner: &DockerRunner,
    resource: &str,
    extra: Vec<OsString>,
    timeout: Duration,
) -> Result<DockerResponse, DockerError> {
    let mut args = vec![resource.into(), "prune".into(), "--force".into()];
    args.extend(extra);
    let output = checked_command(runner, args, timeout).await?;
    Ok(operation_response_with_output(
        json!({
            "resource": resource,
            "message": truncate_text(&String::from_utf8_lossy(&output.stdout), MAX_ERROR_MESSAGE),
            "completed": true
        }),
        &output,
    ))
}

async fn prune_data(
    runner: &DockerRunner,
    resource: &str,
    extra: Vec<OsString>,
) -> Result<Value, DockerError> {
    let response = prune_command(runner, resource, extra, LONG_TIMEOUT).await?;
    match response {
        DockerResponse::OperationComplete { data } => Ok(data),
        _ => unreachable!("prune command always returns operation_complete"),
    }
}

async fn prune_phase(runner: &DockerRunner, resource: &str, extra: Vec<OsString>) -> Value {
    match prune_data(runner, resource, extra).await {
        Ok(data) => data,
        Err(error) => json!({
            "resource": resource,
            "completed": false,
            "error": {
                "code": error.code,
                "message": error.message,
                "retryable": error.retryable,
                "exit_code": error.exit_code,
            }
        }),
    }
}

fn parse_json_output(output: &[u8]) -> Result<Value, DockerError> {
    let text = std::str::from_utf8(output).map_err(|_| {
        DockerError::new(
            DockerErrorCode::CommandFailed,
            "Docker returned non-UTF-8 control output",
            false,
        )
    })?;
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return Ok(Value::Array(Vec::new()));
    }
    if let Ok(value) = serde_json::from_str(trimmed) {
        return Ok(value);
    }
    let mut values = Vec::new();
    for line in trimmed.lines().filter(|line| !line.trim().is_empty()) {
        values.push(serde_json::from_str(line).map_err(|_| {
            DockerError::new(
                DockerErrorCode::CommandFailed,
                "Docker returned malformed JSON output",
                false,
            )
        })?);
    }
    Ok(Value::Array(values))
}

fn parse_single_json(output: &[u8]) -> Result<Value, DockerError> {
    let value = parse_json_output(output)?;
    if value.is_array() {
        return Err(DockerError::new(
            DockerErrorCode::CommandFailed,
            "Docker returned an unexpected JSON response",
            false,
        ));
    }
    Ok(value)
}

fn os_args<const N: usize>(args: [&str; N]) -> Vec<OsString> {
    args.into_iter().map(OsString::from).collect()
}

fn container_create_args(
    runner: &DockerRunner,
    spec: DockerContainerCreateSpec,
) -> Result<Vec<OsString>, DockerError> {
    validate_identifier(&spec.image, "image")?;
    if let Some(name) = &spec.name {
        validate_name(name, "container name")?;
    }
    if spec.command.len() > 256 {
        return Err(invalid(
            "Container command may contain at most 256 arguments",
        ));
    }
    if spec.environment.len() > 256 {
        return Err(invalid("At most 256 environment variables are allowed"));
    }
    if spec.ports.len() > 128 {
        return Err(invalid("At most 128 published ports are allowed"));
    }
    if spec.mounts.len() > 128 {
        return Err(invalid("At most 128 mounts are allowed"));
    }

    let mut args = os_args(["container", "create"]);
    if let Some(name) = spec.name {
        args.push("--name".into());
        args.push(name.into());
    }
    for (key, value) in spec.environment {
        validate_environment_key(&key)?;
        validate_argument(&value, "environment value", 64 * 1024)?;
        args.push("--env".into());
        args.push(format!("{key}={value}").into());
    }
    for port in spec.ports {
        if port.container_port == 0 {
            return Err(invalid("Container port must be between 1 and 65535"));
        }
        if let Some(host_ip) = &port.host_ip {
            validate_host_ip(host_ip)?;
        }
        let protocol = match port.protocol {
            DockerPortProtocol::Tcp => "tcp",
            DockerPortProtocol::Udp => "udp",
        };
        let container = format!("{}/{protocol}", port.container_port);
        let published = match (port.host_ip, port.host_port) {
            (Some(ip), Some(host_port)) => {
                let ip = format_published_host_ip(&ip);
                format!("{ip}:{host_port}:{container}")
            }
            (Some(ip), None) => {
                let ip = format_published_host_ip(&ip);
                format!("{ip}::{container}")
            }
            (None, Some(host_port)) => format!("{host_port}:{container}"),
            (None, None) => container,
        };
        args.push("--publish".into());
        args.push(published.into());
    }
    for mount in spec.mounts {
        validate_container_path(&mount.target)?;
        validate_mount_token(&mount.target, "mount target")?;
        let (kind, source) = match mount.kind {
            DockerMountKind::Volume => {
                validate_name(&mount.source, "volume")?;
                ("volume", mount.source)
            }
            DockerMountKind::Bind => {
                let canonical = validate_bind_source(&mount.source, &runner.protected_paths)?;
                if !mount.read_only && !spec.confirm_bind_write {
                    return Err(invalid(
                        "Read-write bind mounts require explicit confirmation",
                    ));
                }
                let canonical = canonical
                    .into_os_string()
                    .into_string()
                    .map_err(|_| invalid("Canonical bind mount source is not valid UTF-8"))?;
                ("bind", canonical)
            }
        };
        validate_mount_token(&source, "mount source")?;
        let mut value = format!("type={kind},source={},target={}", source, mount.target);
        if mount.read_only {
            value.push_str(",readonly");
        }
        args.push("--mount".into());
        args.push(value.into());
    }
    if let Some(network) = spec.network {
        validate_identifier(&network, "network")?;
        args.push("--network".into());
        args.push(network.into());
    }
    if let Some(policy) = spec.restart_policy {
        let value = match policy {
            DockerRestartPolicy::No => "no".to_string(),
            DockerRestartPolicy::Always => "always".to_string(),
            DockerRestartPolicy::UnlessStopped => "unless-stopped".to_string(),
            DockerRestartPolicy::OnFailure => match spec.restart_max_retries {
                Some(retries) => format!("on-failure:{retries}"),
                None => "on-failure".to_string(),
            },
        };
        if spec.restart_max_retries.is_some() && policy != DockerRestartPolicy::OnFailure {
            return Err(invalid(
                "Restart retry count is valid only for the on_failure policy",
            ));
        }
        args.push("--restart".into());
        args.push(value.into());
    } else if spec.restart_max_retries.is_some() {
        return Err(invalid(
            "Restart retry count requires an explicit restart policy",
        ));
    }
    if let Some(cpus) = spec.cpus {
        if !cpus.is_finite() || !(0.01..=1024.0).contains(&cpus) {
            return Err(invalid("CPU limit must be between 0.01 and 1024"));
        }
        args.push("--cpus".into());
        args.push(cpus.to_string().into());
    }
    if let Some(memory_bytes) = spec.memory_bytes {
        if memory_bytes < 4 * 1024 * 1024 {
            return Err(invalid("Memory limit must be at least 4 MiB"));
        }
        args.push("--memory".into());
        args.push(memory_bytes.to_string().into());
    }
    args.push(spec.image.into());
    for argument in spec.command {
        validate_argument(&argument, "command argument", 64 * 1024)?;
        args.push(argument.into());
    }
    Ok(args)
}

fn network_create_args(spec: DockerNetworkCreateSpec) -> Result<Vec<OsString>, DockerError> {
    validate_name(&spec.name, "network name")?;
    validate_labels(&spec.labels)?;
    let mut args = os_args(["network", "create"]);
    if let Some(driver) = spec.driver {
        validate_identifier(&driver, "network driver")?;
        args.push("--driver".into());
        args.push(driver.into());
    }
    if spec.internal {
        args.push("--internal".into());
    }
    if spec.attachable {
        args.push("--attachable".into());
    }
    append_labels(&mut args, spec.labels);
    args.push(spec.name.into());
    Ok(args)
}

fn volume_create_args(spec: DockerVolumeCreateSpec) -> Result<Vec<OsString>, DockerError> {
    validate_name(&spec.name, "volume name")?;
    validate_labels(&spec.labels)?;
    let mut args = os_args(["volume", "create"]);
    if let Some(driver) = spec.driver {
        validate_identifier(&driver, "volume driver")?;
        args.push("--driver".into());
        args.push(driver.into());
    }
    append_labels(&mut args, spec.labels);
    args.push(spec.name.into());
    Ok(args)
}

fn validate_labels(labels: &BTreeMap<String, String>) -> Result<(), DockerError> {
    if labels.len() > 128 {
        return Err(invalid("At most 128 labels are allowed"));
    }
    for (key, value) in labels {
        validate_environment_key(key)?;
        validate_argument(value, "label value", 4096)?;
    }
    Ok(())
}

fn append_labels(args: &mut Vec<OsString>, labels: BTreeMap<String, String>) {
    for (key, value) in labels {
        args.push("--label".into());
        args.push(format!("{key}={value}").into());
    }
}

fn compose_prefix(target: &DockerComposeTarget) -> Result<Vec<OsString>, DockerError> {
    validate_compose_target(target)?;
    let mut args = os_args(["compose"]);
    for file in &target.files {
        let path = validate_compose_file(file)?;
        args.push("--file".into());
        args.push(path.into_os_string());
    }
    if let Some(project) = &target.project {
        validate_name(project, "Compose project")?;
        args.push("--project-name".into());
        args.push(project.into());
    }
    for profile in &target.profiles {
        validate_name(profile, "Compose profile")?;
        args.push("--profile".into());
        args.push(profile.into());
    }
    Ok(args)
}

fn validate_compose_target(target: &DockerComposeTarget) -> Result<(), DockerError> {
    if target.files.len() > 8 {
        return Err(invalid("Compose supports at most 8 configuration files"));
    }
    if target.profiles.len() > 32 {
        return Err(invalid("Compose supports at most 32 selected profiles"));
    }
    if target.services.len() > 128 {
        return Err(invalid("Compose supports at most 128 selected services"));
    }
    if target.files.is_empty() && target.project.is_none() {
        return Err(invalid(
            "Compose requires configuration files or an explicit project name",
        ));
    }
    for service in &target.services {
        validate_name(service, "Compose service")?;
    }
    Ok(())
}

fn validate_compose_file(value: &str) -> Result<PathBuf, DockerError> {
    validate_argument(value, "Compose file path", 16 * 1024)?;
    let path = Path::new(value);
    if !path.is_absolute() {
        return Err(invalid("Compose file paths must be absolute"));
    }
    let extension = path.extension().and_then(OsStr::to_str);
    if !matches!(extension, Some("yml" | "yaml")) {
        return Err(invalid("Compose files must use a .yml or .yaml extension"));
    }
    let canonical = std::fs::canonicalize(path).map_err(|error| {
        DockerError::new(
            if error.kind() == io::ErrorKind::NotFound {
                DockerErrorCode::NotFound
            } else if error.kind() == io::ErrorKind::PermissionDenied {
                DockerErrorCode::PermissionDenied
            } else {
                DockerErrorCode::InvalidRequest
            },
            format!("Cannot access Compose file: {error}"),
            false,
        )
    })?;
    if !canonical.is_file() {
        return Err(invalid("Compose path is not a regular file"));
    }
    Ok(canonical)
}

fn append_compose_services(
    args: &mut Vec<OsString>,
    services: &[String],
) -> Result<(), DockerError> {
    for service in services {
        validate_name(service, "Compose service")?;
        args.push(service.into());
    }
    Ok(())
}

async fn validate_compose(
    runner: &DockerRunner,
    target: &DockerComposeTarget,
) -> Result<DockerComposeValidation, DockerError> {
    Ok(load_compose_config(runner, target).await?.validation)
}

struct LoadedComposeConfig {
    validation: DockerComposeValidation,
    normalized_config: Vec<u8>,
    uses_bind_mounts: bool,
}

async fn load_compose_config(
    runner: &DockerRunner,
    target: &DockerComposeTarget,
) -> Result<LoadedComposeConfig, DockerError> {
    let mut args = compose_prefix(target)?;
    args.extend(os_args(["config", "--format", "json"]));
    let output = checked_command(runner, args, READ_TIMEOUT).await?;
    ensure_complete_control_output(&output)?;
    let config = parse_single_json(&output.stdout)?;
    let uses_bind_mounts = compose_uses_bind_mounts(&config);
    let project = config
        .get("name")
        .and_then(Value::as_str)
        .map(str::to_string)
        .or_else(|| target.project.clone());
    let services_object = config
        .get("services")
        .and_then(Value::as_object)
        .ok_or_else(|| {
            DockerError::new(
                DockerErrorCode::CommandFailed,
                "Compose configuration did not contain a services object",
                false,
            )
        })?;
    let services = services_object.keys().cloned().collect::<Vec<_>>();
    let service_summaries = compose_service_summaries(services_object);
    let config_summary = DockerComposeConfigSummary {
        service_count: object_len(&config, "services"),
        network_count: object_len(&config, "networks"),
        volume_count: object_len(&config, "volumes"),
        config_count: object_len(&config, "configs"),
        secret_count: object_len(&config, "secrets"),
    };
    let warnings = compose_warnings(&config);
    let config_digest = format!(
        "sha256:{}",
        crate::hex::encode_lower(Sha256::digest(&output.stdout))
    );
    Ok(LoadedComposeConfig {
        validation: DockerComposeValidation {
            project,
            services,
            service_summaries,
            config_summary,
            warnings,
            config_digest,
        },
        normalized_config: output.stdout,
        uses_bind_mounts,
    })
}

fn compose_uses_bind_mounts(config: &Value) -> bool {
    let service_bind = config
        .get("services")
        .and_then(Value::as_object)
        .is_some_and(|services| {
            services.values().any(|service| {
                if service.get("use_api_socket").and_then(Value::as_bool) == Some(true) {
                    return true;
                }
                service
                    .get("volumes")
                    .and_then(Value::as_array)
                    .is_some_and(|volumes| {
                        volumes.iter().any(|volume| {
                            volume.as_object().is_some_and(|volume| {
                                volume.get("type").and_then(Value::as_str) == Some("bind")
                            }) || volume.as_str().is_some_and(|volume| {
                                volume
                                    .split(':')
                                    .next()
                                    .is_some_and(|source| Path::new(source).is_absolute())
                            })
                        })
                    })
            })
        });
    if service_bind {
        return true;
    }

    config
        .get("volumes")
        .and_then(Value::as_object)
        .is_some_and(|volumes| {
            volumes.values().any(|volume| {
                let Some(options) = volume.get("driver_opts").and_then(Value::as_object) else {
                    return false;
                };
                matches!(
                    options.get("type").and_then(Value::as_str),
                    Some("none" | "bind")
                ) || options
                    .get("o")
                    .and_then(Value::as_str)
                    .is_some_and(|options| {
                        options
                            .split(',')
                            .any(|option| matches!(option.trim(), "bind" | "rbind"))
                    })
            })
        })
}

fn object_len(config: &Value, key: &str) -> u32 {
    config
        .get(key)
        .and_then(Value::as_object)
        .map(|items| u32::try_from(items.len()).unwrap_or(u32::MAX))
        .unwrap_or(0)
}

fn compose_service_summaries(
    services: &serde_json::Map<String, Value>,
) -> Vec<DockerComposeServiceSummary> {
    services
        .iter()
        .map(|(name, service)| {
            let service = service.as_object();
            let image = service
                .and_then(|service| service.get("image"))
                .and_then(Value::as_str)
                .map(|image| truncate_text(image, 1024));
            let ports = service
                .and_then(|service| service.get("ports"))
                .and_then(Value::as_array)
                .map(|ports| {
                    ports
                        .iter()
                        .filter_map(compose_port_summary)
                        .take(128)
                        .collect()
                })
                .unwrap_or_default();
            let mounts = service
                .and_then(|service| service.get("volumes"))
                .and_then(Value::as_array)
                .map(|mounts| {
                    mounts
                        .iter()
                        .filter_map(compose_mount_summary)
                        .take(128)
                        .collect()
                })
                .unwrap_or_default();
            let mut networks = service
                .and_then(|service| service.get("networks"))
                .and_then(Value::as_object)
                .map(|networks| networks.keys().cloned().take(128).collect::<Vec<_>>())
                .unwrap_or_default();
            networks.sort();
            let profiles = service
                .and_then(|service| service.get("profiles"))
                .and_then(Value::as_array)
                .map(|profiles| {
                    profiles
                        .iter()
                        .filter_map(Value::as_str)
                        .map(|profile| truncate_text(profile, 128))
                        .take(32)
                        .collect()
                })
                .unwrap_or_default();
            DockerComposeServiceSummary {
                name: name.clone(),
                image,
                ports,
                mounts,
                networks,
                profiles,
            }
        })
        .collect()
}

fn compose_port_summary(port: &Value) -> Option<String> {
    if let Some(port) = port.as_str() {
        return Some(truncate_text(port, 256));
    }
    let port = port.as_object()?;
    let target = compose_scalar(port.get("target")?)?;
    let protocol = port
        .get("protocol")
        .and_then(Value::as_str)
        .unwrap_or("tcp");
    let published = port.get("published").and_then(compose_scalar);
    let host_ip = port.get("host_ip").and_then(Value::as_str);
    let value = match (host_ip, published) {
        (Some(ip), Some(published)) => format!("{ip}:{published}:{target}/{protocol}"),
        (None, Some(published)) => format!("{published}:{target}/{protocol}"),
        _ => format!("{target}/{protocol}"),
    };
    Some(truncate_text(&value, 256))
}

fn compose_mount_summary(mount: &Value) -> Option<String> {
    if let Some(mount) = mount.as_str() {
        return Some(truncate_text(mount, 1024));
    }
    let mount = mount.as_object()?;
    let kind = mount
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or("volume");
    let source = mount
        .get("source")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let target = mount.get("target").and_then(Value::as_str)?;
    let access = if mount.get("read_only").and_then(Value::as_bool) == Some(true) {
        "ro"
    } else {
        "rw"
    };
    Some(truncate_text(
        &format!("{kind}:{source}:{target}:{access}"),
        1024,
    ))
}

fn compose_scalar(value: &Value) -> Option<String> {
    match value {
        Value::String(value) => Some(value.clone()),
        Value::Number(value) => Some(value.to_string()),
        _ => None,
    }
}

fn compose_warnings(config: &Value) -> Vec<String> {
    let mut warnings = Vec::new();
    let services = config
        .get("services")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    for (name, service) in services {
        let Some(service) = service.as_object() else {
            continue;
        };
        if service.get("privileged").and_then(Value::as_bool) == Some(true) {
            warnings.push(format!("Service {name} runs in privileged mode"));
        }
        if service.get("use_api_socket").and_then(Value::as_bool) == Some(true) {
            warnings.push(format!(
                "Service {name} enables use_api_socket and can control the Docker Engine"
            ));
        }
        for key in [
            "network_mode",
            "pid",
            "ipc",
            "uts",
            "userns_mode",
            "cgroup",
            "cgroupns_mode",
        ] {
            if service.get(key).and_then(Value::as_str) == Some("host") {
                warnings.push(format!("Service {name} uses host {key}"));
            }
        }
        for key in [
            "cap_add",
            "devices",
            "device_cgroup_rules",
            "security_opt",
            "volumes_from",
        ] {
            if service.get(key).is_some_and(nonempty_compose_value) {
                warnings.push(format!("Service {name} configures {key}"));
            }
        }
        if service.get("sysctls").is_some_and(nonempty_compose_value) {
            warnings.push(format!("Service {name} configures sysctls"));
        }
        if let Some(volumes) = service.get("volumes").and_then(Value::as_array) {
            for volume in volumes {
                let source = volume
                    .as_object()
                    .filter(|volume| volume.get("type").and_then(Value::as_str) == Some("bind"))
                    .and_then(|volume| volume.get("source"))
                    .and_then(Value::as_str)
                    .or_else(|| {
                        volume.as_str().and_then(|volume| {
                            let source = volume.split(':').next()?;
                            source.starts_with('/').then_some(source)
                        })
                    });
                if let Some(source) = source {
                    warnings.push(format!("Service {name} bind-mounts host path {source}"));
                }
            }
        }
    }
    if let Some(volumes) = config.get("volumes").and_then(Value::as_object) {
        for (name, volume) in volumes {
            let Some(options) = volume.get("driver_opts").and_then(Value::as_object) else {
                continue;
            };
            let kind = options
                .get("type")
                .and_then(Value::as_str)
                .unwrap_or_default();
            let mount_options = options.get("o").and_then(Value::as_str).unwrap_or_default();
            let device = options
                .get("device")
                .and_then(Value::as_str)
                .unwrap_or_default();
            if matches!(kind, "none" | "bind")
                || mount_options
                    .split(',')
                    .any(|option| matches!(option.trim(), "bind" | "rbind"))
            {
                warnings.push(format!(
                    "Volume {name} bind-mounts host path {}",
                    if device.is_empty() {
                        "<unspecified>"
                    } else {
                        device
                    }
                ));
            }
        }
    }
    for section in ["configs", "secrets"] {
        if let Some(entries) = config.get(section).and_then(Value::as_object) {
            for (name, entry) in entries {
                if let Some(path) = entry.get("file").and_then(Value::as_str) {
                    warnings.push(format!("Compose {section} {name} reads host file {path}"));
                }
            }
        }
    }
    warnings.sort();
    warnings.dedup();
    warnings
}

fn nonempty_compose_value(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(value) => *value,
        Value::String(value) => !value.is_empty(),
        Value::Array(value) => !value.is_empty(),
        Value::Object(value) => !value.is_empty(),
        Value::Number(_) => true,
    }
}

struct ComposeSnapshot {
    path: PathBuf,
    project_key: String,
    remove_on_drop: bool,
}

impl ComposeSnapshot {
    fn create(directory: &Path, project: Option<&str>, config: &[u8]) -> Result<Self, DockerError> {
        fs::create_dir_all(directory).map_err(|error| {
            DockerError::new(
                DockerErrorCode::Internal,
                format!("Failed to create protected Compose snapshot directory: {error}"),
                true,
            )
        })?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(directory, fs::Permissions::from_mode(0o700)).map_err(|error| {
                DockerError::new(
                    DockerErrorCode::Internal,
                    format!("Failed to protect Compose snapshot directory: {error}"),
                    true,
                )
            })?;
        }
        let project_key = project.map_or_else(
            || crate::hex::encode_lower(Sha256::digest(config)),
            |project| crate::hex::encode_lower(Sha256::digest(project.as_bytes())),
        );
        let config_key = crate::hex::encode_lower(Sha256::digest(config));
        let version = uuid::Uuid::new_v4();
        let path = directory.join(format!("{project_key}-{config_key}.yaml"));
        if snapshot_matches(&path, config)? {
            return Ok(Self {
                path,
                project_key,
                remove_on_drop: false,
            });
        }
        let temporary = directory.join(format!(".{project_key}-{version}.tmp"));
        let mut options = fs::OpenOptions::new();
        options.create_new(true).write(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(&temporary).map_err(|error| {
            DockerError::new(
                DockerErrorCode::Internal,
                format!("Failed to create protected Compose snapshot: {error}"),
                true,
            )
        })?;
        if let Err(error) = file.write_all(config).and_then(|_| file.sync_all()) {
            let _ = fs::remove_file(&temporary);
            return Err(DockerError::new(
                DockerErrorCode::Internal,
                format!("Failed to write protected Compose snapshot: {error}"),
                true,
            ));
        }
        drop(file);
        match fs::hard_link(&temporary, &path) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                if !snapshot_matches(&path, config)? {
                    let _ = fs::remove_file(&temporary);
                    return Err(DockerError::new(
                        DockerErrorCode::Internal,
                        "Compose snapshot path contains unexpected content",
                        false,
                    ));
                }
                let _ = fs::remove_file(&temporary);
                return Ok(Self {
                    path,
                    project_key,
                    remove_on_drop: false,
                });
            }
            Err(error) => {
                let _ = fs::remove_file(&temporary);
                return Err(DockerError::new(
                    DockerErrorCode::Internal,
                    format!("Failed to publish protected Compose snapshot: {error}"),
                    true,
                ));
            }
        }
        let _ = fs::remove_file(&temporary);
        Ok(Self {
            path,
            project_key,
            remove_on_drop: true,
        })
    }

    fn target(&self, target: &DockerComposeTarget) -> DockerComposeTarget {
        let mut target = target.clone();
        target.files = vec![self.path.to_string_lossy().into_owned()];
        target
    }

    fn retain(&mut self) {
        self.remove_on_drop = false;
    }
}

impl Drop for ComposeSnapshot {
    fn drop(&mut self) {
        if self.remove_on_drop {
            let _ = fs::remove_file(&self.path);
        }
    }
}

fn snapshot_matches(path: &Path, config: &[u8]) -> Result<bool, DockerError> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(error) => {
            return Err(DockerError::new(
                DockerErrorCode::Internal,
                format!("Failed to inspect protected Compose snapshot: {error}"),
                true,
            ));
        }
    };
    if !metadata.file_type().is_file() {
        return Err(DockerError::new(
            DockerErrorCode::Internal,
            "Compose snapshot path is not a regular file",
            false,
        ));
    }
    fs::read(path)
        .map(|contents| contents == config)
        .map_err(|error| {
            DockerError::new(
                DockerErrorCode::Internal,
                format!("Failed to read protected Compose snapshot: {error}"),
                true,
            )
        })
}

fn is_managed_compose_snapshot(path: &Path) -> bool {
    let Some(stem) = path.file_stem().and_then(OsStr::to_str) else {
        return false;
    };
    let mut parts = stem.splitn(3, '-');
    let (Some(project_key), Some(config_key)) = (parts.next(), parts.next()) else {
        return false;
    };
    is_sha256_key(project_key)
        && is_sha256_key(config_key)
        && parts
            .next()
            .is_none_or(|version| uuid::Uuid::parse_str(version).is_ok())
}

fn is_sha256_key(value: &str) -> bool {
    value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn remove_unreferenced_compose_files(
    directory: &Path,
    project_key: &str,
    labels: &[String],
    preserve: &[PathBuf],
) {
    let Ok(canonical_directory) = fs::canonicalize(directory) else {
        return;
    };
    let preserved = preserve
        .iter()
        .filter_map(|path| fs::canonicalize(path).ok())
        .collect::<Vec<_>>();
    let Ok(entries) = fs::read_dir(directory) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(canonical) = fs::canonicalize(&path) else {
            continue;
        };
        if canonical.parent() == Some(canonical_directory.as_path())
            && canonical.extension().and_then(OsStr::to_str) == Some("yaml")
            && is_managed_compose_snapshot(&canonical)
            && canonical
                .file_stem()
                .and_then(OsStr::to_str)
                .is_some_and(|stem| stem.starts_with(&format!("{project_key}-")))
            && !preserved.iter().any(|path| path == &canonical)
            && !labels.iter().any(|labels| {
                labels.contains(path.to_string_lossy().as_ref())
                    || canonical.to_str().is_some_and(|path| labels.contains(path))
            })
        {
            let _ = fs::remove_file(canonical);
        }
    }
}

async fn cleanup_compose_snapshots(
    runner: &DockerRunner,
    project_key: &str,
    endpoint: Option<&str>,
    preserve: &[PathBuf],
) {
    let mut args = os_args([
        "container",
        "ls",
        "--all",
        "--no-trunc",
        "--format",
        "{{json (.Label \"com.docker.compose.project.config_files\")}}",
    ]);
    if let Some(endpoint) = endpoint {
        pin_docker_endpoint(&mut args, endpoint.to_string());
    }
    let Ok(output) = checked_command(runner, args, PROBE_TIMEOUT).await else {
        return;
    };
    if ensure_complete_control_output(&output).is_err() {
        return;
    }
    let Ok(text) = std::str::from_utf8(&output.stdout) else {
        return;
    };
    let mut labels = Vec::new();
    for line in text.lines().filter(|line| !line.trim().is_empty()) {
        let Ok(value) = serde_json::from_str::<String>(line) else {
            return;
        };
        labels.push(value);
    }
    remove_unreferenced_compose_files(&runner.compose_snapshot_dir, project_key, &labels, preserve);
}

fn verify_compose_confirmation(
    validation: &DockerComposeValidation,
    expected_digest: &str,
    confirm_high_risk: bool,
) -> Result<(), DockerError> {
    verify_compose_digest(validation, expected_digest)?;
    verify_compose_high_risk_confirmation(validation, confirm_high_risk)
}

fn verify_compose_high_risk_confirmation(
    validation: &DockerComposeValidation,
    confirm_high_risk: bool,
) -> Result<(), DockerError> {
    if !validation.warnings.is_empty() && !confirm_high_risk {
        return Err(invalid(
            "Compose configuration contains high-risk settings that require explicit confirmation",
        ));
    }
    Ok(())
}

fn verify_compose_action_confirmation(
    action: &DockerComposeAction,
    validation: &DockerComposeValidation,
    confirm_high_risk: bool,
) -> Result<(), DockerError> {
    if action == &DockerComposeAction::Up {
        verify_compose_high_risk_confirmation(validation, confirm_high_risk)?;
    }
    Ok(())
}

fn verify_compose_digest(
    validation: &DockerComposeValidation,
    expected_digest: &str,
) -> Result<(), DockerError> {
    if validation.config_digest != expected_digest {
        return Err(DockerError::new(
            DockerErrorCode::Conflict,
            "Compose configuration changed after validation; validate it again",
            true,
        ));
    }
    Ok(())
}

async fn execute_compose_action(
    runner: &DockerRunner,
    target: DockerComposeTarget,
    action: DockerComposeAction,
    config_digest: String,
    remove_volumes: bool,
    confirm_high_risk: bool,
) -> Result<DockerResponse, DockerError> {
    if remove_volumes && action != DockerComposeAction::Down {
        return Err(invalid(
            "Compose volume removal is valid only for the down action",
        ));
    }
    if remove_volumes && !confirm_high_risk {
        return Err(invalid(
            "Removing Compose volumes requires explicit confirmation",
        ));
    }
    let validated = load_compose_config(runner, &target).await?;
    verify_compose_digest(&validated.validation, &config_digest)?;
    verify_compose_action_confirmation(&action, &validated.validation, confirm_high_risk)?;

    let bind_endpoint = if action == DockerComposeAction::Up && validated.uses_bind_mounts {
        Some(runner.ensure_local_unix_daemon().await?)
    } else {
        None
    };
    let mut snapshot = ComposeSnapshot::create(
        &runner.compose_snapshot_dir,
        validated
            .validation
            .project
            .as_deref()
            .or(target.project.as_deref()),
        &validated.normalized_config,
    )?;
    let snapshot_target = snapshot.target(&target);
    let mut args = compose_prefix(&snapshot_target)?;
    if let Some(endpoint) = bind_endpoint.as_ref() {
        pin_docker_endpoint(&mut args, endpoint.clone());
    }
    match action {
        DockerComposeAction::Pull => args.push("pull".into()),
        DockerComposeAction::Up => {
            args.extend(os_args(["up", "--detach", "--no-build"]));
        }
        DockerComposeAction::Start => args.push("start".into()),
        DockerComposeAction::Stop => args.push("stop".into()),
        DockerComposeAction::Restart => args.push("restart".into()),
        DockerComposeAction::Down => {
            args.push("down".into());
            if remove_volumes {
                args.push("--volumes".into());
            }
        }
    }
    if action != DockerComposeAction::Down {
        append_compose_services(&mut args, &snapshot_target.services)?;
    }
    let timeout = match action {
        DockerComposeAction::Pull | DockerComposeAction::Up | DockerComposeAction::Down => {
            LONG_TIMEOUT
        }
        _ => LIFECYCLE_TIMEOUT,
    };
    if action == DockerComposeAction::Up {
        snapshot.retain();
    }
    let output = checked_command(runner, args, timeout).await;
    if matches!(action, DockerComposeAction::Up | DockerComposeAction::Down) {
        let preserve = (action == DockerComposeAction::Up && output.is_ok())
            .then(|| vec![snapshot.path.clone()])
            .unwrap_or_default();
        cleanup_compose_snapshots(
            runner,
            &snapshot.project_key,
            bind_endpoint.as_deref(),
            &preserve,
        )
        .await;
    }
    let output = output?;
    Ok(operation_response_with_output(
        json!({
            "project": target.project,
            "action": serde_json::to_value(action).unwrap_or(Value::Null),
            "message": last_nonempty_line(&output.stdout),
            "completed": true
        }),
        &output,
    ))
}

fn validate_identifier(value: &str, label: &str) -> Result<(), DockerError> {
    validate_argument(value, label, 1024)?;
    if value.trim().is_empty() || value.starts_with('-') {
        return Err(invalid(format!("Invalid Docker {label}")));
    }
    Ok(())
}

fn validate_name(value: &str, label: &str) -> Result<(), DockerError> {
    validate_argument(value, label, 128)?;
    let mut chars = value.chars();
    if !chars
        .next()
        .is_some_and(|character| character.is_ascii_alphanumeric())
        || chars.any(|character| {
            !(character.is_ascii_alphanumeric() || matches!(character, '_' | '-' | '.'))
        })
    {
        return Err(invalid(format!("Invalid Docker {label}")));
    }
    Ok(())
}

fn validate_argument(value: &str, label: &str, max_bytes: usize) -> Result<(), DockerError> {
    if value.as_bytes().contains(&0) || value.len() > max_bytes {
        return Err(invalid(format!("Invalid Docker {label}")));
    }
    Ok(())
}

fn validate_environment_key(key: &str) -> Result<(), DockerError> {
    if key.is_empty()
        || key.len() > 256
        || key.contains('=')
        || key.as_bytes().contains(&0)
        || !key
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || character == '_')
    {
        return Err(invalid("Invalid environment variable or label key"));
    }
    Ok(())
}

fn validate_host_ip(value: &str) -> Result<(), DockerError> {
    value
        .parse::<std::net::IpAddr>()
        .map(|_| ())
        .map_err(|_| invalid("Published-port host_ip must be an IPv4 or IPv6 address"))
}

fn format_published_host_ip(value: &str) -> String {
    if value.contains(':') {
        format!("[{value}]")
    } else {
        value.to_string()
    }
}

fn validate_mount_token(value: &str, label: &str) -> Result<(), DockerError> {
    validate_argument(value, label, 16 * 1024)?;
    if value.contains([',', '=']) {
        return Err(invalid(format!(
            "Docker {label} contains a reserved character"
        )));
    }
    Ok(())
}

fn validate_container_path(value: &str) -> Result<(), DockerError> {
    let path = Path::new(value);
    if !path.is_absolute()
        || value == "/"
        || path
            .components()
            .any(|component| matches!(component, Component::ParentDir | Component::CurDir))
    {
        return Err(invalid(
            "Container mount target must be a normalized absolute non-root path",
        ));
    }
    Ok(())
}

fn validate_bind_source(
    source: &str,
    configured_protected: &[PathBuf],
) -> Result<PathBuf, DockerError> {
    validate_argument(source, "bind source", 16 * 1024)?;
    let path = Path::new(source);
    if !path.is_absolute() {
        return Err(invalid("Bind mount source must be an absolute path"));
    }
    let canonical = std::fs::canonicalize(path)
        .map_err(|error| bind_source_io_error("Cannot resolve bind mount source", error))?;
    if canonical.parent().is_none() {
        return Err(invalid("Binding the host root directory is forbidden"));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::FileTypeExt;

        let metadata = fs::metadata(&canonical)
            .map_err(|error| bind_source_io_error("Cannot inspect bind mount source", error))?;
        if metadata.file_type().is_socket() {
            return Err(invalid(
                "Bind mount source must not expose a Unix domain socket",
            ));
        }
    }
    let mut protected = vec![
        PathBuf::from("/proc"),
        PathBuf::from("/sys"),
        PathBuf::from("/dev"),
        PathBuf::from("/run/docker.sock"),
        PathBuf::from("/var/run/docker.sock"),
        PathBuf::from("/run/containerd/containerd.sock"),
        PathBuf::from("/var/run/containerd/containerd.sock"),
    ];
    protected.extend(configured_protected.iter().cloned());
    if protected.iter().any(|protected| {
        let protected = std::fs::canonicalize(protected).unwrap_or_else(|_| protected.clone());
        canonical.starts_with(&protected) || protected.starts_with(&canonical)
    }) || canonical
        .file_name()
        .and_then(OsStr::to_str)
        .is_some_and(|name| matches!(name, "docker.sock" | "containerd.sock" | "dockershim.sock"))
    {
        return Err(invalid(
            "Bind mount source exposes a protected host or Agent path",
        ));
    }
    Ok(canonical)
}

fn bind_source_io_error(context: &str, error: io::Error) -> DockerError {
    DockerError::new(
        if error.kind() == io::ErrorKind::NotFound {
            DockerErrorCode::NotFound
        } else if error.kind() == io::ErrorKind::PermissionDenied {
            DockerErrorCode::PermissionDenied
        } else {
            DockerErrorCode::InvalidRequest
        },
        format!("{context}: {error}"),
        false,
    )
}

fn validate_optional_cursor(cursor: Option<&str>) -> Result<(), DockerError> {
    let Some(cursor) = cursor else {
        return Ok(());
    };
    validate_argument(cursor, "log cursor", 128)?;
    if cursor.is_empty()
        || cursor.chars().any(|character| {
            !(character.is_ascii_alphanumeric()
                || matches!(character, '-' | '+' | ':' | '.' | 'T' | 'Z'))
        })
    {
        return Err(invalid("Invalid Docker log cursor"));
    }
    Ok(())
}

fn invalid(message: impl Into<String>) -> DockerError {
    DockerError::new(DockerErrorCode::InvalidRequest, message, false)
}

fn spawn_error(error: io::Error) -> DockerError {
    if error.kind() == io::ErrorKind::NotFound {
        DockerError::new(
            DockerErrorCode::NotInstalled,
            "Docker CLI is not installed or is not available in PATH",
            false,
        )
    } else if error.kind() == io::ErrorKind::PermissionDenied {
        DockerError::new(
            DockerErrorCode::PermissionDenied,
            "Permission denied while starting Docker CLI",
            false,
        )
    } else {
        DockerError::new(
            DockerErrorCode::Internal,
            format!("Failed to start Docker CLI: {error}"),
            true,
        )
    }
}

fn command_failure(output: &CommandOutput) -> DockerError {
    let stderr = String::from_utf8_lossy(&output.stderr);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let raw = if stderr.trim().is_empty() {
        stdout.trim()
    } else {
        stderr.trim()
    };
    let message = if raw.is_empty() {
        "Docker CLI command failed".to_string()
    } else {
        truncate_text(raw, MAX_ERROR_MESSAGE)
    };
    let lower = message.to_ascii_lowercase();
    let (code, retryable) = if lower.contains("permission denied")
        || lower.contains("got permission denied")
        || lower.contains("access is denied")
    {
        (DockerErrorCode::PermissionDenied, false)
    } else if (lower.contains("client version") && lower.contains("too old"))
        || (lower.contains("api version")
            && (lower.contains("too old") || lower.contains("not supported")))
        || lower.contains("unsupported docker api")
    {
        (DockerErrorCode::UnsupportedVersion, false)
    } else if lower.contains("cannot connect to the docker daemon")
        || lower.contains("is the docker daemon running")
        || lower.contains("connection refused")
        || lower.contains("error during connect")
        || lower.contains("daemon is not running")
    {
        (DockerErrorCode::DaemonUnavailable, true)
    } else if lower.contains("no such container")
        || lower.contains("no such image")
        || lower.contains("no such network")
        || lower.contains("no such volume")
        || lower.contains("not found")
    {
        (DockerErrorCode::NotFound, false)
    } else if lower.contains("conflict") || lower.contains("already in use") {
        (DockerErrorCode::Conflict, false)
    } else {
        (DockerErrorCode::CommandFailed, false)
    };
    DockerError::new(code, message, retryable).with_exit_code(output.status.code().map(i64::from))
}

fn nonempty_stdout(output: &CommandOutput) -> Option<String> {
    let value = String::from_utf8_lossy(&output.stdout);
    let value = value.trim();
    (!value.is_empty()).then(|| truncate_text(value, MAX_ERROR_MESSAGE))
}

fn last_nonempty_line(output: &[u8]) -> Option<String> {
    String::from_utf8_lossy(output)
        .lines()
        .rev()
        .find(|line| !line.trim().is_empty())
        .map(|line| truncate_text(line.trim(), MAX_ERROR_MESSAGE))
}

fn truncate_text(value: &str, max_chars: usize) -> String {
    let mut text = value.chars().take(max_chars).collect::<String>();
    if value.chars().count() > max_chars {
        text.push_str("...");
    }
    text
}

fn send_response(outbound: &AgentEventSender, request_id: String, response: DockerResponse) {
    let _ = outbound.send(AgentInbound::DockerResponse {
        request_id,
        response,
    });
}

fn send_error(outbound: &AgentEventSender, request_id: String, error: DockerError) {
    send_response(outbound, request_id, DockerResponse::Error { error });
}

fn send_stream_event(outbound: &AgentEventSender, event: AgentInbound) {
    let _ = outbound.send(event);
}

async fn stream_logs(
    runner: &DockerRunner,
    request_id: &str,
    container: &str,
    tail: u32,
    follow: bool,
    since: Option<&str>,
    outbound: &AgentEventSender,
) -> Result<(), DockerError> {
    let stream = stream_logs_inner(runner, request_id, container, tail, follow, since, outbound);
    if follow {
        stream.await
    } else {
        tokio::time::timeout(READ_TIMEOUT, stream)
            .await
            .map_err(|_| {
                DockerError::new(
                    DockerErrorCode::Timeout,
                    "Docker log request timed out",
                    true,
                )
            })?
    }
}

async fn stream_logs_inner(
    runner: &DockerRunner,
    request_id: &str,
    container: &str,
    tail: u32,
    follow: bool,
    since: Option<&str>,
    outbound: &AgentEventSender,
) -> Result<(), DockerError> {
    let args = docker_log_args(container, tail, follow, since);

    let mut command = Command::new(&runner.binary);
    command
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    configure_docker_command(&mut command);
    let mut child = command.spawn().map_err(spawn_error)?;
    let mut process_group = ProcessGroupGuard::new(child.id());
    let stdout = child.stdout.take().ok_or_else(|| {
        DockerError::new(
            DockerErrorCode::Internal,
            "Docker log stdout pipe was not created",
            false,
        )
    })?;
    let stderr = child.stderr.take().ok_or_else(|| {
        DockerError::new(
            DockerErrorCode::Internal,
            "Docker log stderr pipe was not created",
            false,
        )
    })?;
    let (chunks_tx, mut chunks_rx) = mpsc::channel::<LogStreamChunk>(8);
    let stdout_task = tokio::spawn(pump_log_stream(stdout, chunks_tx.clone()));
    let stderr_task = tokio::spawn(pump_log_stream(stderr, chunks_tx.clone()));
    drop(chunks_tx);

    let mut sequence = 0_u64;
    let mut latest_cursor: Option<String> = None;
    while let Some(chunk) = chunks_rx.recv().await {
        if chunk
            .cursor
            .as_ref()
            .is_some_and(|cursor| latest_cursor.as_ref().is_none_or(|latest| cursor > latest))
        {
            latest_cursor = chunk.cursor;
        }
        let data = String::from_utf8_lossy(&chunk.data).into_owned();
        outbound
            .send_async(AgentInbound::DockerLogChunk {
                request_id: request_id.to_string(),
                sequence,
                data,
                cursor: latest_cursor.clone(),
            })
            .await
            .map_err(|_| {
                DockerError::new(
                    DockerErrorCode::Cancelled,
                    "Docker log consumer disconnected",
                    false,
                )
            })?;
        sequence = sequence.saturating_add(1);
    }
    let status = child.wait().await.map_err(|error| {
        DockerError::new(
            DockerErrorCode::Internal,
            format!("Failed to wait for Docker logs: {error}"),
            true,
        )
    })?;
    process_group.disarm();
    let _ = stdout_task.await;
    let _ = stderr_task.await;
    if status.success() {
        Ok(())
    } else {
        Err(DockerError::new(
            DockerErrorCode::CommandFailed,
            "Docker log stream ended with an error",
            false,
        )
        .with_exit_code(status.code().map(i64::from)))
    }
}

fn docker_log_args(container: &str, tail: u32, follow: bool, since: Option<&str>) -> Vec<OsString> {
    let mut args = os_args(["container", "logs", "--timestamps"]);
    if let Some(since) = since {
        args.push("--since".into());
        args.push(since.into());
    } else {
        args.push("--tail".into());
        args.push(tail.to_string().into());
    }
    if follow {
        args.push("--follow".into());
    }
    args.push(container.into());
    args
}

struct LogStreamChunk {
    data: Vec<u8>,
    cursor: Option<String>,
}

struct LogFramer {
    data: Vec<u8>,
    line_prefix: Vec<u8>,
    prefix_complete: bool,
    cursor: Option<String>,
}

impl LogFramer {
    fn new() -> Self {
        Self {
            data: Vec::with_capacity(LOG_CHUNK_SIZE),
            line_prefix: Vec::with_capacity(64),
            prefix_complete: false,
            cursor: None,
        }
    }

    fn push(&mut self, input: &[u8]) -> Vec<LogStreamChunk> {
        let mut chunks = Vec::new();
        for &byte in input {
            if !self.prefix_complete {
                if matches!(byte, b' ' | b'\t') {
                    if let Some(cursor) = parse_log_cursor_bytes(&self.line_prefix) {
                        self.cursor = Some(cursor);
                    }
                    self.prefix_complete = true;
                } else if !matches!(byte, b'\r' | b'\n') {
                    if self.line_prefix.len() < 64 {
                        self.line_prefix.push(byte);
                    } else {
                        self.prefix_complete = true;
                    }
                }
            }
            self.data.push(byte);
            let line_complete = byte == b'\n';
            if line_complete {
                self.line_prefix.clear();
                self.prefix_complete = false;
            }
            if line_complete || self.data.len() >= LOG_CHUNK_SIZE {
                chunks.push(self.take_chunk());
            }
        }
        chunks
    }

    fn finish(&mut self) -> Option<LogStreamChunk> {
        (!self.data.is_empty()).then(|| self.take_chunk())
    }

    fn take_chunk(&mut self) -> LogStreamChunk {
        LogStreamChunk {
            data: std::mem::replace(&mut self.data, Vec::with_capacity(LOG_CHUNK_SIZE)),
            cursor: self.cursor.take(),
        }
    }
}

async fn pump_log_stream<R>(mut reader: R, outbound: mpsc::Sender<LogStreamChunk>)
where
    R: AsyncRead + Unpin,
{
    let mut buffer = vec![0_u8; LOG_CHUNK_SIZE / 4];
    let mut framer = LogFramer::new();
    loop {
        match reader.read(&mut buffer).await {
            Ok(0) => break,
            Ok(count) => {
                for chunk in framer.push(&buffer[..count]) {
                    if outbound.send(chunk).await.is_err() {
                        return;
                    }
                }
            }
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(_) => break,
        }
    }
    if let Some(chunk) = framer.finish() {
        let _ = outbound.send(chunk).await;
    }
}

fn parse_log_cursor_bytes(candidate: &[u8]) -> Option<String> {
    let candidate = std::str::from_utf8(candidate).ok()?;
    (candidate.len() <= 64
        && candidate.contains('T')
        && candidate
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || "-+:.TZ".contains(character)))
    .then(|| candidate.to_string())
}

enum DockerExecControl {
    Input(Vec<u8>),
    Resize { cols: u16, rows: u16 },
    Close,
    CloseWithReason(String),
}

struct PtyChildGuard {
    child: Box<dyn portable_pty::Child + Send + Sync>,
    reaped: bool,
}

impl PtyChildGuard {
    fn new(child: Box<dyn portable_pty::Child + Send + Sync>) -> Self {
        Self {
            child,
            reaped: false,
        }
    }

    fn try_wait_code(&mut self) -> io::Result<Option<i64>> {
        let status = self.child.try_wait()?;
        if status.is_some() {
            self.reaped = true;
        }
        Ok(status.map(|status| status.exit_code() as i64))
    }

    fn terminate(&mut self) -> Option<i64> {
        if self.reaped {
            return None;
        }
        if let Some(pid) = self.child.process_id() {
            terminate_process_group(pid);
        }
        let _ = self.child.kill();
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        loop {
            match self.try_wait_code() {
                Ok(Some(code)) => return Some(code),
                Ok(None) | Err(_) if std::time::Instant::now() < deadline => {
                    thread::sleep(Duration::from_millis(10));
                }
                Ok(None) | Err(_) => return None,
            }
        }
    }

    fn close_interactive_shell(&mut self, writer: &PtyInputWriter) -> Option<i64> {
        if self.reaped {
            return None;
        }
        if writer.try_write(b"\x03exit 0\r".to_vec()).is_err() {
            return self.terminate();
        }
        let deadline = std::time::Instant::now() + Duration::from_secs(1);
        loop {
            match self.try_wait_code() {
                Ok(Some(code)) => return Some(code),
                Ok(None) | Err(_) if std::time::Instant::now() < deadline => {
                    thread::sleep(Duration::from_millis(10));
                }
                Ok(None) | Err(_) => return self.terminate(),
            }
        }
    }
}

impl Drop for PtyChildGuard {
    fn drop(&mut self) {
        if !self.reaped {
            let _ = self.terminate();
        }
    }
}

struct ActiveDockerExec {
    control: std_mpsc::SyncSender<DockerExecControl>,
    active: Arc<AtomicBool>,
}

struct DockerExecManager {
    binary: OsString,
    stream_outbound: AgentEventSender,
    activity: ActivityTracker,
    stream_slots: Arc<Semaphore>,
    sessions: HashMap<String, ActiveDockerExec>,
}

impl DockerExecManager {
    fn new(
        binary: OsString,
        stream_outbound: AgentEventSender,
        activity: ActivityTracker,
        stream_slots: Arc<Semaphore>,
    ) -> Self {
        Self {
            binary,
            stream_outbound,
            activity,
            stream_slots,
            sessions: HashMap::new(),
        }
    }

    fn open(&mut self, session_id: String, container: String, shell: String, cols: u16, rows: u16) {
        self.prune();
        self.close(&session_id);
        if let Err(error) =
            validate_identifier(&container, "container").and_then(|_| validate_exec_shell(&shell))
        {
            self.closed(session_id, None, Some(error.message));
            return;
        }
        let Ok(permit) = self.stream_slots.clone().try_acquire_owned() else {
            self.closed(
                session_id,
                None,
                Some("Docker stream concurrency limit reached".to_string()),
            );
            return;
        };
        let Some(activity_guard) = self.activity.try_enter() else {
            self.closed(
                session_id,
                None,
                Some("Agent update is waiting to install".to_string()),
            );
            return;
        };
        let (control, controls) = std_mpsc::sync_channel(EXEC_CONTROL_QUEUE_CAPACITY);
        let active = Arc::new(AtomicBool::new(true));
        self.sessions.insert(
            session_id.clone(),
            ActiveDockerExec {
                control,
                active: active.clone(),
            },
        );
        let binary = self.binary.clone();
        let stream_outbound = self.stream_outbound.clone();
        thread::spawn(move || {
            let _permit = permit;
            let _activity_guard = activity_guard;
            let result = run_docker_exec(
                &binary,
                &session_id,
                &container,
                &shell,
                cols.clamp(2, 500),
                rows.clamp(1, 300),
                controls,
                &stream_outbound,
            );
            active.store(false, Ordering::SeqCst);
            let event = match result {
                Ok((exit_code, reason)) => AgentInbound::DockerExecClosed {
                    session_id,
                    exit_code,
                    reason,
                },
                Err(error) => AgentInbound::DockerExecClosed {
                    session_id,
                    exit_code: None,
                    reason: Some(truncate_text(&format!("{error:#}"), MAX_ERROR_MESSAGE)),
                },
            };
            let _ = stream_outbound.blocking_send(event);
        });
    }

    fn input(&mut self, session_id: &str, encoded_data: &str) {
        if encoded_data.len() > MAX_EXEC_INPUT_BYTES {
            self.fail_session(session_id, "Docker terminal input exceeded the size limit");
            return;
        }
        let decoded = STANDARD.decode(encoded_data);
        match (self.sessions.get(session_id), decoded) {
            (Some(session), Ok(data)) => {
                if session
                    .control
                    .try_send(DockerExecControl::Input(data))
                    .is_err()
                {
                    self.fail_session(session_id, "Docker terminal input queue overflowed");
                }
            }
            (Some(_), Err(error)) => {
                self.fail_session(
                    session_id,
                    &format!("Invalid Docker terminal input encoding: {error}"),
                );
            }
            (None, _) => {}
        }
    }

    fn resize(&mut self, session_id: &str, cols: u16, rows: u16) {
        if let Some(session) = self.sessions.get(session_id) {
            if session
                .control
                .try_send(DockerExecControl::Resize {
                    cols: cols.clamp(2, 500),
                    rows: rows.clamp(1, 300),
                })
                .is_err()
            {
                self.fail_session(session_id, "Docker terminal control queue overflowed");
            }
        }
    }

    fn close(&mut self, session_id: &str) {
        if let Some(session) = self.sessions.remove(session_id) {
            let _ = session.control.try_send(DockerExecControl::Close);
        }
    }

    fn close_all(&mut self) {
        for (_, session) in self.sessions.drain() {
            let _ = session.control.try_send(DockerExecControl::Close);
        }
    }

    fn close_all_with_reason(&mut self, reason: &str) {
        for (_, session) in self.sessions.drain() {
            let _ = session
                .control
                .try_send(DockerExecControl::CloseWithReason(reason.to_string()));
        }
    }

    fn fail_session(&mut self, session_id: &str, reason: &str) {
        let Some(session) = self.sessions.remove(session_id) else {
            return;
        };
        let _ = session
            .control
            .try_send(DockerExecControl::CloseWithReason(reason.to_string()));
        self.closed(session_id.to_string(), None, Some(reason.to_string()));
    }

    fn prune(&mut self) {
        self.sessions
            .retain(|_, session| session.active.load(Ordering::SeqCst));
    }

    fn closed(&self, session_id: String, exit_code: Option<i64>, reason: Option<String>) {
        send_stream_event(
            &self.stream_outbound,
            AgentInbound::DockerExecClosed {
                session_id,
                exit_code,
                reason,
            },
        );
    }
}

fn validate_exec_shell(shell: &str) -> Result<(), DockerError> {
    if matches!(shell, "/bin/sh" | "/bin/bash" | "/bin/ash") {
        Ok(())
    } else {
        Err(invalid(
            "Docker exec shell must be /bin/sh, /bin/bash, or /bin/ash",
        ))
    }
}

fn run_docker_exec(
    binary: &OsStr,
    session_id: &str,
    container: &str,
    shell: &str,
    cols: u16,
    rows: u16,
    controls: std_mpsc::Receiver<DockerExecControl>,
    stream_outbound: &AgentEventSender,
) -> anyhow::Result<(Option<i64>, Option<String>)> {
    let pair = native_pty_system().openpty(PtySize {
        rows,
        cols,
        pixel_width: 0,
        pixel_height: 0,
    })?;
    let mut command = CommandBuilder::new(binary);
    command.args(["exec", "-it", container, shell]);
    command.env("TERM", "xterm-256color");
    command.env("COLORTERM", "truecolor");
    let slave = pair.slave;
    let master = pair.master;
    let child = slave.spawn_command(command)?;
    let mut child = PtyChildGuard::new(child);
    drop(slave);
    let mut reader = master.try_clone_reader()?;
    let input_writer = PtyInputWriter::spawn(
        master.take_writer()?,
        EXEC_WRITE_QUEUE_CAPACITY,
        "om-docker-exec-input",
    )?;

    stream_outbound.blocking_send(AgentInbound::DockerExecOpened {
        session_id: session_id.to_string(),
    })?;
    let reader_session_id = session_id.to_string();
    let reader_outbound = stream_outbound.clone();
    let reader_task = thread::spawn(move || {
        let mut buffer = [0_u8; LOG_CHUNK_SIZE];
        loop {
            match reader.read(&mut buffer) {
                Ok(0) => break,
                Ok(count) => {
                    if reader_outbound
                        .blocking_send(AgentInbound::DockerExecOutput {
                            session_id: reader_session_id.clone(),
                            data: STANDARD.encode(&buffer[..count]),
                        })
                        .is_err()
                    {
                        break;
                    }
                }
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(_) => break,
            }
        }
    });

    let outcome = loop {
        if let Some(error) = input_writer.take_failure() {
            break (
                child.terminate(),
                Some(format!("Failed to write terminal: {error}")),
            );
        }
        match controls.recv_timeout(Duration::from_millis(100)) {
            Ok(DockerExecControl::Input(data)) => {
                if let Err(error) = input_writer.try_write(data) {
                    break (
                        child.terminate(),
                        Some(format!("Failed to queue terminal input: {error}")),
                    );
                }
            }
            Ok(DockerExecControl::Resize { cols, rows }) => {
                if let Err(error) = master.resize(PtySize {
                    rows,
                    cols,
                    pixel_width: 0,
                    pixel_height: 0,
                }) {
                    break (
                        child.terminate(),
                        Some(format!("Failed to resize terminal: {error}")),
                    );
                }
            }
            Ok(DockerExecControl::Close) => {
                break (child.close_interactive_shell(&input_writer), None);
            }
            Ok(DockerExecControl::CloseWithReason(reason)) => {
                break (child.close_interactive_shell(&input_writer), Some(reason));
            }
            Err(std_mpsc::RecvTimeoutError::Timeout) => match child.try_wait_code() {
                Ok(Some(exit_code)) => break (Some(exit_code), None),
                Ok(None) => {}
                Err(error) => {
                    break (
                        child.terminate(),
                        Some(format!("Failed to inspect Docker terminal: {error}")),
                    );
                }
            },
            Err(std_mpsc::RecvTimeoutError::Disconnected) => {
                break (
                    child.close_interactive_shell(&input_writer),
                    Some("Docker terminal control channel closed".to_string()),
                );
            }
        }
    };
    drop(input_writer);
    drop(master);
    drop(reader_task);
    Ok(outcome)
}

#[cfg(all(test, unix))]
mod tests {
    use std::{
        fs,
        os::unix::{
            fs::{PermissionsExt, symlink},
            net::UnixListener,
        },
    };

    use uuid::Uuid;

    use super::*;

    fn test_config() -> AgentConfig {
        AgentConfig {
            server: "http://127.0.0.1:13500".to_string(),
            identity_file: None,
            report_interval: 5,
            state_dir: None,
            log_file: None,
            log_max_bytes: 1024 * 1024,
            log_history: 1,
            update_dir: None,
            remote_desktop_consent: crate::config::RemoteDesktopConsent::Required,
            windows_virtual_devices: crate::config::WindowsVirtualDevices::Disabled,
        }
    }

    struct TestDirectory(PathBuf);

    impl TestDirectory {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!("om-docker-test-{}", Uuid::new_v4()));
            fs::create_dir_all(&path).unwrap();
            Self(path)
        }

        fn new_short() -> Self {
            let path = PathBuf::from("/tmp").join(format!("om-ds-{}", Uuid::new_v4().simple()));
            fs::create_dir(&path).unwrap();
            Self(path)
        }

        fn fake_docker(&self, body: &str) -> PathBuf {
            let path = self.0.join("docker");
            fs::write(&path, format!("#!/bin/sh\nset -eu\n{body}\n")).unwrap();
            let mut permissions = fs::metadata(&path).unwrap().permissions();
            permissions.set_mode(0o700);
            fs::set_permissions(&path, permissions).unwrap();
            path
        }
    }

    impl Drop for TestDirectory {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn ready_probe_script(server_command: &str) -> String {
        format!(
            r#"
case "$1" in
  --version) printf '%s\n' 'Docker version 25.0.3, build test' ;;
  version) {server_command} ;;
  compose) printf '%s\n' '2.27.1' ;;
  *) exit 64 ;;
esac
"#
        )
    }

    #[test]
    fn bind_mounts_require_an_absolute_local_unix_endpoint() {
        assert!(is_local_unix_docker_endpoint("unix:///var/run/docker.sock"));
        assert!(is_local_unix_docker_endpoint(
            " unix:///run/user/1000/docker.sock "
        ));
        for endpoint in [
            "unix://relative/docker.sock",
            "tcp://127.0.0.1:2375",
            "ssh://docker@example.com",
            "npipe:////./pipe/docker_engine",
        ] {
            assert!(
                !is_local_unix_docker_endpoint(endpoint),
                "unexpectedly accepted {endpoint}"
            );
        }
    }

    #[tokio::test]
    async fn explicit_context_wins_over_docker_host_and_endpoint_is_pinned() {
        let directory = TestDirectory::new();
        let calls = directory.0.join("calls");
        let script = format!(
            "printf '%s\n' \"$*\" >> '{}'; if [ \"$1\" = context ]; then printf '%s\n' '\"unix:///run/docker.sock\"'; fi",
            calls.display()
        );
        let runner = DockerRunner::new(directory.fake_docker(&script), Vec::new());

        let endpoint = runner
            .ensure_local_unix_daemon_for(
                Some(OsString::from("local-context")),
                Some(OsString::from("ssh://docker@example.com")),
            )
            .await
            .unwrap();
        assert_eq!(endpoint, "unix:///run/docker.sock");

        let mut args = os_args(["container", "create", "alpine"]);
        pin_docker_endpoint(&mut args, endpoint);
        runner.run(args, PROBE_TIMEOUT).await.unwrap();

        let calls = fs::read_to_string(calls).unwrap();
        assert!(
            calls.contains(
                "context inspect local-context --format {{json .Endpoints.docker.Host}}\n"
            )
        );
        assert!(calls.contains("--host unix:///run/docker.sock container create alpine\n"));
        assert!(!calls.contains("ssh://docker@example.com"));
    }

    #[test]
    fn compose_bind_detection_covers_services_and_local_volume_drivers() {
        assert!(compose_uses_bind_mounts(&json!({
            "services": {
                "web": {
                    "volumes": [{"type": "bind", "source": "/srv/app", "target": "/app"}]
                }
            }
        })));
        assert!(compose_uses_bind_mounts(&json!({
            "services": {"web": {"volumes": ["/srv/app:/app:ro"]}}
        })));
        assert!(compose_uses_bind_mounts(&json!({
            "volumes": {
                "data": {"driver_opts": {"type": "none", "o": "bind", "device": "/srv/data"}}
            }
        })));
        assert!(!compose_uses_bind_mounts(&json!({
            "services": {"web": {"volumes": ["data:/app"]}},
            "volumes": {"data": {}}
        })));
    }

    #[tokio::test]
    async fn system_prune_reports_partial_success_and_continues_in_order() {
        let directory = TestDirectory::new();
        let calls = directory.0.join("calls");
        let script = format!(
            "printf '%s\n' \"$1\" >> '{}'; if [ \"$1\" = image ]; then printf '%s\n' failed >&2; exit 1; fi; printf '%s\n' pruned",
            calls.display()
        );
        let runner = DockerRunner::new(directory.fake_docker(&script), Vec::new());
        let response = execute_system_prune(&runner, true, true, true, false, true)
            .await
            .unwrap();
        let DockerResponse::OperationComplete { data } = response else {
            panic!("system prune did not return an operation result");
        };
        assert_eq!(data["completed"], false);
        assert_eq!(data["partial_success"], true);
        assert_eq!(data["succeeded_stages"], 2);
        assert_eq!(data["failed_stages"], 1);
        assert_eq!(data["resources"][0]["resource"], "container");
        assert_eq!(data["resources"][0]["completed"], true);
        assert_eq!(data["resources"][1]["resource"], "image");
        assert_eq!(data["resources"][1]["completed"], false);
        assert_eq!(data["resources"][2]["resource"], "network");
        assert_eq!(data["resources"][2]["completed"], true);
        assert_eq!(
            fs::read_to_string(calls).unwrap(),
            "container\nimage\nnetwork\n"
        );
    }

    #[tokio::test]
    async fn probe_reports_ready_versions_and_optional_compose() {
        let directory = TestDirectory::new();
        let script = ready_probe_script(
            r#"printf '%s\n' '{"Client":{"Version":"25.0.3"},"Server":{"Version":"25.0.3","ApiVersion":"1.44"}}'"#,
        );
        let runner = DockerRunner::new(directory.fake_docker(&script), Vec::new());

        let status = probe_docker(&runner).await;

        assert_eq!(status.state, DockerStatusState::Ready);
        assert_eq!(status.cli_version.as_deref(), Some("25.0.3"));
        assert_eq!(status.engine_version.as_deref(), Some("25.0.3"));
        assert_eq!(status.api_version.as_deref(), Some("1.44"));
        assert_eq!(status.compose_version.as_deref(), Some("2.27.1"));
    }

    #[tokio::test]
    async fn probe_distinguishes_missing_daemon_permission_and_old_cli() {
        let missing =
            DockerRunner::new(format!("/missing/om-docker-{}", Uuid::new_v4()), Vec::new());
        assert_eq!(
            probe_docker(&missing).await.state,
            DockerStatusState::NotInstalled
        );

        let directory = TestDirectory::new();
        let daemon = DockerRunner::new(
            directory.fake_docker(&ready_probe_script(
                "printf '%s\\n' 'Cannot connect to the Docker daemon. Is the docker daemon running?' >&2; exit 1",
            )),
            Vec::new(),
        );
        assert_eq!(
            probe_docker(&daemon).await.state,
            DockerStatusState::DaemonUnreachable
        );

        let permission_directory = TestDirectory::new();
        let permission = DockerRunner::new(
            permission_directory.fake_docker(&ready_probe_script(
                "printf '%s\\n' 'permission denied while trying to connect' >&2; exit 1",
            )),
            Vec::new(),
        );
        assert_eq!(
            probe_docker(&permission).await.state,
            DockerStatusState::PermissionDenied
        );

        let old_directory = TestDirectory::new();
        let old = DockerRunner::new(
            old_directory.fake_docker("printf '%s\\n' 'Docker version 19.03.15, build test'"),
            Vec::new(),
        );
        assert_eq!(
            probe_docker(&old).await.state,
            DockerStatusState::UnsupportedVersion
        );
    }

    #[tokio::test]
    async fn probe_rejects_unknown_or_incomplete_versions_and_has_one_total_timeout() {
        let unknown_directory = TestDirectory::new();
        let unknown = DockerRunner::new(
            unknown_directory.fake_docker("printf '%s\\n' 'Docker version development'"),
            Vec::new(),
        );
        assert_eq!(probe_docker(&unknown).await.state, DockerStatusState::Error);

        let incomplete_directory = TestDirectory::new();
        let incomplete = DockerRunner::new(
            incomplete_directory.fake_docker(&ready_probe_script(
                r#"printf '%s\n' '{"Client":{"Version":"25.0.3"}}'"#,
            )),
            Vec::new(),
        );
        assert_eq!(
            probe_docker(&incomplete).await.state,
            DockerStatusState::Error
        );

        let slow_directory = TestDirectory::new();
        let slow = DockerRunner::new(slow_directory.fake_docker("sleep 2"), Vec::new());
        let started = std::time::Instant::now();
        let status = probe_docker_with_timeout(&slow, Duration::from_millis(50)).await;
        assert_eq!(status.state, DockerStatusState::Error);
        assert!(status.message.unwrap().contains("timed out"));
        assert!(started.elapsed() < Duration::from_millis(500));
    }

    #[tokio::test]
    async fn runner_enforces_timeout_and_control_output_limit() {
        let timeout_directory = TestDirectory::new();
        let timeout_runner =
            DockerRunner::new(timeout_directory.fake_docker("exec sleep 5"), Vec::new());
        let error = timeout_runner
            .run(Vec::new(), Duration::from_millis(50))
            .await
            .unwrap_err();
        assert_eq!(error.code, DockerErrorCode::Timeout);

        let output_directory = TestDirectory::new();
        let output_runner = DockerRunner::new(
            output_directory.fake_docker("head -c 4194304 /dev/zero; printf '%s' 'retained-tail'"),
            Vec::new(),
        );
        let output = output_runner
            .run(Vec::new(), Duration::from_secs(5))
            .await
            .unwrap();
        assert!(output.status.success());
        assert!(output.stdout_truncated);
        assert!(output.stdout.ends_with(b"retained-tail"));
        assert_eq!(
            ensure_complete_control_output(&output).unwrap_err().code,
            DockerErrorCode::OutputTooLarge
        );

        let DockerResponse::OperationComplete { data } =
            operation_response_with_output(json!({"completed": true}), &output)
        else {
            panic!("write operation did not return an operation result");
        };
        assert_eq!(data["completed"], true);
        assert_eq!(data["output_truncated"], true);
    }

    #[tokio::test]
    async fn bounded_reader_keeps_the_exact_tail() {
        use tokio::io::AsyncWriteExt as _;

        let (mut writer, reader) = tokio::io::duplex(32);
        let write = tokio::spawn(async move {
            writer.write_all(b"0123456789").await.unwrap();
            writer.shutdown().await.unwrap();
        });
        let (output, truncated) = read_bounded_tail(reader, 4).await.unwrap();
        write.await.unwrap();

        assert!(truncated);
        assert_eq!(output, b"6789");
    }

    #[tokio::test]
    async fn runner_timeout_terminates_the_entire_process_group() {
        let directory = TestDirectory::new();
        let marker = directory.0.join("orphan-marker");
        let script = format!(
            "(sleep 0.2; printf '%s' leaked > '{}') & wait",
            marker.display()
        );
        let runner = DockerRunner::new(directory.fake_docker(&script), Vec::new());

        assert_eq!(
            runner
                .run(Vec::new(), Duration::from_millis(50))
                .await
                .unwrap_err()
                .code,
            DockerErrorCode::Timeout
        );
        tokio::time::sleep(Duration::from_millis(350)).await;
        assert!(!marker.exists(), "a Docker CLI grandchild survived timeout");
    }

    #[test]
    fn structured_json_parser_accepts_objects_arrays_and_ndjson() {
        assert_eq!(
            parse_json_output(br#"{"ID":"one"}"#).unwrap(),
            json!({"ID": "one"})
        );
        assert_eq!(
            parse_json_output(b"{\"ID\":\"one\"}\n{\"ID\":\"two\"}\n").unwrap(),
            json!([{"ID": "one"}, {"ID": "two"}])
        );
        assert_eq!(parse_json_output(b"  \n").unwrap(), json!([]));
        assert!(parse_json_output(b"not-json").is_err());
    }

    #[test]
    fn container_create_uses_fixed_argv_and_requires_bind_confirmation() {
        let directory = TestDirectory::new();
        let bind = directory.0.join("data");
        let bind_link = directory.0.join("data-link");
        fs::create_dir(&bind).unwrap();
        symlink(&bind, &bind_link).unwrap();
        let runner = DockerRunner::new("docker", Vec::new());
        let mut spec = DockerContainerCreateSpec {
            name: Some("web".to_string()),
            image: "alpine;touch /tmp/should-not-run".to_string(),
            command: vec!["printf".to_string(), "hello world".to_string()],
            environment: BTreeMap::from([("TOKEN".to_string(), "one two".to_string())]),
            ports: Vec::new(),
            mounts: vec![crate::models::DockerMountSpec {
                kind: DockerMountKind::Bind,
                source: bind_link.to_string_lossy().into_owned(),
                target: "/data".to_string(),
                read_only: false,
            }],
            network: None,
            restart_policy: Some(DockerRestartPolicy::OnFailure),
            restart_max_retries: Some(3),
            cpus: Some(1.5),
            memory_bytes: Some(64 * 1024 * 1024),
            confirm_bind_write: false,
        };
        assert_eq!(
            container_create_args(&runner, spec.clone())
                .unwrap_err()
                .code,
            DockerErrorCode::InvalidRequest
        );

        spec.confirm_bind_write = true;
        let args = container_create_args(&runner, spec).unwrap();
        assert!(
            args.iter()
                .any(|arg| arg == "alpine;touch /tmp/should-not-run")
        );
        assert!(args.iter().any(|arg| arg == "TOKEN=one two"));
        assert!(!args.iter().any(|arg| arg == "sh" || arg == "-c"));
        assert_eq!(args[0], "container");
        assert_eq!(args[1], "create");
        let mount = args
            .iter()
            .find_map(|arg| arg.to_str().filter(|arg| arg.starts_with("type=bind")))
            .unwrap();
        let canonical_bind = fs::canonicalize(bind).unwrap();
        assert!(mount.contains(canonical_bind.to_string_lossy().as_ref()));
        assert!(!mount.contains("data-link"));
    }

    #[test]
    fn bind_validation_resolves_symlinks_and_rejects_protected_paths() {
        assert_eq!(
            validate_bind_source("/", &[]).unwrap_err().code,
            DockerErrorCode::InvalidRequest
        );

        let directory = TestDirectory::new();
        let protected = directory.0.join("agent-state");
        fs::create_dir(&protected).unwrap();
        assert!(
            validate_bind_source(
                directory.0.to_str().unwrap(),
                std::slice::from_ref(&protected)
            )
            .is_err()
        );

        let proc_link = directory.0.join("proc-link");
        symlink("/proc", &proc_link).unwrap();
        assert!(validate_bind_source(proc_link.to_str().unwrap(), &[]).is_err());

        let data = directory.0.join("data");
        let data_link = directory.0.join("data-link");
        fs::create_dir(&data).unwrap();
        symlink(&data, &data_link).unwrap();
        assert_eq!(
            validate_bind_source(data_link.to_str().unwrap(), &[]).unwrap(),
            fs::canonicalize(data).unwrap()
        );

        let missing = directory.0.join("ordinary-missing-path");
        assert_eq!(
            validate_bind_source(missing.to_str().unwrap(), &[])
                .unwrap_err()
                .code,
            DockerErrorCode::NotFound
        );
    }

    #[test]
    fn bind_validation_rejects_any_unix_domain_socket_name() {
        let directory = TestDirectory::new_short();
        let runtime = directory.0.join("runtime");
        fs::create_dir(&runtime).unwrap();
        let socket = runtime.join("engine.sock");
        let _listener = UnixListener::bind(&socket).unwrap();

        let error = validate_bind_source(socket.to_str().unwrap(), &[]).unwrap_err();
        assert_eq!(error.code, DockerErrorCode::InvalidRequest);
        assert!(error.message.contains("Unix domain socket"));

        let socket_link = directory.0.join("ordinary-looking-source");
        symlink(&socket, &socket_link).unwrap();
        assert_eq!(
            validate_bind_source(socket_link.to_str().unwrap(), &[])
                .unwrap_err()
                .code,
            DockerErrorCode::InvalidRequest
        );
    }

    #[test]
    fn compose_warnings_detect_high_risk_runtime_settings() {
        let config = json!({
            "services": {
                "web": {
                    "privileged": true,
                    "network_mode": "host",
                    "security_opt": ["apparmor=unconfined"],
                    "cap_add": ["SYS_ADMIN"],
                    "volumes": [{"type": "bind", "source": "/srv", "target": "/srv"}]
                }
            },
            "volumes": {
                "host-root": {
                    "driver_opts": {"type": "none", "o": "bind", "device": "/"}
                }
            }
        });
        let warnings = compose_warnings(&config);
        assert_eq!(warnings.len(), 6);
        assert!(
            warnings
                .iter()
                .any(|warning| warning.contains("privileged"))
        );
        assert!(warnings.iter().any(|warning| warning.contains("/srv")));
        assert!(warnings.iter().any(|warning| warning.contains("host-root")));
    }

    #[test]
    fn compose_use_api_socket_requires_confirmation_for_deploy_and_up() {
        let warnings = compose_warnings(&json!({
            "services": {
                "worker": {
                    "image": "alpine",
                    "use_api_socket": true
                }
            }
        }));
        assert_eq!(
            warnings,
            ["Service worker enables use_api_socket and can control the Docker Engine"]
        );

        let validation = DockerComposeValidation {
            project: Some("socket-test".to_string()),
            services: vec!["worker".to_string()],
            service_summaries: Vec::new(),
            config_summary: DockerComposeConfigSummary {
                service_count: 1,
                network_count: 0,
                volume_count: 0,
                config_count: 0,
                secret_count: 0,
            },
            warnings,
            config_digest: "sha256:socket-test".to_string(),
        };

        assert_eq!(
            verify_compose_confirmation(&validation, "sha256:socket-test", false)
                .unwrap_err()
                .code,
            DockerErrorCode::InvalidRequest
        );
        assert!(verify_compose_confirmation(&validation, "sha256:socket-test", true).is_ok());
        assert_eq!(
            verify_compose_action_confirmation(&DockerComposeAction::Up, &validation, false)
                .unwrap_err()
                .code,
            DockerErrorCode::InvalidRequest
        );
        assert!(
            verify_compose_action_confirmation(&DockerComposeAction::Up, &validation, true).is_ok()
        );
    }

    #[test]
    fn exec_rejects_unapproved_shells_and_update_draining() {
        assert!(validate_exec_shell("/bin/sh").is_ok());
        assert!(validate_exec_shell("/usr/bin/zsh").is_err());

        let activity = ActivityTracker::default();
        activity.start_draining();
        let (stream_outbound, mut inbound, _failed) = AgentEventSender::channel(1);
        let mut exec = DockerExecManager::new(
            "docker".into(),
            stream_outbound,
            activity,
            Arc::new(Semaphore::new(2)),
        );
        exec.open(
            "session-1".to_string(),
            "web".to_string(),
            "/bin/sh".to_string(),
            80,
            24,
        );
        assert!(matches!(
            inbound.try_recv().unwrap(),
            AgentInbound::DockerExecClosed { reason: Some(reason), .. }
                if reason.contains("update")
        ));
    }

    #[test]
    fn docker_exec_closes_a_session_when_its_control_queue_is_full() {
        let activity = ActivityTracker::default();
        let (stream_outbound, mut inbound, _failed) = AgentEventSender::channel(4);
        let mut exec = DockerExecManager::new(
            "docker".into(),
            stream_outbound,
            activity,
            Arc::new(Semaphore::new(2)),
        );
        let (control, _controls) = std_mpsc::sync_channel(1);
        control.try_send(DockerExecControl::Close).unwrap();
        exec.sessions.insert(
            "congested".to_string(),
            ActiveDockerExec {
                control,
                active: Arc::new(AtomicBool::new(true)),
            },
        );

        exec.resize("congested", 100, 40);

        assert!(!exec.sessions.contains_key("congested"));
        assert!(matches!(
            inbound.try_recv(),
            Ok(AgentInbound::DockerExecClosed {
                session_id,
                exit_code: None,
                reason: Some(reason),
            }) if session_id == "congested" && reason.contains("overflowed")
        ));
    }

    #[test]
    fn protected_paths_include_resolved_agent_and_docker_configuration() {
        let config = test_config();
        let paths = protected_paths(&config);

        assert!(paths.contains(&identity_path(None).unwrap()));
        for path in docker_protected_paths(&config).unwrap() {
            assert!(paths.contains(&path));
        }
        assert!(paths.contains(&docker_protected_update_root(&config).unwrap()));
        if env::var_os("DOCKER_CONFIG").is_none()
            && let Some(home) = env::var_os("HOME")
        {
            assert!(paths.contains(&PathBuf::from(home).join(".docker")));
        }
    }

    #[test]
    fn ipv6_publish_addresses_are_bracketed() {
        assert_eq!(format_published_host_ip("127.0.0.1"), "127.0.0.1");
        assert_eq!(format_published_host_ip("::1"), "[::1]");
    }

    #[test]
    fn stream_close_is_queued_after_buffered_data() {
        let (outbound, mut inbound, failed) = AgentEventSender::channel(2);
        outbound
            .send(AgentInbound::DockerLogChunk {
                request_id: "logs-1".to_string(),
                sequence: 0,
                data: "first\n".to_string(),
                cursor: None,
            })
            .unwrap();
        send_stream_event(
            &outbound,
            AgentInbound::DockerLogClosed {
                request_id: "logs-1".to_string(),
                error: None,
            },
        );

        assert!(matches!(
            inbound.try_recv(),
            Ok(AgentInbound::DockerLogChunk { .. })
        ));
        assert!(matches!(
            inbound.try_recv(),
            Ok(AgentInbound::DockerLogClosed { .. })
        ));
        assert!(!*failed.borrow());
    }

    #[test]
    fn full_stream_queue_requests_reconnect_without_a_waiting_sender() {
        let (outbound, mut inbound, failed) = AgentEventSender::channel(1);
        outbound
            .send(AgentInbound::DockerLogChunk {
                request_id: "logs-1".to_string(),
                sequence: 0,
                data: "first\n".to_string(),
                cursor: None,
            })
            .unwrap();

        send_stream_event(
            &outbound,
            AgentInbound::DockerLogClosed {
                request_id: "logs-1".to_string(),
                error: None,
            },
        );

        assert!(*failed.borrow());
        assert!(matches!(
            inbound.try_recv(),
            Ok(AgentInbound::DockerLogChunk { .. })
        ));
        assert!(matches!(
            inbound.try_recv(),
            Err(mpsc::error::TryRecvError::Empty)
        ));
    }

    #[test]
    fn log_framer_keeps_split_timestamps_and_log_resume_omits_tail() {
        let mut framer = LogFramer::new();
        assert!(framer.push(b"2026-07-27T01:02:03.123").is_empty());
        let first = framer.push(b"456789Z first line\n");
        assert_eq!(first.len(), 1);
        assert_eq!(
            first[0].cursor.as_deref(),
            Some("2026-07-27T01:02:03.123456789Z")
        );
        let next = framer.push(b"2026-07-27T01:02:04.000000000Z next\n");
        assert_eq!(next.len(), 1);
        let chunk = &next[0];
        assert_eq!(
            chunk.cursor.as_deref(),
            Some("2026-07-27T01:02:04.000000000Z")
        );
        assert!(framer.finish().is_none());

        let args = docker_log_args("web", 200, true, Some("2026-07-27T01:02:04.000000000Z"));
        let args = args
            .iter()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        assert!(args.contains(&"--since".to_string()));
        assert!(args.contains(&"--follow".to_string()));
        assert!(!args.contains(&"--tail".to_string()));
    }

    #[tokio::test]
    async fn log_pump_flushes_a_complete_line_while_the_source_stays_open() {
        use tokio::io::AsyncWriteExt as _;

        let (mut writer, reader) = tokio::io::duplex(1024);
        let (outbound, mut inbound) = mpsc::channel(2);
        let task = tokio::spawn(pump_log_stream(reader, outbound));

        writer
            .write_all(b"2026-07-27T01:02:03.123456789Z first line\n")
            .await
            .unwrap();
        let chunk = tokio::time::timeout(Duration::from_secs(1), inbound.recv())
            .await
            .expect("a complete line should not wait for EOF or a full buffer")
            .unwrap();
        assert_eq!(chunk.data, b"2026-07-27T01:02:03.123456789Z first line\n");
        assert_eq!(
            chunk.cursor.as_deref(),
            Some("2026-07-27T01:02:03.123456789Z")
        );

        drop(writer);
        task.await.unwrap();
    }

    #[test]
    fn compose_snapshot_is_private_and_service_summary_is_controlled() {
        let config = json!({
            "name": "sample",
            "services": {
                "web": {
                    "image": "nginx:alpine",
                    "ports": [{"target": 80, "published": "8080", "protocol": "tcp"}],
                    "volumes": [{"type": "volume", "source": "data", "target": "/data", "read_only": true}],
                    "networks": {"frontend": null},
                    "profiles": ["production"]
                }
            },
            "networks": {"frontend": {}},
            "volumes": {"data": {}},
            "configs": {},
            "secrets": {}
        });
        let services = config.get("services").and_then(Value::as_object).unwrap();
        let summaries = compose_service_summaries(services);
        assert_eq!(summaries.len(), 1);
        assert_eq!(summaries[0].image.as_deref(), Some("nginx:alpine"));
        assert_eq!(summaries[0].ports, ["8080:80/tcp"]);
        assert_eq!(summaries[0].networks, ["frontend"]);
        assert_eq!(object_len(&config, "service"), 0);
        assert_eq!(object_len(&config, "services"), 1);

        let directory = TestDirectory::new();
        let snapshots = directory.0.join("snapshots");
        let snapshot = ComposeSnapshot::create(
            &snapshots,
            Some("sample"),
            &serde_json::to_vec(&config).unwrap(),
        )
        .unwrap();
        let path = snapshot.path.clone();
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(
            fs::read(&path).unwrap(),
            serde_json::to_vec(&config).unwrap()
        );
        assert_eq!(
            fs::metadata(path.parent().unwrap())
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
        drop(snapshot);
        assert!(!path.exists());

        let mut retained = ComposeSnapshot::create(
            &snapshots,
            Some("sample"),
            &serde_json::to_vec(&config).unwrap(),
        )
        .unwrap();
        let retained_path = retained.path.clone();
        assert_eq!(retained_path, path);
        retained.retain();
        drop(retained);
        assert!(retained_path.exists());

        let reused = ComposeSnapshot::create(
            &snapshots,
            Some("sample"),
            &serde_json::to_vec(&config).unwrap(),
        )
        .unwrap();
        assert_eq!(reused.path, retained_path);
        drop(reused);
        assert!(retained_path.exists());

        let mut different =
            ComposeSnapshot::create(&snapshots, Some("sample"), br#"{"services":{}}"#).unwrap();
        assert_ne!(different.path, retained_path);
        let different_path = different.path.clone();
        let project_key = different.project_key.clone();
        different.retain();
        drop(different);
        assert!(retained_path.exists());
        assert!(different_path.exists());

        let labels = vec![format!(
            "com.docker.compose.project.config_files={}",
            retained_path.display()
        )];
        remove_unreferenced_compose_files(&snapshots, &project_key, &labels, &[]);
        assert!(retained_path.exists());
        assert!(!different_path.exists());
        fs::remove_file(retained_path).unwrap();
    }

    #[tokio::test]
    async fn update_draining_aborts_long_running_docker_logs() {
        let activity = ActivityTracker::default();
        let (outbound, _responses, _failed) = AgentEventSender::channel(32);
        let (stream_outbound, mut streams, _stream_failed) = AgentEventSender::channel(4);
        let mut manager =
            DockerManager::new(&test_config(), outbound, stream_outbound, activity.clone());
        let guard = activity.try_enter().unwrap();
        manager.logs.insert(
            "logs-1".to_string(),
            tokio::spawn(async move {
                let _guard = guard;
                std::future::pending::<()>().await;
            }),
        );

        manager.close_streams_for_update();
        assert!(matches!(
            streams.recv().await,
            Some(AgentInbound::DockerLogClosed {
                error: Some(DockerError {
                    code: DockerErrorCode::Cancelled,
                    ..
                }),
                ..
            })
        ));
        for _ in 0..20 {
            if activity.active_count() == 0 {
                break;
            }
            tokio::task::yield_now().await;
        }
        assert_eq!(activity.active_count(), 0);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn docker_exec_orders_open_output_and_close_and_releases_activity() {
        let directory = TestDirectory::new();
        let binary = directory.fake_docker("exec /bin/sh");
        let activity = ActivityTracker::default();
        let (stream_outbound, mut streams, _stream_failed) = AgentEventSender::channel(8);
        let mut exec = DockerExecManager::new(
            binary.into_os_string(),
            stream_outbound,
            activity.clone(),
            Arc::new(Semaphore::new(2)),
        );

        exec.open(
            "exec-1".to_string(),
            "web".to_string(),
            "/bin/sh".to_string(),
            80,
            24,
        );
        assert!(matches!(
            tokio::time::timeout(Duration::from_secs(2), streams.recv())
                .await
                .unwrap(),
            Some(AgentInbound::DockerExecOpened { .. })
        ));
        exec.close("exec-1");
        loop {
            let event = tokio::time::timeout(Duration::from_secs(3), streams.recv())
                .await
                .unwrap()
                .unwrap();
            if let AgentInbound::DockerExecClosed {
                exit_code, reason, ..
            } = event
            {
                assert!(exit_code.is_some());
                assert!(reason.is_none());
                break;
            }
        }
        for _ in 0..20 {
            if activity.active_count() == 0 {
                break;
            }
            tokio::task::yield_now().await;
        }
        assert_eq!(activity.active_count(), 0);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn blocked_docker_exec_input_does_not_prevent_session_shutdown() {
        let directory = TestDirectory::new();
        let binary = directory.fake_docker("sleep 60");
        let activity = ActivityTracker::default();
        let (stream_outbound, mut streams, _stream_failed) = AgentEventSender::channel(128);
        let mut exec = DockerExecManager::new(
            binary.into_os_string(),
            stream_outbound,
            activity.clone(),
            Arc::new(Semaphore::new(2)),
        );

        exec.open(
            "exec-blocked".to_string(),
            "web".to_string(),
            "/bin/sh".to_string(),
            80,
            24,
        );
        assert!(matches!(
            tokio::time::timeout(Duration::from_secs(2), streams.recv())
                .await
                .unwrap(),
            Some(AgentInbound::DockerExecOpened { .. })
        ));

        let input = STANDARD.encode(vec![b'x'; 32 * 1024]);
        for _ in 0..64 {
            exec.input("exec-blocked", &input);
        }
        exec.close_all();

        let closed = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if matches!(
                    streams.recv().await,
                    Some(AgentInbound::DockerExecClosed { .. })
                ) {
                    break;
                }
            }
        })
        .await;
        assert!(closed.is_ok(), "blocked Docker exec session did not close");
        let activity_deadline = tokio::time::Instant::now() + Duration::from_secs(1);
        while activity.active_count() != 0 && tokio::time::Instant::now() < activity_deadline {
            tokio::task::yield_now().await;
        }
        assert_eq!(activity.active_count(), 0);
    }

    #[test]
    fn version_parsing_handles_packager_suffixes() {
        assert_eq!(
            parse_cli_version("Docker version 24.0.7, build test"),
            Some("24.0.7".into())
        );
        assert_eq!(parse_numeric_version("20.10.24+dfsg1"), Some((20, 10, 24)));
        assert_eq!(parse_numeric_version("19.03"), Some((19, 3, 0)));
    }
}
