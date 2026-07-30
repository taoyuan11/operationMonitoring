use std::{io, process::Stdio, time::Duration};

use tokio::{
    io::{AsyncRead, AsyncReadExt},
    process::Command,
    time::timeout,
};

use crate::activity::ActivityTracker;

const MAX_OUTPUT_BYTES: usize = 64 * 1024;
const OUTPUT_TRUNCATION_MARKER: &str = "\n[output truncated]";

pub async fn execute_command(command: &str) -> (i64, String) {
    execute_command_with_timeout(command, Duration::from_secs(120)).await
}

async fn execute_command_with_timeout(command: &str, command_timeout: Duration) -> (i64, String) {
    let mut process = shell_command(command);
    process.stdout(Stdio::piped()).stderr(Stdio::piped());
    configure_process_group(&mut process);
    let mut child = match process.spawn() {
        Ok(child) => child,
        Err(error) => return (-1, format!("failed to execute command: {error}")),
    };
    let process_id = child.id();
    let Some(stdout) = child.stdout.take() else {
        return (-1, "failed to capture command stdout".to_string());
    };
    let Some(stderr) = child.stderr.take() else {
        return (-1, "failed to capture command stderr".to_string());
    };
    let completed = async {
        let (status, stdout, stderr) = tokio::try_join!(
            child.wait(),
            capture_output(stdout, MAX_OUTPUT_BYTES),
            capture_output(stderr, MAX_OUTPUT_BYTES),
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
            (-1, format!("failed to execute command: {error}"))
        }
        Err(_) => {
            terminate_process_tree(&mut child, process_id).await;
            (
                -1,
                format!(
                    "command timed out after {} seconds",
                    command_timeout.as_secs_f64()
                ),
            )
        }
    }
}

pub async fn execute_tracked_command(command: &str, activity: &ActivityTracker) -> (i64, String) {
    let Some(_guard) = activity.try_enter() else {
        return (
            -1,
            "command rejected because an agent update is waiting to install".to_string(),
        );
    };
    execute_command(command).await
}

struct CapturedOutput {
    bytes: Vec<u8>,
    truncated: bool,
}

async fn capture_output(
    mut reader: impl AsyncRead + Unpin,
    max_bytes: usize,
) -> io::Result<CapturedOutput> {
    let mut bytes = Vec::with_capacity(max_bytes.min(8 * 1024));
    let mut buffer = [0_u8; 8 * 1024];
    let mut truncated = false;
    loop {
        let count = reader.read(&mut buffer).await?;
        if count == 0 {
            break;
        }
        let retained = count.min(max_bytes.saturating_sub(bytes.len()));
        bytes.extend_from_slice(&buffer[..retained]);
        truncated |= retained < count;
    }
    Ok(CapturedOutput { bytes, truncated })
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
        let captured = capture_output(std::io::Cursor::new(input), MAX_OUTPUT_BYTES)
            .await
            .unwrap();

        assert_eq!(captured.bytes.len(), MAX_OUTPUT_BYTES);
        assert!(captured.truncated);
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
