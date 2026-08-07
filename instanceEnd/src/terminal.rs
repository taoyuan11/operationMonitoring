use std::{
    collections::HashMap,
    ffi::OsString,
    io::{self, Read, Write},
    sync::{
        Arc,
        mpsc::{self, RecvTimeoutError},
    },
    thread,
    time::Duration,
};

use anyhow::Context as _;
use base64::{Engine as _, engine::general_purpose::STANDARD};
use portable_pty::{CommandBuilder, PtySize, native_pty_system};
use tokio::sync::Semaphore;

#[cfg(windows)]
use std::{
    os::windows::process::CommandExt,
    process::{Command, Stdio},
};

#[cfg(windows)]
use windows::{
    Win32::System::{
        LibraryLoader::{GetModuleHandleW, GetProcAddress},
        Threading::CREATE_NO_WINDOW,
    },
    core::{s, w},
};

use crate::{
    activity::ActivityTracker, models::AgentInbound, outbound::AgentEventSender,
    pty_io::PtyInputWriter,
};

const MAX_TERMINAL_SESSIONS: usize = 8;
const MAX_TERMINAL_INPUT_BYTES: usize = 64 * 1024;
const TERMINAL_CONTROL_QUEUE_CAPACITY: usize = 128;
const TERMINAL_WRITE_QUEUE_CAPACITY: usize = 16;
const TERMINAL_PROCESS_EXIT_TIMEOUT: Duration = Duration::from_secs(2);
#[cfg(any(windows, test))]
const TERMINAL_PIPE_QUEUE_CAPACITY: usize = 64;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ShellKind {
    #[cfg(any(windows, test))]
    PowerShell,
    #[cfg(any(windows, test))]
    Cmd,
    #[cfg(any(not(windows), test))]
    Unix,
}

#[derive(Clone, Debug)]
struct ShellCandidate {
    name: &'static str,
    program: OsString,
    kind: ShellKind,
}

impl ShellCandidate {
    fn new(name: &'static str, program: impl Into<OsString>, kind: ShellKind) -> Self {
        Self {
            name,
            program: program.into(),
            kind,
        }
    }

    fn arguments(&self) -> &'static [&'static str] {
        match self.kind {
            #[cfg(any(windows, test))]
            ShellKind::PowerShell => &["-NoLogo", "-NoExit"],
            #[cfg(any(windows, test))]
            ShellKind::Cmd => &["/D", "/Q", "/K", "chcp 65001>nul"],
            #[cfg(any(not(windows), test))]
            ShellKind::Unix => &["-i"],
        }
    }

    fn command_builder(&self) -> CommandBuilder {
        let mut command = CommandBuilder::new(&self.program);
        command.args(self.arguments().iter().copied());
        command
    }

    fn description(&self) -> String {
        format!("{} ({})", self.name, self.program.to_string_lossy())
    }
}

#[cfg(any(windows, test))]
fn windows_shell_candidates(comspec: Option<OsString>) -> Vec<ShellCandidate> {
    let mut candidates = vec![
        ShellCandidate::new("PowerShell 7", "pwsh.exe", ShellKind::PowerShell),
        ShellCandidate::new(
            "Windows PowerShell",
            "powershell.exe",
            ShellKind::PowerShell,
        ),
    ];
    if let Some(comspec) = comspec.filter(|value| !value.is_empty()) {
        candidates.push(ShellCandidate::new(
            "cmd (COMSPEC)",
            comspec,
            ShellKind::Cmd,
        ));
    }
    candidates.push(ShellCandidate::new("cmd", "cmd.exe", ShellKind::Cmd));
    candidates
}

#[cfg(any(target_os = "macos", test))]
fn macos_shell_candidates() -> Vec<ShellCandidate> {
    vec![
        ShellCandidate::new("zsh", "/bin/zsh", ShellKind::Unix),
        ShellCandidate::new("bash", "/bin/bash", ShellKind::Unix),
        ShellCandidate::new("sh", "/bin/sh", ShellKind::Unix),
    ]
}

#[cfg(any(all(not(windows), not(target_os = "macos")), test))]
fn linux_shell_candidates() -> Vec<ShellCandidate> {
    vec![
        ShellCandidate::new("bash", "bash", ShellKind::Unix),
        ShellCandidate::new("zsh", "zsh", ShellKind::Unix),
        ShellCandidate::new("ash", "ash", ShellKind::Unix),
        ShellCandidate::new("sh", "sh", ShellKind::Unix),
    ]
}

fn platform_shell_candidates() -> Vec<ShellCandidate> {
    #[cfg(windows)]
    {
        windows_shell_candidates(std::env::var_os("COMSPEC"))
    }
    #[cfg(target_os = "macos")]
    {
        macos_shell_candidates()
    }
    #[cfg(all(not(windows), not(target_os = "macos")))]
    {
        linux_shell_candidates()
    }
}

fn try_shell_candidates<T>(
    candidates: &[ShellCandidate],
    transport: &str,
    mut start: impl FnMut(&ShellCandidate) -> anyhow::Result<T>,
) -> anyhow::Result<T> {
    let mut failures = Vec::with_capacity(candidates.len());
    for candidate in candidates {
        match start(candidate) {
            Ok(terminal) => {
                crate::logging::info(format_args!(
                    "terminal selected {} using {transport}",
                    candidate.description()
                ));
                return Ok(terminal);
            }
            Err(error) => {
                let failure = format!("{}: {error:#}", candidate.description());
                crate::logging::info(format_args!(
                    "terminal could not start {failure}; trying the next shell"
                ));
                failures.push(failure);
            }
        }
    }

    anyhow::bail!(
        "none of the supported shells could be started using {transport}: {}",
        failures.join("; ")
    )
}

enum TerminalControl {
    Input(Vec<u8>),
    Resize { cols: u16, rows: u16 },
    Close,
}

pub struct TerminalManager {
    sessions: HashMap<String, mpsc::SyncSender<TerminalControl>>,
    session_slots: Arc<Semaphore>,
    outbound: AgentEventSender,
    stream_outbound: AgentEventSender,
    activity: ActivityTracker,
}

impl TerminalManager {
    pub fn new(
        outbound: AgentEventSender,
        stream_outbound: AgentEventSender,
        activity: ActivityTracker,
    ) -> Self {
        Self {
            sessions: HashMap::new(),
            session_slots: Arc::new(Semaphore::new(MAX_TERMINAL_SESSIONS)),
            outbound,
            stream_outbound,
            activity,
        }
    }

    pub fn open(&mut self, session_id: String, cols: u16, rows: u16) {
        if self.sessions.contains_key(&session_id) {
            let _ = self.outbound.send(AgentInbound::TerminalClosed {
                session_id,
                exit_code: None,
                reason: Some("终端会话已存在".to_string()),
            });
            return;
        }
        let Some(activity_guard) = self.activity.try_enter() else {
            let _ = self.outbound.send(AgentInbound::TerminalClosed {
                session_id,
                exit_code: None,
                reason: Some("agent update is waiting to install".to_string()),
            });
            return;
        };
        let Ok(session_slot) = self.session_slots.clone().try_acquire_owned() else {
            let _ = self.outbound.send(AgentInbound::TerminalClosed {
                session_id,
                exit_code: None,
                reason: Some("终端会话数量已达到上限".to_string()),
            });
            return;
        };
        let (control_tx, control_rx) = mpsc::sync_channel(TERMINAL_CONTROL_QUEUE_CAPACITY);
        let stream_outbound = self.stream_outbound.clone();
        let worker_session_id = session_id.clone();
        let worker = thread::Builder::new().spawn(move || {
            let _activity_guard = activity_guard;
            let _session_slot = session_slot;
            run_terminal(worker_session_id, cols, rows, control_rx, stream_outbound);
        });
        match worker {
            Ok(_) => {
                self.sessions.insert(session_id, control_tx);
            }
            Err(error) => {
                let _ = self.outbound.send(AgentInbound::TerminalClosed {
                    session_id,
                    exit_code: None,
                    reason: Some(format!("无法创建终端线程: {error}")),
                });
            }
        }
    }

    pub fn input(&mut self, session_id: &str, encoded_data: &str) {
        let Some(session) = self.sessions.get(session_id) else {
            return;
        };
        if encoded_data.len() > MAX_TERMINAL_INPUT_BYTES {
            self.fail_session(session_id, "终端输入过大");
            return;
        }
        match STANDARD.decode(encoded_data) {
            Ok(data) => {
                if session.try_send(TerminalControl::Input(data)).is_err() {
                    self.fail_session(session_id, "终端输入队列拥塞");
                }
            }
            Err(error) => {
                self.fail_session(session_id, &format!("终端输入编码无效: {error}"));
            }
        }
    }

    pub fn resize(&mut self, session_id: &str, cols: u16, rows: u16) {
        let Some(session) = self.sessions.get(session_id) else {
            return;
        };
        if session
            .try_send(TerminalControl::Resize {
                cols: cols.clamp(2, 500),
                rows: rows.clamp(1, 300),
            })
            .is_err()
        {
            self.fail_session(session_id, "终端控制队列拥塞");
        }
    }

    pub fn close(&mut self, session_id: &str) {
        if let Some(session) = self.sessions.remove(session_id) {
            let _ = session.try_send(TerminalControl::Close);
        }
    }

    pub fn close_all(&mut self) {
        for (_, session) in self.sessions.drain() {
            let _ = session.try_send(TerminalControl::Close);
        }
    }

    fn fail_session(&mut self, session_id: &str, reason: &str) {
        self.sessions.remove(session_id);
        let _ = self.outbound.send(AgentInbound::TerminalClosed {
            session_id: session_id.to_string(),
            exit_code: None,
            reason: Some(reason.to_string()),
        });
    }
}

fn run_terminal(
    session_id: String,
    cols: u16,
    rows: u16,
    control_rx: mpsc::Receiver<TerminalControl>,
    stream_outbound: AgentEventSender,
) {
    if let Err(error) = run_terminal_inner(
        &session_id,
        cols.clamp(2, 500),
        rows.clamp(1, 300),
        control_rx,
        stream_outbound.clone(),
    ) {
        crate::logging::error(format_args!(
            "terminal session {session_id} failed: {error:#}"
        ));
        let _ = stream_outbound.blocking_send(AgentInbound::TerminalClosed {
            session_id,
            exit_code: None,
            reason: Some(format!("无法启动交互式终端: {error:#}")),
        });
    }
}

fn run_terminal_inner(
    session_id: &str,
    cols: u16,
    rows: u16,
    control_rx: mpsc::Receiver<TerminalControl>,
    stream_outbound: AgentEventSender,
) -> anyhow::Result<()> {
    let RunningTerminal {
        mut process,
        master,
        mut reader,
        writer,
    } = open_terminal(cols, rows)?;
    let input_writer =
        match PtyInputWriter::spawn(writer, TERMINAL_WRITE_QUEUE_CAPACITY, "om-terminal-input") {
            Ok(writer) => writer,
            Err(error) => {
                let _ = process.terminate();
                return Err(error.into());
            }
        };

    if let Err(error) = stream_outbound.blocking_send(AgentInbound::TerminalOpened {
        session_id: session_id.to_string(),
    }) {
        let _ = process.terminate();
        return Err(error.into());
    }

    let reader_session_id = session_id.to_string();
    let reader_outbound = stream_outbound.clone();
    let reader_task = thread::spawn(move || {
        forward_terminal_output(&mut reader, reader_session_id, reader_outbound)
    });

    let (exit_code, reason) = loop {
        if let Some(error) = input_writer.take_failure() {
            break (process.terminate(), Some(format!("写入终端失败: {error}")));
        }
        match control_rx.recv_timeout(Duration::from_millis(100)) {
            Ok(TerminalControl::Input(data)) => {
                if let Err(error) = input_writer.try_write(data) {
                    break (
                        process.terminate(),
                        Some(format!("终端输入写入失败: {error}")),
                    );
                }
            }
            Ok(TerminalControl::Resize { cols, rows }) => {
                if let Err(error) = master.resize(cols, rows) {
                    break (
                        process.terminate(),
                        Some(format!("调整终端大小失败: {error}")),
                    );
                }
            }
            Ok(TerminalControl::Close) => {
                break (process.terminate(), None);
            }
            Err(RecvTimeoutError::Timeout) => {
                if let Some(exit_code) = process.try_wait_code()? {
                    break (Some(exit_code), None);
                }
            }
            Err(RecvTimeoutError::Disconnected) => {
                break (process.terminate(), Some("终端控制通道已关闭".to_string()));
            }
        }
    };

    drop(input_writer);
    drop(master);
    drop(reader_task);
    let _ = stream_outbound.blocking_send(AgentInbound::TerminalClosed {
        session_id: session_id.to_string(),
        exit_code,
        reason,
    });
    Ok(())
}

fn forward_terminal_output<R: Read>(
    reader: &mut R,
    session_id: String,
    outbound: AgentEventSender,
) {
    let mut buffer = [0_u8; 16 * 1024];
    loop {
        match reader.read(&mut buffer) {
            Ok(0) => break,
            Ok(count) => {
                let data = STANDARD.encode(&buffer[..count]);
                if outbound
                    .blocking_send(AgentInbound::TerminalOutput {
                        session_id: session_id.clone(),
                        data,
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
}

struct RunningTerminal {
    process: TerminalProcess,
    master: TerminalMaster,
    reader: Box<dyn Read + Send>,
    writer: Box<dyn Write + Send>,
}

enum TerminalProcess {
    Pty(Box<dyn portable_pty::Child + Send + Sync>),
    #[cfg(windows)]
    Pipe(std::process::Child),
}

impl TerminalProcess {
    fn try_wait_code(&mut self) -> io::Result<Option<i64>> {
        match self {
            Self::Pty(child) => child
                .try_wait()
                .map(|status| status.map(|status| status.exit_code() as i64)),
            #[cfg(windows)]
            Self::Pipe(child) => child
                .try_wait()
                .map(|status| status.map(|status| status.code().unwrap_or(-1) as i64)),
        }
    }

    fn terminate(&mut self) -> Option<i64> {
        if let Ok(Some(exit_code)) = self.try_wait_code() {
            return Some(exit_code);
        }
        let pid = match self {
            Self::Pty(child) => child.process_id(),
            #[cfg(windows)]
            Self::Pipe(child) => Some(child.id()),
        };
        if let Some(pid) = pid {
            terminate_terminal_process_group(pid);
        }
        match self {
            Self::Pty(child) => {
                let _ = child.kill();
            }
            #[cfg(windows)]
            Self::Pipe(child) => {
                let _ = child.kill();
            }
        }

        let deadline = std::time::Instant::now() + TERMINAL_PROCESS_EXIT_TIMEOUT;
        loop {
            match self.try_wait_code() {
                Ok(Some(exit_code)) => return Some(exit_code),
                Ok(None) | Err(_) if std::time::Instant::now() < deadline => {
                    thread::sleep(Duration::from_millis(10));
                }
                Ok(None) | Err(_) => return None,
            }
        }
    }
}

#[cfg(unix)]
fn terminate_terminal_process_group(pid: u32) {
    if let Ok(pid) = i32::try_from(pid) {
        unsafe {
            let _ = libc::kill(-pid, libc::SIGKILL);
        }
    }
}

#[cfg(not(unix))]
fn terminate_terminal_process_group(_pid: u32) {}

enum TerminalMaster {
    Pty(Box<dyn portable_pty::MasterPty + Send>),
    #[cfg(windows)]
    Pipe,
}

impl TerminalMaster {
    fn resize(&self, cols: u16, rows: u16) -> anyhow::Result<()> {
        match self {
            Self::Pty(master) => master.resize(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            }),
            #[cfg(windows)]
            Self::Pipe => Ok(()),
        }
    }
}

fn open_terminal(cols: u16, rows: u16) -> anyhow::Result<RunningTerminal> {
    #[cfg(windows)]
    {
        if conpty_available() {
            match open_pty_terminal(cols, rows) {
                Ok(terminal) => return Ok(terminal),
                Err(error) => crate::logging::error(format_args!(
                    "ConPTY terminal initialization failed; falling back to a pipe-backed terminal: {error:#}"
                )),
            }
        } else {
            crate::logging::info(format_args!(
                "ConPTY is unavailable on this Windows version; using a pipe-backed terminal"
            ));
        }

        return open_pipe_terminal().context("failed to start the legacy Windows terminal");
    }

    #[cfg(not(windows))]
    open_pty_terminal(cols, rows)
}

fn open_pty_terminal(cols: u16, rows: u16) -> anyhow::Result<RunningTerminal> {
    let candidates = platform_shell_candidates();
    let transport = if cfg!(windows) { "ConPTY" } else { "PTY" };
    try_shell_candidates(&candidates, transport, |candidate| {
        open_pty_terminal_with_shell(candidate, cols, rows)
    })
}

fn open_pty_terminal_with_shell(
    candidate: &ShellCandidate,
    cols: u16,
    rows: u16,
) -> anyhow::Result<RunningTerminal> {
    let pair = native_pty_system()
        .openpty(PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        })
        .with_context(|| format!("failed to allocate a PTY for {}", candidate.description()))?;
    let mut command = candidate.command_builder();
    command.env("TERM", "xterm-256color");
    command.env("COLORTERM", "truecolor");

    let slave = pair.slave;
    let master = pair.master;
    let child = slave
        .spawn_command(command)
        .with_context(|| format!("failed to spawn {}", candidate.description()))?;
    drop(slave);
    let reader = match master.try_clone_reader() {
        Ok(reader) => reader,
        Err(error) => {
            let mut process = TerminalProcess::Pty(child);
            let _ = process.terminate();
            return Err(error).with_context(|| {
                format!(
                    "failed to open the output stream for {}",
                    candidate.description()
                )
            });
        }
    };
    let writer = match master.take_writer() {
        Ok(writer) => writer,
        Err(error) => {
            let mut process = TerminalProcess::Pty(child);
            let _ = process.terminate();
            return Err(error).with_context(|| {
                format!(
                    "failed to open the input stream for {}",
                    candidate.description()
                )
            });
        }
    };

    Ok(RunningTerminal {
        process: TerminalProcess::Pty(child),
        master: TerminalMaster::Pty(master),
        reader,
        writer,
    })
}

#[cfg(windows)]
fn conpty_available() -> bool {
    // portable-pty lazily loads these functions and panics when they are not
    // exported by kernel32.dll.  Detect support before touching the native
    // implementation so legacy Windows (notably Server 2016) cannot abort the
    // agent process when a browser opens a terminal.
    unsafe {
        let Ok(kernel32) = GetModuleHandleW(w!("kernel32.dll")) else {
            return false;
        };
        GetProcAddress(kernel32, s!("CreatePseudoConsole")).is_some()
            && GetProcAddress(kernel32, s!("ResizePseudoConsole")).is_some()
            && GetProcAddress(kernel32, s!("ClosePseudoConsole")).is_some()
    }
}

#[cfg(windows)]
fn open_pipe_terminal() -> anyhow::Result<RunningTerminal> {
    let candidates = platform_shell_candidates();
    try_shell_candidates(&candidates, "pipe-backed terminal", |candidate| {
        open_pipe_terminal_with_shell(candidate)
    })
}

#[cfg(windows)]
fn open_pipe_terminal_with_shell(candidate: &ShellCandidate) -> anyhow::Result<RunningTerminal> {
    let mut command = Command::new(&candidate.program);
    command
        .args(candidate.arguments())
        .env("TERM", "xterm-256color")
        .env("COLORTERM", "truecolor")
        .creation_flags(CREATE_NO_WINDOW.0)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let mut child = command
        .spawn()
        .with_context(|| format!("failed to spawn {}", candidate.description()))?;
    let (Some(stdin), Some(stdout), Some(stderr)) =
        (child.stdin.take(), child.stdout.take(), child.stderr.take())
    else {
        let _ = child.kill();
        let _ = child.wait();
        anyhow::bail!(
            "redirected standard handles were not created for {}",
            candidate.description()
        );
    };

    Ok(RunningTerminal {
        process: TerminalProcess::Pipe(child),
        master: TerminalMaster::Pipe,
        reader: merged_pipe_reader(stdout, stderr),
        writer: Box::new(stdin),
    })
}

#[cfg(windows)]
fn merged_pipe_reader(
    stdout: std::process::ChildStdout,
    stderr: std::process::ChildStderr,
) -> Box<dyn Read + Send> {
    let (chunks_tx, chunks_rx) = mpsc::sync_channel(TERMINAL_PIPE_QUEUE_CAPACITY);
    forward_pipe(stdout, chunks_tx.clone());
    forward_pipe(stderr, chunks_tx);
    Box::new(PipeReader {
        chunks: chunks_rx,
        pending: Vec::new(),
        offset: 0,
    })
}

#[cfg(windows)]
fn forward_pipe<R>(mut reader: R, chunks_tx: mpsc::SyncSender<Vec<u8>>)
where
    R: Read + Send + 'static,
{
    thread::spawn(move || {
        let mut buffer = [0_u8; 16 * 1024];
        loop {
            match reader.read(&mut buffer) {
                Ok(0) => break,
                Ok(count) => {
                    if chunks_tx.send(buffer[..count].to_vec()).is_err() {
                        break;
                    }
                }
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(_) => break,
            }
        }
    });
}

#[cfg(any(windows, test))]
struct PipeReader {
    chunks: mpsc::Receiver<Vec<u8>>,
    pending: Vec<u8>,
    offset: usize,
}

#[cfg(any(windows, test))]
impl Read for PipeReader {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        if buffer.is_empty() {
            return Ok(0);
        }

        loop {
            if self.offset < self.pending.len() {
                let count = (self.pending.len() - self.offset).min(buffer.len());
                buffer[..count].copy_from_slice(&self.pending[self.offset..self.offset + count]);
                self.offset += count;
                if self.offset == self.pending.len() {
                    self.pending.clear();
                    self.offset = 0;
                }
                return Ok(count);
            }

            match self.chunks.recv() {
                Ok(chunk) if !chunk.is_empty() => {
                    self.pending = chunk;
                    self.offset = 0;
                }
                Ok(_) => {}
                Err(_) => return Ok(0),
            }
        }
    }
}

#[cfg(test)]
mod shell_selection_tests {
    use super::*;

    fn candidate_names(candidates: &[ShellCandidate]) -> Vec<&'static str> {
        candidates.iter().map(|candidate| candidate.name).collect()
    }

    #[test]
    fn shell_candidates_follow_platform_priority() {
        let windows = windows_shell_candidates(Some(r"C:\Windows\System32\cmd.exe".into()));
        assert_eq!(
            candidate_names(&windows),
            ["PowerShell 7", "Windows PowerShell", "cmd (COMSPEC)", "cmd"]
        );
        assert_eq!(windows[0].arguments(), ["-NoLogo", "-NoExit"]);
        assert_eq!(windows[2].arguments(), ["/D", "/Q", "/K", "chcp 65001>nul"]);

        let macos = macos_shell_candidates();
        assert_eq!(candidate_names(&macos), ["zsh", "bash", "sh"]);
        assert_eq!(macos[0].program, OsString::from("/bin/zsh"));

        let linux = linux_shell_candidates();
        assert_eq!(candidate_names(&linux), ["bash", "zsh", "ash", "sh"]);
        assert!(
            linux
                .iter()
                .all(|candidate| candidate.arguments() == ["-i"])
        );
    }

    #[test]
    fn windows_candidates_fall_back_to_path_cmd_without_comspec() {
        let candidates = windows_shell_candidates(None);
        assert_eq!(
            candidate_names(&candidates),
            ["PowerShell 7", "Windows PowerShell", "cmd"]
        );
        assert_eq!(candidates[2].program, OsString::from("cmd.exe"));
    }

    #[test]
    fn shell_selection_tries_candidates_until_one_starts() {
        let candidates = linux_shell_candidates();
        let mut attempted = Vec::new();
        let selected = try_shell_candidates(&candidates, "test terminal", |candidate| {
            attempted.push(candidate.name);
            if candidate.name == "ash" {
                Ok(candidate.name.to_string())
            } else {
                Err(anyhow::anyhow!("not installed"))
            }
        })
        .unwrap();

        assert_eq!(selected, "ash");
        assert_eq!(attempted, ["bash", "zsh", "ash"]);
    }

    #[test]
    fn shell_selection_reports_every_failure() {
        let candidates = macos_shell_candidates();
        let error = try_shell_candidates(&candidates, "test terminal", |candidate| {
            Err::<(), _>(anyhow::anyhow!("{} unavailable", candidate.name))
        })
        .unwrap_err()
        .to_string();

        assert!(error.contains("none of the supported shells"));
        for candidate in candidates {
            assert!(
                error.contains(candidate.name),
                "missing {} from {error}",
                candidate.name
            );
        }
    }
}

#[cfg(all(test, unix))]
mod tests {
    use std::{
        io::Cursor,
        time::{Duration, Instant},
    };

    use base64::{Engine as _, engine::general_purpose::STANDARD};

    use super::*;

    #[test]
    fn interactive_terminal_keeps_context_and_utf8_bytes() {
        let (outbound, mut control_events, _failed) = AgentEventSender::channel(32);
        let (stream_outbound, mut inbound, _stream_failed) = AgentEventSender::channel(32);
        let mut manager =
            TerminalManager::new(outbound, stream_outbound, ActivityTracker::default());
        let session_id = "terminal-test".to_string();
        manager.open(session_id.clone(), 80, 24);

        let deadline = Instant::now() + Duration::from_secs(8);
        let mut opened = false;
        let mut output = Vec::new();
        let input = "printf '__OM_UTF8_中文__\\n'; cd /; pwd; exit\n";

        while Instant::now() < deadline {
            match inbound.try_recv() {
                Ok(AgentInbound::TerminalOpened { .. }) if !opened => {
                    opened = true;
                    manager.input(&session_id, &STANDARD.encode(input.as_bytes()));
                }
                Ok(AgentInbound::TerminalOutput { data, .. }) => {
                    output.extend(STANDARD.decode(data).unwrap());
                }
                Ok(AgentInbound::TerminalClosed { .. }) => break,
                Ok(_) => {}
                Err(tokio::sync::mpsc::error::TryRecvError::Empty) => {
                    thread::sleep(Duration::from_millis(20));
                }
                Err(tokio::sync::mpsc::error::TryRecvError::Disconnected) => break,
            }
        }

        manager.close_all();
        let output = String::from_utf8_lossy(&output);
        assert!(opened, "terminal was not opened");
        assert!(output.contains("__OM_UTF8_中文__"), "output was: {output}");
        assert!(
            output.contains("/"),
            "shell context did not change: {output}"
        );
        assert!(control_events.try_recv().is_err());
    }

    #[test]
    fn pipe_reader_preserves_all_merged_output_across_small_reads() {
        let (chunks, received) = mpsc::sync_channel(TERMINAL_PIPE_QUEUE_CAPACITY);
        chunks.send(b"stdout-".to_vec()).unwrap();
        chunks.send(Vec::new()).unwrap();
        chunks.send(b"stderr".to_vec()).unwrap();
        drop(chunks);

        let mut reader = PipeReader {
            chunks: received,
            pending: Vec::new(),
            offset: 0,
        };
        let mut output = Vec::new();
        let mut buffer = [0_u8; 3];
        loop {
            let count = reader.read(&mut buffer).unwrap();
            if count == 0 {
                break;
            }
            output.extend_from_slice(&buffer[..count]);
        }

        assert_eq!(output, b"stdout-stderr");
    }

    #[test]
    fn terminal_output_waits_for_queue_capacity_without_disconnect() {
        let payload = vec![b'x'; 32 * 1024];
        let (outbound, mut inbound, failed) = AgentEventSender::channel(1);
        let worker = thread::spawn(move || {
            forward_terminal_output(
                &mut Cursor::new(payload),
                "backpressure".to_string(),
                outbound,
            );
        });
        thread::sleep(Duration::from_millis(20));

        assert!(!*failed.borrow());
        let first = inbound.blocking_recv().expect("first terminal output");
        let second = inbound.blocking_recv().expect("second terminal output");
        worker.join().unwrap();
        assert!(!*failed.borrow());
        assert!(matches!(first, AgentInbound::TerminalOutput { .. }));
        assert!(matches!(second, AgentInbound::TerminalOutput { .. }));
    }

    #[test]
    fn blocked_terminal_input_does_not_prevent_session_shutdown() {
        let activity = ActivityTracker::default();
        let (outbound, _control_events, _failed) = AgentEventSender::channel(128);
        let (stream_outbound, mut inbound, _stream_failed) = AgentEventSender::channel(128);
        let mut manager = TerminalManager::new(outbound, stream_outbound, activity.clone());
        let session_id = "blocked-input".to_string();
        manager.open(session_id.clone(), 80, 24);

        let opened_deadline = Instant::now() + Duration::from_secs(2);
        loop {
            match inbound.try_recv() {
                Ok(AgentInbound::TerminalOpened { .. }) => break,
                Ok(_) | Err(tokio::sync::mpsc::error::TryRecvError::Empty)
                    if Instant::now() < opened_deadline =>
                {
                    thread::sleep(Duration::from_millis(10));
                }
                event => panic!("terminal did not open: {event:?}"),
            }
        }

        manager.input(&session_id, &STANDARD.encode(b"sleep 60\n"));
        thread::sleep(Duration::from_millis(100));
        let input = STANDARD.encode(vec![b'x'; 32 * 1024]);
        for _ in 0..64 {
            manager.input(&session_id, &input);
        }
        manager.close_all();

        let close_deadline = Instant::now() + Duration::from_secs(5);
        let mut closed = false;
        while Instant::now() < close_deadline {
            match inbound.try_recv() {
                Ok(AgentInbound::TerminalClosed { .. }) => {
                    closed = true;
                    break;
                }
                Ok(_) | Err(tokio::sync::mpsc::error::TryRecvError::Empty) => {
                    thread::sleep(Duration::from_millis(10));
                }
                Err(tokio::sync::mpsc::error::TryRecvError::Disconnected) => break,
            }
        }

        assert!(closed, "blocked terminal session did not close");
        let activity_deadline = Instant::now() + Duration::from_secs(1);
        while activity.active_count() != 0 && Instant::now() < activity_deadline {
            thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(activity.active_count(), 0);
    }

    #[test]
    fn terminal_manager_rejects_sessions_above_the_limit() {
        let (outbound, mut inbound, _failed) = AgentEventSender::channel(32);
        let mut manager =
            TerminalManager::new(outbound.clone(), outbound, ActivityTracker::default());
        let mut slots = Vec::new();

        for _ in 0..MAX_TERMINAL_SESSIONS {
            slots.push(
                manager
                    .session_slots
                    .clone()
                    .try_acquire_owned()
                    .expect("test should acquire every configured terminal slot"),
            );
        }

        manager.open("overflow".to_string(), 80, 24);

        assert!(manager.sessions.is_empty());
        assert!(matches!(
            inbound.try_recv(),
            Ok(AgentInbound::TerminalClosed {
                session_id,
                exit_code: None,
                reason: Some(reason),
            }) if session_id == "overflow" && reason.contains("上限")
        ));
        drop(slots);
    }

    #[test]
    fn terminal_manager_closes_a_session_when_its_control_queue_is_full() {
        let (outbound, mut inbound, _failed) = AgentEventSender::channel(32);
        let mut manager =
            TerminalManager::new(outbound.clone(), outbound, ActivityTracker::default());
        let (control, _receiver) = mpsc::sync_channel(1);
        control.try_send(TerminalControl::Close).unwrap();
        manager.sessions.insert("congested".to_string(), control);

        manager.resize("congested", 100, 40);

        assert!(!manager.sessions.contains_key("congested"));
        assert!(matches!(
            inbound.try_recv(),
            Ok(AgentInbound::TerminalClosed {
                session_id,
                exit_code: None,
                reason: Some(reason),
            }) if session_id == "congested" && reason.contains("拥塞")
        ));
    }
}
