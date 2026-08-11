use std::{future::Future, sync::Arc, time::Duration};

use anyhow::{Context, Result};
use futures_util::{SinkExt, StreamExt};
use reqwest::{Url, redirect::Policy};
use tokio::sync::{Semaphore, mpsc, watch};
use tokio::task::JoinHandle;
use tokio_tungstenite::{
    connect_async_with_config,
    tungstenite::{
        Message, client::IntoClientRequest, http::HeaderValue, protocol::WebSocketConfig,
    },
};

use crate::{
    activity::ActivityTracker,
    command::execute_tracked_command,
    config::{AgentConfig, ServerEndpoint},
    docker::{CAPABILITY as DOCKER_CAPABILITY, DockerManager},
    file_manager::{CAPABILITY as FILE_MANAGER_CAPABILITY, FileManager},
    http::register_once,
    metrics::{METRICS_SAMPLE_TIMEOUT, MetricsSampleEvent, MetricsSampler},
    models::{AgentInbound, AgentOutbound, Identity, RollbackOffer, UpdateOffer, UpdateStatus},
    outbound::AgentEventSender,
    profile::host_profile,
    remote_desktop::{CAPABILITY as DESKTOP_CAPABILITY, DesktopManager, DesktopOpenRequest},
    terminal::{CAPABILITY as TERMINAL_SHELLS_CAPABILITY, TerminalManager, available_shells},
    update::{PrepareResult, UpdateManager, update_capability},
};

const MANIFEST_INTERVAL: Duration = Duration::from_secs(60);
const AGENT_EVENT_QUEUE_CAPACITY: usize = 128;
const TERMINAL_STREAM_QUEUE_CAPACITY: usize = 128;
const MAX_CONCURRENT_COMMANDS: usize = 4;
const MAX_SERVER_MESSAGE_BYTES: usize = 2 * 1024 * 1024;
const SOCKET_SEND_TIMEOUT: Duration = Duration::from_secs(5);
const ROLLBACK_CAPABILITY: &str = "agent_rollback_v1";

enum SocketOutcome {
    Disconnected,
    ApplyUpdate,
    Shutdown,
}

enum UpdateTaskEvent {
    ReadyToApply {
        operation_id: String,
        offer: UpdateOffer,
    },
    Finished {
        operation_id: String,
    },
}

struct ActiveUpdate {
    operation_id: String,
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
    #[cfg(windows)]
    let mut identity = identity;
    let http_client = reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(30))
        .read_timeout(Duration::from_secs(30))
        .redirect(agent_redirect_policy())
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
    let command_slots = Arc::new(Semaphore::new(MAX_CONCURRENT_COMMANDS));
    let mut metrics_sampler =
        MetricsSampler::new(METRICS_SAMPLE_TIMEOUT).context("failed to start metrics sampler")?;
    loop {
        let registration = tokio::select! {
            biased;
            _ = shutdown_requested(&mut shutdown) => return Ok(()),
            registration = register_once(&config, &identity, &http_client) => registration,
        };
        let response = match registration {
            Ok(response) => {
                #[cfg(windows)]
                crate::identity::complete_secret_rotation(
                    config.identity_file.clone(),
                    &mut identity,
                )?;
                response
            }
            Err(error) => {
                crate::logging::error(format_args!("register before websocket failed: {error:#}"));
                if wait_or_shutdown(Duration::from_secs(10), &mut shutdown).await {
                    return Ok(());
                }
                continue;
            }
        };
        match response {
            response if response.disabled => {
                crate::logging::info(format_args!("websocket paused: instance disabled"));
                if wait_or_shutdown(Duration::from_secs(10), &mut shutdown).await {
                    return Ok(());
                }
                continue;
            }
            response if !response.approved => {
                crate::logging::info(format_args!(
                    "websocket waiting for approval: {}",
                    response.message
                ));
                if wait_or_shutdown(Duration::from_secs(10), &mut shutdown).await {
                    return Ok(());
                }
                continue;
            }
            _ => {}
        }

        let request = websocket_request(&config.server, &identity)?;
        crate::logging::info(format_args!(
            "connecting websocket for instance {}",
            identity.instance_id
        ));
        let connection = tokio::select! {
            biased;
            _ = shutdown_requested(&mut shutdown) => return Ok(()),
            connection = connect_async_with_config(
                request,
                Some(
                    WebSocketConfig::default()
                        .max_message_size(Some(MAX_SERVER_MESSAGE_BYTES))
                        .max_frame_size(Some(MAX_SERVER_MESSAGE_BYTES)),
                ),
                false,
            ) => connection,
        };
        match connection {
            Ok((stream, _)) => {
                crate::logging::info(format_args!("websocket connected"));
                match handle_agent_socket(
                    stream,
                    &config,
                    &identity,
                    activity.clone(),
                    command_slots.clone(),
                    update_manager.clone(),
                    &mut metrics_sampler,
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

fn agent_redirect_policy() -> Policy {
    Policy::custom(|attempt| {
        if attempt.previous().len() > 10 {
            return attempt.error("too many agent HTTP redirects");
        }
        if attempt
            .previous()
            .first()
            .is_some_and(|origin| same_origin(origin, attempt.url()))
        {
            attempt.follow()
        } else {
            attempt.error("cross-origin agent HTTP redirect refused")
        }
    })
}

fn same_origin(left: &Url, right: &Url) -> bool {
    left.scheme() == right.scheme()
        && left.host_str() == right.host_str()
        && left.port_or_known_default() == right.port_or_known_default()
}

async fn handle_agent_socket(
    stream: tokio_tungstenite::WebSocketStream<
        tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
    >,
    config: &AgentConfig,
    _identity: &Identity,
    activity: ActivityTracker,
    command_slots: Arc<Semaphore>,
    update_manager: Option<UpdateManager>,
    metrics_sampler: &mut MetricsSampler,
    shutdown: &mut watch::Receiver<bool>,
) -> Result<SocketOutcome> {
    let (mut write, mut read) = stream.split();
    let (outbound_tx, mut outbound_rx, mut outbound_failed_rx) =
        AgentEventSender::channel(AGENT_EVENT_QUEUE_CAPACITY);
    let (terminal_stream_tx, mut terminal_stream_rx, mut terminal_stream_failed_rx) =
        AgentEventSender::channel(TERMINAL_STREAM_QUEUE_CAPACITY);
    let (binary_tx, mut binary_rx) = mpsc::channel(4);
    let (docker_stream_tx, mut docker_stream_rx, mut docker_stream_failed_rx) =
        AgentEventSender::channel(8);
    let mut terminals =
        TerminalManager::new(outbound_tx.clone(), terminal_stream_tx, activity.clone());
    let mut files = FileManager::new(outbound_tx.clone(), binary_tx, activity.clone());
    let mut desktops = DesktopManager::new(config.clone(), activity.clone(), outbound_tx.clone());
    let mut docker = DockerManager::new(
        config,
        outbound_tx.clone(),
        docker_stream_tx,
        activity.clone(),
    );
    let mut draining_updates = activity.subscribe_draining();
    docker.start_probe();
    let mut report_interval =
        tokio::time::interval(Duration::from_secs(config.report_interval.max(1)));
    report_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut manifest_interval = tokio::time::interval(MANIFEST_INTERVAL);
    manifest_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut docker_probe_interval = tokio::time::interval(MANIFEST_INTERVAL);
    docker_probe_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let (update_event_tx, mut update_event_rx) = mpsc::channel(1);
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
            let metric_sample_in_flight = metrics_sampler.sample_in_flight();
            tokio::select! {
            _ = shutdown_requested(shutdown) => {
                break SocketOutcome::Shutdown;
            }
            _ = outbound_failed(&mut outbound_failed_rx) => {
                crate::logging::error(format_args!(
                    "agent event queue overflowed or closed; reconnecting websocket"
                ));
                break SocketOutcome::Disconnected;
            }
            _ = outbound_failed(&mut docker_stream_failed_rx) => {
                crate::logging::error(format_args!(
                    "docker stream queue overflowed or closed; reconnecting websocket"
                ));
                break SocketOutcome::Disconnected;
            }
            _ = outbound_failed(&mut terminal_stream_failed_rx) => {
                crate::logging::error(format_args!(
                    "terminal stream queue closed; reconnecting websocket"
                ));
                break SocketOutcome::Disconnected;
            }
            changed = draining_updates.changed() => {
                if changed.is_ok() && *draining_updates.borrow_and_update() {
                    docker.close_streams_for_update();
                }
            }
            _ = report_interval.tick() => {
                metrics_sampler.request_sample();
            }
            event = metrics_sampler.next_event(), if metric_sample_in_flight => {
                match event {
                    MetricsSampleEvent::Ready(metrics) => {
                        let profile = host_profile();
                        outbound_tx.send(AgentInbound::Metrics {
                            hostname: profile.hostname,
                            os: profile.os,
                            arch: profile.arch,
                            agent_version: profile.agent_version,
                            package_type: capability.package_type.clone(),
                            native_arch: capability.native_arch.clone(),
                            update_privileged: Some(capability.update_privileged),
                            rollback_supported: Some(true),
                            rollback_version: update_manager
                                .as_ref()
                                .and_then(UpdateManager::rollback_version),
                            docker_status: docker.status(),
                            metrics,
                        })?;
                    }
                    MetricsSampleEvent::TimedOut => crate::logging::error(format_args!(
                        "metric collection timed out; websocket processing will continue"
                    )),
                    MetricsSampleEvent::Discarded => crate::logging::error(format_args!(
                        "discarded metric sample that completed after its deadline"
                    )),
                    MetricsSampleEvent::WorkerStopped => crate::logging::error(format_args!(
                        "metrics worker stopped; metric reporting is unavailable"
                    )),
                }
            }
            _ = docker_probe_interval.tick() => {
                docker.start_probe();
            }
            _ = manifest_interval.tick(), if update_manager.is_some() && active_update.is_none() => {
                let manager = update_manager.as_ref().expect("guarded by update_manager.is_some()");
                match manager.fetch_manifest().await {
                    Ok((update, rollback)) => {
                        if let Some(offer) = rollback {
                            match manager.can_start_rollback(&offer) {
                                Ok(true) => {
                                    active_update = Some(spawn_rollback_task(
                                        manager.clone(),
                                        offer,
                                        outbound_tx.clone(),
                                        update_event_tx.clone(),
                                    ));
                                }
                                Ok(false) => {}
                                Err(error) => crate::logging::error(format_args!(
                                    "failed to inspect local update state before rollback offer: {error:#}"
                                )),
                            }
                        } else if let Some(offer) = update {
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
                    }
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
                    UpdateTaskEvent::ReadyToApply { operation_id, offer } => {
                        if active_update
                            .as_ref()
                            .is_some_and(|active| active.operation_id == operation_id)
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
                    UpdateTaskEvent::Finished { operation_id } => {
                        if active_update
                            .as_ref()
                            .is_some_and(|active| active.operation_id == operation_id)
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
                send_with_deadline(
                    SOCKET_SEND_TIMEOUT,
                    write.send(Message::Text(payload.into())),
                )
                .await
                .context("failed to send agent websocket text event")?;
            }
            outbound = terminal_stream_rx.recv() => {
                let Some(outbound) = outbound else {
                    break SocketOutcome::Disconnected;
                };
                let payload = serde_json::to_string(&outbound)?;
                send_with_deadline(
                    SOCKET_SEND_TIMEOUT,
                    write.send(Message::Text(payload.into())),
                )
                .await
                .context("failed to send agent websocket terminal stream event")?;
            }
            outbound = docker_stream_rx.recv() => {
                let Some(outbound) = outbound else {
                    break SocketOutcome::Disconnected;
                };
                let payload = serde_json::to_string(&outbound)?;
                send_with_deadline(
                    SOCKET_SEND_TIMEOUT,
                    write.send(Message::Text(payload.into())),
                )
                .await
                .context("failed to send agent websocket stream event")?;
            }
            binary = binary_rx.recv() => {
                let Some(binary) = binary else {
                    break SocketOutcome::Disconnected;
                };
                send_with_deadline(
                    SOCKET_SEND_TIMEOUT,
                    write.send(Message::Binary(binary.into())),
                )
                .await
                .context("failed to send agent websocket binary event")?;
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
                                dispatch_command_job(
                                    job_id,
                                    command,
                                    &command_slots,
                                    &activity,
                                    &outbound_tx,
                                );
                            }
                            AgentOutbound::TerminalShellsRequest { request_id } => {
                                outbound_tx.send(AgentInbound::TerminalShellsResponse {
                                    request_id,
                                    shells: available_shells(),
                                })?;
                            }
                            AgentOutbound::TerminalOpen {
                                session_id,
                                shell,
                                cols,
                                rows,
                            } => {
                                terminals.open(session_id, shell, cols, rows);
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
                            AgentOutbound::DockerRequest { request_id, request } => {
                                docker.handle_request(request_id, request);
                            }
                            AgentOutbound::DockerCancel { request_id } => {
                                docker.cancel(&request_id);
                            }
                            AgentOutbound::DockerLogStart {
                                request_id,
                                container,
                                tail,
                                follow,
                                since,
                            } => {
                                docker.start_logs(
                                    request_id,
                                    container,
                                    tail,
                                    follow,
                                    since,
                                );
                            }
                            AgentOutbound::DockerLogCancel { request_id } => {
                                docker.cancel_logs(&request_id);
                            }
                            AgentOutbound::DockerExecOpen {
                                session_id,
                                container,
                                shell,
                                cols,
                                rows,
                            } => {
                                docker.exec_open(session_id, container, shell, cols, rows);
                            }
                            AgentOutbound::DockerExecInput { session_id, data } => {
                                docker.exec_input(&session_id, &data);
                            }
                            AgentOutbound::DockerExecResize { session_id, cols, rows } => {
                                docker.exec_resize(&session_id, cols, rows);
                            }
                            AgentOutbound::DockerExecClose { session_id } => {
                                docker.exec_close(&session_id);
                            }
                            AgentOutbound::UpdateAvailable {
                                attempt_id,
                                instance_id,
                                release_id,
                                version,
                                artifact_id,
                                download_url,
                                sha256,
                                size_bytes,
                                package_type,
                                native_arch,
                                target_os,
                                signature_key_id,
                                signature,
                                signature_v2,
                                retry_count,
                            } => {
                                let offer = UpdateOffer {
                                    attempt_id,
                                    instance_id,
                                    release_id,
                                    version,
                                    artifact_id,
                                    download_url,
                                    sha256,
                                    size_bytes,
                                    package_type,
                                    native_arch,
                                    target_os,
                                    signature_key_id,
                                    signature,
                                    signature_v2,
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
                                            attempt_id: offer.attempt_id,
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
                            AgentOutbound::RollbackAvailable { offer } => {
                                if active_update.is_none() {
                                    if let Some(manager) = &update_manager {
                                        match manager.can_start_rollback(&offer) {
                                            Ok(true) => {
                                                active_update = Some(spawn_rollback_task(
                                                    manager.clone(),
                                                    offer,
                                                    outbound_tx.clone(),
                                                    update_event_tx.clone(),
                                                ));
                                            }
                                            Ok(false) => crate::logging::info(format_args!(
                                                "ignored duplicate rollback offer {}",
                                                offer.attempt_id
                                            )),
                                            Err(error) => crate::logging::error(format_args!(
                                                "failed to inspect local update state before websocket rollback offer: {error:#}"
                                            )),
                                        }
                                    } else {
                                        let _ = outbound_tx.send(AgentInbound::RollbackStatus {
                                            attempt_id: offer.attempt_id,
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
                        send_with_deadline(
                            SOCKET_SEND_TIMEOUT,
                            write.send(Message::Pong(data)),
                        )
                        .await
                        .context("failed to send agent websocket pong")?;
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
    docker.close_all();
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

fn dispatch_command_job(
    job_id: String,
    command: String,
    command_slots: &Arc<Semaphore>,
    activity: &ActivityTracker,
    outbound: &AgentEventSender,
) {
    let Ok(command_slot) = command_slots.clone().try_acquire_owned() else {
        let _ = outbound.send(AgentInbound::CommandResult {
            job_id,
            exit_code: -1,
            output: format!(
                "command rejected because the concurrent command limit ({MAX_CONCURRENT_COMMANDS}) was reached"
            ),
        });
        return;
    };

    crate::logging::info(format_args!("running command job {job_id}"));
    let command_outbound = outbound.clone();
    let output_outbound = outbound.clone();
    let output_job_id = job_id.clone();
    let command_activity = activity.clone();
    tokio::spawn(async move {
        let _command_slot = command_slot;
        let (exit_code, output) =
            execute_tracked_command(&command, &command_activity, move |output| {
                let _ = output_outbound.send(AgentInbound::CommandOutput {
                    job_id: output_job_id.clone(),
                    output,
                });
            })
            .await;
        let _ = command_outbound.send(AgentInbound::CommandResult {
            job_id,
            exit_code,
            output,
        });
    });
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

async fn outbound_failed(failed: &mut watch::Receiver<bool>) {
    if *failed.borrow() {
        return;
    }
    while failed.changed().await.is_ok() {
        if *failed.borrow_and_update() {
            return;
        }
    }
}

async fn send_with_deadline<F, E>(timeout: Duration, send: F) -> Result<()>
where
    F: Future<Output = Result<(), E>>,
    E: std::error::Error + Send + Sync + 'static,
{
    tokio::time::timeout(timeout, send)
        .await
        .context("agent websocket send timed out")?
        .map_err(anyhow::Error::new)
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
    outbound: AgentEventSender,
    events: mpsc::Sender<UpdateTaskEvent>,
) -> ActiveUpdate {
    let operation_id = offer
        .attempt_id
        .clone()
        .unwrap_or_else(|| offer.artifact_id.clone());
    let task_operation_id = operation_id.clone();
    let task_manager = manager.clone();
    let task = tokio::spawn(async move {
        let result = task_manager.prepare(offer, outbound).await;
        let event = match result {
            PrepareResult::ReadyToApply { offer } => UpdateTaskEvent::ReadyToApply {
                operation_id: task_operation_id.clone(),
                offer,
            },
            PrepareResult::Finished => UpdateTaskEvent::Finished {
                operation_id: task_operation_id,
            },
        };
        let _ = events.send(event).await;
    });
    ActiveUpdate {
        operation_id,
        task,
        manager,
    }
}

fn spawn_rollback_task(
    manager: UpdateManager,
    offer: RollbackOffer,
    outbound: AgentEventSender,
    events: mpsc::Sender<UpdateTaskEvent>,
) -> ActiveUpdate {
    let operation_id = offer.attempt_id.clone();
    let task_operation_id = operation_id.clone();
    let task_manager = manager.clone();
    let task = tokio::spawn(async move {
        let result = task_manager.prepare_rollback(offer, outbound).await;
        let event = match result {
            PrepareResult::ReadyToApply { offer } => UpdateTaskEvent::ReadyToApply {
                operation_id: task_operation_id.clone(),
                offer,
            },
            PrepareResult::Finished => UpdateTaskEvent::Finished {
                operation_id: task_operation_id,
            },
        };
        let _ = events.send(event).await;
    });
    ActiveUpdate {
        operation_id,
        task,
        manager,
    }
}

fn websocket_request(
    server: &str,
    identity: &Identity,
) -> Result<tokio_tungstenite::tungstenite::http::Request<()>> {
    let endpoint = ServerEndpoint::parse(server)?;
    let mut capabilities = vec![
        FILE_MANAGER_CAPABILITY,
        ROLLBACK_CAPABILITY,
        TERMINAL_SHELLS_CAPABILITY,
    ];
    if cfg!(windows) {
        capabilities.push(DESKTOP_CAPABILITY);
    }
    if crate::docker::supported() {
        capabilities.push(DOCKER_CAPABILITY);
    }
    let capabilities = capabilities.join(",");
    let mut url = endpoint.websocket_url("api/agent/ws")?;
    url.query_pairs_mut()
        .append_pair("instance_id", &identity.instance_id)
        .append_pair("capabilities", &capabilities);
    let mut request = url
        .as_str()
        .into_client_request()
        .context("invalid agent websocket URL")?;
    request.headers_mut().insert(
        "authorization",
        HeaderValue::from_str(&format!("Bearer {}", identity.secret))
            .context("invalid agent authentication secret")?,
    );
    Ok(request)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn websocket_request_keeps_secret_out_of_url_and_advertises_capabilities() {
        let request = websocket_request(
            "https://monitor.example/",
            &Identity {
                instance_id: "instance-1".to_string(),
                secret: "secret-1".to_string(),
                credential_version: 1,
                previous_secret: None,
            },
        )
        .unwrap();
        let url = request.uri().to_string();
        assert!(url.starts_with(
            "wss://monitor.example/api/agent/ws?instance_id=instance-1&capabilities=file_manager_v1"
        ));
        assert!(!url.contains("secret-1"));
        assert_eq!(
            request.headers().get("authorization").unwrap(),
            "Bearer secret-1"
        );
        assert!(url.contains(ROLLBACK_CAPABILITY));
        assert!(url.contains(TERMINAL_SHELLS_CAPABILITY));
        assert_eq!(url.contains(DESKTOP_CAPABILITY), cfg!(windows));
        assert_eq!(url.contains(DOCKER_CAPABILITY), cfg!(target_os = "linux"));
    }

    #[test]
    fn websocket_request_supports_http_and_encodes_query_values() {
        let request = websocket_request(
            "HTTP://monitor.example/prefix/",
            &Identity {
                instance_id: "instance with spaces".to_string(),
                secret: "secret-1".to_string(),
                credential_version: 1,
                previous_secret: None,
            },
        )
        .unwrap();

        assert!(request.uri().to_string().starts_with(
            "ws://monitor.example/prefix/api/agent/ws?instance_id=instance+with+spaces"
        ));
    }

    #[test]
    fn authenticated_http_redirects_stay_on_the_original_origin() {
        let origin = Url::parse("https://monitor.example/api/agent/update/manifest").unwrap();
        assert!(same_origin(
            &origin,
            &Url::parse("https://monitor.example:443/other").unwrap()
        ));
        assert!(!same_origin(
            &origin,
            &Url::parse("https://downloads.example/package").unwrap()
        ));
        assert!(!same_origin(
            &origin,
            &Url::parse("https://monitor.example:8443/package").unwrap()
        ));
        assert!(!same_origin(
            &origin,
            &Url::parse("http://monitor.example/package").unwrap()
        ));
    }

    #[tokio::test]
    async fn event_queue_overflow_wakes_the_socket_disconnect_waiter() {
        let (outbound, _inbound, mut failed) = AgentEventSender::channel(1);
        outbound.send(AgentInbound::Pong { now: 1 }).unwrap();
        assert!(outbound.send(AgentInbound::Pong { now: 2 }).is_err());

        tokio::time::timeout(Duration::from_millis(100), outbound_failed(&mut failed))
            .await
            .expect("overflow notification should wake the socket loop");
    }

    #[tokio::test]
    async fn websocket_send_deadline_rejects_a_stalled_write() {
        let stalled = std::future::pending::<std::io::Result<()>>();

        assert!(
            send_with_deadline(Duration::from_millis(1), stalled)
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn command_dispatch_rejects_work_when_all_slots_are_in_use() {
        let (outbound, mut inbound, _failed) = AgentEventSender::channel(1);
        dispatch_command_job(
            "job-overflow".to_string(),
            "true".to_string(),
            &Arc::new(Semaphore::new(0)),
            &ActivityTracker::default(),
            &outbound,
        );

        assert!(matches!(
            inbound.recv().await,
            Some(AgentInbound::CommandResult {
                job_id,
                exit_code: -1,
                output,
            }) if job_id == "job-overflow" && output.contains("concurrent command limit")
        ));
    }
}
