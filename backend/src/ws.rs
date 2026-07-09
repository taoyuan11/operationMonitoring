use std::time::Duration;

use axum::{
    extract::ws::{Message, WebSocket},
    http::StatusCode,
};
use futures_util::{SinkExt, StreamExt};
use tokio::sync::{mpsc, oneshot};
use tracing::{error, info, warn};
use uuid::Uuid;

use crate::{
    error::{AppError, AppResult},
    jobs::{complete_command_job, create_command_job, dispatch_command},
    models::{AgentInbound, AgentOutbound, CommandOutcome},
    state::{AgentHandle, AppState},
    utils::now_ts,
};

pub async fn agent_socket(state: AppState, instance_id: String, socket: WebSocket) {
    let (mut sender, mut receiver) = socket.split();
    let (tx, mut rx) = mpsc::unbounded_channel::<AgentOutbound>();
    state
        .agents
        .write()
        .await
        .insert(instance_id.clone(), AgentHandle { tx });

    let outbound_instance_id = instance_id.clone();
    let outbound = tokio::spawn(async move {
        while let Some(message) = rx.recv().await {
            match serde_json::to_string(&message) {
                Ok(text) => {
                    if sender.send(Message::Text(text.into())).await.is_err() {
                        break;
                    }
                }
                Err(error) => error!(?error, "failed to serialize agent outbound message"),
            }
        }
        info!(instance_id = %outbound_instance_id, "agent outbound loop ended");
    });

    info!(instance_id = %instance_id, "agent websocket connected");
    while let Some(Ok(message)) = receiver.next().await {
        match message {
            Message::Text(text) => match serde_json::from_str::<AgentInbound>(&text) {
                Ok(AgentInbound::Pong { .. }) => {}
                Ok(AgentInbound::CommandResult {
                    job_id,
                    exit_code,
                    output,
                }) => {
                    if let Err(error) =
                        complete_command_job(&state, &job_id, exit_code, &output).await
                    {
                        error!(?error, "failed to complete command job");
                    }
                    let outcome = CommandOutcome {
                        job_id: job_id.clone(),
                        exit_code,
                        output,
                    };
                    if let Some(waiter) = state.command_waiters.lock().await.remove(&job_id) {
                        let _ = waiter.send(outcome);
                    }
                }
                Err(error) => warn!(?error, %text, "invalid agent websocket message"),
            },
            Message::Close(_) => break,
            _ => {}
        }
    }

    state.agents.write().await.remove(&instance_id);
    outbound.abort();
    info!(instance_id = %instance_id, "agent websocket disconnected");
}

pub async fn terminal_socket(state: AppState, instance_id: String, socket: WebSocket) {
    let session_id = Uuid::new_v4().to_string();
    let started_at = now_ts();
    if let Err(error) = sqlx::query(
        "INSERT INTO ssh_sessions(id, instance_id, actor, started_at) VALUES(?, ?, 'admin', ?)",
    )
    .bind(&session_id)
    .bind(&instance_id)
    .bind(started_at)
    .execute(&state.db)
    .await
    {
        error!(?error, "failed to create terminal session");
    }

    let (mut sender, mut receiver) = socket.split();
    let _ = sender
        .send(Message::Text(
            "Web 终端已连接。输入命令后按回车执行。\n"
                .to_string()
                .into(),
        ))
        .await;

    while let Some(Ok(message)) = receiver.next().await {
        let Message::Text(command_text) = message else {
            continue;
        };
        let command = command_text.trim();
        if command.is_empty() {
            continue;
        }

        match run_terminal_command(&state, &instance_id, command).await {
            Ok(outcome) => {
                let payload = format!(
                    "$ {}\n{}\n[job: {} | exit code: {}]\n",
                    command, outcome.output, outcome.job_id, outcome.exit_code
                );
                if sender.send(Message::Text(payload.into())).await.is_err() {
                    break;
                }
            }
            Err(error) => {
                let payload = format!("执行失败：{}\n", error.message);
                if sender.send(Message::Text(payload.into())).await.is_err() {
                    break;
                }
            }
        }
    }

    let _ = sqlx::query("UPDATE ssh_sessions SET ended_at = ? WHERE id = ?")
        .bind(now_ts())
        .bind(&session_id)
        .execute(&state.db)
        .await;
}

async fn run_terminal_command(
    state: &AppState,
    instance_id: &str,
    command: &str,
) -> AppResult<CommandOutcome> {
    let job = create_command_job(state, None, instance_id, command, "admin-terminal").await?;
    let (tx, rx) = oneshot::channel();
    state
        .command_waiters
        .lock()
        .await
        .insert(job.id.clone(), tx);

    if let Err(error) = dispatch_command(state, &job.id, instance_id, command).await {
        state.command_waiters.lock().await.remove(&job.id);
        return Err(error);
    }

    match tokio::time::timeout(Duration::from_secs(120), rx).await {
        Ok(Ok(outcome)) => Ok(outcome),
        Ok(Err(_)) => Err(AppError::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            "命令结果通道已关闭",
        )),
        Err(_) => {
            state.command_waiters.lock().await.remove(&job.id);
            complete_command_job(state, &job.id, -1, "命令执行超时").await?;
            Err(AppError::new(StatusCode::REQUEST_TIMEOUT, "命令执行超时"))
        }
    }
}
