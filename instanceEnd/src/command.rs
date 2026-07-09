use std::time::Duration;

use tokio::{process::Command, time::timeout};

pub async fn execute_command(command: &str) -> (i64, String) {
    let mut process = shell_command(command);
    match timeout(Duration::from_secs(120), process.output()).await {
        Ok(Ok(output)) => {
            let mut combined = String::new();
            combined.push_str(&String::from_utf8_lossy(&output.stdout));
            combined.push_str(&String::from_utf8_lossy(&output.stderr));
            if combined.len() > 64 * 1024 {
                combined.truncate(64 * 1024);
                combined.push_str("\n[output truncated]");
            }
            (output.status.code().unwrap_or(-1) as i64, combined)
        }
        Ok(Err(error)) => (-1, format!("failed to execute command: {error}")),
        Err(_) => (-1, "command timed out after 120 seconds".to_string()),
    }
}

#[cfg(target_os = "windows")]
fn shell_command(command: &str) -> Command {
    let mut process = Command::new("cmd");
    process.args(["/C", command]);
    process
}

#[cfg(not(target_os = "windows"))]
fn shell_command(command: &str) -> Command {
    let mut process = Command::new("sh");
    process.arg("-c").arg(command);
    process
}
