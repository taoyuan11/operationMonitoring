use std::{io, process::Stdio, time::Duration};

use tokio::{
    io::{AsyncRead, AsyncReadExt},
    process::Command,
    sync::mpsc,
    time::timeout,
};

use crate::activity::ActivityTracker;

const MAX_OUTPUT_BYTES: usize = 64 * 1024;
const OUTPUT_TRUNCATION_MARKER: &str = "\n[output truncated]";

#[cfg(test)]
async fn execute_command_with_timeout(command: &str, command_timeout: Duration) -> (i64, String) {
    execute_command_with_timeout_streaming(command, command_timeout, |_| {}).await
}

async fn execute_command_with_timeout_streaming<F>(
    command: &str,
    command_timeout: Duration,
    mut on_output: F,
) -> (i64, String)
where
    F: FnMut(String) + Send,
{
    let mut process = shell_command(command);
    process.stdout(Stdio::piped()).stderr(Stdio::piped());
    configure_process_group(&mut process);
    let mut child = match process.spawn() {
        Ok(child) => child,
        Err(error) => {
            let output = format!("failed to execute command: {error}");
            on_output(output.clone());
            return (-1, output);
        }
    };
    let process_id = child.id();
    let Some(stdout) = child.stdout.take() else {
        let output = "failed to capture command stdout".to_string();
        on_output(output.clone());
        return (-1, output);
    };
    let Some(stderr) = child.stderr.take() else {
        let output = "failed to capture command stderr".to_string();
        on_output(output.clone());
        return (-1, output);
    };
    let (output_tx, output_rx) = mpsc::channel(16);
    let stdout_tx = output_tx.clone();
    let stderr_tx = output_tx;
    let completed = async {
        let (status, stdout, stderr, ()) = tokio::try_join!(
            child.wait(),
            capture_output(
                stdout,
                MAX_OUTPUT_BYTES,
                Some((CommandOutputStream::Stdout, stdout_tx)),
            ),
            capture_output(
                stderr,
                MAX_OUTPUT_BYTES,
                Some((CommandOutputStream::Stderr, stderr_tx)),
            ),
            forward_output(output_rx, &mut on_output),
        )?;
        Ok::<_, io::Error>((status, stdout, stderr))
    };
    match timeout(command_timeout, completed).await {
        Ok(Ok((status, stdout, stderr))) => {
            let truncated_while_reading = stdout.truncated || stderr.truncated;
            let mut bytes = stdout.bytes;
            bytes.extend_from_slice(&stderr.bytes);
            let mut combined = String::from_utf8_lossy(&bytes).into_owned();
            let truncated_after_decoding = truncate_utf8(&mut combined, MAX_OUTPUT_BYTES);
            if truncated_while_reading && !truncated_after_decoding {
                append_truncation_marker(&mut combined, MAX_OUTPUT_BYTES);
            }
            (status.code().unwrap_or(-1) as i64, combined)
        }
        Ok(Err(error)) => {
            terminate_process_tree(&mut child, process_id).await;
            let output = format!("failed to execute command: {error}");
            on_output(output.clone());
            (-1, output)
        }
        Err(_) => {
            terminate_process_tree(&mut child, process_id).await;
            let output = format!(
                "command timed out after {} seconds",
                command_timeout.as_secs_f64()
            );
            on_output(output.clone());
            (-1, output)
        }
    }
}

pub async fn execute_tracked_command<F>(
    command: &str,
    activity: &ActivityTracker,
    mut on_output: F,
) -> (i64, String)
where
    F: FnMut(String) + Send,
{
    let Some(_guard) = activity.try_enter() else {
        let output = "command rejected because an agent update is waiting to install".to_string();
        on_output(output.clone());
        return (-1, output);
    };
    execute_command_with_timeout_streaming(command, Duration::from_secs(120), on_output).await
}

#[derive(Clone, Copy)]
enum CommandOutputStream {
    Stdout,
    Stderr,
}

struct CapturedOutput {
    bytes: Vec<u8>,
    truncated: bool,
}

async fn capture_output(
    mut reader: impl AsyncRead + Unpin,
    max_bytes: usize,
    stream_output: Option<(
        CommandOutputStream,
        mpsc::Sender<(CommandOutputStream, Vec<u8>)>,
    )>,
) -> io::Result<CapturedOutput> {
    let mut bytes = Vec::with_capacity(max_bytes.min(8 * 1024));
    let mut buffer = [0_u8; 8 * 1024];
    let mut truncated = false;
    loop {
        let count = reader.read(&mut buffer).await?;
        if count == 0 {
            break;
        }
        if let Some((stream, sender)) = &stream_output {
            let _ = sender.send((*stream, buffer[..count].to_vec())).await;
        }
        let retained = count.min(max_bytes.saturating_sub(bytes.len()));
        bytes.extend_from_slice(&buffer[..retained]);
        truncated |= retained < count;
    }
    Ok(CapturedOutput { bytes, truncated })
}

async fn forward_output<F>(
    mut output_rx: mpsc::Receiver<(CommandOutputStream, Vec<u8>)>,
    on_output: &mut F,
) -> io::Result<()>
where
    F: FnMut(String) + Send,
{
    let mut stdout_pending = Vec::new();
    let mut stderr_pending = Vec::new();
    let mut streamed_bytes = 0;
    while let Some((stream, bytes)) = output_rx.recv().await {
        let retained = bytes
            .len()
            .min(MAX_OUTPUT_BYTES.saturating_sub(streamed_bytes));
        streamed_bytes += retained;
        if retained == 0 {
            continue;
        }
        let pending = match stream {
            CommandOutputStream::Stdout => &mut stdout_pending,
            CommandOutputStream::Stderr => &mut stderr_pending,
        };
        let decoded = decode_utf8_chunk(pending, &bytes[..retained], false);
        if !decoded.is_empty() {
            on_output(decoded);
        }
    }
    for pending in [&mut stdout_pending, &mut stderr_pending] {
        let decoded = decode_utf8_chunk(pending, &[], true);
        if !decoded.is_empty() {
            on_output(decoded);
        }
    }
    Ok(())
}

fn decode_utf8_chunk(pending: &mut Vec<u8>, bytes: &[u8], flush: bool) -> String {
    pending.extend_from_slice(bytes);
    let mut decoded = String::new();
    loop {
        match std::str::from_utf8(pending) {
            Ok(valid) => {
                decoded.push_str(valid);
                pending.clear();
                break;
            }
            Err(error) => {
                let valid_up_to = error.valid_up_to();
                let error_len = error.error_len();
                if valid_up_to > 0 {
                    decoded.push_str(
                        std::str::from_utf8(&pending[..valid_up_to])
                            .expect("prefix reported by Utf8Error is valid"),
                    );
                    pending.drain(..valid_up_to);
                    continue;
                }
                if let Some(error_len) = error_len {
                    decoded.push('\u{fffd}');
                    pending.drain(..error_len);
                    continue;
                }
                if flush {
                    decoded.push_str(&String::from_utf8_lossy(pending));
                    pending.clear();
                }
                break;
            }
        }
    }
    decoded
}

#[cfg(unix)]
fn configure_process_group(command: &mut Command) {
    use std::os::unix::process::CommandExt;

    command.as_std_mut().process_group(0);
}

#[cfg(windows)]
fn configure_process_group(command: &mut Command) {
    use std::os::windows::process::CommandExt;
    use windows_sys::Win32::System::Threading::CREATE_NEW_PROCESS_GROUP;

    command
        .as_std_mut()
        .creation_flags(CREATE_NEW_PROCESS_GROUP);
}

#[cfg(not(any(unix, windows)))]
fn configure_process_group(_command: &mut Command) {}

async fn terminate_process_tree(child: &mut tokio::process::Child, process_id: Option<u32>) {
    #[cfg(unix)]
    if let Some(pid) = process_id.and_then(|pid| i32::try_from(pid).ok()) {
        unsafe {
            let _ = libc::kill(-pid, libc::SIGKILL);
        }
    }
    #[cfg(windows)]
    if let Some(pid) = process_id {
        let _ = timeout(
            Duration::from_secs(10),
            Command::new("taskkill.exe")
                .args(["/PID", &pid.to_string(), "/T", "/F"])
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status(),
        )
        .await;
    }
    let _ = child.kill().await;
    let _ = child.wait().await;
}

fn truncate_utf8(output: &mut String, max_bytes: usize) -> bool {
    if output.len() <= max_bytes {
        return false;
    }
    append_truncation_marker(output, max_bytes);
    true
}

fn append_truncation_marker(output: &mut String, max_bytes: usize) {
    let content_limit = max_bytes.saturating_sub(OUTPUT_TRUNCATION_MARKER.len());
    if output.len() > content_limit {
        let boundary = output
            .char_indices()
            .map(|(index, _)| index)
            .take_while(|index| *index <= content_limit)
            .last()
            .unwrap_or(0);
        output.truncate(boundary);
    }
    let marker_bytes = max_bytes.saturating_sub(output.len());
    output.push_str(&OUTPUT_TRUNCATION_MARKER[..marker_bytes.min(OUTPUT_TRUNCATION_MARKER.len())]);
}

#[cfg(target_os = "windows")]
fn shell_command(command: &str) -> Command {
    let mut process = Command::new("cmd");
    process
        .args(["/D", "/Q", "/C", &format!("chcp 65001>nul & {command}")])
        .kill_on_drop(true);
    process
}

#[cfg(not(target_os = "windows"))]
fn shell_command(command: &str) -> Command {
    let mut process = Command::new("sh");
    process
        .arg("-c")
        .arg(command)
        .env("TERM", "xterm-256color")
        .kill_on_drop(true);
    process
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn utf8_truncation_keeps_character_boundaries() {
        let mut output = "中文输出".repeat(30_000);
        truncate_utf8(&mut output, MAX_OUTPUT_BYTES);
        assert!(output.is_char_boundary(output.len()));
        assert!(output.ends_with("[output truncated]"));
        assert!(output.len() <= MAX_OUTPUT_BYTES);
    }

    #[tokio::test]
    async fn output_capture_discards_bytes_beyond_the_retention_limit() {
        let input = vec![b'x'; MAX_OUTPUT_BYTES * 4];
        let captured = capture_output(std::io::Cursor::new(input), MAX_OUTPUT_BYTES, None)
            .await
            .unwrap();

        assert_eq!(captured.bytes.len(), MAX_OUTPUT_BYTES);
        assert!(captured.truncated);
    }

    #[test]
    fn streaming_decoder_preserves_split_utf8_characters() {
        let value = "命".as_bytes();
        let mut pending = Vec::new();

        assert_eq!(decode_utf8_chunk(&mut pending, &value[..2], false), "");
        assert_eq!(decode_utf8_chunk(&mut pending, &value[2..], false), "命");
        assert!(pending.is_empty());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn command_output_is_streamed_before_process_completion() {
        let (output_tx, mut output_rx) = mpsc::unbounded_channel();
        let execution = tokio::spawn(async move {
            execute_command_with_timeout_streaming(
                "printf first; sleep 0.5; printf second",
                Duration::from_secs(2),
                move |output| {
                    let _ = output_tx.send(output);
                },
            )
            .await
        });

        let first = timeout(Duration::from_millis(300), output_rx.recv())
            .await
            .expect("first output should arrive while the command is running")
            .expect("output channel should remain open");
        assert_eq!(first, "first");
        assert!(!execution.is_finished());

        let (exit_code, output) = execution.await.unwrap();
        assert_eq!(exit_code, 0);
        assert_eq!(output, "firstsecond");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn timeout_terminates_the_entire_command_process_group() {
        let directory = std::env::temp_dir().join(format!("om-command-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&directory).unwrap();
        let marker = directory.join("orphan-marker");
        let script = format!("(sleep 0.2; printf leaked > '{}') &", marker.display());

        let (exit_code, output) =
            execute_command_with_timeout(&script, Duration::from_millis(50)).await;

        assert_eq!(exit_code, -1);
        assert!(output.contains("timed out"));
        tokio::time::sleep(Duration::from_millis(350)).await;
        assert!(!marker.exists(), "a command grandchild survived timeout");
        std::fs::remove_dir_all(directory).unwrap();
    }
}
