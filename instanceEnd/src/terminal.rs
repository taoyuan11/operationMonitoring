use std::{
    collections::HashMap,
    io::{Read, Write},
    sync::mpsc::{self, RecvTimeoutError},
    thread,
    time::Duration,
};

use base64::{Engine as _, engine::general_purpose::STANDARD};
use portable_pty::{CommandBuilder, PtySize, native_pty_system};
use tokio::sync::mpsc as tokio_mpsc;

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
    let pair = native_pty_system().openpty(PtySize {
        rows,
        cols,
        pixel_width: 0,
        pixel_height: 0,
    })?;
    let mut command = interactive_shell();
    command.env("TERM", "xterm-256color");
    command.env("COLORTERM", "truecolor");

    let mut child = pair.slave.spawn_command(command)?;
    drop(pair.slave);
    let mut reader = pair.master.try_clone_reader()?;
    let mut writer = pair.master.take_writer()?;

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
                if let Err(error) = pair.master.resize(PtySize {
                    rows,
                    cols,
                    pixel_width: 0,
                    pixel_height: 0,
                }) {
                    break (None, Some(format!("调整终端大小失败: {error}")));
                }
            }
            Ok(TerminalControl::Close) => {
                let _ = child.kill();
                let status = child.wait().ok();
                break (status.map(|value| value.exit_code() as i64), None);
            }
            Err(RecvTimeoutError::Timeout) => {
                if let Some(status) = child.try_wait()? {
                    break (Some(status.exit_code() as i64), None);
                }
            }
            Err(RecvTimeoutError::Disconnected) => {
                let _ = child.kill();
                let _ = child.wait();
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
}
