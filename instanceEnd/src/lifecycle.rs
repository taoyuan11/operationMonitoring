use std::{
    fs::{self, File, OpenOptions, TryLockError},
    io,
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    thread,
    time::{Duration, Instant},
};

use anyhow::{Context, Result, anyhow, bail};
use directories::ProjectDirs;

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

    let (stdout, stderr) = open_log_files(&paths.log_file)?;
    let mut command = Command::new(std::env::current_exe()?);
    command
        .arg("start")
        .arg("--daemon-child")
        .stdin(Stdio::null())
        .stdout(Stdio::from(stdout))
        .stderr(Stdio::from(stderr));
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
    let paths = RuntimePaths::from_config(config)?;
    paths.prepare()?;

    let pid = match paths.process_state()? {
        ProcessState::Stopped => {
            paths.remove_stale_files();
            println!("agent is not running");
            return Ok(());
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
            return Ok(());
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
    let paths = RuntimePaths::from_config(config)?;
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

pub async fn run_agent(config: AgentConfig) -> Result<()> {
    let paths = RuntimePaths::from_config(&config)?;
    paths.prepare()?;
    let guard = RuntimeGuard::acquire(paths)?;
    let identity = load_or_create_identity(config.identity_file.clone())?;

    println!("agent instance_id: {}", identity.instance_id);
    println!("server: {}", config.server);
    guard.mark_ready()?;

    tokio::select! {
        result = agent_ws_loop(config, identity) => result,
        result = wait_for_stop(guard.stop_file(), guard.pid()) => {
            result?;
            println!("stop requested; agent is shutting down");
            Ok(())
        },
        result = wait_for_shutdown_signal() => {
            result?;
            println!("shutdown signal received; agent is shutting down");
            Ok(())
        },
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

fn open_log_files(path: &Path) -> Result<(File, File)> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create log directory {}", parent.display()))?;
    }
    let stdout = OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .with_context(|| format!("failed to open log file {}", path.display()))?;
    let stderr = stdout.try_clone()?;
    Ok((stdout, stderr))
}

fn print_running(prefix: &str, pid: Option<u32>, log_file: &Path) {
    match pid {
        Some(pid) => println!("{prefix} (pid {pid})"),
        None => println!("{prefix}"),
    }
    println!("log: {}", log_file.display());
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
        let state_dir = match &config.state_dir {
            Some(path) => path.clone(),
            None => ProjectDirs::from("com", "operation-monitoring", "agent")
                .map(|dirs| dirs.data_local_dir().join("runtime"))
                .unwrap_or(std::env::current_dir()?.join(".om-agent")),
        };
        let log_file = config
            .log_file
            .clone()
            .unwrap_or_else(|| state_dir.join("agent.log"));
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
}
