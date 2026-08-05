use std::{
    collections::VecDeque,
    future::Future,
    time::{Duration, Instant},
};

use axum::extract::ws::{Message, WebSocket};
use futures_util::{SinkExt, StreamExt};
use serde_json::json;
use tokio::sync::mpsc;
use tracing::{error, info, warn};
use uuid::Uuid;

use crate::{
    alerts, audit,
    auth::AdminSessionGuard,
    db::normalize_metric_timestamp,
    docker::{
        CAPABILITY as DOCKER_CAPABILITY, cancel_instance_docker, close_connection_docker,
        handle_docker_exec_event, handle_docker_log_chunk, handle_docker_log_closed,
        handle_docker_response, update_current_docker_status,
    },
    error::AppResult,
    files::{close_connection_file_requests, handle_agent_file_binary, handle_agent_file_response},
    jobs::{
        MAX_COMMAND_OUTPUT_BYTES, append_command_job_output, complete_command_job,
        fail_connection_command_jobs,
    },
    models::{
        AgentInbound, AgentOutbound, MAX_AGENT_UPDATE_RETRY_COUNT, MetricPayload,
        TerminalClientMessage, TerminalServerMessage,
    },
    remote_desktop::{close_connection_desktops, desktop_agent_closed, desktop_agent_opened},
    state::{AgentHandle, AgentOutboundSender, AppState, TerminalSessionHandle},
    updates::{
        confirm_update_version, offer_update_on_connect, record_connection_rollback_status,
        record_connection_update_status,
    },
    utils::now_ts,
};

const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(15);
const HEARTBEAT_TIMEOUT: Duration = Duration::from_secs(45);
const MAX_PENDING_HEARTBEATS: usize = 8;
pub(crate) const MAX_AGENT_MESSAGE_BYTES: usize = 2 * 1024 * 1024;
const MAX_AGENT_RESPONSE_BYTES: usize = 1024 * 1024;
const MAX_STREAM_CHUNK_BYTES: usize = 64 * 1024;
const MAX_STATUS_MESSAGE_BYTES: usize = 4 * 1024;
const MAX_AGENT_ID_BYTES: usize = 128;
const AGENT_OUTBOUND_QUEUE_CAPACITY: usize = 256;
const TERMINAL_EVENT_QUEUE_CAPACITY: usize = 128;
const MAX_TERMINAL_SESSIONS_PER_INSTANCE: usize = 8;
const SOCKET_SEND_TIMEOUT: Duration = Duration::from_secs(5);
const TERMINAL_SESSION_TIMEOUT: Duration = Duration::from_secs(2 * 60 * 60);

#[derive(Debug)]
struct PendingHeartbeat {
    token: i64,
    sent_at: Instant,
}

pub async fn agent_socket(
    state: AppState,
    instance_id: String,
    capabilities: Vec<String>,
    socket: WebSocket,
) {
    let connection_id = Uuid::new_v4();
    let (mut sender, mut receiver) = socket.split();
    let (shutdown_tx, mut shutdown_rx) = tokio::sync::watch::channel(false);
    let (tx, mut rx) =
        AgentOutboundSender::channel(AGENT_OUTBOUND_QUEUE_CAPACITY, shutdown_tx.clone());
    let (binary_tx, mut binary_rx) = mpsc::channel::<Vec<u8>>(4);

    let docker_capable = capabilities
        .iter()
        .any(|capability| capability == DOCKER_CAPABILITY);
    let replaced = state.agents.write().await.insert(
        instance_id.clone(),
        AgentHandle {
            connection_id,
            tx,
            binary_tx,
            shutdown_tx,
            capabilities,
            docker_status: Default::default(),
        },
    );
    if let Some(replaced) = replaced {
        replaced.shutdown_tx.send_replace(true);
        if let Err(error) =
            fail_connection_command_jobs(&state, &instance_id, replaced.connection_id).await
        {
            warn!(?error, %instance_id, connection_id = %replaced.connection_id, "failed to close replaced agent command jobs");
        }
        close_connection_terminals(&state, &instance_id, replaced.connection_id).await;
        close_connection_desktops(&state, &instance_id, replaced.connection_id).await;
        close_connection_file_requests(&state, &instance_id, replaced.connection_id).await;
        close_connection_docker(&state, &instance_id, replaced.connection_id).await;
    }
    let authorized = sqlx::query_scalar::<_, bool>(
        "SELECT EXISTS(SELECT 1 FROM instances WHERE id = $1 AND approved = 1 AND disabled = 0)",
    )
    .bind(&instance_id)
    .fetch_one(&state.db)
    .await;
    if !authorized.as_ref().is_ok_and(|authorized| *authorized) {
        warn!(?authorized, %instance_id, %connection_id, "closing unauthorized agent websocket after upgrade");
        remove_current_agent(&state, &instance_id, connection_id).await;
        close_connection_terminals(&state, &instance_id, connection_id).await;
        close_connection_desktops(&state, &instance_id, connection_id).await;
        close_connection_file_requests(&state, &instance_id, connection_id).await;
        close_connection_docker(&state, &instance_id, connection_id).await;
        return;
    }
    if !docker_capable
        && let Err(error) =
            update_current_docker_status(&state, &instance_id, connection_id, None).await
    {
        warn!(?error, %instance_id, %connection_id, "failed to clear stale Docker status");
    }
    let _ = sqlx::query("UPDATE instances SET last_seen = $1 WHERE id = $2")
        .bind(now_ts())
        .bind(&instance_id)
        .execute(&state.db)
        .await;
    offer_update_on_connect(&state, &instance_id).await;
    if let Err(error) = alerts::observe_connection(&state, &instance_id, true).await {
        warn!(?error, %instance_id, %connection_id, "failed to evaluate connected agent alerts");
    }

    info!(%instance_id, %connection_id, "agent websocket connected");
    let mut heartbeat = tokio::time::interval(HEARTBEAT_INTERVAL);
    heartbeat.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut last_inbound = Instant::now();
    let mut heartbeat_token = 0_i64;
    let mut pending_heartbeats = VecDeque::new();
    let mut latency_ms = None;
    let mut latency_sampled_at = None;

    loop {
        tokio::select! {
            changed = shutdown_rx.changed() => {
                if changed.is_err() || *shutdown_rx.borrow() {
                    break;
                }
            }
            outbound = rx.recv() => {
                let Some(outbound) = outbound else {
                    break;
                };
                match serde_json::to_string(&outbound) {
                    Ok(text) => {
                        if socket_send_with_timeout(sender.send(Message::Text(text.into())))
                            .await
                            .is_err()
                        {
                            break;
                        }
                    }
                    Err(error) => error!(?error, "failed to serialize agent outbound message"),
                }
            }
            outbound = binary_rx.recv() => {
                let Some(outbound) = outbound else {
                    break;
                };
                if socket_send_with_timeout(sender.send(Message::Binary(outbound.into())))
                    .await
                    .is_err()
                {
                    break;
                }
            }
            incoming = receiver.next() => {
                let Some(incoming) = incoming else {
                    break;
                };
                match incoming {
                    Ok(Message::Text(text)) => {
                        last_inbound = Instant::now();
                        if text.len() > MAX_AGENT_MESSAGE_BYTES {
                            warn!(%instance_id, message_bytes = text.len(), "agent websocket text frame exceeded limit");
                            break;
                        }
                        match serde_json::from_str::<AgentInbound>(&text) {
                            Ok(message) => {
                                if let Err(reason) = validate_agent_inbound(&message) {
                                    warn!(%instance_id, %reason, "agent websocket message exceeded business limits");
                                    break;
                                }
                                match message {
                                    AgentInbound::Pong { now } => {
                                        if let Some(sample) = accept_heartbeat_pong(
                                            &mut pending_heartbeats,
                                            now,
                                            Instant::now(),
                                        ) {
                                            latency_ms = Some(sample);
                                            latency_sampled_at = Some(now_ts());
                                        }
                                    }
                                    message => {
                                        if let Err(error) = handle_agent_message(
                                            &state,
                                            &instance_id,
                                            connection_id,
                                            message,
                                            latency_ms,
                                            latency_sampled_at,
                                        ).await {
                                            error!(?error, %instance_id, "failed to handle agent websocket message");
                                        }
                                    }
                                }
                            }
                            Err(error) => warn!(?error, message_bytes = text.len(), "invalid agent websocket message"),
                        }
                    }
                    Ok(Message::Pong(_)) => last_inbound = Instant::now(),
                    Ok(Message::Binary(data)) => {
                        last_inbound = Instant::now();
                        handle_agent_file_binary(
                            &state,
                            &instance_id,
                            connection_id,
                            &data,
                        ).await;
                    }
                    Ok(Message::Close(_)) | Err(_) => break,
                    _ => {}
                }
            }
            _ = heartbeat.tick() => {
                if last_inbound.elapsed() > HEARTBEAT_TIMEOUT {
                    warn!(%instance_id, %connection_id, "agent websocket heartbeat timed out");
                    break;
                }
                heartbeat_token = heartbeat_token.wrapping_add(1);
                let sent_at = Instant::now();
                let ping = AgentOutbound::Ping { now: heartbeat_token };
                let Ok(text) = serde_json::to_string(&ping) else {
                    continue;
                };
                if socket_send_with_timeout(sender.send(Message::Text(text.into())))
                    .await
                    .is_err()
                {
                    break;
                }
                retain_recent_heartbeats(&mut pending_heartbeats, sent_at);
                if pending_heartbeats.len() == MAX_PENDING_HEARTBEATS {
                    pending_heartbeats.pop_front();
                }
                pending_heartbeats.push_back(PendingHeartbeat {
                    token: heartbeat_token,
                    sent_at,
                });
            }
        }
    }

    let removed = remove_current_agent(&state, &instance_id, connection_id).await;
    if removed && let Err(error) = alerts::observe_connection(&state, &instance_id, false).await {
        warn!(?error, %instance_id, %connection_id, "failed to evaluate disconnected agent alerts");
    }

    if let Err(error) = fail_connection_command_jobs(&state, &instance_id, connection_id).await {
        warn!(?error, %instance_id, %connection_id, "failed to close disconnected agent command jobs");
    }

    close_connection_terminals(&state, &instance_id, connection_id).await;
    close_connection_desktops(&state, &instance_id, connection_id).await;
    close_connection_file_requests(&state, &instance_id, connection_id).await;
    close_connection_docker(&state, &instance_id, connection_id).await;
    info!(%instance_id, %connection_id, "agent websocket disconnected");
}

async fn remove_current_agent(state: &AppState, instance_id: &str, connection_id: Uuid) -> bool {
    let mut agents = state.agents.write().await;
    if agents
        .get(instance_id)
        .is_some_and(|handle| handle.connection_id == connection_id)
    {
        return agents.remove(instance_id).is_some();
    }
    false
}

pub async fn revoke_instance_connection(state: &AppState, instance_id: &str) {
    let agent = state.agents.write().await.remove(instance_id);
    let Some(agent) = agent else {
        cancel_instance_docker(state, instance_id, None).await;
        return;
    };
    if let Err(error) = alerts::observe_connection(state, instance_id, false).await {
        warn!(?error, %instance_id, connection_id = %agent.connection_id, "failed to evaluate revoked agent alerts");
    }
    agent.shutdown_tx.send_replace(true);
    if let Err(error) = fail_connection_command_jobs(state, instance_id, agent.connection_id).await
    {
        warn!(?error, %instance_id, connection_id = %agent.connection_id, "failed to close revoked agent command jobs");
    }
    close_connection_terminals(state, instance_id, agent.connection_id).await;
    close_connection_desktops(state, instance_id, agent.connection_id).await;
    close_connection_file_requests(state, instance_id, agent.connection_id).await;
    cancel_instance_docker(state, instance_id, Some(&agent)).await;
}

fn validate_agent_inbound(message: &AgentInbound) -> Result<(), &'static str> {
    let id = |value: &str| value.len() <= MAX_AGENT_ID_BYTES;
    let status = |value: &str| value.len() <= MAX_STATUS_MESSAGE_BYTES;
    match message {
        AgentInbound::Pong { .. } => {}
        AgentInbound::Metrics {
            hostname,
            os,
            arch,
            agent_version,
            package_type,
            native_arch,
            rollback_version,
            docker_status,
            ..
        } => {
            if hostname.len() > 255
                || os.len() > 64
                || arch.len() > 64
                || agent_version.len() > 64
                || package_type.as_ref().is_some_and(|value| value.len() > 64)
                || native_arch.as_ref().is_some_and(|value| value.len() > 64)
                || rollback_version
                    .as_ref()
                    .is_some_and(|value| value.len() > 64)
                || docker_status.as_ref().is_some_and(|value| {
                    serde_json::to_vec(value)
                        .map(|encoded| encoded.len() > MAX_STATUS_MESSAGE_BYTES)
                        .unwrap_or(true)
                })
            {
                return Err("metrics metadata too large");
            }
        }
        AgentInbound::CommandResult { job_id, output, .. } => {
            if !id(job_id) || output.len() > MAX_COMMAND_OUTPUT_BYTES {
                return Err("command result too large");
            }
        }
        AgentInbound::CommandOutput { job_id, output } => {
            if !id(job_id) || output.len() > MAX_STREAM_CHUNK_BYTES {
                return Err("command output chunk too large");
            }
        }
        AgentInbound::TerminalOpened { session_id }
        | AgentInbound::DesktopOpened { session_id }
        | AgentInbound::DockerExecOpened { session_id } => {
            if !id(session_id) {
                return Err("session identifier too large");
            }
        }
        AgentInbound::TerminalOutput { session_id, data }
        | AgentInbound::DockerExecOutput { session_id, data } => {
            if !id(session_id) || data.len() > MAX_STREAM_CHUNK_BYTES {
                return Err("stream output too large");
            }
        }
        AgentInbound::TerminalClosed {
            session_id, reason, ..
        }
        | AgentInbound::DockerExecClosed {
            session_id, reason, ..
        } => {
            if !id(session_id) || reason.as_ref().is_some_and(|value| !status(value)) {
                return Err("stream close message too large");
            }
        }
        AgentInbound::DesktopClosed { session_id, reason } => {
            if !id(session_id) || !status(reason) {
                return Err("desktop close message too large");
            }
        }
        AgentInbound::FileResponse {
            request_id,
            response,
        } => {
            if !id(request_id) || serialized_len_exceeds(response, MAX_AGENT_RESPONSE_BYTES) {
                return Err("file response too large");
            }
        }
        AgentInbound::DockerResponse {
            request_id,
            response,
        } => {
            if !id(request_id) || serialized_len_exceeds(response, MAX_AGENT_RESPONSE_BYTES) {
                return Err("docker response too large");
            }
        }
        AgentInbound::DockerLogChunk {
            request_id,
            data,
            cursor,
            ..
        } => {
            if !id(request_id)
                || data.len() > MAX_STREAM_CHUNK_BYTES
                || cursor.as_ref().is_some_and(|value| !status(value))
            {
                return Err("docker log chunk too large");
            }
        }
        AgentInbound::DockerLogClosed { request_id, error } => {
            if !id(request_id)
                || error
                    .as_ref()
                    .is_some_and(|value| serialized_len_exceeds(value, MAX_STATUS_MESSAGE_BYTES))
            {
                return Err("docker log close message too large");
            }
        }
        AgentInbound::UpdateStatus {
            attempt_id,
            release_id,
            artifact_id,
            version,
            retry_count,
            status: update_status,
            message,
        } => {
            if attempt_id.as_ref().is_some_and(|value| !id(value))
                || !id(release_id)
                || !id(artifact_id)
                || version.len() > 64
                || !(0..=MAX_AGENT_UPDATE_RETRY_COUNT).contains(retry_count)
                || update_status.len() > 64
                || message.as_ref().is_some_and(|value| !status(value))
            {
                return Err("update status too large");
            }
        }
        AgentInbound::RollbackStatus {
            attempt_id,
            retry_count,
            status: rollback_status,
            message,
        } => {
            if !id(attempt_id)
                || !(0..=MAX_AGENT_UPDATE_RETRY_COUNT).contains(retry_count)
                || rollback_status.len() > 64
                || message.as_ref().is_some_and(|value| !status(value))
            {
                return Err("rollback status too large");
            }
        }
    }
    Ok(())
}

fn serialized_len_exceeds<T: serde::Serialize>(value: &T, max_bytes: usize) -> bool {
    serde_json::to_vec(value)
        .map(|encoded| encoded.len() > max_bytes)
        .unwrap_or(true)
}

async fn handle_agent_message(
    state: &AppState,
    instance_id: &str,
    connection_id: Uuid,
    message: AgentInbound,
    latency_ms: Option<f64>,
    latency_sampled_at: Option<i64>,
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
            rollback_supported,
            rollback_version,
            docker_status,
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
                rollback_supported,
                rollback_version.as_deref(),
                connection_id,
                docker_status,
                metrics,
                latency_ms,
                latency_sampled_at,
            )
            .await?;
        }
        AgentInbound::CommandResult {
            job_id,
            exit_code,
            output,
        } => {
            if !complete_command_job(
                state,
                &job_id,
                instance_id,
                connection_id,
                exit_code,
                &output,
            )
            .await?
            {
                warn!(%instance_id, %connection_id, %job_id, "ignored unmatched or terminal command result");
            }
        }
        AgentInbound::CommandOutput { job_id, output } => {
            if !append_command_job_output(state, &job_id, instance_id, connection_id, &output)
                .await?
            {
                warn!(%instance_id, %connection_id, %job_id, "ignored unmatched or terminal command output");
            }
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
        AgentInbound::DesktopOpened { session_id } => {
            desktop_agent_opened(state, instance_id, connection_id, &session_id).await;
        }
        AgentInbound::DesktopClosed { session_id, reason } => {
            desktop_agent_closed(state, instance_id, connection_id, &session_id, &reason).await;
        }
        AgentInbound::FileResponse {
            request_id,
            response,
        } => {
            handle_agent_file_response(state, instance_id, connection_id, &request_id, response)
                .await;
        }
        AgentInbound::DockerResponse {
            request_id,
            response,
        } => {
            handle_docker_response(state, instance_id, connection_id, &request_id, response).await;
        }
        AgentInbound::DockerLogChunk {
            request_id,
            sequence,
            data,
            cursor,
        } => {
            handle_docker_log_chunk(
                state,
                instance_id,
                connection_id,
                &request_id,
                sequence,
                data,
                cursor,
            )
            .await;
        }
        AgentInbound::DockerLogClosed { request_id, error } => {
            handle_docker_log_closed(state, instance_id, connection_id, &request_id, error).await;
        }
        AgentInbound::DockerExecOpened { session_id } => {
            handle_docker_exec_event(
                state,
                instance_id,
                connection_id,
                &session_id,
                TerminalServerMessage::Ready,
                false,
            )
            .await;
        }
        AgentInbound::DockerExecOutput { session_id, data } => {
            handle_docker_exec_event(
                state,
                instance_id,
                connection_id,
                &session_id,
                TerminalServerMessage::Output { data },
                false,
            )
            .await;
        }
        AgentInbound::DockerExecClosed {
            session_id,
            exit_code,
            reason,
        } => {
            handle_docker_exec_event(
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
            attempt_id,
            release_id,
            artifact_id,
            version,
            retry_count,
            status,
            message,
        } => {
            if !record_connection_update_status(
                state,
                instance_id,
                connection_id,
                attempt_id.as_deref(),
                &release_id,
                &artifact_id,
                &version,
                retry_count,
                &status,
                message.as_deref(),
            )
            .await?
            {
                warn!(%instance_id, %connection_id, %release_id, %artifact_id, "ignored update status from stale agent connection");
            }
        }
        AgentInbound::RollbackStatus {
            attempt_id,
            retry_count,
            status,
            message,
        } => {
            if !record_connection_rollback_status(
                state,
                instance_id,
                connection_id,
                &attempt_id,
                retry_count,
                &status,
                message.as_deref(),
            )
            .await?
            {
                warn!(%instance_id, %connection_id, %attempt_id, "ignored rollback status from stale agent connection");
            }
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
    rollback_supported: Option<bool>,
    rollback_version: Option<&str>,
    connection_id: Uuid,
    docker_status: Option<crate::models::DockerStatus>,
    metrics: MetricPayload,
    latency_ms: Option<f64>,
    latency_sampled_at: Option<i64>,
) -> AppResult<()> {
    if !state
        .agents
        .read()
        .await
        .get(instance_id)
        .is_some_and(|agent| agent.connection_id == connection_id)
    {
        return Ok(());
    }
    let received_at = now_ts();
    let metric_timestamp = normalize_metric_timestamp(metrics.ts, received_at)?;
    sqlx::query(
        r#"
        UPDATE instances
        SET hostname = $1, os = $2, arch = $3, agent_version = $4,
            package_type = COALESCE($5, package_type),
            native_arch = COALESCE($6, native_arch),
            update_privileged = COALESCE($7, update_privileged),
            rollback_supported = CASE
                WHEN $8 IS NULL AND EXISTS (
                    SELECT 1 FROM agent_update_attempts AS u
                    WHERE u.instance_id = $11 AND u.operation = 'rollback'
                      AND u.target_version = $4
                ) THEN 0
                ELSE COALESCE($8, 0)
            END,
            rollback_version = CASE
                WHEN $8 IS NULL AND EXISTS (
                    SELECT 1 FROM agent_update_attempts AS u
                    WHERE u.instance_id = $11 AND u.operation = 'rollback'
                      AND u.target_version = $4
                ) THEN ''
                WHEN $8 = 1 THEN COALESCE($9, '')
                ELSE ''
            END,
            last_seen = $10
        WHERE id = $11
        "#,
    )
    .bind(hostname)
    .bind(os)
    .bind(arch)
    .bind(agent_version)
    .bind(package_type)
    .bind(native_arch)
    .bind(update_privileged.map(i64::from))
    .bind(rollback_supported.map(i64::from))
    .bind(rollback_version)
    .bind(now_ts())
    .bind(instance_id)
    .execute(&state.db)
    .await?;

    confirm_update_version(state, instance_id, agent_version).await?;

    update_current_docker_status(state, instance_id, connection_id, docker_status).await?;

    sqlx::query(
        r#"
        INSERT INTO metrics(instance_id, ts, cpu_percent, memory_used, memory_total,
                            disk_used, disk_total, network_rx, network_tx, gpu_percent,
                            gpu_memory_used, gpu_memory_total, uptime_seconds, load_average,
                            latency_ms, received_at, latency_sampled_at)
        VALUES($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, $16, $17)
        "#,
    )
    .bind(instance_id)
    .bind(metric_timestamp)
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
    .bind(latency_ms)
    .bind(received_at)
    .bind(latency_sampled_at)
    .execute(&state.db)
    .await?;

    alerts::observe_metric(
        state,
        instance_id,
        &metrics,
        latency_ms,
        received_at,
        latency_sampled_at,
    )
    .await?;

    Ok(())
}

fn accept_heartbeat_pong(
    pending: &mut VecDeque<PendingHeartbeat>,
    token: i64,
    received_at: Instant,
) -> Option<f64> {
    let position = pending
        .iter()
        .position(|heartbeat| heartbeat.token == token)?;
    let heartbeat = pending.remove(position)?;
    Some(
        received_at
            .saturating_duration_since(heartbeat.sent_at)
            .as_secs_f64()
            * 1_000.0,
    )
}

fn retain_recent_heartbeats(pending: &mut VecDeque<PendingHeartbeat>, now: Instant) {
    while pending.front().is_some_and(|heartbeat| {
        now.saturating_duration_since(heartbeat.sent_at) > HEARTBEAT_TIMEOUT
    }) {
        pending.pop_front();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn explicit_shutdown_survives_active_agent_handle_clones() {
        let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
        let (tx, _rx) = AgentOutboundSender::channel(1, shutdown_tx.clone());
        let (binary_tx, _binary_rx) = mpsc::channel(1);
        let handle = AgentHandle {
            connection_id: Uuid::new_v4(),
            tx,
            binary_tx,
            shutdown_tx,
            capabilities: Vec::new(),
            docker_status: Default::default(),
        };
        let active_session_handle = handle.clone();

        handle.shutdown_tx.send_replace(true);

        assert!(*shutdown_rx.borrow());
        assert!(*active_session_handle.shutdown_tx.borrow());
    }

    #[test]
    fn agent_control_queue_rejects_messages_at_capacity() {
        let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
        let (tx, mut rx) = AgentOutboundSender::channel(1, shutdown_tx);

        assert!(tx.send(AgentOutbound::Ping { now: 1 }).is_ok());
        assert_eq!(
            tx.send(AgentOutbound::Ping { now: 2 }),
            Err(crate::state::AgentOutboundSendError::Full)
        );
        assert!(matches!(rx.try_recv(), Ok(AgentOutbound::Ping { now: 1 })));
        assert!(*shutdown_rx.borrow());
    }

    #[tokio::test]
    async fn websocket_sends_are_bounded_by_a_timeout() {
        let pending_send = std::future::pending::<Result<(), ()>>();

        assert!(
            send_with_timeout(Duration::from_millis(1), pending_send)
                .await
                .is_err()
        );
    }

    #[test]
    fn terminal_session_limit_only_counts_the_target_instance() {
        let below_limit = vec!["instance-a"; MAX_TERMINAL_SESSIONS_PER_INSTANCE - 1];
        assert!(!terminal_session_limit_reached(
            below_limit.iter().copied(),
            "instance-a"
        ));

        let mut at_limit = below_limit;
        at_limit.push("instance-b");
        assert!(!terminal_session_limit_reached(
            at_limit.iter().copied(),
            "instance-a"
        ));
        at_limit.push("instance-a");
        assert!(terminal_session_limit_reached(
            at_limit.iter().copied(),
            "instance-a"
        ));
    }

    #[test]
    fn matching_pong_returns_millisecond_latency_and_clears_pending_heartbeat() {
        let sent_at = Instant::now();
        let mut pending = VecDeque::from([PendingHeartbeat { token: 42, sent_at }]);

        let latency = accept_heartbeat_pong(&mut pending, 42, sent_at + Duration::from_millis(37))
            .expect("matching pong should produce a latency sample");

        assert!((latency - 37.0).abs() < f64::EPSILON);
        assert!(pending.is_empty());
    }

    #[test]
    fn mismatched_pong_is_ignored_without_clearing_pending_heartbeat() {
        let sent_at = Instant::now();
        let mut pending = VecDeque::from([PendingHeartbeat { token: 42, sent_at }]);

        assert!(accept_heartbeat_pong(&mut pending, 41, sent_at).is_none());
        assert_eq!(pending.front().map(|heartbeat| heartbeat.token), Some(42));
    }

    #[test]
    fn out_of_order_pongs_match_their_own_pending_heartbeat() {
        let sent_at = Instant::now();
        let mut pending = VecDeque::from([
            PendingHeartbeat { token: 41, sent_at },
            PendingHeartbeat {
                token: 42,
                sent_at: sent_at + Duration::from_millis(5),
            },
        ]);

        let latency = accept_heartbeat_pong(&mut pending, 41, sent_at + Duration::from_millis(37))
            .expect("an older pong should still match");

        assert!((latency - 37.0).abs() < f64::EPSILON);
        assert_eq!(pending.front().map(|heartbeat| heartbeat.token), Some(42));
    }

    #[test]
    fn agent_inbound_business_limits_reject_oversized_outputs() {
        let accepted = AgentInbound::TerminalOutput {
            session_id: "session-1".to_string(),
            data: "x".repeat(MAX_STREAM_CHUNK_BYTES),
        };
        assert!(validate_agent_inbound(&accepted).is_ok());

        let oversized_stream = AgentInbound::TerminalOutput {
            session_id: "session-1".to_string(),
            data: "x".repeat(MAX_STREAM_CHUNK_BYTES + 1),
        };
        assert!(validate_agent_inbound(&oversized_stream).is_err());

        let oversized_command = AgentInbound::CommandResult {
            job_id: "job-1".to_string(),
            exit_code: 0,
            output: "x".repeat(MAX_COMMAND_OUTPUT_BYTES + 1),
        };
        assert!(validate_agent_inbound(&oversized_command).is_err());

        let command_output = AgentInbound::CommandOutput {
            job_id: "job-1".to_string(),
            output: "x".repeat(MAX_STREAM_CHUNK_BYTES),
        };
        assert!(validate_agent_inbound(&command_output).is_ok());

        let oversized_command_output = AgentInbound::CommandOutput {
            job_id: "job-1".to_string(),
            output: "x".repeat(MAX_STREAM_CHUNK_BYTES + 1),
        };
        assert!(validate_agent_inbound(&oversized_command_output).is_err());

        let oversized_id = AgentInbound::DesktopOpened {
            session_id: "x".repeat(MAX_AGENT_ID_BYTES + 1),
        };
        assert!(validate_agent_inbound(&oversized_id).is_err());
    }
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
    let Some(handle) = handle else {
        return;
    };
    if handle.tx.try_send(event).is_err() && !remove {
        let removed = {
            let mut sessions = state.terminal_sessions.write().await;
            if sessions.get(session_id).is_some_and(|current| {
                current.instance_id == instance_id && current.agent_connection_id == connection_id
            }) {
                sessions.remove(session_id).is_some()
            } else {
                false
            }
        };
        if removed {
            warn!(%instance_id, %connection_id, %session_id, "terminal browser event queue overflowed");
            close_agent_terminal(state, instance_id, connection_id, session_id).await;
        }
    }
}

async fn close_agent_terminal(
    state: &AppState,
    instance_id: &str,
    connection_id: Uuid,
    session_id: &str,
) {
    let agent = state
        .agents
        .read()
        .await
        .get(instance_id)
        .filter(|agent| agent.connection_id == connection_id)
        .cloned();
    if let Some(agent) = agent {
        send_terminal_agent_message(
            &agent,
            AgentOutbound::TerminalClose {
                session_id: session_id.to_string(),
            },
        );
    }
}

fn send_terminal_agent_message(agent: &AgentHandle, message: AgentOutbound) -> bool {
    if agent.tx.send(message).is_ok() {
        return true;
    }

    warn!(connection_id = %agent.connection_id, "agent control queue unavailable during terminal operation");
    agent.shutdown_tx.send_replace(true);
    false
}

async fn close_connection_terminals(state: &AppState, instance_id: &str, connection_id: Uuid) {
    let mut sessions = state.terminal_sessions.write().await;
    sessions.retain(|_, handle| {
        let matches =
            handle.instance_id == instance_id && handle.agent_connection_id == connection_id;
        if matches {
            let _ = handle.tx.try_send(TerminalServerMessage::Closed {
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
    user_id: String,
    audit_context: audit::AuditContext,
    session_guard: AdminSessionGuard,
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

    if let Err(error) = audit::insert_event(
        &state.db,
        &audit::AuditEventInput {
            category: "terminal".to_string(),
            kind: "session".to_string(),
            actor: actor.clone(),
            user_id: Some(user_id),
            action: "terminal_session".to_string(),
            target: instance_id.clone(),
            detail: "启动终端会话".to_string(),
            metadata: json!({}),
            instance_id: Some(instance_id.clone()),
            node_snapshot: audit::instance_snapshot(&state.db, &instance_id).await,
            context: audit_context,
            session_id: Some(session_id.clone()),
            operation_id: None,
            status: "running".to_string(),
            error_code: None,
            error_reason: String::new(),
            created_at: started_at,
            completed_at: None,
        },
    )
    .await
    {
        warn!(?error, %instance_id, %session_id, "failed to create terminal audit event");
        send_single_terminal_message(
            socket,
            TerminalServerMessage::Error {
                message: "无法创建终端审计记录".to_string(),
            },
        )
        .await;
        return;
    }

    let (event_tx, mut event_rx) = mpsc::channel(TERMINAL_EVENT_QUEUE_CAPACITY);
    let terminal_limit_reached = {
        let mut sessions = state.terminal_sessions.write().await;
        let limit_reached = terminal_session_limit_reached(
            sessions.values().map(|handle| handle.instance_id.as_str()),
            &instance_id,
        );
        if !limit_reached {
            sessions.insert(
                session_id.clone(),
                TerminalSessionHandle {
                    instance_id: instance_id.clone(),
                    agent_connection_id: agent.connection_id,
                    tx: event_tx,
                },
            );
        }
        limit_reached
    };
    if terminal_limit_reached {
        mark_terminal_session_ended(
            &state,
            &session_id,
            "failed",
            Some("session_limit"),
            "终端会话已达到上限",
        )
        .await;
        send_single_terminal_message(
            socket,
            TerminalServerMessage::Error {
                message: "该实例的终端会话已达到上限".to_string(),
            },
        )
        .await;
        return;
    }

    if !send_terminal_agent_message(
        &agent,
        AgentOutbound::TerminalOpen {
            session_id: session_id.clone(),
            cols: 80,
            rows: 24,
        },
    ) {
        finish_terminal_session(
            &state,
            &agent,
            &session_id,
            "failed",
            Some("offline"),
            "实例连接已断开",
        )
        .await;
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
        finish_terminal_session(
            &state,
            &agent,
            &session_id,
            "failed",
            Some("client_disconnected"),
            "客户端连接已断开",
        )
        .await;
        return;
    }
    let authorization = session_guard.wait_until_invalid(state.clone());
    tokio::pin!(authorization);
    let session_timeout = tokio::time::sleep(TERMINAL_SESSION_TIMEOUT);
    tokio::pin!(session_timeout);
    let mut end_status = "success";
    let mut end_code = None;
    let mut end_reason = String::new();

    loop {
        tokio::select! {
            _ = &mut session_timeout => {
                let _ = send_terminal_message(
                    &mut sender,
                    &TerminalServerMessage::Error {
                        message: "终端会话已超时".to_string(),
                    },
                )
                .await;
                end_status = "failed";
                end_code = Some("session_timeout");
                end_reason = "终端会话已超时".to_string();
                break;
            }
            _ = &mut authorization => {
                let _ = send_terminal_message(
                    &mut sender,
                    &TerminalServerMessage::Error {
                        message: "管理员会话已失效".to_string(),
                    },
                )
                .await;
                end_status = "failed";
                end_code = Some("authorization_revoked");
                end_reason = "管理员会话已失效".to_string();
                break;
            }
            browser_message = receiver.next() => {
                let Some(browser_message) = browser_message else {
                    end_status = "failed";
                    end_code = Some("client_disconnected");
                    end_reason = "客户端连接已断开".to_string();
                    break;
                };
                match browser_message {
                    Ok(Message::Text(text)) => {
                        match serde_json::from_str::<TerminalClientMessage>(&text) {
                            Ok(TerminalClientMessage::Input { data })
                                if data.len() <= MAX_STREAM_CHUNK_BYTES => {
                                if !send_terminal_agent_message(
                                    &agent,
                                    AgentOutbound::TerminalInput {
                                        session_id: session_id.clone(),
                                        data,
                                    },
                                ) {
                                    end_status = "failed";
                                    end_code = Some("agent_disconnected");
                                    end_reason = "实例连接已断开".to_string();
                                    break;
                                }
                            }
                            Ok(TerminalClientMessage::Input { .. }) => {
                                warn!(%instance_id, %session_id, "terminal browser input exceeded limit");
                                end_status = "failed";
                                end_code = Some("message_too_large");
                                end_reason = "终端消息超过大小限制".to_string();
                                break;
                            }
                            Ok(TerminalClientMessage::Resize { cols, rows }) => {
                                if !send_terminal_agent_message(
                                    &agent,
                                    AgentOutbound::TerminalResize {
                                        session_id: session_id.clone(),
                                        cols: cols.clamp(2, 500),
                                        rows: rows.clamp(1, 300),
                                    },
                                ) {
                                    end_status = "failed";
                                    end_code = Some("agent_disconnected");
                                    end_reason = "实例连接已断开".to_string();
                                    break;
                                }
                            }
                            Err(error) => warn!(?error, "invalid browser terminal message"),
                        }
                    }
                    Ok(Message::Close(_)) => break,
                    Err(_) => {
                        end_status = "failed";
                        end_code = Some("client_disconnected");
                        end_reason = "客户端连接已断开".to_string();
                        break;
                    }
                    _ => {}
                }
            }
            event = event_rx.recv() => {
                let Some(event) = event else {
                    end_status = "failed";
                    end_code = Some("agent_disconnected");
                    end_reason = "实例连接已断开".to_string();
                    break;
                };
                let terminal_closed = matches!(event, TerminalServerMessage::Closed { .. });
                if let TerminalServerMessage::Closed { exit_code, reason } = &event {
                    if let Some(reason) = reason.as_deref().filter(|reason| !reason.is_empty()) {
                        end_status = "failed";
                        end_code = Some(if reason.contains("断开") {
                            "agent_disconnected"
                        } else {
                            "terminal_closed"
                        });
                        end_reason = reason.to_string();
                    } else if exit_code.is_some_and(|exit_code| exit_code != 0) {
                        end_status = "failed";
                        end_code = Some("terminal_nonzero_exit");
                        end_reason = format!("终端进程退出码 {}", exit_code.unwrap_or_default());
                    }
                }
                if send_terminal_message(&mut sender, &event).await.is_err() || terminal_closed {
                    if !terminal_closed {
                        end_status = "failed";
                        end_code = Some("client_disconnected");
                        end_reason = "客户端连接已断开".to_string();
                    }
                    break;
                }
            }
        }
    }

    finish_terminal_session(
        &state,
        &agent,
        &session_id,
        end_status,
        end_code,
        &end_reason,
    )
    .await;
    let _ = tokio::time::timeout(SOCKET_SEND_TIMEOUT, sender.close()).await;
}

async fn finish_terminal_session(
    state: &AppState,
    agent: &AgentHandle,
    session_id: &str,
    status: &str,
    error_code: Option<&str>,
    error_reason: &str,
) {
    state.terminal_sessions.write().await.remove(session_id);
    send_terminal_agent_message(
        agent,
        AgentOutbound::TerminalClose {
            session_id: session_id.to_string(),
        },
    );
    mark_terminal_session_ended(state, session_id, status, error_code, error_reason).await;
}

async fn mark_terminal_session_ended(
    state: &AppState,
    session_id: &str,
    status: &str,
    error_code: Option<&str>,
    error_reason: &str,
) {
    let _ =
        audit::finish_session_event(&state.db, session_id, status, error_code, error_reason).await;
}

fn terminal_session_limit_reached<'a>(
    instance_ids: impl Iterator<Item = &'a str>,
    instance_id: &str,
) -> bool {
    instance_ids
        .filter(|current| *current == instance_id)
        .take(MAX_TERMINAL_SESSIONS_PER_INSTANCE)
        .count()
        >= MAX_TERMINAL_SESSIONS_PER_INSTANCE
}

async fn send_single_terminal_message(mut socket: WebSocket, event: TerminalServerMessage) {
    if let Ok(text) = serde_json::to_string(&event) {
        let _ = socket_send_with_timeout(socket.send(Message::Text(text.into()))).await;
    }
    let _ = tokio::time::timeout(SOCKET_SEND_TIMEOUT, socket.close()).await;
}

async fn send_terminal_message(
    sender: &mut futures_util::stream::SplitSink<WebSocket, Message>,
    event: &TerminalServerMessage,
) -> Result<(), ()> {
    let text = serde_json::to_string(event)
        .unwrap_or_else(|_| r#"{"type":"error","message":"终端消息序列化失败"}"#.to_string());
    socket_send_with_timeout(sender.send(Message::Text(text.into()))).await
}

async fn socket_send_with_timeout<F, E>(send: F) -> Result<(), ()>
where
    F: Future<Output = Result<(), E>>,
{
    send_with_timeout(SOCKET_SEND_TIMEOUT, send).await
}

async fn send_with_timeout<F, E>(timeout: Duration, send: F) -> Result<(), ()>
where
    F: Future<Output = Result<(), E>>,
{
    tokio::time::timeout(timeout, send)
        .await
        .map_err(|_| ())?
        .map_err(|_| ())
}
