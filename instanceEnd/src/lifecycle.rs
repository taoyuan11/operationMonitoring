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
    #[cfg(windows)]
    if config.state_dir.is_none() && installed_runtime_paths().is_some() {
        reject_installed_windows_service_overrides()?;
        return control_installed_windows_service("start", Duration::from_secs(30));
    }
    config.server_endpoint()?;
    let paths = RuntimePaths::from_config(config)?;
    paths.prepare()?;

    if let ProcessState::Running(pid) = paths.process_state()? {
        print_running("agent is already running", pid, &paths.log_file);
        return Ok(());
    }
    paths.remove_stale_files();

    let mut command = Command::new(env::current_exe()?);
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
    #[cfg(windows)]
    if config.state_dir.is_none() && installed_runtime_paths().is_some() {
        reject_installed_windows_service_overrides()?;
        return control_installed_windows_service("stop", Duration::from_secs(timeout_seconds));
    }
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

pub async fn run_agent(mut config: AgentConfig) -> Result<()> {
    let started_at = Instant::now();
    config.normalize_server()?;
    crate::logging::info(format_args!(
        "agent starting: version={} pid={} server={:?}",
        env!("CARGO_PKG_VERSION"),
        std::process::id(),
        config.server
    ));
    let paths = RuntimePaths::from_config(&config)?;
    paths.prepare()?;
    let guard = RuntimeGuard::acquire(paths)?;
    let identity = load_or_create_identity(config.identity_file.clone())?;

    guard.mark_ready()?;
    crate::logging::info(format_args!(
        "agent started: version={} pid={} instance_id={:?} server={:?}",
        env!("CARGO_PKG_VERSION"),
        guard.pid(),
        identity.instance_id,
        config.server
    ));

    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let websocket = agent_ws_loop(config, identity, shutdown_rx);
    tokio::pin!(websocket);
    let (stop_reason, lifecycle_result) = tokio::select! {
        result = &mut websocket => {
            log_agent_stopped("websocket_loop_ended", started_at.elapsed(), &result);
            return result;
        },
        result = wait_for_stop(guard.stop_file(), guard.pid()) => {
            if result.is_ok() {
                crate::logging::info(format_args!(
                    "agent stopping: reason=stop_request pid={}",
                    guard.pid()
                ));
            }
            ("stop_request", result)
        },
        result = wait_for_shutdown_signal() => {
            if result.is_ok() {
                crate::logging::info(format_args!(
                    "agent stopping: reason=shutdown_signal pid={}",
                    guard.pid()
                ));
            }
            ("shutdown_signal", result)
        },
    };

    shutdown_tx.send_replace(true);
    let websocket_result = websocket.await;
    let result = lifecycle_result.and(websocket_result);
    log_agent_stopped(stop_reason, started_at.elapsed(), &result);
    result
}

fn log_agent_stopped(reason: &str, uptime: Duration, result: &Result<()>) {
    match result {
        Ok(()) => crate::logging::info(format_args!(
            "agent stopped: reason={reason} pid={} uptime_ms={}",
            std::process::id(),
            uptime.as_millis()
        )),
        Err(error) => crate::logging::error(format_args!(
            "agent stopped with error: reason={reason} pid={} uptime_ms={} error={error:#}",
            std::process::id(),
            uptime.as_millis()
        )),
    }
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
    let paths = RuntimePaths::from_config(config)?;
    paths.prepare()?;
    paths.prepare_log()?;
    Ok(paths.log_file)
}

pub(crate) fn docker_protected_paths(config: &AgentConfig) -> Result<Vec<PathBuf>> {
    let paths = RuntimePaths::from_config(config)?;
    Ok(vec![paths.state_dir, paths.log_file])
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
        let identity = file_identity(&file)?;
        Ok(Self {
            path,
            file,
            identity,
            position: 0,
        })
    }

    fn copy_available(&mut self, output: &mut impl Write) -> io::Result<()> {
        match path_file_identity(&self.path) {
            Ok(identity) if identity != self.identity => self.reopen()?,
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
        self.identity = file_identity(&file)?;
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
fn file_identity(file: &File) -> io::Result<FileIdentity> {
    use std::os::unix::fs::MetadataExt;

    let metadata = file.metadata()?;
    Ok(FileIdentity {
        device: metadata.dev(),
        inode: metadata.ino(),
    })
}

#[cfg(windows)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct FileIdentity {
    volume_serial_number: u32,
    file_index: u64,
}

#[cfg(windows)]
fn file_identity(file: &File) -> io::Result<FileIdentity> {
    use std::os::windows::io::AsRawHandle;
    use windows::{
        Win32::{
            Foundation::HANDLE,
            Storage::FileSystem::{BY_HANDLE_FILE_INFORMATION, GetFileInformationByHandle},
        },
        core::Error,
    };

    let mut information = BY_HANDLE_FILE_INFORMATION::default();
    unsafe { GetFileInformationByHandle(HANDLE(file.as_raw_handle()), &mut information) }
        .map_err(|error: Error| io::Error::other(error))?;
    Ok(FileIdentity {
        volume_serial_number: information.dwVolumeSerialNumber,
        file_index: (u64::from(information.nFileIndexHigh) << 32)
            | u64::from(information.nFileIndexLow),
    })
}

#[cfg(unix)]
fn path_file_identity(path: &Path) -> io::Result<FileIdentity> {
    use std::os::unix::fs::MetadataExt;

    let metadata = fs::metadata(path)?;
    Ok(FileIdentity {
        device: metadata.dev(),
        inode: metadata.ino(),
    })
}

#[cfg(windows)]
fn path_file_identity(path: &Path) -> io::Result<FileIdentity> {
    let file = File::open(path)?;
    file_identity(&file)
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
    configured_state_dir: bool,
    lock_file: PathBuf,
    pid_file: PathBuf,
    ready_file: PathBuf,
    stop_file: PathBuf,
    log_file: PathBuf,
    configured_log_file: bool,
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
        let state_dir = resolve_state_dir(config, installed, || {
            Ok(env::current_dir()?.join(".om-agent"))
        })?;
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
            configured_state_dir: config.state_dir.is_some(),
            log_file,
            configured_log_file: config.log_file.is_some(),
        })
    }

    fn prepare(&self) -> Result<()> {
        crate::privileged_path::prepare_configured_directory(
            &self.state_dir,
            self.configured_state_dir,
            "agent state directory",
        )?;
        Ok(())
    }

    fn prepare_log(&self) -> Result<()> {
        let parent = self
            .log_file
            .parent()
            .context("agent log path has no parent directory")?;
        crate::privileged_path::prepare_configured_directory(
            parent,
            self.configured_log_file || self.configured_state_dir,
            "agent log directory",
        )?;
        if self.log_file.try_exists()? {
            crate::privileged_path::validate_regular_file(&self.log_file, "agent log file")?;
        }
        Ok(())
    }

    fn open_lock(&self) -> Result<File> {
        if self.lock_file.try_exists()? {
            crate::privileged_path::validate_regular_file(&self.lock_file, "agent process lock")?;
        }
        let mut options = OpenOptions::new();
        options.create(true).read(true).write(true).truncate(false);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
        }
        let file = options
            .open(&self.lock_file)
            .with_context(|| format!("failed to open process lock {}", self.lock_file.display()))?;
        if !file.metadata()?.is_file() {
            bail!(
                "agent process lock {} is not a regular file",
                self.lock_file.display()
            );
        }
        Ok(file)
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

fn resolve_state_dir(
    config: &AgentConfig,
    installed: Option<&InstalledRuntimePaths>,
    fallback: impl FnOnce() -> Result<PathBuf>,
) -> Result<PathBuf> {
    if let Some(path) = &config.state_dir {
        return Ok(path.clone());
    }
    if let Some(paths) = installed {
        return Ok(paths.state_dir.clone());
    }
    if let Some(dirs) = ProjectDirs::from("com", "operation-monitoring", "agent") {
        return Ok(dirs.data_local_dir().join("runtime"));
    }
    fallback()
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct InstalledServiceStatus {
    state: InstalledServiceState,
    pid: Option<u32>,
}

#[cfg(any(windows, test))]
const WINDOWS_SERVICE_CONFIG_OPTIONS: &[&str] = &[
    "--server",
    "--identity-file",
    "--report-interval",
    "--log-file",
    "--log-max-bytes",
    "--log-history",
    "--update-dir",
    "--remote-desktop-consent",
    "--windows-virtual-devices",
];

#[cfg(any(windows, test))]
const WINDOWS_SERVICE_CONFIG_ENV: &[&str] = &[
    "OM_SERVER",
    "OM_AGENT_ID_FILE",
    "OM_REPORT_INTERVAL",
    "OM_AGENT_LOG_FILE",
    "OM_AGENT_LOG_MAX_BYTES",
    "OM_AGENT_LOG_HISTORY",
    "OM_AGENT_UPDATE_DIR",
    "OM_REMOTE_DESKTOP_CONSENT",
    "OM_WINDOWS_VIRTUAL_DEVICES",
];

#[cfg(any(windows, test))]
fn explicit_windows_service_override<I, S, F>(
    arguments: I,
    mut environment_contains: F,
) -> Option<String>
where
    I: IntoIterator<Item = S>,
    S: AsRef<std::ffi::OsStr>,
    F: FnMut(&str) -> bool,
{
    for argument in arguments {
        let Some(argument) = argument.as_ref().to_str() else {
            continue;
        };
        if let Some(option) = WINDOWS_SERVICE_CONFIG_OPTIONS.iter().find(|option| {
            argument == **option
                || argument
                    .strip_prefix(**option)
                    .is_some_and(|suffix| suffix.starts_with('='))
        }) {
            return Some((*option).to_owned());
        }
    }
    WINDOWS_SERVICE_CONFIG_ENV
        .iter()
        .find(|name| environment_contains(name))
        .map(|name| (*name).to_owned())
}

#[cfg(windows)]
fn reject_installed_windows_service_overrides() -> Result<()> {
    if let Some(setting) =
        explicit_windows_service_override(env::args_os(), |name| env::var_os(name).is_some())
    {
        bail!(
            "{setting} cannot override the installed Windows service configuration; run `om-agent install` again to change service settings, or use --state-dir for a separate agent"
        );
    }
    Ok(())
}

#[cfg(windows)]
fn installed_runtime_paths() -> Option<InstalledRuntimePaths> {
    let data_dir = crate::windows_security::program_data_directory()
        .ok()?
        .join("OperationMonitoring");
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
    Ok(preferred_installed_windows_service()?.map(|(_, status)| status))
}

#[cfg(any(windows, test))]
fn select_preferred_windows_service(
    services: &[(&'static str, Option<InstalledServiceStatus>)],
) -> Option<(&'static str, InstalledServiceStatus)> {
    services
        .iter()
        .filter_map(|(name, status)| status.map(|status| (*name, status)))
        .find(|(_, status)| status.state != InstalledServiceState::Stopped)
        .or_else(|| {
            services
                .iter()
                .find_map(|(name, status)| status.map(|status| (*name, status)))
        })
}

#[cfg(windows)]
fn preferred_installed_windows_service() -> Result<Option<(&'static str, InstalledServiceStatus)>> {
    let services = [
        (
            "operation-monitoring-agent",
            installed_service_status_for_name("operation-monitoring-agent")?,
        ),
        ("om-agent", installed_service_status_for_name("om-agent")?),
    ];
    Ok(select_preferred_windows_service(&services))
}

#[cfg(any(windows, test))]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum WindowsServiceControlPlan {
    Unchanged,
    Wait(InstalledServiceState),
    Command {
        verb: &'static str,
        expected: InstalledServiceState,
    },
    WaitThenCommand {
        wait_for: InstalledServiceState,
        verb: &'static str,
        expected: InstalledServiceState,
    },
}

#[cfg(any(windows, test))]
fn windows_service_control_plan(
    action: &str,
    state: InstalledServiceState,
) -> Option<WindowsServiceControlPlan> {
    use InstalledServiceState::*;
    use WindowsServiceControlPlan::*;

    match (action, state) {
        ("start", Running) | ("stop", Stopped) => Some(Unchanged),
        ("start", Starting | Resuming) => Some(Wait(Running)),
        ("start", Stopping) => Some(WaitThenCommand {
            wait_for: Stopped,
            verb: "start",
            expected: Running,
        }),
        ("start", Pausing) => Some(WaitThenCommand {
            wait_for: Paused,
            verb: "continue",
            expected: Running,
        }),
        ("start", Paused) => Some(Command {
            verb: "continue",
            expected: Running,
        }),
        ("start", Stopped) => Some(Command {
            verb: "start",
            expected: Running,
        }),
        ("stop", Stopping) => Some(Wait(Stopped)),
        ("stop", Starting | Resuming) => Some(WaitThenCommand {
            wait_for: Running,
            verb: "stop",
            expected: Stopped,
        }),
        ("stop", Pausing) => Some(WaitThenCommand {
            wait_for: Paused,
            verb: "stop",
            expected: Stopped,
        }),
        ("stop", Running | Paused) => Some(Command {
            verb: "stop",
            expected: Stopped,
        }),
        _ => None,
    }
}

#[cfg(windows)]
fn control_installed_windows_service(action: &str, timeout: Duration) -> Result<()> {
    let Some(_) = windows_service_control_plan(action, InstalledServiceState::Stopped) else {
        bail!("unsupported Windows service action: {action}");
    };
    if !crate::install::is_elevated_for_service_control() {
        return crate::install::elevate_service_control(action);
    }

    let (service_name, status) = preferred_installed_windows_service()?
        .context("installed agent service was not found; run `om-agent install` again")?;
    let plan = windows_service_control_plan(action, status.state)
        .context("unsupported Windows service control transition")?;
    let started = Instant::now();
    let remaining = || timeout.saturating_sub(started.elapsed());
    match plan {
        WindowsServiceControlPlan::Unchanged => {
            if action == "start" {
                println!("agent is already running");
            } else {
                println!("agent is not running");
            }
            return Ok(());
        }
        WindowsServiceControlPlan::Wait(expected) => {
            wait_for_windows_service_state(service_name, expected, remaining())?;
        }
        WindowsServiceControlPlan::Command { verb, expected } => {
            run_windows_service_command(service_name, verb, expected, remaining())?;
        }
        WindowsServiceControlPlan::WaitThenCommand {
            wait_for,
            verb,
            expected,
        } => {
            let reached = wait_for_windows_service_state_or(
                service_name,
                wait_for,
                Some(expected),
                remaining(),
            )?;
            if reached != expected {
                run_windows_service_command(service_name, verb, expected, remaining())?;
            }
        }
    }
    let completed_action = if action == "start" {
        "started"
    } else {
        "stopped"
    };
    println!("agent service {completed_action} ({service_name})");
    Ok(())
}

#[cfg(windows)]
fn run_windows_service_command(
    service_name: &str,
    verb: &str,
    expected: InstalledServiceState,
    timeout: Duration,
) -> Result<()> {
    let status = Command::new("sc.exe")
        .args([verb, service_name])
        .status()
        .with_context(|| format!("failed to {verb} Windows agent service"))?;
    if !status.success() {
        let current = installed_service_status_for_name(service_name)?
            .map(|status| status.state)
            .context("installed Windows agent service disappeared")?;
        let progressing = matches!(
            (current, expected),
            (
                InstalledServiceState::Starting | InstalledServiceState::Resuming,
                InstalledServiceState::Running
            ) | (
                InstalledServiceState::Stopping,
                InstalledServiceState::Stopped
            )
        );
        if current != expected && !progressing {
            bail!("sc {verb} {service_name} exited with {status}");
        }
    }
    wait_for_windows_service_state(service_name, expected, timeout)
}

#[cfg(windows)]
fn wait_for_windows_service_state(
    service_name: &str,
    expected: InstalledServiceState,
    timeout: Duration,
) -> Result<()> {
    wait_for_windows_service_state_or(service_name, expected, None, timeout).map(|_| ())
}

#[cfg(windows)]
fn wait_for_windows_service_state_or(
    service_name: &str,
    expected: InstalledServiceState,
    alternate: Option<InstalledServiceState>,
    timeout: Duration,
) -> Result<InstalledServiceState> {
    let started = Instant::now();
    loop {
        if let Some(status) = installed_service_status_for_name(service_name)? {
            if status.state == expected || alternate == Some(status.state) {
                return Ok(status.state);
            }
            if expected == InstalledServiceState::Running
                && status.state == InstalledServiceState::Stopped
            {
                let log = installed_runtime_paths()
                    .map(|paths| paths.log_file.display().to_string())
                    .unwrap_or_else(|| "the installed agent log".to_owned());
                bail!("Windows agent service stopped during startup; inspect {log}");
            }
        }
        if started.elapsed() >= timeout {
            bail!(
                "Windows agent service {service_name} did not reach the expected state within {} seconds",
                timeout.as_secs()
            );
        }
        thread::sleep(POLL_INTERVAL);
    }
}

#[cfg(windows)]
fn installed_service_status_for_name(service_name: &str) -> Result<Option<InstalledServiceStatus>> {
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
            return Ok(None);
        }
        bail!(
            "sc queryex {service_name} exited with {}: {}",
            output.status,
            text.trim()
        );
    }
    Ok(Some(parse_windows_service_query(&text)?))
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
fn is_macos_agent_process(
    executable: Option<&Path>,
    name: &std::ffi::OsStr,
    command: &[std::ffi::OsString],
) -> bool {
    let name = name.to_string_lossy();
    let known_name = matches!(name.as_ref(), "om-agent" | "operation-monitoring-agent")
        || name.starts_with("om-agent_");
    let known_executable = executable.is_some_and(|path| {
        path.file_name().is_some_and(|value| {
            let value = value.to_string_lossy();
            matches!(value.as_ref(), "om-agent" | "operation-monitoring-agent")
                || value.starts_with("om-agent_")
        })
    });
    let service_mode = command
        .iter()
        .any(|argument| argument == std::ffi::OsStr::new("service-run"))
        || (command
            .iter()
            .any(|argument| argument == std::ffi::OsStr::new("start"))
            && command
                .iter()
                .any(|argument| argument == std::ffi::OsStr::new("--daemon-child")));
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
            remote_desktop_consent: crate::config::RemoteDesktopConsent::Required,
            windows_virtual_devices: crate::config::WindowsVirtualDevices::Disabled,
        }
    }

    #[test]
    fn runtime_guard_exposes_running_state_and_cleans_up() {
        let state_dir = env::temp_dir().join(format!("om-agent-test-{}", uuid::Uuid::new_v4()));
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
        let state_dir = env::temp_dir().join(format!("om-agent-test-{}", uuid::Uuid::new_v4()));
        let paths = RuntimePaths::from_config(&test_config(state_dir)).unwrap();

        assert!(matches!(
            paths.process_state().unwrap(),
            ProcessState::Stopped
        ));
    }

    #[test]
    fn stopping_a_missing_agent_does_not_create_runtime_files() {
        let state_dir = env::temp_dir().join(format!("om-agent-test-{}", uuid::Uuid::new_v4()));

        assert!(!stop_if_running(&test_config(state_dir.clone()), 0).unwrap());
        assert!(!state_dir.exists());
    }

    #[test]
    fn observer_uses_installed_paths_when_options_are_not_explicit() {
        let root = env::temp_dir().join(format!("om-agent-test-{}", uuid::Uuid::new_v4()));
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
    fn installed_paths_do_not_require_a_valid_working_directory() {
        let root = env::temp_dir().join(format!("om-agent-test-{}", uuid::Uuid::new_v4()));
        let installed = InstalledRuntimePaths {
            state_dir: root.join("runtime"),
            log_file: root.join("logs/agent.log"),
        };

        let state_dir = resolve_state_dir(
            &test_config_with_optional_state(None),
            Some(&installed),
            || bail!("working directory fallback must not be evaluated"),
        )
        .unwrap();

        assert_eq!(state_dir, installed.state_dir);
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
        let root = env::temp_dir().join(format!("om-agent-test-{}", uuid::Uuid::new_v4()));
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
        let root = env::temp_dir().join(format!("om-agent-test-{}", uuid::Uuid::new_v4()));
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
    fn windows_service_selection_prefers_an_active_legacy_service() {
        let selected = select_preferred_windows_service(&[
            (
                "operation-monitoring-agent",
                Some(InstalledServiceStatus {
                    state: InstalledServiceState::Stopped,
                    pid: None,
                }),
            ),
            (
                "om-agent",
                Some(InstalledServiceStatus {
                    state: InstalledServiceState::Running,
                    pid: Some(42),
                }),
            ),
        ])
        .unwrap();

        assert_eq!(selected.0, "om-agent");
        assert_eq!(selected.1.state, InstalledServiceState::Running);
    }

    #[test]
    fn windows_service_selection_prefers_the_current_name_when_both_are_stopped() {
        let selected = select_preferred_windows_service(&[
            (
                "operation-monitoring-agent",
                Some(InstalledServiceStatus {
                    state: InstalledServiceState::Stopped,
                    pid: None,
                }),
            ),
            (
                "om-agent",
                Some(InstalledServiceStatus {
                    state: InstalledServiceState::Stopped,
                    pid: None,
                }),
            ),
        ])
        .unwrap();

        assert_eq!(selected.0, "operation-monitoring-agent");
    }

    #[test]
    fn windows_service_start_plans_cover_every_transition_state() {
        use InstalledServiceState::*;
        use WindowsServiceControlPlan::*;

        assert_eq!(
            windows_service_control_plan("start", Running),
            Some(Unchanged)
        );
        assert_eq!(
            windows_service_control_plan("start", Starting),
            Some(Wait(Running))
        );
        assert_eq!(
            windows_service_control_plan("start", Resuming),
            Some(Wait(Running))
        );
        assert_eq!(
            windows_service_control_plan("start", Stopping),
            Some(WaitThenCommand {
                wait_for: Stopped,
                verb: "start",
                expected: Running,
            })
        );
        assert_eq!(
            windows_service_control_plan("start", Pausing),
            Some(WaitThenCommand {
                wait_for: Paused,
                verb: "continue",
                expected: Running,
            })
        );
        assert_eq!(
            windows_service_control_plan("start", Paused),
            Some(Command {
                verb: "continue",
                expected: Running,
            })
        );
        assert_eq!(
            windows_service_control_plan("start", Stopped),
            Some(Command {
                verb: "start",
                expected: Running,
            })
        );
    }

    #[test]
    fn windows_service_stop_plans_wait_for_pending_transitions() {
        use InstalledServiceState::*;
        use WindowsServiceControlPlan::*;

        assert_eq!(
            windows_service_control_plan("stop", Stopped),
            Some(Unchanged)
        );
        assert_eq!(
            windows_service_control_plan("stop", Stopping),
            Some(Wait(Stopped))
        );
        for state in [Starting, Resuming] {
            assert_eq!(
                windows_service_control_plan("stop", state),
                Some(WaitThenCommand {
                    wait_for: Running,
                    verb: "stop",
                    expected: Stopped,
                })
            );
        }
        assert_eq!(
            windows_service_control_plan("stop", Pausing),
            Some(WaitThenCommand {
                wait_for: Paused,
                verb: "stop",
                expected: Stopped,
            })
        );
        for state in [Running, Paused] {
            assert_eq!(
                windows_service_control_plan("stop", state),
                Some(Command {
                    verb: "stop",
                    expected: Stopped,
                })
            );
        }
    }

    #[test]
    fn installed_windows_service_rejects_explicit_configuration_overrides() {
        assert_eq!(
            explicit_windows_service_override(
                ["om-agent", "start", "--server=https://monitor.example"],
                |_| false,
            ),
            Some("--server".to_owned())
        );
        assert_eq!(
            explicit_windows_service_override(["om-agent", "stop"], |name| {
                name == "OM_AGENT_LOG_FILE"
            }),
            Some("OM_AGENT_LOG_FILE".to_owned())
        );
    }

    #[test]
    fn independent_windows_agent_and_stop_timeout_are_not_service_overrides() {
        assert_eq!(
            explicit_windows_service_override(
                [
                    "om-agent",
                    "stop",
                    "--state-dir",
                    r"C:\\agent-state",
                    "--timeout",
                    "20",
                ],
                |_| false,
            ),
            None
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
            std::ffi::OsStr::new("om-agent"),
            &["/usr/local/bin/om-agent".into(), "service-run".into()],
        ));
        assert!(is_macos_agent_process(
            None,
            std::ffi::OsStr::new("om-agent"),
            &["/usr/local/bin/om-agent".into(), "service-run".into()],
        ));
        assert!(is_macos_agent_process(
            Some(Path::new("/tmp/om-agent_0.1.5_macos_arm64.bin")),
            std::ffi::OsStr::new("om-agent_0.1.5_macos_arm64.bin"),
            &[
                "/tmp/om-agent_0.1.5_macos_arm64.bin".into(),
                "start".into(),
                "--daemon-child".into(),
            ],
        ));
        assert!(!is_macos_agent_process(
            Some(Path::new("/usr/local/bin/om-agent")),
            std::ffi::OsStr::new("om-agent"),
            &["/usr/local/bin/om-agent".into(), "log".into()],
        ));
        assert!(!is_macos_agent_process(
            Some(Path::new("/tmp/om-agent")),
            std::ffi::OsStr::new("unrelated"),
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
