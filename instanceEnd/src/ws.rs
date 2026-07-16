use std::time::Duration;

use anyhow::Result;
use futures_util::{SinkExt, StreamExt};
use tokio::sync::{mpsc, watch};
use tokio::task::JoinHandle;
use tokio_tungstenite::{connect_async, tungstenite::Message};

use crate::{
    activity::ActivityTracker,
    command::execute_tracked_command,
    config::AgentConfig,
    file_manager::{CAPABILITY as FILE_MANAGER_CAPABILITY, FileManager},
    http::register_once,
    metrics::MetricsCollector,
    models::{AgentInbound, AgentOutbound, Identity, UpdateOffer, UpdateStatus},
    profile::host_profile,
    remote_desktop::{CAPABILITY as DESKTOP_CAPABILITY, DesktopManager, DesktopOpenRequest},
    terminal::TerminalManager,
    update::{PrepareResult, UpdateManager, update_capability},
};

const MANIFEST_INTERVAL: Duration = Duration::from_secs(60);

enum SocketOutcome {
    Disconnected,
    ApplyUpdate,
    Shutdown,
}

enum UpdateTaskEvent {
    ReadyToApply { offer: UpdateOffer },
    Finished { artifact_id: String },
}

struct ActiveUpdate {
    artifact_id: String,
    task: JoinHandle<()>,
    manager: UpdateManager,
}

impl Drop for ActiveUpdate {
    fn drop(&mut self) {
        self.task.abort();
        self.manager.cancel_preparation();
    }
}

pub async fn agent_ws_loop(
    config: AgentConfig,
    identity: Identity,
    mut shutdown: watch::Receiver<bool>,
) -> Result<()> {
    let http_client = reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(30))
        .read_timeout(Duration::from_secs(30))
        .build()?;
    let activity = ActivityTracker::default();
    let update_manager = match UpdateManager::new(
        config.clone(),
        identity.clone(),
        http_client.clone(),
        activity.clone(),
    ) {
        Ok(manager) => Some(manager),
        Err(error) => {
            crate::logging::error(format_args!("agent updates are unavailable: {error:#}"));
            None
        }
    };
    loop {
        let registration = tokio::select! {
            biased;
            _ = shutdown_requested(&mut shutdown) => return Ok(()),
            registration = register_once(&config, &identity, &http_client) => registration,
        };
        match registration {
            Ok(response) if response.disabled => {
                crate::logging::info(format_args!("websocket paused: instance disabled"));
                if wait_or_shutdown(Duration::from_secs(10), &mut shutdown).await {
                    return Ok(());
                }
                continue;
            }
            Ok(response) if !response.approved => {
                crate::logging::info(format_args!(
                    "websocket waiting for approval: {}",
                    response.message
                ));
                if wait_or_shutdown(Duration::from_secs(10), &mut shutdown).await {
                    return Ok(());
                }
                continue;
            }
            Ok(_) => {}
            Err(error) => {
                crate::logging::error(format_args!("register before websocket failed: {error:#}"));
                if wait_or_shutdown(Duration::from_secs(10), &mut shutdown).await {
                    return Ok(());
                }
                continue;
            }
        }

        let url = websocket_url(&config.server, &identity);
        crate::logging::info(format_args!("connecting websocket: {url}"));
        let connection = tokio::select! {
            biased;
            _ = shutdown_requested(&mut shutdown) => return Ok(()),
            connection = connect_async(&url) => connection,
        };
        match connection {
            Ok((stream, _)) => {
                crate::logging::info(format_args!("websocket connected"));
                match handle_agent_socket(
                    stream,
                    &config,
                    &identity,
                    activity.clone(),
                    update_manager.clone(),
                    &mut shutdown,
                )
                .await
                {
                    Ok(SocketOutcome::ApplyUpdate) => return Ok(()),
                    Ok(SocketOutcome::Shutdown) => return Ok(()),
                    Ok(SocketOutcome::Disconnected) => {}
                    Err(error) => crate::logging::error(format_args!("websocket error: {error:#}")),
                }
            }
            Err(error) => {
                crate::logging::error(format_args!("websocket connect failed: {error:#}"))
            }
        }
        if wait_or_shutdown(Duration::from_secs(5), &mut shutdown).await {
            return Ok(());
        }
    }
}

async fn handle_agent_socket(
    stream: tokio_tungstenite::WebSocketStream<
        tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
    >,
    config: &AgentConfig,
    _identity: &Identity,
    activity: ActivityTracker,
    update_manager: Option<UpdateManager>,
    shutdown: &mut watch::Receiver<bool>,
) -> Result<SocketOutcome> {
    let (mut write, mut read) = stream.split();
    let (outbound_tx, mut outbound_rx) = mpsc::unbounded_channel::<AgentInbound>();
    let (binary_tx, mut binary_rx) = mpsc::channel(4);
    let mut terminals = TerminalManager::new(outbound_tx.clone(), activity.clone());
    let mut files = FileManager::new(outbound_tx.clone(), binary_tx, activity.clone());
    let mut desktops = DesktopManager::new(config.clone(), activity.clone(), outbound_tx.clone());
    let mut collector = MetricsCollector::new();
    let mut report_interval =
        tokio::time::interval(Duration::from_secs(config.report_interval.max(1)));
    report_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut manifest_interval = tokio::time::interval(MANIFEST_INTERVAL);
    manifest_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let (update_event_tx, mut update_event_rx) = mpsc::unbounded_channel();
    let mut active_update: Option<ActiveUpdate> = None;
    let capability = update_capability();

    if let Some(manager) = &update_manager {
        match manager.connected_status() {
            Ok(Some(status)) => {
                let _ = outbound_tx.send(status);
            }
            Ok(None) => {}
            Err(error) => {
                crate::logging::error(format_args!("failed to restore update status: {error:#}"))
            }
        }
    }
    let result: Result<SocketOutcome> = async {
        let outcome = loop {
            tokio::select! {
            biased;
            _ = shutdown_requested(shutdown) => {
                break SocketOutcome::Shutdown;
            }
            _ = report_interval.tick() => {
                let profile = host_profile();
                outbound_tx.send(AgentInbound::Metrics {
                    hostname: profile.hostname,
                    os: profile.os,
                    arch: profile.arch,
                    agent_version: profile.agent_version,
                    package_type: capability.package_type.clone(),
                    native_arch: capability.native_arch.clone(),
                    update_privileged: Some(capability.update_privileged),
                    metrics: collector.sample(),
                })?;
            }
            _ = manifest_interval.tick(), if update_manager.is_some() && active_update.is_none() => {
                let manager = update_manager.as_ref().expect("guarded by update_manager.is_some()");
                match manager.fetch_manifest().await {
                    Ok(Some(offer)) => {
                        match manager.can_start_offer(&offer) {
                            Ok(true) => {
                                active_update = Some(spawn_update_task(
                                    manager.clone(),
                                    offer,
                                    outbound_tx.clone(),
                                    update_event_tx.clone(),
                                ));
                            }
                            Ok(false) => {}
                            Err(error) => crate::logging::error(format_args!(
                                "failed to inspect local update state before manifest offer: {error:#}"
                            )),
                        }
                    }
                    Ok(None) => {}
                    Err(error) => crate::logging::error(format_args!(
                        "failed to check for an agent update: {error:#}"
                    )),
                }
            }
            event = update_event_rx.recv(), if active_update.is_some() => {
                let Some(event) = event else {
                    continue;
                };
                match event {
                    UpdateTaskEvent::ReadyToApply { offer } => {
                        if active_update
                            .as_ref()
                            .is_some_and(|active| active.artifact_id == offer.artifact_id)
                        {
                            let active = active_update
                                .take()
                                .expect("active update was checked above");
                            if !active
                                .manager
                                .launch_prepared_update(&offer, &outbound_tx)
                            {
                                continue;
                            }

                            let flush_result = tokio::time::timeout(
                                Duration::from_secs(2),
                                async {
                                    while let Ok(outbound) = outbound_rx.try_recv() {
                                        let payload = serde_json::to_string(&outbound)?;
                                        write.send(Message::Text(payload.into())).await?;
                                    }
                                    write.flush().await?;
                                    Result::<()>::Ok(())
                                },
                            )
                            .await;
                            match flush_result {
                                Ok(Ok(())) => {}
                                Ok(Err(error)) => crate::logging::error(format_args!(
                                    "failed to flush final update status before exiting: {error:#}"
                                )),
                                Err(_) => crate::logging::error(format_args!(
                                    "timed out flushing final update status before exiting"
                                )),
                            }
                            break SocketOutcome::ApplyUpdate;
                        }
                    }
                    UpdateTaskEvent::Finished { artifact_id } => {
                        if active_update
                            .as_ref()
                            .is_some_and(|active| active.artifact_id == artifact_id)
                        {
                            active_update.take();
                        }
                    }
                }
            }
            outbound = outbound_rx.recv() => {
                let Some(outbound) = outbound else {
                    break SocketOutcome::Disconnected;
                };
                let payload = serde_json::to_string(&outbound)?;
                write.send(Message::Text(payload.into())).await?;
            }
            binary = binary_rx.recv() => {
                let Some(binary) = binary else {
                    break SocketOutcome::Disconnected;
                };
                write.send(Message::Binary(binary.into())).await?;
            }
            incoming = read.next() => {
                let Some(incoming) = incoming else {
                    break SocketOutcome::Disconnected;
                };
                match incoming? {
                    Message::Text(text) => {
                        let message = serde_json::from_str::<AgentOutbound>(&text)?;
                        match message {
                            AgentOutbound::Ping { now } => {
                                outbound_tx.send(AgentInbound::Pong { now })?;
                            }
                            AgentOutbound::RunCommand { job_id, command } => {
                                crate::logging::info(format_args!(
                                    "running command job {job_id}: {command}"
                                ));
                                let command_outbound = outbound_tx.clone();
                                let command_activity = activity.clone();
                                tokio::spawn(async move {
                                    let (exit_code, output) =
                                        execute_tracked_command(&command, &command_activity).await;
                                    let _ = command_outbound.send(AgentInbound::CommandResult {
                                        job_id,
                                        exit_code,
                                        output,
                                    });
                                });
                            }
                            AgentOutbound::TerminalOpen { session_id, cols, rows } => {
                                terminals.open(session_id, cols, rows);
                            }
                            AgentOutbound::TerminalInput { session_id, data } => {
                                terminals.input(&session_id, &data);
                            }
                            AgentOutbound::TerminalResize { session_id, cols, rows } => {
                                terminals.resize(&session_id, cols, rows);
                            }
                            AgentOutbound::TerminalClose { session_id } => {
                                terminals.close(&session_id);
                            }
                            AgentOutbound::FileRequest { request_id, request } => {
                                files.handle_request(request_id, request);
                            }
                            AgentOutbound::FileTransferFinish { request_id } => {
                                files.finish_upload(&request_id);
                            }
                            AgentOutbound::FileTransferAck { request_id, sequence } => {
                                files.acknowledge_download(&request_id, sequence);
                            }
                            AgentOutbound::FileTransferCancel { request_id } => {
                                files.cancel(&request_id);
                            }
                            AgentOutbound::UpdateAvailable {
                                release_id,
                                version,
                                artifact_id,
                                download_url,
                                sha256,
                                size_bytes,
                                package_type,
                                native_arch,
                                retry_count,
                            } => {
                                let offer = UpdateOffer {
                                    release_id,
                                    version,
                                    artifact_id,
                                    download_url,
                                    sha256,
                                    size_bytes,
                                    package_type,
                                    native_arch,
                                    retry_count,
                                };
                                if active_update.is_none() {
                                    if let Some(manager) = &update_manager {
                                        match manager.can_start_offer(&offer) {
                                            Ok(true) => {
                                                active_update = Some(spawn_update_task(
                                                    manager.clone(),
                                                    offer,
                                                    outbound_tx.clone(),
                                                    update_event_tx.clone(),
                                                ));
                                            }
                                            Ok(false) => crate::logging::info(format_args!(
                                                "ignored duplicate update offer for active handoff {}",
                                                offer.artifact_id
                                            )),
                                            Err(error) => crate::logging::error(format_args!(
                                                "failed to inspect local update state before websocket offer: {error:#}"
                                            )),
                                        }
                                    } else {
                                        let _ = outbound_tx.send(AgentInbound::UpdateStatus {
                                            release_id: offer.release_id,
                                            artifact_id: offer.artifact_id,
                                            version: offer.version,
                                            retry_count: offer.retry_count,
                                            status: UpdateStatus::Failed,
                                            message: Some(
                                                "agent update storage could not be initialized"
                                                    .to_string(),
                                            ),
                                        });
                                    }
                                }
                            }
                            AgentOutbound::DesktopOpen {
                                session_id,
                                stream_token,
                                max_width,
                                max_height,
                                min_fps,
                                max_fps,
                                jpeg_quality,
                            } => desktops.open(DesktopOpenRequest {
                                session_id,
                                stream_token,
                                max_width,
                                max_height,
                                min_fps,
                                max_fps,
                                jpeg_quality,
                            }),
                            AgentOutbound::DesktopClose { session_id, reason } => {
                                desktops.close(&session_id, &reason);
                            }
                        }
                    }
                    Message::Binary(data) => files.handle_binary(&data),
                    Message::Ping(data) => {
                        write.send(Message::Pong(data)).await?;
                    }
                    Message::Close(_) => break SocketOutcome::Disconnected,
                    _ => {}
                }
            }
            }
        };
        Ok(outcome)
    }
    .await;

    terminals.close_all();
    files.close_all();
    let close_reason = if matches!(
        &result,
        Ok(SocketOutcome::Shutdown | SocketOutcome::ApplyUpdate)
    ) {
        "agent_shutdown"
    } else {
        "agent_disconnected"
    };
    desktops.close_all(close_reason).await;
    drop(active_update);
    result
}

async fn shutdown_requested(shutdown: &mut watch::Receiver<bool>) {
    if *shutdown.borrow() {
        return;
    }
    while shutdown.changed().await.is_ok() {
        if *shutdown.borrow() {
            return;
        }
    }
}

async fn wait_or_shutdown(duration: Duration, shutdown: &mut watch::Receiver<bool>) -> bool {
    tokio::select! {
        biased;
        _ = shutdown_requested(shutdown) => true,
        _ = tokio::time::sleep(duration) => false,
    }
}

fn spawn_update_task(
    manager: UpdateManager,
    offer: UpdateOffer,
    outbound: mpsc::UnboundedSender<AgentInbound>,
    events: mpsc::UnboundedSender<UpdateTaskEvent>,
) -> ActiveUpdate {
    let artifact_id = offer.artifact_id.clone();
    let task_artifact_id = artifact_id.clone();
    let ready_offer = offer.clone();
    let task_manager = manager.clone();
    let task = tokio::spawn(async move {
        let result = task_manager.prepare(offer, outbound).await;
        let event = match result {
            PrepareResult::ReadyToApply => UpdateTaskEvent::ReadyToApply { offer: ready_offer },
            PrepareResult::Finished => UpdateTaskEvent::Finished {
                artifact_id: task_artifact_id,
            },
        };
        let _ = events.send(event);
    });
    ActiveUpdate {
        artifact_id,
        task,
        manager,
    }
}

fn websocket_url(server: &str, identity: &Identity) -> String {
    let trimmed = server.trim_end_matches('/');
    let base = if let Some(rest) = trimmed.strip_prefix("https://") {
        format!("wss://{rest}")
    } else if let Some(rest) = trimmed.strip_prefix("http://") {
        format!("ws://{rest}")
    } else {
        format!("ws://{trimmed}")
    };
    let capabilities = if cfg!(windows) {
        format!("{FILE_MANAGER_CAPABILITY},{DESKTOP_CAPABILITY}")
    } else {
        FILE_MANAGER_CAPABILITY.to_string()
    };
    format!(
        "{base}/api/agent/ws?instance_id={}&secret={}&capabilities={capabilities}",
        identity.instance_id, identity.secret
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn websocket_url_advertises_file_manager_capability() {
        let url = websocket_url(
            "https://monitor.example/",
            &Identity {
                instance_id: "instance-1".to_string(),
                secret: "secret-1".to_string(),
            },
        );
        assert!(url.starts_with(
            "wss://monitor.example/api/agent/ws?instance_id=instance-1&secret=secret-1&capabilities=file_manager_v1"
        ));
        assert_eq!(url.contains(DESKTOP_CAPABILITY), cfg!(windows));
    }
}
