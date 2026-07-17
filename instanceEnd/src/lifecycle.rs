use std::{
    env,
    fs::{self, File, OpenOptions, TryLockError},
    io::{self, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    thread,
    time::{Duration, Instant},
};

use anyhow::{Context, Result, anyhow, bail};
use directories::ProjectDirs;
use tokio::sync::watch;

use crate::{config::AgentConfig, identity::load_or_create_identity, ws::agent_ws_loop};

const START_TIMEOUT: Duration = Duration::from_secs(5);
const POLL_INTERVAL: Duration = Duration::from_millis(200);

pub fn start(config: &AgentConfig) -> Result<()> {
    let paths = RuntimePaths::from_config(config)?;
    paths.prepare()?;

    if let ProcessState::Running(pid) = paths.process_state()? {
        print_running("agent is already running", pid, &paths.log_file);
        return Ok(());
    }
    paths.remove_stale_files();

    let mut command = Command::new(std::env::current_exe()?);
    command
        .arg("start")
        .arg("--daemon-child")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    config.append_cli_args(&mut command);
    detach(&mut command);

    let mut child = command
        .spawn()
        .context("failed to start background agent")?;
    let pid = child.id();
    wait_until_ready(&mut child, pid, &paths)?;
    println!("agent started in the background (pid {pid})");
    println!("log: {}", paths.log_file.display());
    Ok(())
}

pub fn stop(config: &AgentConfig, timeout_seconds: u64) -> Result<()> {
    if !stop_if_running(config, timeout_seconds)? {
        println!("agent is not running");
    }
    Ok(())
}

pub fn stop_if_running(config: &AgentConfig, timeout_seconds: u64) -> Result<bool> {
    let paths = RuntimePaths::from_config(config)?;

    let pid = match paths.process_state()? {
        ProcessState::Stopped => {
            paths.remove_stale_files();
            return Ok(false);
        }
        ProcessState::Running(pid) => pid,
    };

    let request = pid.map_or_else(|| "stop".to_owned(), |pid| pid.to_string());
    fs::write(&paths.stop_file, request)
        .with_context(|| format!("failed to write stop request {}", paths.stop_file.display()))?;

    let timeout = Duration::from_secs(timeout_seconds);
    let started = Instant::now();
    while started.elapsed() <= timeout {
        if matches!(paths.process_state()?, ProcessState::Stopped) {
            paths.remove_stale_files();
            match pid {
                Some(pid) => println!("agent stopped (pid {pid})"),
                None => println!("agent stopped"),
            }
            return Ok(true);
        }
        thread::sleep(POLL_INTERVAL);
    }

    bail!(
        "agent did not stop within {} seconds; inspect {}",
        timeout_seconds,
        paths.log_file.display()
    )
}

pub fn status(config: &AgentConfig) -> Result<()> {
    if status_from_installed_service(config)? {
        return Ok(());
    }
    let paths = RuntimePaths::for_observer(config)?;
    match paths.process_state()? {
        ProcessState::Running(pid) => {
            let prefix = if paths.ready_pid() == pid && pid.is_some() {
                "agent is running"
            } else {
                "agent is starting"
            };
            print_running(prefix, pid, &paths.log_file);
        }
        ProcessState::Stopped => println!("agent is not running"),
    }
    Ok(())
}

fn status_from_installed_service(config: &AgentConfig) -> Result<bool> {
    if config.state_dir.is_some()
        || (installed_runtime_paths().is_none() && !query_service_without_install_marker())
    {
        return Ok(false);
    }
    let Some(service) = installed_service_status()? else {
        return Ok(false);
    };
    let log_file = RuntimePaths::for_observer(config)?.log_file;
    match service.state {
        InstalledServiceState::Stopped => println!("agent is not running"),
        InstalledServiceState::Starting => {
            print_running("agent is starting", service.pid, &log_file)
        }
        InstalledServiceState::Running => print_running("agent is running", service.pid, &log_file),
        InstalledServiceState::Stopping => {
            print_running("agent is stopping", service.pid, &log_file)
        }
        InstalledServiceState::Resuming => {
            print_running("agent is resuming", service.pid, &log_file)
        }
        InstalledServiceState::Pausing => print_running("agent is pausing", service.pid, &log_file),
        InstalledServiceState::Paused => print_running("agent is paused", service.pid, &log_file),
    }
    Ok(true)
}

#[cfg(target_os = "macos")]
fn query_service_without_install_marker() -> bool {
    true
}

#[cfg(not(target_os = "macos"))]
fn query_service_without_install_marker() -> bool {
    false
}

pub async fn follow_logs(config: &AgentConfig) -> Result<()> {
    let path = RuntimePaths::for_observer(config)?.log_file;
    let mut follower = LogFollower::open(path.clone())
        .with_context(|| format!("failed to open agent log {}", path.display()))?;
    let stdout = io::stdout();
    let mut output = stdout.lock();
    if let Err(error) = follower.copy_available(&mut output) {
        if error.kind() == io::ErrorKind::BrokenPipe {
            return Ok(());
        }
        return Err(error).with_context(|| format!("failed to read agent log {}", path.display()));
    }

    let mut interval = tokio::time::interval(POLL_INTERVAL);
    interval.tick().await;
    let shutdown = wait_for_shutdown_signal();
    tokio::pin!(shutdown);
    loop {
        tokio::select! {
            result = &mut shutdown => return result,
            _ = interval.tick() => {
                if let Err(error) = follower.copy_available(&mut output) {
                    if error.kind() == io::ErrorKind::BrokenPipe {
                        return Ok(());
                    }
                    return Err(error).with_context(|| {
                        format!("failed to follow agent log {}", path.display())
                    });
                }
            }
        }
    }
}

pub async fn run_agent(config: AgentConfig) -> Result<()> {
    let paths = RuntimePaths::from_config(&config)?;
    paths.prepare()?;
    let guard = RuntimeGuard::acquire(paths)?;
    let identity = load_or_create_identity(config.identity_file.clone())?;

    crate::logging::info(format_args!("agent instance_id: {}", identity.instance_id));
    crate::logging::info(format_args!("server: {}", config.server));
    guard.mark_ready()?;

    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let websocket = agent_ws_loop(config, identity, shutdown_rx);
    tokio::pin!(websocket);
    let lifecycle_result = tokio::select! {
        result = &mut websocket => return result,
        result = wait_for_stop(guard.stop_file(), guard.pid()) => {
            if result.is_ok() {
                crate::logging::info(format_args!("stop requested; agent is shutting down"));
            }
            result
        },
        result = wait_for_shutdown_signal() => {
            if result.is_ok() {
                crate::logging::info(format_args!("shutdown signal received; agent is shutting down"));
            }
            result
        },
    };

    shutdown_tx.send_replace(true);
    let websocket_result = websocket.await;
    lifecycle_result?;
    websocket_result
}

#[cfg(unix)]
async fn wait_for_shutdown_signal() -> Result<()> {
    use tokio::signal::unix::{SignalKind, signal};

    let mut interrupt =
        signal(SignalKind::interrupt()).context("failed to listen for the interrupt signal")?;
    let mut terminate =
        signal(SignalKind::terminate()).context("failed to listen for the terminate signal")?;
    tokio::select! {
        _ = interrupt.recv() => {}
        _ = terminate.recv() => {}
    }
    Ok(())
}

#[cfg(windows)]
async fn wait_for_shutdown_signal() -> Result<()> {
    tokio::signal::ctrl_c()
        .await
        .context("failed to listen for Ctrl+C")
}

fn wait_until_ready(child: &mut Child, pid: u32, paths: &RuntimePaths) -> Result<()> {
    let started = Instant::now();
    while started.elapsed() <= START_TIMEOUT {
        if let Some(exit_status) = child.try_wait()? {
            bail!(
                "background agent exited during startup with {exit_status}; inspect {}",
                paths.log_file.display()
            );
        }
        if paths.ready_pid() == Some(pid)
            && matches!(paths.process_state()?, ProcessState::Running(Some(value)) if value == pid)
        {
            return Ok(());
        }
        thread::sleep(POLL_INTERVAL);
    }

    let _ = child.kill();
    let _ = child.wait();
    bail!(
        "background agent did not become ready within {} seconds; inspect {}",
        START_TIMEOUT.as_secs(),
        paths.log_file.display()
    )
}

async fn wait_for_stop(path: &Path, pid: u32) -> Result<()> {
    let mut interval = tokio::time::interval(POLL_INTERVAL);
    loop {
        interval.tick().await;
        match fs::read_to_string(path) {
            Ok(value) if value.trim() == "stop" || value.trim() == pid.to_string() => return Ok(()),
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("failed to read stop request {}", path.display()));
            }
        }
    }
}

fn print_running(prefix: &str, pid: Option<u32>, log_file: &Path) {
    match pid {
        Some(pid) => println!("{prefix} (pid {pid})"),
        None => println!("{prefix}"),
    }
    println!("log: {}", log_file.display());
}

pub fn log_file(config: &AgentConfig) -> Result<PathBuf> {
    Ok(RuntimePaths::from_config(config)?.log_file)
}

struct LogFollower {
    path: PathBuf,
    file: File,
    identity: FileIdentity,
    position: u64,
}

impl LogFollower {
    fn open(path: PathBuf) -> io::Result<Self> {
        let file = File::open(&path)?;
        let identity = file_identity(&file.metadata()?);
        Ok(Self {
            path,
            file,
            identity,
            position: 0,
        })
    }

    fn copy_available(&mut self, output: &mut impl Write) -> io::Result<()> {
        match fs::metadata(&self.path) {
            Ok(metadata) if file_identity(&metadata) != self.identity => self.reopen()?,
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(error),
        }

        if self.file.metadata()?.len() < self.position {
            self.position = 0;
        }
        self.file.seek(SeekFrom::Start(self.position))?;
        let copied = io::copy(&mut self.file, output)?;
        self.position = self.position.saturating_add(copied);
        output.flush()
    }

    fn reopen(&mut self) -> io::Result<()> {
        let file = File::open(&self.path)?;
        self.identity = file_identity(&file.metadata()?);
        self.file = file;
        self.position = 0;
        Ok(())
    }
}

#[cfg(unix)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct FileIdentity {
    device: u64,
    inode: u64,
}

#[cfg(unix)]
fn file_identity(metadata: &fs::Metadata) -> FileIdentity {
    use std::os::unix::fs::MetadataExt;

    FileIdentity {
        device: metadata.dev(),
        inode: metadata.ino(),
    }
}

#[cfg(windows)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct FileIdentity(u64);

#[cfg(windows)]
fn file_identity(metadata: &fs::Metadata) -> FileIdentity {
    use std::os::windows::fs::MetadataExt;

    FileIdentity(metadata.creation_time())
}

#[cfg(unix)]
fn detach(command: &mut Command) {
    use std::os::unix::process::CommandExt;

    unsafe {
        command.pre_exec(|| {
            if libc::setsid() == -1 {
                Err(io::Error::last_os_error())
            } else {
                Ok(())
            }
        });
    }
}

#[cfg(windows)]
fn detach(command: &mut Command) {
    use std::os::windows::process::CommandExt;
    use windows_sys::Win32::System::Threading::{CREATE_NEW_PROCESS_GROUP, CREATE_NO_WINDOW};

    command.creation_flags(CREATE_NEW_PROCESS_GROUP | CREATE_NO_WINDOW);
}

struct RuntimeGuard {
    paths: RuntimePaths,
    _lock: File,
    pid: u32,
}

impl RuntimeGuard {
    fn acquire(paths: RuntimePaths) -> Result<Self> {
        let lock = paths.open_lock()?;
        match lock.try_lock() {
            Ok(()) => {}
            Err(TryLockError::WouldBlock) => {
                let pid = paths.pid();
                bail!(
                    "agent is already running{}",
                    pid.map_or_else(String::new, |pid| format!(" (pid {pid})"))
                );
            }
            Err(TryLockError::Error(error)) => {
                return Err(error).context("failed to acquire agent process lock");
            }
        }

        paths.remove_stale_files();
        let pid = std::process::id();
        fs::write(&paths.pid_file, pid.to_string())
            .with_context(|| format!("failed to write PID file {}", paths.pid_file.display()))?;
        Ok(Self {
            paths,
            _lock: lock,
            pid,
        })
    }

    fn mark_ready(&self) -> Result<()> {
        fs::write(&self.paths.ready_file, self.pid.to_string()).with_context(|| {
            format!(
                "failed to write ready file {}",
                self.paths.ready_file.display()
            )
        })
    }

    fn stop_file(&self) -> &Path {
        &self.paths.stop_file
    }

    fn pid(&self) -> u32 {
        self.pid
    }
}

impl Drop for RuntimeGuard {
    fn drop(&mut self) {
        self.paths.remove_if_owned(&self.paths.pid_file, self.pid);
        self.paths.remove_if_owned(&self.paths.ready_file, self.pid);
        self.paths.remove_if_owned(&self.paths.stop_file, self.pid);
        if fs::read_to_string(&self.paths.stop_file).is_ok_and(|value| value.trim() == "stop") {
            let _ = fs::remove_file(&self.paths.stop_file);
        }
    }
}

#[derive(Debug)]
struct RuntimePaths {
    state_dir: PathBuf,
    lock_file: PathBuf,
    pid_file: PathBuf,
    ready_file: PathBuf,
    stop_file: PathBuf,
    log_file: PathBuf,
}

impl RuntimePaths {
    fn from_config(config: &AgentConfig) -> Result<Self> {
        Self::from_config_with_installed_paths(config, None)
    }

    fn for_observer(config: &AgentConfig) -> Result<Self> {
        let installed = installed_runtime_paths();
        Self::from_config_with_installed_paths(config, installed.as_ref())
    }

    fn from_config_with_installed_paths(
        config: &AgentConfig,
        installed: Option<&InstalledRuntimePaths>,
    ) -> Result<Self> {
        let state_dir = config
            .state_dir
            .clone()
            .or_else(|| installed.map(|paths| paths.state_dir.clone()))
            .or_else(|| {
                ProjectDirs::from("com", "operation-monitoring", "agent")
                    .map(|dirs| dirs.data_local_dir().join("runtime"))
            })
            .unwrap_or(env::current_dir()?.join(".om-agent"));
        let log_file = config.log_file.clone().unwrap_or_else(|| {
            if config.state_dir.is_none()
                && let Some(paths) = installed
            {
                return paths.log_file.clone();
            }
            state_dir.join("agent.log")
        });
        Ok(Self {
            lock_file: state_dir.join("agent.lock"),
            pid_file: state_dir.join("agent.pid"),
            ready_file: state_dir.join("agent.ready"),
            stop_file: state_dir.join("agent.stop"),
            state_dir,
            log_file,
        })
    }

    fn prepare(&self) -> Result<()> {
        fs::create_dir_all(&self.state_dir).with_context(|| {
            format!(
                "failed to create agent state directory {}",
                self.state_dir.display()
            )
        })
    }

    fn open_lock(&self) -> Result<File> {
        OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(&self.lock_file)
            .with_context(|| format!("failed to open process lock {}", self.lock_file.display()))
    }

    fn process_state(&self) -> Result<ProcessState> {
        if !self.lock_file.try_exists()? {
            return Ok(ProcessState::Stopped);
        }
        let lock = self.open_lock()?;
        match lock.try_lock() {
            Ok(()) => {
                lock.unlock()?;
                Ok(ProcessState::Stopped)
            }
            Err(TryLockError::WouldBlock) => Ok(ProcessState::Running(self.pid())),
            Err(TryLockError::Error(error)) => {
                Err(anyhow!(error)).context("failed to inspect agent process lock")
            }
        }
    }

    fn pid(&self) -> Option<u32> {
        read_pid(&self.pid_file)
    }

    fn ready_pid(&self) -> Option<u32> {
        read_pid(&self.ready_file)
    }

    fn remove_stale_files(&self) {
        for path in [&self.pid_file, &self.ready_file, &self.stop_file] {
            let _ = fs::remove_file(path);
        }
    }

    fn remove_if_owned(&self, path: &Path, pid: u32) {
        if read_pid(path) == Some(pid) {
            let _ = fs::remove_file(path);
        }
    }
}

#[derive(Debug)]
struct InstalledRuntimePaths {
    state_dir: PathBuf,
    log_file: PathBuf,
}

#[allow(dead_code)] // Some service states only exist on specific target platforms.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum InstalledServiceState {
    Stopped,
    Starting,
    Running,
    Stopping,
    Resuming,
    Pausing,
    Paused,
}

#[derive(Debug, PartialEq, Eq)]
struct InstalledServiceStatus {
    state: InstalledServiceState,
    pid: Option<u32>,
}

#[cfg(windows)]
fn installed_runtime_paths() -> Option<InstalledRuntimePaths> {
    let data_dir = PathBuf::from(env::var_os("ProgramData")?).join("OperationMonitoring");
    if !data_dir.join("install-type").is_file() {
        return None;
    }
    Some(InstalledRuntimePaths {
        state_dir: data_dir.join("runtime"),
        log_file: data_dir.join("logs/agent.log"),
    })
}

#[cfg(target_os = "macos")]
fn installed_runtime_paths() -> Option<InstalledRuntimePaths> {
    let data_dir = PathBuf::from("/Library/Application Support/OperationMonitoring");
    let installed = data_dir.join("install-type").is_file()
        || Path::new("/Library/LaunchDaemons/com.operation-monitoring.agent.plist").is_file()
        || Path::new("/usr/local/bin/om-agent").is_file()
        || Path::new("/usr/local/bin/operation-monitoring-agent").is_file();
    if !installed {
        return None;
    }
    Some(InstalledRuntimePaths {
        state_dir: data_dir.join("runtime"),
        log_file: PathBuf::from("/Library/Logs/OperationMonitoring/agent.log"),
    })
}

#[cfg(all(unix, not(target_os = "macos")))]
fn installed_runtime_paths() -> Option<InstalledRuntimePaths> {
    let (name, openwrt) = if Path::new("/etc/om-agent/install-type").is_file() {
        ("om-agent", Path::new("/etc/init.d/om-agent").exists())
    } else if Path::new("/etc/operation-monitoring-agent/install-type").is_file() {
        (
            "operation-monitoring-agent",
            Path::new("/etc/init.d/operation-monitoring-agent").exists(),
        )
    } else {
        return None;
    };
    Some(unix_installed_runtime_paths(name, openwrt))
}

#[cfg(any(all(unix, not(target_os = "macos")), test))]
fn unix_installed_runtime_paths(name: &str, openwrt: bool) -> InstalledRuntimePaths {
    let runtime_root = if openwrt { "/var/run" } else { "/run" };
    InstalledRuntimePaths {
        state_dir: PathBuf::from(runtime_root).join(name),
        log_file: PathBuf::from("/var/log").join(name).join("agent.log"),
    }
}

#[cfg(windows)]
fn installed_service_status() -> Result<Option<InstalledServiceStatus>> {
    let mut stopped = None;
    for service_name in ["operation-monitoring-agent", "om-agent"] {
        let output = Command::new("sc.exe")
            .args(["queryex", service_name])
            .output()
            .with_context(|| format!("failed to query Windows service {service_name}"))?;
        let text = format!(
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        if !output.status.success() {
            if text.contains("1060") {
                continue;
            }
            bail!(
                "sc queryex {service_name} exited with {}: {}",
                output.status,
                text.trim()
            );
        }
        let status = parse_windows_service_query(&text)?;
        if status.state != InstalledServiceState::Stopped {
            return Ok(Some(status));
        }
        stopped = Some(status);
    }
    Ok(stopped)
}

#[cfg(any(windows, test))]
fn parse_windows_service_query(text: &str) -> Result<InstalledServiceStatus> {
    let mut state = None;
    let mut pid = None;
    for line in text.lines() {
        let Some((label, fields)) = line.split_once(':') else {
            continue;
        };
        let value = fields.split_whitespace().next();
        if state.is_none() {
            state =
                value
                    .and_then(|value| value.parse::<u32>().ok())
                    .and_then(|value| match value {
                        1 => Some(InstalledServiceState::Stopped),
                        2 => Some(InstalledServiceState::Starting),
                        3 => Some(InstalledServiceState::Stopping),
                        4 => Some(InstalledServiceState::Running),
                        5 => Some(InstalledServiceState::Resuming),
                        6 => Some(InstalledServiceState::Pausing),
                        7 => Some(InstalledServiceState::Paused),
                        _ => None,
                    });
        }
        if label.trim().eq_ignore_ascii_case("PID") {
            pid = value
                .and_then(|value| value.parse::<u32>().ok())
                .filter(|value| *value != 0);
        }
    }
    Ok(InstalledServiceStatus {
        state: state.context("sc queryex did not report a service state")?,
        pid,
    })
}

#[cfg(all(unix, not(target_os = "macos")))]
fn installed_service_status() -> Result<Option<InstalledServiceStatus>> {
    for service_path in [
        "/etc/init.d/om-agent",
        "/etc/init.d/operation-monitoring-agent",
    ] {
        if Path::new(service_path).exists() {
            let status = Command::new(service_path)
                .arg("status")
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .with_context(|| format!("failed to query service {service_path}"))?;
            let pid = installed_runtime_paths()
                .as_ref()
                .and_then(|paths| read_pid(&paths.state_dir.join("agent.pid")));
            return Ok(Some(InstalledServiceStatus {
                state: if status.success() {
                    InstalledServiceState::Running
                } else {
                    InstalledServiceState::Stopped
                },
                pid,
            }));
        }
    }

    let mut stopped = None;
    for service_name in ["om-agent.service", "operation-monitoring-agent.service"] {
        let output = Command::new("systemctl")
            .args([
                "show",
                service_name,
                "--property=LoadState",
                "--property=ActiveState",
                "--property=SubState",
                "--property=MainPID",
                "--no-pager",
            ])
            .output()
            .with_context(|| format!("failed to query systemd service {service_name}"))?;
        let text = format!(
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        if !output.status.success() {
            if text.contains("not found") || text.contains("could not be found") {
                continue;
            }
            bail!(
                "systemctl show {service_name} exited with {}: {}",
                output.status,
                text.trim()
            );
        }
        let Some(status) = parse_systemd_service_query(&text)? else {
            continue;
        };
        if status.state != InstalledServiceState::Stopped {
            return Ok(Some(status));
        }
        stopped = Some(status);
    }
    Ok(stopped)
}

#[cfg(any(all(unix, not(target_os = "macos")), test))]
fn parse_systemd_service_query(text: &str) -> Result<Option<InstalledServiceStatus>> {
    let mut load_state = None;
    let mut active_state = None;
    let mut pid = None;
    for line in text.lines() {
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        match key.trim() {
            "LoadState" => load_state = Some(value.trim()),
            "ActiveState" => active_state = Some(value.trim()),
            "MainPID" => {
                pid = value.trim().parse::<u32>().ok().filter(|value| *value != 0);
            }
            _ => {}
        }
    }
    if load_state == Some("not-found") {
        return Ok(None);
    }
    let state = match active_state.context("systemctl show did not report ActiveState")? {
        "active" => InstalledServiceState::Running,
        "activating" | "reloading" => InstalledServiceState::Starting,
        "deactivating" => InstalledServiceState::Stopping,
        "inactive" | "failed" => InstalledServiceState::Stopped,
        _ => InstalledServiceState::Starting,
    };
    Ok(Some(InstalledServiceStatus { state, pid }))
}

#[cfg(target_os = "macos")]
fn installed_service_status() -> Result<Option<InstalledServiceStatus>> {
    let output = Command::new("/bin/launchctl")
        .args(["print", "system/com.operation-monitoring.agent"])
        .output()
        .context("failed to query launchd service")?;
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    if !output.status.success() {
        if text.contains("Could not find service") || text.contains("service not found") {
            return Ok(Some(macos_agent_process_status().unwrap_or(
                InstalledServiceStatus {
                    state: InstalledServiceState::Stopped,
                    pid: None,
                },
            )));
        }
        bail!(
            "launchctl print exited with {}: {}",
            output.status,
            text.trim()
        );
    }
    let status = parse_launchd_service_query(&text);
    if status.state == InstalledServiceState::Stopped
        && let Some(process) = macos_agent_process_status()
    {
        return Ok(Some(process));
    }
    Ok(Some(status))
}

#[cfg(target_os = "macos")]
fn macos_agent_process_status() -> Option<InstalledServiceStatus> {
    let system = sysinfo::System::new_all();
    system.processes().values().find_map(|process| {
        is_macos_agent_process(process.exe(), process.name(), process.cmd()).then(|| {
            InstalledServiceStatus {
                state: InstalledServiceState::Running,
                pid: Some(process.pid().as_u32()),
            }
        })
    })
}

#[cfg(any(target_os = "macos", test))]
fn is_macos_agent_process(executable: Option<&Path>, name: &str, command: &[String]) -> bool {
    let known_name =
        matches!(name, "om-agent" | "operation-monitoring-agent") || name.starts_with("om-agent_");
    let known_executable = executable.is_some_and(|path| {
        path.file_name().is_some_and(|value| {
            let value = value.to_string_lossy();
            matches!(value.as_ref(), "om-agent" | "operation-monitoring-agent")
                || value.starts_with("om-agent_")
        })
    });
    let service_mode = command.iter().any(|argument| argument == "service-run")
        || (command.iter().any(|argument| argument == "start")
            && command.iter().any(|argument| argument == "--daemon-child"));
    (known_name || known_executable) && service_mode
}

#[cfg(any(target_os = "macos", test))]
fn parse_launchd_service_query(text: &str) -> InstalledServiceStatus {
    let mut state = None;
    let mut pid = None;
    for line in text.lines().map(str::trim) {
        if state.is_none()
            && let Some(value) = line.strip_prefix("state = ")
        {
            state = Some(match value.trim() {
                "running" => InstalledServiceState::Running,
                "exited" => InstalledServiceState::Stopped,
                _ => InstalledServiceState::Starting,
            });
        } else if let Some(value) = line.strip_prefix("pid = ") {
            pid = value.trim().parse::<u32>().ok().filter(|value| *value != 0);
        }
    }
    InstalledServiceStatus {
        state: state.unwrap_or(InstalledServiceState::Starting),
        pid,
    }
}

enum ProcessState {
    Running(Option<u32>),
    Stopped,
}

fn read_pid(path: &Path) -> Option<u32> {
    fs::read_to_string(path).ok()?.trim().parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_config(state_dir: PathBuf) -> AgentConfig {
        AgentConfig {
            server: "http://127.0.0.1:13500".to_owned(),
            identity_file: None,
            report_interval: 5,
            state_dir: Some(state_dir),
            log_file: None,
            log_max_bytes: 10 * 1024 * 1024,
            log_history: 3,
            update_dir: None,
        }
    }

    #[test]
    fn runtime_guard_exposes_running_state_and_cleans_up() {
        let state_dir =
            std::env::temp_dir().join(format!("om-agent-test-{}", uuid::Uuid::new_v4()));
        let paths = RuntimePaths::from_config(&test_config(state_dir.clone())).unwrap();
        paths.prepare().unwrap();

        let guard = RuntimeGuard::acquire(paths).unwrap();
        guard.mark_ready().unwrap();
        assert!(matches!(
            guard.paths.process_state().unwrap(),
            ProcessState::Running(Some(pid)) if pid == std::process::id()
        ));
        assert_eq!(guard.paths.ready_pid(), Some(std::process::id()));

        drop(guard);
        let paths = RuntimePaths::from_config(&test_config(state_dir.clone())).unwrap();
        assert!(matches!(
            paths.process_state().unwrap(),
            ProcessState::Stopped
        ));
        assert_eq!(paths.pid(), None);
        let _ = fs::remove_dir_all(state_dir);
    }

    #[test]
    fn missing_state_directory_is_reported_as_stopped() {
        let state_dir =
            std::env::temp_dir().join(format!("om-agent-test-{}", uuid::Uuid::new_v4()));
        let paths = RuntimePaths::from_config(&test_config(state_dir)).unwrap();

        assert!(matches!(
            paths.process_state().unwrap(),
            ProcessState::Stopped
        ));
    }

    #[test]
    fn stopping_a_missing_agent_does_not_create_runtime_files() {
        let state_dir =
            std::env::temp_dir().join(format!("om-agent-test-{}", uuid::Uuid::new_v4()));

        assert!(!stop_if_running(&test_config(state_dir.clone()), 0).unwrap());
        assert!(!state_dir.exists());
    }

    #[test]
    fn observer_uses_installed_paths_when_options_are_not_explicit() {
        let root = std::env::temp_dir().join(format!("om-agent-test-{}", uuid::Uuid::new_v4()));
        let installed = InstalledRuntimePaths {
            state_dir: root.join("runtime"),
            log_file: root.join("logs/agent.log"),
        };

        let paths = RuntimePaths::from_config_with_installed_paths(
            &test_config_with_optional_state(None),
            Some(&installed),
        )
        .unwrap();

        assert_eq!(paths.state_dir, installed.state_dir);
        assert_eq!(paths.log_file, installed.log_file);
    }

    #[test]
    fn resolves_systemd_openwrt_and_legacy_runtime_paths() {
        let systemd = unix_installed_runtime_paths("om-agent", false);
        assert_eq!(systemd.state_dir, PathBuf::from("/run/om-agent"));
        assert_eq!(
            systemd.log_file,
            PathBuf::from("/var/log/om-agent/agent.log")
        );

        let openwrt = unix_installed_runtime_paths("om-agent", true);
        assert_eq!(openwrt.state_dir, PathBuf::from("/var/run/om-agent"));

        let legacy = unix_installed_runtime_paths("operation-monitoring-agent", false);
        assert_eq!(
            legacy.state_dir,
            PathBuf::from("/run/operation-monitoring-agent")
        );
        assert_eq!(
            legacy.log_file,
            PathBuf::from("/var/log/operation-monitoring-agent/agent.log")
        );
    }

    #[test]
    fn explicit_paths_override_installed_paths() {
        let root = std::env::temp_dir().join(format!("om-agent-test-{}", uuid::Uuid::new_v4()));
        let explicit_state = root.join("explicit-runtime");
        let explicit_log = root.join("explicit.log");
        let installed = InstalledRuntimePaths {
            state_dir: root.join("installed-runtime"),
            log_file: root.join("installed.log"),
        };
        let mut config = test_config(explicit_state.clone());
        config.log_file = Some(explicit_log.clone());

        let paths =
            RuntimePaths::from_config_with_installed_paths(&config, Some(&installed)).unwrap();

        assert_eq!(paths.state_dir, explicit_state);
        assert_eq!(paths.log_file, explicit_log);
    }

    #[test]
    fn log_follower_reads_appends_and_rotated_files() {
        let root = std::env::temp_dir().join(format!("om-agent-test-{}", uuid::Uuid::new_v4()));
        let path = root.join("agent.log");
        fs::create_dir_all(&root).unwrap();
        fs::write(&path, "first\n").unwrap();
        let mut follower = LogFollower::open(path.clone()).unwrap();
        let mut output = Vec::new();

        follower.copy_available(&mut output).unwrap();
        OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap()
            .write_all(b"second\n")
            .unwrap();
        follower.copy_available(&mut output).unwrap();
        fs::rename(&path, root.join("agent.log.1")).unwrap();
        fs::write(&path, "third\n").unwrap();
        follower.copy_available(&mut output).unwrap();

        assert_eq!(String::from_utf8(output).unwrap(), "first\nsecond\nthird\n");
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn parses_windows_service_state_and_pid() {
        let status = parse_windows_service_query(
            "SERVICE_NAME: operation-monitoring-agent\n        TYPE               : 10  WIN32_OWN_PROCESS\n        STATE              : 4  RUNNING\n        PID                : 1234\n",
        )
        .unwrap();

        assert_eq!(
            status,
            InstalledServiceStatus {
                state: InstalledServiceState::Running,
                pid: Some(1234),
            }
        );
    }

    #[test]
    fn parses_systemd_service_state_and_pid() {
        let status = parse_systemd_service_query(
            "LoadState=loaded\nActiveState=active\nSubState=running\nMainPID=4321\n",
        )
        .unwrap()
        .unwrap();

        assert_eq!(
            status,
            InstalledServiceStatus {
                state: InstalledServiceState::Running,
                pid: Some(4321),
            }
        );
        assert_eq!(
            parse_systemd_service_query(
                "LoadState=not-found\nActiveState=inactive\nSubState=dead\nMainPID=0\n"
            )
            .unwrap(),
            None
        );
    }

    #[test]
    fn parses_launchd_service_state_and_pid() {
        let status = parse_launchd_service_query(
            "system/com.operation-monitoring.agent = {\n\tstate = running\n\tpid = 2468\n\tresource coalition = {\n\t\tstate = active\n\t}\n\tjetsam coalition = {\n\t\tstate = active\n\t}\n}\n",
        );

        assert_eq!(
            status,
            InstalledServiceStatus {
                state: InstalledServiceState::Running,
                pid: Some(2468),
            }
        );
    }

    #[test]
    fn recognizes_only_installed_macos_service_processes() {
        assert!(is_macos_agent_process(
            Some(Path::new("/usr/local/bin/om-agent")),
            "om-agent",
            &["/usr/local/bin/om-agent".into(), "service-run".into()],
        ));
        assert!(is_macos_agent_process(
            None,
            "om-agent",
            &["/usr/local/bin/om-agent".into(), "service-run".into()],
        ));
        assert!(is_macos_agent_process(
            Some(Path::new("/tmp/om-agent_0.1.5_macos_arm64.bin")),
            "om-agent_0.1.5_macos_arm64.bin",
            &[
                "/tmp/om-agent_0.1.5_macos_arm64.bin".into(),
                "start".into(),
                "--daemon-child".into(),
            ],
        ));
        assert!(!is_macos_agent_process(
            Some(Path::new("/usr/local/bin/om-agent")),
            "om-agent",
            &["/usr/local/bin/om-agent".into(), "log".into()],
        ));
        assert!(!is_macos_agent_process(
            Some(Path::new("/tmp/om-agent")),
            "unrelated",
            &["/tmp/unrelated".into(), "status".into()],
        ));
    }

    fn test_config_with_optional_state(state_dir: Option<PathBuf>) -> AgentConfig {
        AgentConfig {
            state_dir,
            ..test_config(PathBuf::new())
        }
    }
}
