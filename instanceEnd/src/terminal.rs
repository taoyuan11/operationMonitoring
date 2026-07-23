use std::{
    collections::HashMap,
    io::{self, Read, Write},
    sync::mpsc::{self, RecvTimeoutError},
    thread,
    time::Duration,
};

use base64::{Engine as _, engine::general_purpose::STANDARD};
use portable_pty::{CommandBuilder, PtySize, native_pty_system};
use tokio::sync::mpsc as tokio_mpsc;

#[cfg(windows)]
use anyhow::Context as _;

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

use crate::{activity::ActivityTracker, models::AgentInbound};

enum TerminalControl {
    Input(Vec<u8>),
    Resize { cols: u16, rows: u16 },
    Close,
}

pub struct TerminalManager {
    sessions: HashMap<String, mpsc::Sender<TerminalControl>>,
    outbound: tokio_mpsc::UnboundedSender<AgentInbound>,
    activity: ActivityTracker,
}

impl TerminalManager {
    pub fn new(
        outbound: tokio_mpsc::UnboundedSender<AgentInbound>,
        activity: ActivityTracker,
    ) -> Self {
        Self {
            sessions: HashMap::new(),
            outbound,
            activity,
        }
    }

    pub fn open(&mut self, session_id: String, cols: u16, rows: u16) {
        self.close(&session_id);
        let Some(activity_guard) = self.activity.try_enter() else {
            let _ = self.outbound.send(AgentInbound::TerminalClosed {
                session_id,
                exit_code: None,
                reason: Some("agent update is waiting to install".to_string()),
            });
            return;
        };
        let (control_tx, control_rx) = mpsc::channel();
        self.sessions.insert(session_id.clone(), control_tx);
        let outbound = self.outbound.clone();
        thread::spawn(move || {
            let _activity_guard = activity_guard;
            run_terminal(session_id, cols, rows, control_rx, outbound);
        });
    }

    pub fn input(&self, session_id: &str, encoded_data: &str) {
        let Some(session) = self.sessions.get(session_id) else {
            return;
        };
        match STANDARD.decode(encoded_data) {
            Ok(data) => {
                let _ = session.send(TerminalControl::Input(data));
            }
            Err(error) => {
                let _ = self.outbound.send(AgentInbound::TerminalClosed {
                    session_id: session_id.to_string(),
                    exit_code: None,
                    reason: Some(format!("终端输入编码无效: {error}")),
                });
            }
        }
    }

    pub fn resize(&self, session_id: &str, cols: u16, rows: u16) {
        if let Some(session) = self.sessions.get(session_id) {
            let _ = session.send(TerminalControl::Resize {
                cols: cols.clamp(2, 500),
                rows: rows.clamp(1, 300),
            });
        }
    }

    pub fn close(&mut self, session_id: &str) {
        if let Some(session) = self.sessions.remove(session_id) {
            let _ = session.send(TerminalControl::Close);
        }
    }

    pub fn close_all(&mut self) {
        for (_, session) in self.sessions.drain() {
            let _ = session.send(TerminalControl::Close);
        }
    }
}

fn run_terminal(
    session_id: String,
    cols: u16,
    rows: u16,
    control_rx: mpsc::Receiver<TerminalControl>,
    outbound: tokio_mpsc::UnboundedSender<AgentInbound>,
) {
    if let Err(error) = run_terminal_inner(
        &session_id,
        cols.clamp(2, 500),
        rows.clamp(1, 300),
        control_rx,
        outbound.clone(),
    ) {
        crate::logging::error(format_args!(
            "terminal session {session_id} failed: {error:#}"
        ));
        let _ = outbound.send(AgentInbound::TerminalClosed {
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
    outbound: tokio_mpsc::UnboundedSender<AgentInbound>,
) -> anyhow::Result<()> {
    let mut terminal = open_terminal(cols, rows)?;
    let mut reader = terminal.reader;
    let mut writer = terminal.writer;

    let reader_session_id = session_id.to_string();
    let reader_outbound = outbound.clone();
    thread::spawn(move || {
        let mut buffer = [0_u8; 16 * 1024];
        loop {
            match reader.read(&mut buffer) {
                Ok(0) => break,
                Ok(count) => {
                    let data = STANDARD.encode(&buffer[..count]);
                    if reader_outbound
                        .send(AgentInbound::TerminalOutput {
                            session_id: reader_session_id.clone(),
                            data,
                        })
                        .is_err()
                    {
                        break;
                    }
                }
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(_) => break,
            }
        }
    });

    outbound.send(AgentInbound::TerminalOpened {
        session_id: session_id.to_string(),
    })?;

    let (exit_code, reason) = loop {
        match control_rx.recv_timeout(Duration::from_millis(100)) {
            Ok(TerminalControl::Input(data)) => {
                if let Err(error) = writer.write_all(&data).and_then(|_| writer.flush()) {
                    break (None, Some(format!("写入终端失败: {error}")));
                }
            }
            Ok(TerminalControl::Resize { cols, rows }) => {
                if let Err(error) = terminal.master.resize(cols, rows) {
                    break (None, Some(format!("调整终端大小失败: {error}")));
                }
            }
            Ok(TerminalControl::Close) => {
                break (terminal.process.kill_and_wait(), None);
            }
            Err(RecvTimeoutError::Timeout) => {
                if let Some(exit_code) = terminal.process.try_wait_code()? {
                    break (Some(exit_code), None);
                }
            }
            Err(RecvTimeoutError::Disconnected) => {
                let _ = terminal.process.kill_and_wait();
                break (None, Some("终端控制通道已关闭".to_string()));
            }
        }
    };

    let _ = outbound.send(AgentInbound::TerminalClosed {
        session_id: session_id.to_string(),
        exit_code,
        reason,
    });
    Ok(())
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

    fn kill_and_wait(&mut self) -> Option<i64> {
        match self {
            Self::Pty(child) => {
                let _ = child.kill();
                child.wait().ok().map(|status| status.exit_code() as i64)
            }
            #[cfg(windows)]
            Self::Pipe(child) => {
                let _ = child.kill();
                child
                    .wait()
                    .ok()
                    .map(|status| status.code().unwrap_or(-1) as i64)
            }
        }
    }
}

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
                    "ConPTY terminal initialization failed; falling back to a pipe-backed cmd terminal: {error:#}"
                )),
            }
        } else {
            crate::logging::info(format_args!(
                "ConPTY is unavailable on this Windows version; using a pipe-backed cmd terminal"
            ));
        }

        return open_pipe_terminal().context("failed to start the legacy Windows terminal");
    }

    #[cfg(not(windows))]
    open_pty_terminal(cols, rows)
}

fn open_pty_terminal(cols: u16, rows: u16) -> anyhow::Result<RunningTerminal> {
    let pair = native_pty_system().openpty(PtySize {
        rows,
        cols,
        pixel_width: 0,
        pixel_height: 0,
    })?;
    let mut command = interactive_shell();
    command.env("TERM", "xterm-256color");
    command.env("COLORTERM", "truecolor");

    let slave = pair.slave;
    let master = pair.master;
    let child = slave.spawn_command(command)?;
    drop(slave);
    let reader = master.try_clone_reader()?;
    let writer = master.take_writer()?;

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
    let program = std::env::var_os("COMSPEC").unwrap_or_else(|| "cmd.exe".into());
    let mut command = Command::new(program);
    command
        .args(["/D", "/Q", "/K", "chcp 65001>nul"])
        .env("TERM", "xterm-256color")
        .env("COLORTERM", "truecolor")
        .creation_flags(CREATE_NO_WINDOW.0)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let mut child = command
        .spawn()
        .context("failed to spawn cmd.exe with redirected standard handles")?;
    let stdin = child
        .stdin
        .take()
        .context("cmd.exe stdin pipe was not created")?;
    let stdout = child
        .stdout
        .take()
        .context("cmd.exe stdout pipe was not created")?;
    let stderr = child
        .stderr
        .take()
        .context("cmd.exe stderr pipe was not created")?;

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
    let (chunks_tx, chunks_rx) = mpsc::channel();
    forward_pipe(stdout, chunks_tx.clone());
    forward_pipe(stderr, chunks_tx);
    Box::new(PipeReader {
        chunks: chunks_rx,
        pending: Vec::new(),
        offset: 0,
    })
}

#[cfg(windows)]
fn forward_pipe<R>(mut reader: R, chunks_tx: mpsc::Sender<Vec<u8>>)
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

#[cfg(windows)]
fn interactive_shell() -> CommandBuilder {
    let program = std::env::var_os("COMSPEC").unwrap_or_else(|| "cmd.exe".into());
    let mut command = CommandBuilder::new(program);
    command.args(["/D", "/Q", "/K", "chcp 65001>nul"]);
    command
}

#[cfg(not(windows))]
fn interactive_shell() -> CommandBuilder {
    let program = std::env::var_os("SHELL").unwrap_or_else(|| "/bin/sh".into());
    CommandBuilder::new(program)
}

#[cfg(all(test, unix))]
mod tests {
    use std::time::{Duration, Instant};

    use base64::{Engine as _, engine::general_purpose::STANDARD};

    use super::*;

    #[test]
    fn interactive_terminal_keeps_context_and_utf8_bytes() {
        let (outbound, mut inbound) = tokio_mpsc::unbounded_channel();
        let mut manager = TerminalManager::new(outbound, ActivityTracker::default());
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
                Err(tokio_mpsc::error::TryRecvError::Empty) => {
                    thread::sleep(Duration::from_millis(20));
                }
                Err(tokio_mpsc::error::TryRecvError::Disconnected) => break,
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
    }

    #[test]
    fn pipe_reader_preserves_all_merged_output_across_small_reads() {
        let (chunks, received) = mpsc::channel();
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
}
