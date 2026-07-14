use std::time::{Duration, Instant};

use axum::extract::ws::{Message, WebSocket};
use futures_util::{SinkExt, StreamExt};
use tokio::sync::mpsc;
use tracing::{error, info, warn};
use uuid::Uuid;

use crate::{
    error::AppResult,
    jobs::complete_command_job,
    models::{
        AgentInbound, AgentOutbound, MetricPayload, TerminalClientMessage, TerminalServerMessage,
    },
    state::{AgentHandle, AppState, TerminalSessionHandle},
    updates::{confirm_update_version, offer_update_on_connect, record_update_status},
    utils::now_ts,
};

const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(15);
const HEARTBEAT_TIMEOUT: Duration = Duration::from_secs(45);

pub async fn agent_socket(state: AppState, instance_id: String, socket: WebSocket) {
    let connection_id = Uuid::new_v4();
    let (mut sender, mut receiver) = socket.split();
    let (tx, mut rx) = mpsc::unbounded_channel::<AgentOutbound>();

    state
        .agents
        .write()
        .await
        .insert(instance_id.clone(), AgentHandle { connection_id, tx });
    let _ = sqlx::query("UPDATE instances SET last_seen = $1 WHERE id = $2")
        .bind(now_ts())
        .bind(&instance_id)
        .execute(&state.db)
        .await;
    offer_update_on_connect(&state, &instance_id).await;

    info!(%instance_id, %connection_id, "agent websocket connected");
    let mut heartbeat = tokio::time::interval(HEARTBEAT_INTERVAL);
    heartbeat.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut last_inbound = Instant::now();

    loop {
        tokio::select! {
            outbound = rx.recv() => {
                let Some(outbound) = outbound else {
                    break;
                };
                match serde_json::to_string(&outbound) {
                    Ok(text) => {
                        if sender.send(Message::Text(text.into())).await.is_err() {
                            break;
                        }
                    }
                    Err(error) => error!(?error, "failed to serialize agent outbound message"),
                }
            }
            incoming = receiver.next() => {
                let Some(incoming) = incoming else {
                    break;
                };
                match incoming {
                    Ok(Message::Text(text)) => {
                        last_inbound = Instant::now();
                        match serde_json::from_str::<AgentInbound>(&text) {
                            Ok(message) => {
                                if let Err(error) = handle_agent_message(
                                    &state,
                                    &instance_id,
                                    connection_id,
                                    message,
                                ).await {
                                    error!(?error, %instance_id, "failed to handle agent websocket message");
                                }
                            }
                            Err(error) => warn!(?error, %text, "invalid agent websocket message"),
                        }
                    }
                    Ok(Message::Pong(_)) => last_inbound = Instant::now(),
                    Ok(Message::Close(_)) | Err(_) => break,
                    _ => {}
                }
            }
            _ = heartbeat.tick() => {
                if last_inbound.elapsed() > HEARTBEAT_TIMEOUT {
                    warn!(%instance_id, %connection_id, "agent websocket heartbeat timed out");
                    break;
                }
                let ping = AgentOutbound::Ping { now: now_ts() };
                let Ok(text) = serde_json::to_string(&ping) else {
                    continue;
                };
                if sender.send(Message::Text(text.into())).await.is_err() {
                    break;
                }
            }
        }
    }

    let mut agents = state.agents.write().await;
    if agents
        .get(&instance_id)
        .is_some_and(|handle| handle.connection_id == connection_id)
    {
        agents.remove(&instance_id);
    }
    drop(agents);

    close_connection_terminals(&state, &instance_id, connection_id).await;
    info!(%instance_id, %connection_id, "agent websocket disconnected");
}

async fn handle_agent_message(
    state: &AppState,
    instance_id: &str,
    connection_id: Uuid,
    message: AgentInbound,
) -> AppResult<()> {
    match message {
        AgentInbound::Pong { .. } => {}
        AgentInbound::Metrics {
            hostname,
            os,
            arch,
            agent_version,
            package_type,
            native_arch,
            update_privileged,
            metrics,
        } => {
            store_metrics(
                state,
                instance_id,
                &hostname,
                &os,
                &arch,
                &agent_version,
                package_type.as_deref(),
                native_arch.as_deref(),
                update_privileged,
                metrics,
            )
            .await?;
        }
        AgentInbound::CommandResult {
            job_id,
            exit_code,
            output,
        } => {
            complete_command_job(state, &job_id, exit_code, &output).await?;
        }
        AgentInbound::TerminalOpened { session_id } => {
            send_terminal_event(
                state,
                instance_id,
                connection_id,
                &session_id,
                TerminalServerMessage::Ready,
                false,
            )
            .await;
        }
        AgentInbound::TerminalOutput { session_id, data } => {
            send_terminal_event(
                state,
                instance_id,
                connection_id,
                &session_id,
                TerminalServerMessage::Output { data },
                false,
            )
            .await;
        }
        AgentInbound::TerminalClosed {
            session_id,
            exit_code,
            reason,
        } => {
            send_terminal_event(
                state,
                instance_id,
                connection_id,
                &session_id,
                TerminalServerMessage::Closed { exit_code, reason },
                true,
            )
            .await;
        }
        AgentInbound::UpdateStatus {
            release_id,
            artifact_id,
            version,
            retry_count,
            status,
            message,
        } => {
            record_update_status(
                state,
                instance_id,
                &release_id,
                &artifact_id,
                &version,
                retry_count,
                &status,
                message.as_deref(),
            )
            .await?;
        }
    }
    Ok(())
}

async fn store_metrics(
    state: &AppState,
    instance_id: &str,
    hostname: &str,
    os: &str,
    arch: &str,
    agent_version: &str,
    package_type: Option<&str>,
    native_arch: Option<&str>,
    update_privileged: Option<bool>,
    metrics: MetricPayload,
) -> AppResult<()> {
    sqlx::query(
        r#"
        UPDATE instances
        SET hostname = $1, os = $2, arch = $3, agent_version = $4,
            package_type = COALESCE($5, package_type),
            native_arch = COALESCE($6, native_arch),
            update_privileged = COALESCE($7, update_privileged), last_seen = $8
        WHERE id = $9
        "#,
    )
    .bind(hostname)
    .bind(os)
    .bind(arch)
    .bind(agent_version)
    .bind(package_type)
    .bind(native_arch)
    .bind(update_privileged.map(i64::from))
    .bind(now_ts())
    .bind(instance_id)
    .execute(&state.db)
    .await?;

    confirm_update_version(state, instance_id, agent_version).await?;

    sqlx::query(
        r#"
        INSERT INTO metrics(instance_id, ts, cpu_percent, memory_used, memory_total,
                            disk_used, disk_total, network_rx, network_tx, gpu_percent,
                            gpu_memory_used, gpu_memory_total, uptime_seconds, load_average)
        VALUES($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14)
        "#,
    )
    .bind(instance_id)
    .bind(metrics.ts)
    .bind(metrics.cpu_percent)
    .bind(metrics.memory_used)
    .bind(metrics.memory_total)
    .bind(metrics.disk_used)
    .bind(metrics.disk_total)
    .bind(metrics.network_rx)
    .bind(metrics.network_tx)
    .bind(metrics.gpu_percent)
    .bind(metrics.gpu_memory_used)
    .bind(metrics.gpu_memory_total)
    .bind(metrics.uptime_seconds)
    .bind(metrics.load_average)
    .execute(&state.db)
    .await?;

    Ok(())
}

async fn send_terminal_event(
    state: &AppState,
    instance_id: &str,
    connection_id: Uuid,
    session_id: &str,
    event: TerminalServerMessage,
    remove: bool,
) {
    let handle = if remove {
        let mut sessions = state.terminal_sessions.write().await;
        let matches = sessions.get(session_id).is_some_and(|handle| {
            handle.instance_id == instance_id && handle.agent_connection_id == connection_id
        });
        if matches {
            sessions.remove(session_id)
        } else {
            None
        }
    } else {
        state
            .terminal_sessions
            .read()
            .await
            .get(session_id)
            .filter(|handle| {
                handle.instance_id == instance_id && handle.agent_connection_id == connection_id
            })
            .cloned()
    };
    if let Some(handle) = handle {
        let _ = handle.tx.send(event);
    }
}

async fn close_connection_terminals(state: &AppState, instance_id: &str, connection_id: Uuid) {
    let mut sessions = state.terminal_sessions.write().await;
    sessions.retain(|_, handle| {
        let matches =
            handle.instance_id == instance_id && handle.agent_connection_id == connection_id;
        if matches {
            let _ = handle.tx.send(TerminalServerMessage::Closed {
                exit_code: None,
                reason: Some("实例连接已断开".to_string()),
            });
        }
        !matches
    });
}

pub async fn terminal_socket(
    state: AppState,
    instance_id: String,
    actor: String,
    socket: WebSocket,
) {
    let session_id = Uuid::new_v4().to_string();
    let started_at = now_ts();
    let Some(agent) = state.agents.read().await.get(&instance_id).cloned() else {
        send_single_terminal_message(
            socket,
            TerminalServerMessage::Error {
                message: "实例不在线".to_string(),
            },
        )
        .await;
        return;
    };

    if let Err(error) = sqlx::query(
        "INSERT INTO ssh_sessions(id, instance_id, actor, started_at) VALUES($1, $2, $3, $4)",
    )
    .bind(&session_id)
    .bind(&instance_id)
    .bind(&actor)
    .bind(started_at)
    .execute(&state.db)
    .await
    {
        error!(?error, "failed to create terminal session");
    }

    let (event_tx, mut event_rx) = mpsc::unbounded_channel();
    state.terminal_sessions.write().await.insert(
        session_id.clone(),
        TerminalSessionHandle {
            instance_id: instance_id.clone(),
            agent_connection_id: agent.connection_id,
            tx: event_tx,
        },
    );

    if agent
        .tx
        .send(AgentOutbound::TerminalOpen {
            session_id: session_id.clone(),
            cols: 80,
            rows: 24,
        })
        .is_err()
    {
        state.terminal_sessions.write().await.remove(&session_id);
        send_single_terminal_message(
            socket,
            TerminalServerMessage::Error {
                message: "实例连接已断开".to_string(),
            },
        )
        .await;
        return;
    }

    let (mut sender, mut receiver) = socket.split();
    if send_terminal_message(&mut sender, &TerminalServerMessage::Opening)
        .await
        .is_err()
    {
        state.terminal_sessions.write().await.remove(&session_id);
        return;
    }

    loop {
        tokio::select! {
            browser_message = receiver.next() => {
                let Some(browser_message) = browser_message else {
                    break;
                };
                match browser_message {
                    Ok(Message::Text(text)) => {
                        match serde_json::from_str::<TerminalClientMessage>(&text) {
                            Ok(TerminalClientMessage::Input { data }) => {
                                if agent.tx.send(AgentOutbound::TerminalInput {
                                    session_id: session_id.clone(),
                                    data,
                                }).is_err() {
                                    break;
                                }
                            }
                            Ok(TerminalClientMessage::Resize { cols, rows }) => {
                                let _ = agent.tx.send(AgentOutbound::TerminalResize {
                                    session_id: session_id.clone(),
                                    cols: cols.clamp(2, 500),
                                    rows: rows.clamp(1, 300),
                                });
                            }
                            Err(error) => warn!(?error, "invalid browser terminal message"),
                        }
                    }
                    Ok(Message::Close(_)) | Err(_) => break,
                    _ => {}
                }
            }
            event = event_rx.recv() => {
                let Some(event) = event else {
                    break;
                };
                let terminal_closed = matches!(event, TerminalServerMessage::Closed { .. });
                if send_terminal_message(&mut sender, &event).await.is_err() || terminal_closed {
                    break;
                }
            }
        }
    }

    state.terminal_sessions.write().await.remove(&session_id);
    let _ = agent.tx.send(AgentOutbound::TerminalClose {
        session_id: session_id.clone(),
    });
    let _ = sqlx::query("UPDATE ssh_sessions SET ended_at = $1 WHERE id = $2")
        .bind(now_ts())
        .bind(&session_id)
        .execute(&state.db)
        .await;
}

async fn send_single_terminal_message(mut socket: WebSocket, event: TerminalServerMessage) {
    if let Ok(text) = serde_json::to_string(&event) {
        let _ = socket.send(Message::Text(text.into())).await;
    }
    let _ = socket.close().await;
}

async fn send_terminal_message(
    sender: &mut futures_util::stream::SplitSink<WebSocket, Message>,
    event: &TerminalServerMessage,
) -> Result<(), axum::Error> {
    let text = serde_json::to_string(event)
        .unwrap_or_else(|_| r#"{"type":"error","message":"终端消息序列化失败"}"#.to_string());
    sender.send(Message::Text(text.into())).await
}
