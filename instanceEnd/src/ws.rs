use std::time::Duration;

use anyhow::Result;
use futures_util::{SinkExt, StreamExt};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio_tungstenite::{connect_async, tungstenite::Message};

use crate::{
    activity::ActivityTracker,
    command::execute_tracked_command,
    config::AgentConfig,
    http::register_once,
    metrics::MetricsCollector,
    models::{AgentInbound, AgentOutbound, Identity, UpdateOffer, UpdateStatus},
    profile::host_profile,
    terminal::TerminalManager,
    update::{PrepareResult, UpdateManager, update_capability},
};

const MANIFEST_INTERVAL: Duration = Duration::from_secs(60);

enum SocketOutcome {
    Disconnected,
    ApplyUpdate,
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

pub async fn agent_ws_loop(config: AgentConfig, identity: Identity) -> Result<()> {
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
        match register_once(&config, &identity, &http_client).await {
            Ok(response) if response.disabled => {
                crate::logging::info(format_args!("websocket paused: instance disabled"));
                tokio::time::sleep(Duration::from_secs(10)).await;
                continue;
            }
            Ok(response) if !response.approved => {
                crate::logging::info(format_args!(
                    "websocket waiting for approval: {}",
                    response.message
                ));
                tokio::time::sleep(Duration::from_secs(10)).await;
                continue;
            }
            Ok(_) => {}
            Err(error) => {
                crate::logging::error(format_args!("register before websocket failed: {error:#}"));
                tokio::time::sleep(Duration::from_secs(10)).await;
                continue;
            }
        }

        let url = websocket_url(&config.server, &identity);
        crate::logging::info(format_args!("connecting websocket: {url}"));
        match connect_async(&url).await {
            Ok((stream, _)) => {
                crate::logging::info(format_args!("websocket connected"));
                match handle_agent_socket(
                    stream,
                    &config,
                    &identity,
                    activity.clone(),
                    update_manager.clone(),
                )
                .await
                {
                    Ok(SocketOutcome::ApplyUpdate) => return Ok(()),
                    Ok(SocketOutcome::Disconnected) => {}
                    Err(error) => crate::logging::error(format_args!("websocket error: {error:#}")),
                }
            }
            Err(error) => {
                crate::logging::error(format_args!("websocket connect failed: {error:#}"))
            }
        }
        tokio::time::sleep(Duration::from_secs(5)).await;
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
) -> Result<SocketOutcome> {
    let (mut write, mut read) = stream.split();
    let (outbound_tx, mut outbound_rx) = mpsc::unbounded_channel::<AgentInbound>();
    let mut terminals = TerminalManager::new(outbound_tx.clone(), activity.clone());
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
    let outcome = loop {
        tokio::select! {
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
                        }
                    }
                    Message::Ping(data) => {
                        write.send(Message::Pong(data)).await?;
                    }
                    Message::Close(_) => break SocketOutcome::Disconnected,
                    _ => {}
                }
            }
        }
    };

    terminals.close_all();
    drop(active_update);
    Ok(outcome)
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
    format!(
        "{base}/api/agent/ws?instance_id={}&secret={}",
        identity.instance_id, identity.secret
    )
}
