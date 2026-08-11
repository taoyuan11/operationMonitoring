use std::{
    sync::Arc,
    time::{Duration, Instant},
};

use axum::{
    extract::{
        ConnectInfo, Path, Query, State,
        ws::{Message, WebSocket, WebSocketUpgrade},
    },
    http::{HeaderMap, StatusCode, header},
    response::Response,
};
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use futures_util::{SinkExt, StreamExt};
use rand::{RngCore, rngs::OsRng};
use serde::Deserialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;
use tokio::sync::{broadcast, mpsc, oneshot, watch};
use tracing::{error, info, warn};
use uuid::Uuid;

use crate::{
    audit,
    auth::{AdminSessionGuard, require_admin},
    db::get_instance,
    error::{AppError, AppResult},
    models::{AgentOutbound, DesktopAgentWsQuery},
    request_security::{ensure_same_origin, request_scheme},
    state::{AppState, DesktopAudioPacket, DesktopSessionHandle},
    utils::now_ts,
};

const DESKTOP_CAPABILITY: &str = "remote_desktop_v1";
const DESKTOP_AUDIO_CAPABILITY: &str = "remote_desktop_audio_v1";
const AUDIO_CODEC_OPUS: &str = "opus";
const TOKEN_TTL_SECONDS: i64 = 30;
const CONTROL_MESSAGE_MAX_BYTES: usize = 16 * 1024;
const FRAME_MAX_BYTES: usize = 2 * 1024 * 1024;
const FRAME_HEADER_BYTES: usize = 32;
const AUDIO_RELAY_CAPACITY: usize = 8;
const OPUS_MAX_PACKET_BYTES: usize = 1275;
const OPUS_CHANNELS: u8 = 2;
const OPUS_SAMPLE_RATE: u32 = 48_000;
const OPUS_SAMPLES_PER_CHANNEL: u32 = 960;
const AUDIO_DISCONTINUITY_FLAG: u8 = 0x01;
const HELPER_JOIN_TIMEOUT: Duration = Duration::from_secs(15);
const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(10);
const HEARTBEAT_TIMEOUT: Duration = Duration::from_secs(30);
const SOCKET_SEND_TIMEOUT: Duration = Duration::from_secs(5);
const IDLE_TIMEOUT: Duration = Duration::from_secs(15 * 60);
const SESSION_TIMEOUT: Duration = Duration::from_secs(2 * 60 * 60);
const CONTROL_RATE_PER_SECOND: f64 = 120.0;
const CONTROL_RATE_BURST: f64 = 240.0;

struct ControlRateLimiter {
    tokens: f64,
    updated_at: Instant,
}

impl ControlRateLimiter {
    fn new(now: Instant) -> Self {
        Self {
            tokens: CONTROL_RATE_BURST,
            updated_at: now,
        }
    }

    fn allow(&mut self, now: Instant) -> bool {
        let elapsed = now.saturating_duration_since(self.updated_at);
        self.updated_at = now;
        self.tokens =
            (self.tokens + elapsed.as_secs_f64() * CONTROL_RATE_PER_SECOND).min(CONTROL_RATE_BURST);
        if self.tokens < 1.0 {
            return false;
        }
        self.tokens -= 1.0;
        true
    }
}

#[derive(Debug, Clone, Copy, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
enum DesktopQuality {
    Low,
    #[default]
    Balanced,
    High,
    Original,
}

#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
enum DesktopAudioCodec {
    Opus,
}

impl DesktopAudioCodec {
    fn as_str(self) -> &'static str {
        match self {
            Self::Opus => AUDIO_CODEC_OPUS,
        }
    }
}

#[derive(Debug, Deserialize)]
pub struct DesktopBrowserWsQuery {
    #[serde(default)]
    quality: DesktopQuality,
    #[serde(default)]
    audio: Option<DesktopAudioCodec>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct DesktopStreamSettings {
    max_width: u32,
    max_height: u32,
    min_fps: u8,
    max_fps: u8,
    jpeg_quality: u8,
}

impl DesktopQuality {
    fn stream_settings(self) -> DesktopStreamSettings {
        match self {
            Self::Low => DesktopStreamSettings {
                max_width: 960,
                max_height: 540,
                min_fps: 4,
                max_fps: 6,
                jpeg_quality: 35,
            },
            Self::Balanced => DesktopStreamSettings {
                max_width: 1280,
                max_height: 720,
                min_fps: 6,
                max_fps: 8,
                jpeg_quality: 50,
            },
            Self::High => DesktopStreamSettings {
                max_width: 1600,
                max_height: 900,
                min_fps: 8,
                max_fps: 10,
                jpeg_quality: 60,
            },
            Self::Original => DesktopStreamSettings {
                max_width: 1920,
                max_height: 1080,
                min_fps: 8,
                max_fps: 12,
                jpeg_quality: 70,
            },
        }
    }
}

pub async fn admin_desktop_ws(
    State(state): State<AppState>,
    ConnectInfo(peer_addr): ConnectInfo<std::net::SocketAddr>,
    headers: HeaderMap,
    Path(instance_id): Path<String>,
    Query(query): Query<DesktopBrowserWsQuery>,
    ws: WebSocketUpgrade,
) -> AppResult<Response> {
    ensure_same_origin(&headers, request_scheme(&state, &headers, peer_addr.ip())?)?;
    let admin = require_admin(&state, &headers).await?;
    let instance = get_instance(&state.db, &instance_id).await?;
    if instance.disabled == 1 {
        return Err(AppError::new(StatusCode::FORBIDDEN, "实例已停用"));
    }
    if !instance.os.to_ascii_lowercase().contains("windows") {
        return Err(AppError::new(
            StatusCode::BAD_REQUEST,
            "远程桌面仅支持 Windows 实例",
        ));
    }
    let agent = state.agents.read().await.get(&instance_id).cloned();
    let Some(agent) = agent else {
        return Err(AppError::new(StatusCode::CONFLICT, "实例不在线"));
    };
    if !agent
        .capabilities
        .iter()
        .any(|value| value == DESKTOP_CAPABILITY)
    {
        return Err(AppError::new(
            StatusCode::BAD_REQUEST,
            "实例 Agent 不支持远程桌面",
        ));
    }

    let session_guard = admin.session_guard();
    let user_id = admin.user_id.clone();
    let audit_context = audit::AuditContext::from_headers(&headers);
    Ok(ws
        .max_message_size(CONTROL_MESSAGE_MAX_BYTES)
        .max_frame_size(CONTROL_MESSAGE_MAX_BYTES)
        .on_upgrade(move |socket| {
            desktop_browser_socket(
                state,
                instance_id,
                admin.username,
                user_id,
                audit_context,
                session_guard,
                query.quality,
                query.audio,
                socket,
            )
        }))
}

pub async fn agent_desktop_ws(
    State(state): State<AppState>,
    Query(query): Query<DesktopAgentWsQuery>,
    headers: HeaderMap,
    ws: WebSocketUpgrade,
) -> AppResult<Response> {
    let token = bearer_token(&headers)?;
    claim_agent_data_channel(&state, &query.session_id, token).await?;
    Ok(ws
        .max_message_size(FRAME_MAX_BYTES)
        .max_frame_size(FRAME_MAX_BYTES)
        .on_upgrade(move |socket| desktop_agent_socket(state, query.session_id, socket)))
}

async fn desktop_browser_socket(
    state: AppState,
    instance_id: String,
    actor: String,
    user_id: String,
    audit_context: audit::AuditContext,
    session_guard: AdminSessionGuard,
    quality: DesktopQuality,
    requested_audio_codec: Option<DesktopAudioCodec>,
    socket: WebSocket,
) {
    let Some(agent) = state.agents.read().await.get(&instance_id).cloned() else {
        send_single_message(socket, server_error("offline", "实例不在线")).await;
        return;
    };
    if !agent
        .capabilities
        .iter()
        .any(|value| value == DESKTOP_CAPABILITY)
    {
        send_single_message(
            socket,
            server_error("unsupported", "实例 Agent 不支持远程桌面"),
        )
        .await;
        return;
    }
    let audio_codec = negotiate_audio_codec(requested_audio_codec, &agent.capabilities);

    let session_id = Uuid::new_v4().to_string();
    let (stream_token, token_hash) = new_stream_token();
    let (browser_tx, mut browser_rx) = mpsc::channel::<String>(32);
    let (frame_tx, mut frame_rx) = watch::channel::<Option<Arc<Vec<u8>>>>(None);
    let (audio_tx, mut audio_rx) = broadcast::channel::<DesktopAudioPacket>(AUDIO_RELAY_CAPACITY);
    let (agent_input_tx, agent_input_rx) = mpsc::channel::<String>(64);
    let (close_tx, mut close_rx) = watch::channel::<Option<String>>(None);

    {
        let mut sessions = state.desktop_sessions.write().await;
        if sessions
            .values()
            .any(|session| session.instance_id == instance_id)
        {
            drop(sessions);
            send_single_message(
                socket,
                server_error("desktop_busy", "该实例已有远程桌面会话"),
            )
            .await;
            return;
        }
        sessions.insert(
            session_id.clone(),
            DesktopSessionHandle {
                instance_id: instance_id.clone(),
                agent_connection_id: agent.connection_id,
                token_hash,
                token_expires_at: now_ts() + TOKEN_TTL_SECONDS,
                token_claimed: false,
                browser_tx,
                frame_tx,
                audio_tx,
                audio_codec: audio_codec.map(|codec| codec.as_str().to_string()),
                agent_input_rx: Arc::new(tokio::sync::Mutex::new(Some(agent_input_rx))),
                close_tx,
            },
        );
    }

    if let Err(error) = audit::insert_event(
        &state.db,
        &audit::AuditEventInput {
            category: "desktop".to_string(),
            kind: "session".to_string(),
            actor: actor.clone(),
            user_id: Some(user_id),
            action: "desktop_session".to_string(),
            target: instance_id.clone(),
            detail: "启动远程桌面会话".to_string(),
            metadata: json!({
                "quality": format!("{quality:?}").to_ascii_lowercase(),
                "audio_requested": requested_audio_codec.is_some(),
                "audio_enabled": audio_codec.is_some(),
                "audio_codec": audio_codec.map(DesktopAudioCodec::as_str),
            }),
            instance_id: Some(instance_id.clone()),
            node_snapshot: audit::instance_snapshot(&state.db, &instance_id).await,
            context: audit_context,
            session_id: Some(session_id.clone()),
            operation_id: None,
            status: "running".to_string(),
            error_code: None,
            error_reason: String::new(),
            created_at: now_ts(),
            completed_at: None,
        },
    )
    .await
    {
        error!(?error, %session_id, "failed to write desktop start action log");
        end_desktop_session(&state, &session_id, "audit_error").await;
        send_single_message(
            socket,
            server_error("audit_error", "无法记录远程桌面审计日志"),
        )
        .await;
        return;
    }

    let stream_settings = quality.stream_settings();
    if agent
        .tx
        .send(AgentOutbound::DesktopOpen {
            session_id: session_id.clone(),
            stream_token,
            audio_codec: audio_codec.map(|codec| codec.as_str().to_string()),
            max_width: stream_settings.max_width,
            max_height: stream_settings.max_height,
            min_fps: stream_settings.min_fps,
            max_fps: stream_settings.max_fps,
            jpeg_quality: stream_settings.jpeg_quality,
        })
        .is_err()
    {
        end_desktop_session(&state, &session_id, "agent_disconnected").await;
        send_single_message(socket, server_error("offline", "实例连接已断开")).await;
        return;
    }

    let (mut sender, mut receiver) = socket.split();
    if !send_text(&mut sender, &opening_message(audio_codec)).await {
        end_desktop_session(&state, &session_id, "browser_disconnected").await;
        return;
    }

    info!(%session_id, %instance_id, "desktop browser websocket connected");
    let started = Instant::now();
    let mut last_activity = started;
    let mut last_inbound = started;
    let mut control_rate = ControlRateLimiter::new(started);
    let mut joined = false;
    let mut control_ready = false;
    let mut browser_audio = BrowserAudioRelayGate::new();
    let mut helper_timeout = Box::pin(tokio::time::sleep(HELPER_JOIN_TIMEOUT));
    let mut heartbeat = tokio::time::interval(HEARTBEAT_INTERVAL);
    heartbeat.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let authorization_state = state.clone();
    let authorization_session_id = session_id.clone();
    let (authorization_cancel_tx, authorization_cancel_rx) = oneshot::channel();
    let mut authorization = tokio::spawn(async move {
        tokio::select! {
            _ = session_guard.wait_until_invalid(authorization_state.clone()) => {
                end_desktop_session(
                    &authorization_state,
                    &authorization_session_id,
                    "authorization_revoked",
                )
                .await;
                true
            }
            _ = authorization_cancel_rx => false,
        }
    });
    let mut reason = "browser_disconnected".to_string();
    let mut deferred_browser_message = None;

    loop {
        tokio::select! {
            biased;
            authorization_result = &mut authorization => {
                if !matches!(authorization_result, Ok(true)) {
                    break;
                }
                reason = "authorization_revoked".to_string();
                deferred_browser_message = Some(
                    json!({"type":"closed", "reason":"authorization_revoked"}).to_string(),
                );
                break;
            }
            changed = close_rx.changed() => {
                if changed.is_err() { break; }
                let close_reason = { close_rx.borrow_and_update().clone() };
                if let Some(close_reason) = close_reason {
                    deferred_browser_message =
                        Some(json!({"type":"closed", "reason":&close_reason}).to_string());
                    reason = close_reason;
                    break;
                }
            }
            incoming = receiver.next() => {
                let Some(incoming) = incoming else { break; };
                match incoming {
                    Ok(Message::Text(text)) => {
                        last_inbound = Instant::now();
                        match validate_browser_message(&text, audio_codec.is_some()) {
                            Ok(is_activity) => {
                                if !control_rate.allow(Instant::now()) {
                                    deferred_browser_message = Some(server_error(
                                        "control_rate_limited",
                                        "远程桌面控制消息速率过高",
                                    ));
                                    reason = "control_rate_limited".to_string();
                                    break;
                                }
                                if !control_ready && !bypasses_desktop_control_gate(&text) {
                                    continue;
                                }
                                let forwarded = if let Some(enabled) = audio_control_enabled(&text) {
                                    clear_pending_audio(&mut audio_rx);
                                    browser_audio.set_control(enabled).map(|generation| {
                                        json!({
                                            "type":"audio_control",
                                            "enabled":enabled,
                                            "generation":generation,
                                        })
                                        .to_string()
                                    })
                                } else {
                                    Some(text.to_string())
                                };
                                if is_activity { last_activity = Instant::now(); }
                                let reliable = is_reliable_browser_message(&text);
                                let delivered = match forwarded {
                                    None => true,
                                    Some(forwarded) if reliable => {
                                        tokio::time::timeout(Duration::from_secs(1), agent_input_tx.send(forwarded)).await
                                            .is_ok_and(|result| result.is_ok())
                                    }
                                    Some(forwarded) => agent_input_tx.try_send(forwarded).is_ok(),
                                };
                                if reliable && !delivered {
                                    deferred_browser_message =
                                        Some(server_error("input_queue_overflow", "远程输入队列拥塞"));
                                    reason = "input_queue_overflow".to_string();
                                    break;
                                }
                            }
                            Err((code, message)) => {
                                deferred_browser_message = Some(server_error(code, message));
                                reason = "invalid_control_message".to_string();
                                break;
                            }
                        }
                    }
                    Ok(Message::Pong(_)) => last_inbound = Instant::now(),
                    Ok(Message::Ping(data)) => {
                        last_inbound = Instant::now();
                        if !send_socket_message(&mut sender, Message::Pong(data)).await { break; }
                    }
                    Ok(Message::Close(_)) | Err(_) => break,
                    Ok(Message::Binary(_)) => {
                        deferred_browser_message =
                            Some(server_error("invalid_message", "浏览器不得发送二进制数据"));
                        reason = "invalid_control_message".to_string();
                        break;
                    }
                }
            }
            message = browser_rx.recv() => {
                let Some(message) = message else { break; };
                let kind = message_type(&message);
                if browser_audio.observe_state(&message) {
                    clear_pending_audio(&mut audio_rx);
                }
                if matches!(kind.as_deref(), Some("ready" | "consent_required")) { joined = true; }
                if stops_audio_stream(&message) {
                    clear_pending_audio(&mut audio_rx);
                }
                match kind.as_deref() {
                    Some("ready") => control_ready = true,
                    Some("consent_required" | "paused" | "closed" | "error") => control_ready = false,
                    Some("desktop_state") if message_desktop(&message).as_deref() != Some("default") => {
                        control_ready = false;
                    }
                    _ => {}
                }
                let close_reason = (kind.as_deref() == Some("closed"))
                    .then(|| message_reason(&message).unwrap_or_else(|| "agent_closed".to_string()));
                if let Some(close_reason) = close_reason {
                    deferred_browser_message = Some(message);
                    reason = close_reason;
                    break;
                }
                if !send_text(&mut sender, &message).await { break; }
            }
            audio = audio_rx.recv(), if audio_codec.is_some() => {
                match audio {
                    Ok(packet) => {
                        if !browser_audio.accepts(&packet) {
                            continue;
                        }
                        if !send_socket_message(
                            &mut sender,
                            Message::Binary(packet.frame.as_ref().clone().into()),
                        )
                        .await
                        {
                            reason = "browser_send_timeout".to_string();
                            break;
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(broadcast::error::RecvError::Closed) => break,
                }
            }
            changed = frame_rx.changed() => {
                if changed.is_err() { break; }
                let frame = frame_rx.borrow_and_update().clone();
                if let Some(frame) = frame {
                    if !send_socket_message(
                        &mut sender,
                        Message::Binary(frame.as_ref().clone().into()),
                    )
                    .await
                    {
                        reason = "browser_send_timeout".to_string();
                        break;
                    }
                }
            }
            _ = &mut helper_timeout, if !joined => {
                deferred_browser_message = Some(server_error("helper_timeout", "远程桌面启动超时"));
                reason = "helper_timeout".to_string();
                break;
            }
            _ = heartbeat.tick() => {
                let now = Instant::now();
                if now.duration_since(last_inbound) > HEARTBEAT_TIMEOUT {
                    reason = "browser_heartbeat_timeout".to_string();
                    break;
                }
                if now.duration_since(last_activity) >= IDLE_TIMEOUT {
                    deferred_browser_message =
                        Some(json!({"type":"closed", "reason":"idle_timeout"}).to_string());
                    reason = "idle_timeout".to_string();
                    break;
                }
                if now.duration_since(started) >= SESSION_TIMEOUT {
                    deferred_browser_message =
                        Some(json!({"type":"closed", "reason":"session_timeout"}).to_string());
                    reason = "session_timeout".to_string();
                    break;
                }
                if !send_socket_message(&mut sender, Message::Ping(Vec::new().into())).await {
                    reason = "browser_send_timeout".to_string();
                    break;
                }
            }
        }
    }

    let _ = authorization_cancel_tx.send(());
    end_desktop_session(&state, &session_id, &reason).await;
    if let Some(message) = deferred_browser_message {
        let _ = send_text(&mut sender, &message).await;
    }
    info!(%session_id, %instance_id, %reason, "desktop browser websocket disconnected");
}

async fn desktop_agent_socket(state: AppState, session_id: String, socket: WebSocket) {
    let handle = state
        .desktop_sessions
        .read()
        .await
        .get(&session_id)
        .cloned();
    let Some(handle) = handle else {
        close_socket(socket).await;
        return;
    };
    let Some(mut input_rx) = handle.agent_input_rx.lock().await.take() else {
        close_socket(socket).await;
        end_desktop_session(&state, &session_id, "duplicate_agent_data_channel").await;
        return;
    };
    let mut close_rx = handle.close_tx.subscribe();

    let (mut sender, mut receiver) = socket.split();
    let mut heartbeat = tokio::time::interval(HEARTBEAT_INTERVAL);
    heartbeat.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut last_inbound = Instant::now();
    let mut reason = "agent_data_disconnected".to_string();
    let audio_negotiated = handle.audio_codec.as_deref() == Some(AUDIO_CODEC_OPUS);
    let mut audio_relay = AudioRelayGate::new();
    info!(%session_id, instance_id = %handle.instance_id, "desktop agent data websocket connected");

    loop {
        tokio::select! {
            incoming = receiver.next() => {
                let Some(incoming) = incoming else { break; };
                match incoming {
                    Ok(Message::Text(text)) => {
                        last_inbound = Instant::now();
                        match validate_agent_message(&text, handle.audio_codec.is_some()) {
                            Ok(message) => {
                                audio_relay.observe_state(&message);
                                let close_reason = (message_type(&message).as_deref() == Some("closed"))
                                    .then(|| message_reason(&message).unwrap_or_else(|| "agent_closed".to_string()));
                                let delivered = tokio::time::timeout(
                                    Duration::from_secs(1),
                                    handle.browser_tx.send(message),
                                )
                                .await
                                .is_ok_and(|result| result.is_ok());
                                if !delivered {
                                    reason = "browser_control_queue_overflow".to_string();
                                    break;
                                }
                                if let Some(close_reason) = close_reason {
                                    reason = close_reason;
                                    break;
                                }
                            }
                            Err((code, message)) => {
                                try_send_browser_error(&handle.browser_tx, code, message);
                                reason = "invalid_agent_message".to_string();
                                break;
                            }
                        }
                    }
                    Ok(Message::Binary(frame)) => {
                        last_inbound = Instant::now();
                        match classify_agent_binary_frame(&frame, audio_negotiated) {
                            Ok(AgentBinaryFrameDisposition::Video) => {
                                handle.frame_tx.send_replace(Some(Arc::new(frame.to_vec())));
                            }
                            Ok(AgentBinaryFrameDisposition::Audio) if audio_relay.accepts_audio(&frame) => {
                                let _ = handle.audio_tx.send(DesktopAudioPacket {
                                    generation: audio_relay.generation,
                                    frame: Arc::new(frame.to_vec()),
                                });
                            }
                            Ok(AgentBinaryFrameDisposition::Audio) => {}
                            Err((code, message)) => {
                                try_send_browser_error(&handle.browser_tx, code, message);
                                reason = "invalid_frame".to_string();
                                break;
                            }
                        }
                    }
                    Ok(Message::Pong(_)) => last_inbound = Instant::now(),
                    Ok(Message::Ping(data)) => {
                        last_inbound = Instant::now();
                        if !send_socket_message(&mut sender, Message::Pong(data)).await { break; }
                    }
                    Ok(Message::Close(_)) | Err(_) => break,
                }
            }
            input = input_rx.recv() => {
                let Some(input) = input else { break; };
                if let (Some(enabled), Some(generation)) = (
                    audio_control_enabled(&input),
                    audio_control_generation(&input),
                ) {
                    audio_relay.set_control(generation, enabled);
                }
                if !send_socket_message(&mut sender, Message::Text(input.into())).await {
                    reason = "agent_send_timeout".to_string();
                    break;
                }
            }
            changed = close_rx.changed() => {
                if changed.is_err() { break; }
                let close_reason = { close_rx.borrow_and_update().clone() };
                if let Some(close_reason) = close_reason {
                    let _ = send_socket_message(&mut sender, Message::Close(None)).await;
                    reason = close_reason;
                    break;
                }
            }
            _ = heartbeat.tick() => {
                if last_inbound.elapsed() > HEARTBEAT_TIMEOUT {
                    reason = "agent_heartbeat_timeout".to_string();
                    break;
                }
                if !send_socket_message(&mut sender, Message::Ping(Vec::new().into())).await {
                    reason = "agent_send_timeout".to_string();
                    break;
                }
            }
        }
    }

    end_desktop_session(&state, &session_id, &reason).await;
    info!(%session_id, %reason, "desktop agent data websocket disconnected");
}

async fn claim_agent_data_channel(
    state: &AppState,
    session_id: &str,
    token: &str,
) -> AppResult<()> {
    let token_hash: [u8; 32] = Sha256::digest(token.as_bytes()).into();
    let mut sessions = state.desktop_sessions.write().await;
    let Some(session) = sessions.get_mut(session_id) else {
        return Err(AppError::new(StatusCode::UNAUTHORIZED, "远程桌面令牌无效"));
    };
    if session.token_claimed
        || session.token_expires_at <= now_ts()
        || session.token_hash.ct_eq(&token_hash).unwrap_u8() != 1
    {
        return Err(AppError::new(StatusCode::UNAUTHORIZED, "远程桌面令牌无效"));
    }
    let current_connection = state
        .agents
        .read()
        .await
        .get(&session.instance_id)
        .map(|agent| agent.connection_id);
    if current_connection != Some(session.agent_connection_id) {
        return Err(AppError::new(StatusCode::UNAUTHORIZED, "实例连接已变更"));
    }
    session.token_claimed = true;
    Ok(())
}

pub async fn desktop_agent_opened(
    state: &AppState,
    instance_id: &str,
    connection_id: Uuid,
    session_id: &str,
) {
    let valid = state
        .desktop_sessions
        .read()
        .await
        .get(session_id)
        .is_some_and(|session| {
            session.instance_id == instance_id && session.agent_connection_id == connection_id
        });
    if !valid {
        warn!(%instance_id, %session_id, "ignored desktop_opened for unknown session");
    }
}

pub async fn desktop_agent_closed(
    state: &AppState,
    instance_id: &str,
    connection_id: Uuid,
    session_id: &str,
    reason: &str,
) {
    let valid = state
        .desktop_sessions
        .read()
        .await
        .get(session_id)
        .is_some_and(|session| {
            session.instance_id == instance_id && session.agent_connection_id == connection_id
        });
    if valid {
        end_desktop_session(state, session_id, reason).await;
    }
}

pub async fn close_connection_desktops(state: &AppState, instance_id: &str, connection_id: Uuid) {
    let session_ids = state
        .desktop_sessions
        .read()
        .await
        .iter()
        .filter(|(_, session)| {
            session.instance_id == instance_id && session.agent_connection_id == connection_id
        })
        .map(|(session_id, _)| session_id.clone())
        .collect::<Vec<_>>();
    for session_id in session_ids {
        end_desktop_session(state, &session_id, "agent_disconnected").await;
    }
}

async fn end_desktop_session(state: &AppState, session_id: &str, reason: &str) {
    let handle = state.desktop_sessions.write().await.remove(session_id);
    let Some(handle) = handle else {
        return;
    };
    let reason = sanitize_reason(reason);
    handle.close_tx.send_replace(Some(reason.clone()));
    if let Some(agent) = state.agents.read().await.get(&handle.instance_id)
        && agent.connection_id == handle.agent_connection_id
    {
        let _ = agent.tx.send(AgentOutbound::DesktopClose {
            session_id: session_id.to_string(),
            reason: reason.clone(),
        });
    }
    let status = if matches!(reason.as_str(), "client_closed" | "agent_closed" | "") {
        "success"
    } else {
        "failed"
    };
    let _ = audit::finish_session_event(
        &state.db,
        session_id,
        status,
        (status == "failed").then_some(reason.as_str()),
        &reason,
    )
    .await;
}

fn bearer_token(headers: &HeaderMap) -> AppResult<&str> {
    let authorization = headers
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .ok_or_else(|| AppError::new(StatusCode::UNAUTHORIZED, "缺少远程桌面令牌"))?;
    authorization
        .strip_prefix("Bearer ")
        .filter(|token| !token.is_empty() && token.len() <= 256)
        .ok_or_else(|| AppError::new(StatusCode::UNAUTHORIZED, "远程桌面令牌无效"))
}

fn new_stream_token() -> (String, [u8; 32]) {
    let mut bytes = [0_u8; 32];
    OsRng.fill_bytes(&mut bytes);
    let token = URL_SAFE_NO_PAD.encode(bytes);
    let hash = Sha256::digest(token.as_bytes()).into();
    (token, hash)
}

fn negotiate_audio_codec(
    requested: Option<DesktopAudioCodec>,
    capabilities: &[String],
) -> Option<DesktopAudioCodec> {
    requested.filter(|_| {
        capabilities
            .iter()
            .any(|value| value == DESKTOP_AUDIO_CAPABILITY)
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DesktopBinaryFrameKind {
    Video,
    Audio,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AgentBinaryFrameDisposition {
    Video,
    Audio,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct BrowserAudioRelayGate {
    generation: u64,
    enabled: bool,
    acknowledged: bool,
}

impl BrowserAudioRelayGate {
    fn new() -> Self {
        Self {
            generation: 0,
            enabled: false,
            acknowledged: false,
        }
    }

    fn set_control(&mut self, enabled: bool) -> Option<u64> {
        if self.enabled == enabled {
            return None;
        }
        self.generation = self.generation.wrapping_add(1).max(1);
        self.enabled = enabled;
        self.acknowledged = false;
        Some(self.generation)
    }

    /// Returns true when the current enable command has just been acknowledged.
    fn observe_state(&mut self, message: &str) -> bool {
        if !self.enabled || self.acknowledged {
            return false;
        }
        let Ok(value) = serde_json::from_str::<Value>(message) else {
            return false;
        };
        if value.get("type").and_then(Value::as_str) == Some("audio_state")
            && value.get("state").and_then(Value::as_str) == Some("starting")
            && value.get("reason").and_then(Value::as_str) == Some("control_ack")
            && value.get("generation").and_then(Value::as_u64) == Some(self.generation)
        {
            self.acknowledged = true;
            return true;
        }
        false
    }

    fn accepts(&self, packet: &DesktopAudioPacket) -> bool {
        self.enabled && self.acknowledged && packet.generation == self.generation
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AudioRelayPhase {
    Disabled,
    AwaitingControlAck,
    AwaitingDiscontinuity,
    Playing,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct AudioRelayGate {
    generation: u64,
    enabled: bool,
    phase: AudioRelayPhase,
}

impl AudioRelayGate {
    fn new() -> Self {
        Self {
            generation: 0,
            enabled: false,
            phase: AudioRelayPhase::Disabled,
        }
    }

    fn set_control(&mut self, generation: u64, enabled: bool) {
        self.generation = generation;
        self.enabled = enabled;
        self.phase = if enabled {
            AudioRelayPhase::AwaitingControlAck
        } else {
            AudioRelayPhase::Disabled
        };
    }

    fn observe_state(&mut self, message: &str) {
        if !self.enabled {
            return;
        }
        let Ok(value) = serde_json::from_str::<Value>(message) else {
            return;
        };
        if value.get("type").and_then(Value::as_str) != Some("audio_state") {
            return;
        }
        let state = value.get("state").and_then(Value::as_str);
        let reason = value.get("reason").and_then(Value::as_str);
        if reason == Some("control_ack") {
            if state == Some("starting")
                && value.get("generation").and_then(Value::as_u64) == Some(self.generation)
            {
                self.phase = AudioRelayPhase::AwaitingDiscontinuity;
            }
            return;
        }
        if self.phase != AudioRelayPhase::AwaitingControlAck && !matches!(state, Some("playing")) {
            self.phase = AudioRelayPhase::AwaitingDiscontinuity;
        }
    }

    fn accepts_audio(&mut self, frame: &[u8]) -> bool {
        match self.phase {
            AudioRelayPhase::Playing => true,
            AudioRelayPhase::AwaitingDiscontinuity
                if frame
                    .get(7)
                    .is_some_and(|flags| flags & AUDIO_DISCONTINUITY_FLAG != 0) =>
            {
                self.phase = AudioRelayPhase::Playing;
                true
            }
            AudioRelayPhase::Disabled
            | AudioRelayPhase::AwaitingControlAck
            | AudioRelayPhase::AwaitingDiscontinuity => false,
        }
    }
}

fn validate_binary_frame(
    frame: &[u8],
) -> Result<DesktopBinaryFrameKind, (&'static str, &'static str)> {
    match frame.get(..4) {
        Some(b"OMRD") => {
            validate_video_frame(frame)?;
            Ok(DesktopBinaryFrameKind::Video)
        }
        Some(b"OMRA") => {
            validate_audio_frame(frame)?;
            Ok(DesktopBinaryFrameKind::Audio)
        }
        _ => Err(("unsupported_frame", "远程桌面媒体帧类型不受支持")),
    }
}

fn classify_agent_binary_frame(
    frame: &[u8],
    audio_negotiated: bool,
) -> Result<AgentBinaryFrameDisposition, (&'static str, &'static str)> {
    let is_audio = frame.get(..4) == Some(b"OMRA");
    if is_audio && !audio_negotiated {
        return Err(("unexpected_audio", "当前远程桌面会话未启用音频"));
    }
    match validate_binary_frame(frame)? {
        DesktopBinaryFrameKind::Video => Ok(AgentBinaryFrameDisposition::Video),
        DesktopBinaryFrameKind::Audio => Ok(AgentBinaryFrameDisposition::Audio),
    }
}

fn validate_video_frame(frame: &[u8]) -> Result<(), (&'static str, &'static str)> {
    if frame.len() < FRAME_HEADER_BYTES || frame.len() > FRAME_MAX_BYTES {
        return Err(("invalid_frame", "桌面图像帧大小无效"));
    }
    if &frame[0..4] != b"OMRD" || frame[4] != 1 || frame[5] != 1 || frame[6] != 0 || frame[7] != 0 {
        return Err(("unsupported_frame", "桌面图像帧版本或编码不受支持"));
    }
    let width = u32::from_be_bytes(frame[24..28].try_into().expect("fixed frame width"));
    let height = u32::from_be_bytes(frame[28..32].try_into().expect("fixed frame height"));
    if width == 0
        || height == 0
        || width > 1920
        || height > 1080
        || frame[32..].len() < 2
        || frame[32] != 0xff
        || frame[33] != 0xd8
    {
        return Err(("invalid_frame", "桌面图像帧元数据无效"));
    }
    Ok(())
}

fn validate_audio_frame(frame: &[u8]) -> Result<(), (&'static str, &'static str)> {
    if frame.len() <= FRAME_HEADER_BYTES || frame.len() > FRAME_HEADER_BYTES + OPUS_MAX_PACKET_BYTES
    {
        return Err(("invalid_audio_frame", "桌面音频帧大小无效"));
    }
    if frame[4] != 1 || frame[5] != 1 {
        return Err(("unsupported_audio_frame", "桌面音频帧版本或编码不受支持"));
    }
    if frame[6] != OPUS_CHANNELS || frame[7] & !AUDIO_DISCONTINUITY_FLAG != 0 {
        return Err(("invalid_audio_frame", "桌面音频帧声道或标志无效"));
    }
    let sample_rate =
        u32::from_be_bytes(frame[24..28].try_into().expect("fixed audio sample rate"));
    let samples_per_channel =
        u32::from_be_bytes(frame[28..32].try_into().expect("fixed audio sample count"));
    if sample_rate != OPUS_SAMPLE_RATE || samples_per_channel != OPUS_SAMPLES_PER_CHANNEL {
        return Err(("invalid_audio_frame", "桌面音频帧采样参数无效"));
    }
    if !valid_opus_packet(&frame[FRAME_HEADER_BYTES..]) {
        return Err(("invalid_audio_frame", "桌面音频 Opus 包结构无效"));
    }
    Ok(())
}

fn valid_opus_packet(packet: &[u8]) -> bool {
    let Some(&toc) = packet.first() else {
        return false;
    };
    let frame_samples = opus_frame_samples_48k(toc);
    match toc & 0x03 {
        0 => frame_samples == OPUS_SAMPLES_PER_CHANNEL as usize,
        1 => {
            frame_samples * 2 == OPUS_SAMPLES_PER_CHANNEL as usize
                && (packet.len() - 1).is_multiple_of(2)
        }
        2 => {
            if frame_samples * 2 != OPUS_SAMPLES_PER_CHANNEL as usize {
                return false;
            }
            let mut cursor = 1;
            let Some(first_len) = opus_frame_length(packet, &mut cursor) else {
                return false;
            };
            let remaining = packet.len() - cursor;
            first_len <= remaining && remaining - first_len <= OPUS_MAX_PACKET_BYTES
        }
        3 => valid_opus_code_three_packet(packet, toc),
        _ => unreachable!(),
    }
}

fn valid_opus_code_three_packet(packet: &[u8], toc: u8) -> bool {
    let Some(&frame_control) = packet.get(1) else {
        return false;
    };
    let frame_count = usize::from(frame_control & 0x3f);
    if frame_count == 0
        || frame_count > 48
        || frame_count * opus_frame_samples_48k(toc) != OPUS_SAMPLES_PER_CHANNEL as usize
    {
        return false;
    }

    let variable_bitrate = frame_control & 0x80 != 0;
    let mut cursor = 2;
    let mut padding = 0_usize;
    if frame_control & 0x40 != 0 {
        loop {
            let Some(&value) = packet.get(cursor) else {
                return false;
            };
            cursor += 1;
            padding += if value == 255 {
                254
            } else {
                usize::from(value)
            };
            if value != 255 {
                break;
            }
        }
    }
    let Some(data_end) = packet.len().checked_sub(padding) else {
        return false;
    };
    if cursor > data_end {
        return false;
    }

    if !variable_bitrate {
        let frame_bytes = data_end - cursor;
        return frame_bytes.is_multiple_of(frame_count)
            && frame_bytes / frame_count <= OPUS_MAX_PACKET_BYTES;
    }

    let mut described_bytes = 0_usize;
    for _ in 1..frame_count {
        let Some(frame_len) = opus_frame_length(&packet[..data_end], &mut cursor) else {
            return false;
        };
        described_bytes += frame_len;
    }
    let remaining = data_end - cursor;
    described_bytes <= remaining && remaining - described_bytes <= OPUS_MAX_PACKET_BYTES
}

fn opus_frame_length(packet: &[u8], cursor: &mut usize) -> Option<usize> {
    let first = usize::from(*packet.get(*cursor)?);
    *cursor += 1;
    if first < 252 {
        return Some(first);
    }
    let second = usize::from(*packet.get(*cursor)?);
    *cursor += 1;
    Some(first + 4 * second)
}

fn opus_frame_samples_48k(toc: u8) -> usize {
    let config = usize::from(toc >> 3);
    match config {
        0..=11 => [480, 960, 1_920, 2_880][config % 4],
        12..=15 => [480, 960][config % 2],
        _ => [120, 240, 480, 960][config % 4],
    }
}

fn validate_browser_message(
    text: &str,
    audio_enabled: bool,
) -> Result<bool, (&'static str, &'static str)> {
    if text.len() > CONTROL_MESSAGE_MAX_BYTES {
        return Err(("message_too_large", "远程桌面控制消息过大"));
    }
    let value: Value =
        serde_json::from_str(text).map_err(|_| ("invalid_message", "远程桌面控制消息格式无效"))?;
    let kind = value
        .get("type")
        .and_then(Value::as_str)
        .ok_or(("invalid_message", "远程桌面控制消息缺少类型"))?;
    match kind {
        "pointer_move" => {
            normalized_coordinate(&value, "x")?;
            normalized_coordinate(&value, "y")?;
            Ok(true)
        }
        "pointer_button" => {
            normalized_coordinate(&value, "x")?;
            normalized_coordinate(&value, "y")?;
            let button = value.get("button").and_then(Value::as_u64);
            let down = value.get("down").and_then(Value::as_bool);
            if !matches!(button, Some(0..=2)) || down.is_none() {
                return Err(("invalid_message", "鼠标按键消息无效"));
            }
            Ok(true)
        }
        "wheel" => {
            normalized_coordinate(&value, "x")?;
            normalized_coordinate(&value, "y")?;
            bounded_integer(&value, "delta_x")?;
            bounded_integer(&value, "delta_y")?;
            Ok(true)
        }
        "key" => {
            let code = value
                .get("code")
                .and_then(Value::as_str)
                .unwrap_or_default();
            if code.is_empty()
                || code.len() > 64
                || value.get("down").and_then(Value::as_bool).is_none()
                || value.get("repeat").and_then(Value::as_bool).is_none()
                || !valid_modifiers(value.get("modifiers"))
            {
                return Err(("invalid_message", "键盘消息无效"));
            }
            Ok(true)
        }
        "release_all" => Ok(true),
        "audio_control" => {
            if !audio_enabled {
                return Err(("audio_unavailable", "当前远程桌面会话未启用音频"));
            }
            if !has_only_fields(&value, &["type", "enabled"])
                || value.get("enabled").and_then(Value::as_bool).is_none()
            {
                return Err(("invalid_message", "远程桌面音频控制消息无效"));
            }
            Ok(false)
        }
        "secure_attention" => Err((
            "secure_attention_unavailable",
            "不允许通过远程桌面发送 Ctrl+Alt+Del",
        )),
        "feedback" => {
            if value.get("sequence").and_then(Value::as_u64).is_none() {
                return Err(("invalid_message", "桌面反馈消息无效"));
            }
            finite_number(&value, "fps")?;
            finite_number(&value, "decode_ms")?;
            Ok(false)
        }
        _ => Err(("unknown_message", "未知的远程桌面控制消息")),
    }
}

fn is_reliable_browser_message(text: &str) -> bool {
    !matches!(
        message_type(text).as_deref(),
        Some("pointer_move" | "feedback")
    )
}

fn bypasses_desktop_control_gate(text: &str) -> bool {
    message_type(text).as_deref() == Some("audio_control")
}

fn audio_control_enabled(text: &str) -> Option<bool> {
    let value = serde_json::from_str::<Value>(text).ok()?;
    (value.get("type").and_then(Value::as_str) == Some("audio_control"))
        .then(|| value.get("enabled").and_then(Value::as_bool))
        .flatten()
}

fn audio_control_generation(text: &str) -> Option<u64> {
    let value = serde_json::from_str::<Value>(text).ok()?;
    (value.get("type").and_then(Value::as_str) == Some("audio_control"))
        .then(|| value.get("generation").and_then(Value::as_u64))
        .flatten()
}

fn stops_audio_stream(text: &str) -> bool {
    let Ok(value) = serde_json::from_str::<Value>(text) else {
        return false;
    };
    match value.get("type").and_then(Value::as_str) {
        Some("audio_state") => value.get("state").and_then(Value::as_str) != Some("playing"),
        Some("desktop_state") => value.get("desktop").and_then(Value::as_str) != Some("default"),
        Some("consent_required" | "paused" | "closed" | "error") => true,
        _ => false,
    }
}

fn clear_pending_audio<T: Clone>(receiver: &mut broadcast::Receiver<T>) {
    loop {
        match receiver.try_recv() {
            Ok(_) | Err(broadcast::error::TryRecvError::Lagged(_)) => {}
            Err(broadcast::error::TryRecvError::Empty | broadcast::error::TryRecvError::Closed) => {
                break;
            }
        }
    }
}

fn validate_agent_message(
    text: &str,
    audio_enabled: bool,
) -> Result<String, (&'static str, &'static str)> {
    if text.len() > CONTROL_MESSAGE_MAX_BYTES {
        return Err(("message_too_large", "远程桌面状态消息过大"));
    }
    let value: Value =
        serde_json::from_str(text).map_err(|_| ("invalid_message", "远程桌面状态消息格式无效"))?;
    let kind = value
        .get("type")
        .and_then(Value::as_str)
        .ok_or(("invalid_message", "远程桌面状态消息缺少类型"))?;
    if kind == "audio_state" {
        if !audio_enabled {
            return Err(("unexpected_audio", "当前远程桌面会话未启用音频"));
        }
        let state = value.get("state").and_then(Value::as_str);
        let reason = value.get("reason").and_then(Value::as_str);
        let generation = value.get("generation").and_then(Value::as_u64);
        let valid_generation = match (state, reason, generation) {
            (Some("starting"), Some("control_ack"), Some(_)) => true,
            (_, Some("control_ack"), _) | (_, _, Some(_)) => false,
            _ => true,
        };
        if !has_only_fields(&value, &["type", "state", "reason", "generation"])
            || !matches!(
                state,
                Some("off" | "starting" | "playing" | "paused" | "unavailable")
            )
            || !valid_optional_status_code(value.get("reason"))
            || !valid_generation
        {
            return Err(("invalid_message", "远程桌面音频状态消息无效"));
        }
        return Ok(match (reason, generation) {
            (Some(reason), Some(generation)) => {
                json!({"type":"audio_state", "state":state, "reason":reason, "generation":generation})
            }
            (Some(reason), None) => json!({"type":"audio_state", "state":state, "reason":reason}),
            (None, None) => json!({"type":"audio_state", "state":state}),
            (None, Some(_)) => unreachable!("generation validation requires a reason"),
        }
        .to_string());
    }
    if matches!(
        kind,
        "consent_required"
            | "ready"
            | "display"
            | "desktop_state"
            | "notice"
            | "paused"
            | "closed"
            | "error"
    ) {
        Ok(text.to_string())
    } else {
        Err(("unknown_message", "未知的远程桌面状态消息"))
    }
}

fn has_only_fields(value: &Value, allowed: &[&str]) -> bool {
    value
        .as_object()
        .is_some_and(|object| object.keys().all(|key| allowed.contains(&key.as_str())))
}

fn valid_optional_status_code(value: Option<&Value>) -> bool {
    let Some(value) = value else {
        return true;
    };
    matches!(
        value.as_str(),
        Some(
            "secure_desktop"
                | "no_output_device"
                | "audio_service_unavailable"
                | "device_invalidated"
                | "user_token_unavailable"
                | "capture_failed"
                | "encoder_failed"
                | "control_ack"
        )
    )
}

fn normalized_coordinate(value: &Value, field: &str) -> Result<(), (&'static str, &'static str)> {
    let number = value
        .get(field)
        .and_then(Value::as_f64)
        .ok_or(("invalid_message", "鼠标坐标无效"))?;
    if !number.is_finite() || !(0.0..=1.0).contains(&number) {
        return Err(("invalid_message", "鼠标坐标超出范围"));
    }
    Ok(())
}

fn finite_number(value: &Value, field: &str) -> Result<(), (&'static str, &'static str)> {
    let number = value
        .get(field)
        .and_then(Value::as_f64)
        .ok_or(("invalid_message", "数值字段无效"))?;
    if !number.is_finite() || number.abs() > 100_000.0 {
        return Err(("invalid_message", "数值字段超出范围"));
    }
    Ok(())
}

fn bounded_integer(value: &Value, field: &str) -> Result<(), (&'static str, &'static str)> {
    let number = value
        .get(field)
        .and_then(Value::as_i64)
        .ok_or(("invalid_message", "滚轮数值无效"))?;
    if !(-100_000..=100_000).contains(&number) {
        return Err(("invalid_message", "滚轮数值超出范围"));
    }
    Ok(())
}

fn valid_modifiers(value: Option<&Value>) -> bool {
    value.and_then(Value::as_array).is_some_and(|modifiers| {
        modifiers.len() <= 4
            && modifiers.iter().all(|modifier| {
                matches!(modifier.as_str(), Some("alt" | "ctrl" | "shift" | "meta"))
            })
    })
}

fn message_type(text: &str) -> Option<String> {
    serde_json::from_str::<Value>(text)
        .ok()?
        .get("type")?
        .as_str()
        .map(str::to_string)
}

fn message_reason(text: &str) -> Option<String> {
    serde_json::from_str::<Value>(text)
        .ok()?
        .get("reason")?
        .as_str()
        .map(sanitize_reason)
}

fn message_desktop(text: &str) -> Option<String> {
    serde_json::from_str::<Value>(text)
        .ok()?
        .get("desktop")?
        .as_str()
        .map(str::to_string)
}

fn sanitize_reason(reason: &str) -> String {
    let reason = reason.trim();
    if reason.is_empty() {
        "unknown".to_string()
    } else {
        reason.chars().take(128).collect()
    }
}

fn server_error(code: &str, message: &str) -> String {
    json!({"type":"error", "code":code, "message":message}).to_string()
}

fn opening_message(audio_codec: Option<DesktopAudioCodec>) -> String {
    match audio_codec {
        Some(codec) => json!({"type":"opening", "audio_codec":codec.as_str()}).to_string(),
        None => json!({"type":"opening"}).to_string(),
    }
}

fn try_send_browser_error(sender: &mpsc::Sender<String>, code: &str, message: &str) {
    let _ = sender.try_send(server_error(code, message));
}

async fn close_socket(mut socket: WebSocket) {
    let _ = tokio::time::timeout(SOCKET_SEND_TIMEOUT, socket.close()).await;
}

async fn send_single_message(mut socket: WebSocket, message: String) {
    let _ = tokio::time::timeout(
        SOCKET_SEND_TIMEOUT,
        socket.send(Message::Text(message.into())),
    )
    .await;
    let _ = tokio::time::timeout(SOCKET_SEND_TIMEOUT, socket.close()).await;
}

async fn send_text(
    sender: &mut futures_util::stream::SplitSink<WebSocket, Message>,
    text: &str,
) -> bool {
    send_socket_message(sender, Message::Text(text.to_string().into())).await
}

async fn send_socket_message(
    sender: &mut futures_util::stream::SplitSink<WebSocket, Message>,
    message: Message,
) -> bool {
    tokio::time::timeout(SOCKET_SEND_TIMEOUT, sender.send(message))
        .await
        .is_ok_and(|result| result.is_ok())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn valid_frame() -> Vec<u8> {
        let mut frame = vec![0; FRAME_HEADER_BYTES];
        frame[0..4].copy_from_slice(b"OMRD");
        frame[4] = 1;
        frame[5] = 1;
        frame[24..28].copy_from_slice(&1920_u32.to_be_bytes());
        frame[28..32].copy_from_slice(&1080_u32.to_be_bytes());
        frame.extend_from_slice(&[0xff, 0xd8, 0xff, 0xd9]);
        frame
    }

    fn valid_audio_frame() -> Vec<u8> {
        let mut frame = vec![0; FRAME_HEADER_BYTES];
        frame[0..4].copy_from_slice(b"OMRA");
        frame[4] = 1;
        frame[5] = 1;
        frame[6] = OPUS_CHANNELS;
        frame[8..16].copy_from_slice(&7_u64.to_be_bytes());
        frame[16..24].copy_from_slice(&20_000_u64.to_be_bytes());
        frame[24..28].copy_from_slice(&OPUS_SAMPLE_RATE.to_be_bytes());
        frame[28..32].copy_from_slice(&OPUS_SAMPLES_PER_CHANNEL.to_be_bytes());
        frame.push(0xf8);
        frame
    }

    #[test]
    fn validates_omrd_jpeg_frame() {
        assert_eq!(
            validate_binary_frame(&valid_frame()),
            Ok(DesktopBinaryFrameKind::Video)
        );
        let mut invalid = valid_frame();
        invalid[4] = 2;
        assert!(validate_binary_frame(&invalid).is_err());
    }

    #[test]
    fn validates_strict_omra_opus_frame() {
        assert_eq!(
            validate_binary_frame(&valid_audio_frame()),
            Ok(DesktopBinaryFrameKind::Audio)
        );

        let mut discontinuity = valid_audio_frame();
        discontinuity[7] = AUDIO_DISCONTINUITY_FLAG;
        assert_eq!(
            validate_binary_frame(&discontinuity),
            Ok(DesktopBinaryFrameKind::Audio)
        );

        for (index, value) in [(4, 2), (5, 2), (6, 1), (7, 2)] {
            let mut invalid = valid_audio_frame();
            invalid[index] = value;
            assert!(validate_binary_frame(&invalid).is_err());
        }

        let mut invalid_rate = valid_audio_frame();
        invalid_rate[24..28].copy_from_slice(&44_100_u32.to_be_bytes());
        assert!(validate_binary_frame(&invalid_rate).is_err());

        let mut invalid_samples = valid_audio_frame();
        invalid_samples[28..32].copy_from_slice(&480_u32.to_be_bytes());
        assert!(validate_binary_frame(&invalid_samples).is_err());
    }

    #[test]
    fn rejects_invalid_omra_payload_sizes_and_unknown_media() {
        let mut empty = valid_audio_frame();
        empty.truncate(FRAME_HEADER_BYTES);
        assert!(validate_binary_frame(&empty).is_err());

        let mut oversized = valid_audio_frame();
        oversized.resize(FRAME_HEADER_BYTES + OPUS_MAX_PACKET_BYTES + 1, 0);
        assert!(validate_binary_frame(&oversized).is_err());
        assert!(validate_binary_frame(b"NOPE").is_err());

        let mut missing_vbr_length = valid_audio_frame();
        missing_vbr_length.truncate(FRAME_HEADER_BYTES);
        missing_vbr_length.push(0x02);
        assert!(validate_binary_frame(&missing_vbr_length).is_err());

        let mut odd_cbr = valid_audio_frame();
        odd_cbr.truncate(FRAME_HEADER_BYTES);
        odd_cbr.extend_from_slice(&[0x01, 0xaa]);
        assert!(validate_binary_frame(&odd_cbr).is_err());

        let mut excessive_duration = valid_audio_frame();
        excessive_duration.truncate(FRAME_HEADER_BYTES);
        excessive_duration.extend_from_slice(&[0x1b, 0x03]);
        assert!(validate_binary_frame(&excessive_duration).is_err());

        let mut truncated_padding = valid_audio_frame();
        truncated_padding.truncate(FRAME_HEADER_BYTES);
        truncated_padding.extend_from_slice(&[0xfb, 0x41, 0xff]);
        assert!(validate_binary_frame(&truncated_padding).is_err());
    }

    #[test]
    fn malformed_negotiated_audio_is_a_protocol_error() {
        let mut malformed_audio = valid_audio_frame();
        malformed_audio[4] = 2;
        assert!(classify_agent_binary_frame(&malformed_audio, true).is_err());
        assert_eq!(
            classify_agent_binary_frame(&valid_frame(), true),
            Ok(AgentBinaryFrameDisposition::Video)
        );

        assert_eq!(
            classify_agent_binary_frame(&valid_audio_frame(), false),
            Err(("unexpected_audio", "当前远程桌面会话未启用音频"))
        );
        let mut malformed_video = valid_frame();
        malformed_video[4] = 2;
        assert!(classify_agent_binary_frame(&malformed_video, true).is_err());
    }

    #[test]
    fn accepts_rfc_6716_opus_packet_framing_modes() {
        for packet in [
            vec![0xf8, 0xaa],
            vec![0xf1, 0xaa, 0xbb],
            vec![0xf2, 0x01, 0xaa, 0xbb],
            vec![0xeb, 0x84, 0x01, 0x01, 0x01, 0xaa, 0xbb, 0xcc, 0xdd],
            vec![0xf3, 0x42, 0x01, 0xaa, 0xbb, 0x00],
        ] {
            assert!(valid_opus_packet(&packet), "rejected packet {packet:02x?}");
        }
        assert!(!valid_opus_packet(&[0xf9, 0xaa, 0xbb]));
    }

    #[test]
    fn desktop_quality_defaults_to_balanced() {
        let query: DesktopBrowserWsQuery = serde_json::from_value(json!({})).unwrap();
        assert_eq!(query.quality, DesktopQuality::Balanced);
        assert_eq!(query.audio, None);
        assert_eq!(
            query.quality.stream_settings(),
            DesktopStreamSettings {
                max_width: 1280,
                max_height: 720,
                min_fps: 6,
                max_fps: 8,
                jpeg_quality: 50,
            }
        );
    }

    #[test]
    fn desktop_quality_presets_map_to_stream_limits() {
        assert_eq!(
            [
                DesktopQuality::Low.stream_settings(),
                DesktopQuality::Balanced.stream_settings(),
                DesktopQuality::High.stream_settings(),
                DesktopQuality::Original.stream_settings(),
            ],
            [
                DesktopStreamSettings {
                    max_width: 960,
                    max_height: 540,
                    min_fps: 4,
                    max_fps: 6,
                    jpeg_quality: 35,
                },
                DesktopStreamSettings {
                    max_width: 1280,
                    max_height: 720,
                    min_fps: 6,
                    max_fps: 8,
                    jpeg_quality: 50,
                },
                DesktopStreamSettings {
                    max_width: 1600,
                    max_height: 900,
                    min_fps: 8,
                    max_fps: 10,
                    jpeg_quality: 60,
                },
                DesktopStreamSettings {
                    max_width: 1920,
                    max_height: 1080,
                    min_fps: 8,
                    max_fps: 12,
                    jpeg_quality: 70,
                },
            ]
        );
    }

    #[test]
    fn rejects_unknown_desktop_quality() {
        assert!(serde_json::from_value::<DesktopQuality>(json!("ultra")).is_err());
    }

    #[test]
    fn negotiates_opus_only_for_capable_agents() {
        let query: DesktopBrowserWsQuery = serde_json::from_value(json!({"audio":"opus"})).unwrap();
        assert_eq!(query.audio, Some(DesktopAudioCodec::Opus));
        assert!(serde_json::from_value::<DesktopBrowserWsQuery>(json!({"audio":"pcm"})).is_err());

        assert_eq!(negotiate_audio_codec(query.audio, &[]), None);
        assert_eq!(
            negotiate_audio_codec(query.audio, &[DESKTOP_AUDIO_CAPABILITY.to_string()],),
            Some(DesktopAudioCodec::Opus)
        );
    }

    #[test]
    fn audio_codec_is_optional_in_opening_and_agent_open_messages() {
        let legacy_opening: Value = serde_json::from_str(&opening_message(None)).unwrap();
        assert!(legacy_opening.get("audio_codec").is_none());
        assert_eq!(
            serde_json::from_str::<Value>(&opening_message(Some(DesktopAudioCodec::Opus))).unwrap()
                ["audio_codec"],
            AUDIO_CODEC_OPUS
        );

        let legacy = json!({
            "type": "desktop_open",
            "session_id": "desktop-1",
            "stream_token": "token",
            "max_width": 1280,
            "max_height": 720,
            "min_fps": 6,
            "max_fps": 8,
            "jpeg_quality": 50
        });
        assert!(matches!(
            serde_json::from_value::<AgentOutbound>(legacy).unwrap(),
            AgentOutbound::DesktopOpen {
                audio_codec: None,
                ..
            }
        ));

        let no_audio = serde_json::to_value(AgentOutbound::DesktopOpen {
            session_id: "desktop-1".to_string(),
            stream_token: "token".to_string(),
            audio_codec: None,
            max_width: 1280,
            max_height: 720,
            min_fps: 6,
            max_fps: 8,
            jpeg_quality: 50,
        })
        .unwrap();
        assert!(no_audio.get("audio_codec").is_none());

        let encoded = serde_json::to_value(AgentOutbound::DesktopOpen {
            session_id: "desktop-1".to_string(),
            stream_token: "token".to_string(),
            audio_codec: Some(AUDIO_CODEC_OPUS.to_string()),
            max_width: 1280,
            max_height: 720,
            min_fps: 6,
            max_fps: 8,
            jpeg_quality: 50,
        })
        .unwrap();
        assert_eq!(encoded["audio_codec"], AUDIO_CODEC_OPUS);
    }

    #[test]
    fn validates_browser_control_message_types() {
        assert_eq!(
            validate_browser_message(r#"{"type":"pointer_move","x":0.5,"y":1.0}"#, false),
            Ok(true)
        );
        assert_eq!(
            validate_browser_message(
                r#"{"type":"feedback","sequence":7,"fps":10.0,"decode_ms":4.2}"#,
                false,
            ),
            Ok(false)
        );
        assert_eq!(
            validate_browser_message(r#"{"type":"audio_control","enabled":false}"#, true),
            Ok(false)
        );
        assert!(bypasses_desktop_control_gate(
            r#"{"type":"audio_control","enabled":false}"#
        ));
        assert!(!bypasses_desktop_control_gate(
            r#"{"type":"pointer_move","x":0.5,"y":1.0}"#
        ));
        assert!(is_reliable_browser_message(
            r#"{"type":"audio_control","enabled":false}"#
        ));
        assert!(
            validate_browser_message(r#"{"type":"audio_control","enabled":false}"#, false).is_err()
        );
        assert!(
            validate_browser_message(r#"{"type":"audio_control","enabled":"no"}"#, true).is_err()
        );
        assert!(
            validate_browser_message(
                r#"{"type":"audio_control","enabled":true,"extra":"raw"}"#,
                true,
            )
            .is_err()
        );
        assert_eq!(
            validate_browser_message(r#"{"type":"secure_attention"}"#, false),
            Err((
                "secure_attention_unavailable",
                "不允许通过远程桌面发送 Ctrl+Alt+Del"
            ))
        );
        assert!(validate_browser_message(r#"{"type":"pointer_move","x":2,"y":0}"#, false).is_err());
        assert!(validate_browser_message(r#"{"type":"unknown"}"#, false).is_err());
    }

    #[test]
    fn validates_local_consent_status_message() {
        assert!(validate_agent_message(r#"{"type":"consent_required"}"#, false).is_ok());
    }

    #[test]
    fn validates_audio_state_and_safe_reason_codes() {
        for state in ["off", "starting", "playing", "paused", "unavailable"] {
            let message = json!({
                "type": "audio_state",
                "state": state,
                "reason": "device_invalidated",
            })
            .to_string();
            assert!(validate_agent_message(&message, true).is_ok());
        }
        assert!(
            validate_agent_message(r#"{"type":"audio_state","state":"playing"}"#, true).is_ok()
        );
        assert!(
            validate_agent_message(r#"{"type":"audio_state","state":"playing"}"#, false).is_err()
        );
        let control_ack = validate_agent_message(
            r#"{"type":"audio_state","state":"starting","reason":"control_ack","generation":7}"#,
            true,
        )
        .unwrap();
        assert_eq!(
            serde_json::from_str::<Value>(&control_ack).unwrap(),
            json!({
                "type": "audio_state",
                "state": "starting",
                "reason": "control_ack",
                "generation": 7,
            })
        );
        assert!(
            validate_agent_message(
                r#"{"type":"audio_state","state":"starting","reason":"control_ack"}"#,
                true,
            )
            .is_err()
        );
        assert!(
            validate_agent_message(
                r#"{"type":"audio_state","state":"playing","reason":"control_ack","generation":7}"#,
                true,
            )
            .is_err()
        );
        assert!(
            validate_agent_message(
                r#"{"type":"audio_state","state":"playing","generation":7}"#,
                true,
            )
            .is_err()
        );
        assert!(
            validate_agent_message(
                r#"{"type":"audio_state","state":"unknown","reason":"ok"}"#,
                true,
            )
            .is_err()
        );
        assert!(
            validate_agent_message(
                r#"{"type":"audio_state","state":"unavailable","reason":"unsafe reason"}"#,
                true,
            )
            .is_err()
        );
        assert!(
            validate_agent_message(
                r#"{"type":"audio_state","state":"unavailable","reason":"other_failure"}"#,
                true,
            )
            .is_err()
        );
        assert!(
            validate_agent_message(
                r#"{"type":"audio_state","state":"unavailable","reason":"capture_failed","error":"raw WASAPI failure"}"#,
                true,
            )
            .is_err()
        );
        let sanitized = validate_agent_message(
            r#"{"type":"audio_state","state":"unavailable","reason":"raw WASAPI failure","reason":"capture_failed"}"#,
            true,
        )
        .unwrap();
        assert_eq!(
            serde_json::from_str::<Value>(&sanitized).unwrap(),
            json!({
                "type": "audio_state",
                "state": "unavailable",
                "reason": "capture_failed"
            })
        );
        assert!(!sanitized.contains("raw WASAPI failure"));
    }

    #[tokio::test]
    async fn audio_relay_overwrites_the_oldest_packet_at_capacity() {
        let (sender, mut receiver) = broadcast::channel::<Arc<Vec<u8>>>(AUDIO_RELAY_CAPACITY);
        for sequence in 0..=AUDIO_RELAY_CAPACITY {
            sender.send(Arc::new(vec![sequence as u8])).unwrap();
        }

        assert!(matches!(
            receiver.recv().await,
            Err(broadcast::error::RecvError::Lagged(1))
        ));
        assert_eq!(receiver.recv().await.unwrap().as_slice(), &[1]);
    }

    #[test]
    fn audio_relay_waits_for_the_current_control_ack_and_discontinuity() {
        let mut gate = AudioRelayGate::new();
        let continuous = valid_audio_frame();
        let mut discontinuous = valid_audio_frame();
        discontinuous[7] = AUDIO_DISCONTINUITY_FLAG;

        gate.set_control(1, true);
        gate.observe_state(
            r#"{"type":"audio_state","state":"starting","reason":"control_ack","generation":0}"#,
        );
        assert!(!gate.accepts_audio(&discontinuous));

        gate.observe_state(
            r#"{"type":"audio_state","state":"starting","reason":"control_ack","generation":1}"#,
        );
        assert!(!gate.accepts_audio(&continuous));
        assert!(gate.accepts_audio(&discontinuous));
        assert!(gate.accepts_audio(&continuous));

        gate.set_control(2, false);
        assert!(!gate.accepts_audio(&discontinuous));
        gate.set_control(3, true);
        gate.observe_state(
            r#"{"type":"audio_state","state":"starting","reason":"control_ack","generation":2}"#,
        );
        assert!(!gate.accepts_audio(&discontinuous));
        gate.observe_state(
            r#"{"type":"audio_state","state":"starting","reason":"control_ack","generation":3}"#,
        );
        assert!(gate.accepts_audio(&discontinuous));

        gate.observe_state(r#"{"type":"audio_state","state":"unavailable"}"#);
        assert!(!gate.accepts_audio(&continuous));
        assert!(gate.accepts_audio(&discontinuous));
    }

    #[test]
    fn browser_audio_relay_merges_duplicate_controls_and_rejects_old_generations() {
        let mut gate = BrowserAudioRelayGate::new();
        let packet = |generation| DesktopAudioPacket {
            generation,
            frame: Arc::new(valid_audio_frame()),
        };

        assert_eq!(gate.set_control(false), None);
        assert_eq!(gate.set_control(true), Some(1));
        assert!(!gate.accepts(&packet(0)));
        assert!(!gate.observe_state(
            r#"{"type":"audio_state","state":"starting","reason":"control_ack","generation":0}"#,
        ));
        assert!(gate.observe_state(
            r#"{"type":"audio_state","state":"starting","reason":"control_ack","generation":1}"#,
        ));
        assert!(gate.accepts(&packet(1)));
        assert_eq!(gate.set_control(true), None);
        assert!(gate.accepts(&packet(1)));

        assert_eq!(gate.set_control(false), Some(2));
        assert!(!gate.accepts(&packet(1)));
        assert_eq!(gate.set_control(true), Some(3));
        assert!(!gate.observe_state(
            r#"{"type":"audio_state","state":"starting","reason":"control_ack","generation":1}"#,
        ));
        assert!(!gate.accepts(&packet(1)));
        assert!(gate.observe_state(
            r#"{"type":"audio_state","state":"starting","reason":"control_ack","generation":3}"#,
        ));
        assert!(!gate.accepts(&packet(1)));
        assert!(gate.accepts(&packet(3)));
    }

    #[test]
    fn audio_stop_events_discard_pending_relay_packets() {
        let (sender, mut receiver) = broadcast::channel::<Arc<Vec<u8>>>(AUDIO_RELAY_CAPACITY);
        sender.send(Arc::new(vec![1])).unwrap();
        sender.send(Arc::new(vec![2])).unwrap();

        assert_eq!(
            audio_control_enabled(r#"{"type":"audio_control","enabled":false}"#),
            Some(false)
        );
        assert_eq!(
            audio_control_enabled(r#"{"type":"audio_control","enabled":true}"#),
            Some(true)
        );
        assert_eq!(
            audio_control_generation(r#"{"type":"audio_control","enabled":true,"generation":9}"#),
            Some(9)
        );
        assert_eq!(
            audio_control_enabled(r#"{"type":"pointer_move","enabled":true}"#),
            None
        );
        assert!(stops_audio_stream(
            r#"{"type":"audio_state","state":"paused","reason":"secure_desktop"}"#
        ));
        assert!(stops_audio_stream(
            r#"{"type":"desktop_state","desktop":"other"}"#
        ));
        assert!(!stops_audio_stream(
            r#"{"type":"audio_state","state":"playing"}"#
        ));

        clear_pending_audio(&mut receiver);
        assert!(matches!(
            receiver.try_recv(),
            Err(broadcast::error::TryRecvError::Empty)
        ));
    }

    #[test]
    fn desktop_control_rate_limiter_allows_a_bounded_burst_and_refills() {
        let started = Instant::now();
        let mut limiter = ControlRateLimiter::new(started);
        for _ in 0..CONTROL_RATE_BURST as usize {
            assert!(limiter.allow(started));
        }
        assert!(!limiter.allow(started));

        let refilled = started + Duration::from_secs(1);
        for _ in 0..CONTROL_RATE_PER_SECOND as usize {
            assert!(limiter.allow(refilled));
        }
        assert!(!limiter.allow(refilled));
    }

    #[test]
    fn stream_tokens_are_random_and_hashed() {
        let (first, first_hash) = new_stream_token();
        let (second, second_hash) = new_stream_token();
        assert_ne!(first, second);
        assert_ne!(first_hash, second_hash);
        assert_eq!(
            first_hash,
            <[u8; 32]>::from(Sha256::digest(first.as_bytes()))
        );
    }

    #[test]
    fn extracts_and_sanitizes_agent_close_reason() {
        assert_eq!(
            message_reason(r#"{"type":"closed","reason":" helper_error "}"#).as_deref(),
            Some("helper_error")
        );
        assert_eq!(message_reason(r#"{"type":"closed"}"#), None);
    }

    #[tokio::test]
    async fn browser_error_notification_does_not_wait_for_a_full_queue() {
        let (sender, mut receiver) = mpsc::channel(1);
        sender
            .send("queued-control-message".to_string())
            .await
            .unwrap();

        try_send_browser_error(&sender, "invalid_frame", "invalid frame");

        assert_eq!(
            receiver.recv().await.as_deref(),
            Some("queued-control-message")
        );
        assert!(receiver.try_recv().is_err());
    }
}
