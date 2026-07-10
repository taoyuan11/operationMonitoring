use std::time::Duration;

use tokio::{process::Command, time::timeout};

const MAX_OUTPUT_BYTES: usize = 64 * 1024;

pub async fn execute_command(command: &str) -> (i64, String) {
    let mut process = shell_command(command);
    match timeout(Duration::from_secs(120), process.output()).await {
        Ok(Ok(output)) => {
            let mut bytes = output.stdout;
            bytes.extend_from_slice(&output.stderr);
            let mut combined = String::from_utf8_lossy(&bytes).into_owned();
            truncate_utf8(&mut combined, MAX_OUTPUT_BYTES);
            (output.status.code().unwrap_or(-1) as i64, combined)
        }
        Ok(Err(error)) => (-1, format!("failed to execute command: {error}")),
        Err(_) => (-1, "command timed out after 120 seconds".to_string()),
    }
}

fn truncate_utf8(output: &mut String, max_bytes: usize) {
    if output.len() <= max_bytes {
        return;
    }
    let boundary = output
        .char_indices()
        .map(|(index, _)| index)
        .take_while(|index| *index <= max_bytes)
        .last()
        .unwrap_or(0);
    output.truncate(boundary);
    output.push_str("\n[output truncated]");
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
    }
}
