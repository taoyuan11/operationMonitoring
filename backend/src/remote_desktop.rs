use std::{
    sync::Arc,
    time::{Duration, Instant},
};

use axum::{
    extract::{
        Path, Query, State,
        ws::{Message, WebSocket, WebSocketUpgrade},
    },
    http::{HeaderMap, StatusCode, header},
    response::Response,
};
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use futures_util::{SinkExt, StreamExt};
use rand::{RngCore, rngs::OsRng};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;
use tokio::sync::{mpsc, watch};
use tracing::{error, info, warn};
use uuid::Uuid;

use crate::{
    auth::require_admin,
    db::{get_instance, write_action_log},
    error::{AppError, AppResult},
    models::{AgentOutbound, DesktopAgentWsQuery},
    state::{AppState, DesktopSessionHandle},
    utils::now_ts,
};

const DESKTOP_CAPABILITY: &str = "remote_desktop_v1";
const TOKEN_TTL_SECONDS: i64 = 30;
const CONTROL_MESSAGE_MAX_BYTES: usize = 16 * 1024;
const FRAME_MAX_BYTES: usize = 2 * 1024 * 1024;
const FRAME_HEADER_BYTES: usize = 32;
const HELPER_JOIN_TIMEOUT: Duration = Duration::from_secs(15);
const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(10);
const HEARTBEAT_TIMEOUT: Duration = Duration::from_secs(30);
const SOCKET_SEND_TIMEOUT: Duration = Duration::from_secs(5);
const IDLE_TIMEOUT: Duration = Duration::from_secs(15 * 60);
const SESSION_TIMEOUT: Duration = Duration::from_secs(2 * 60 * 60);

pub async fn admin_desktop_ws(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(instance_id): Path<String>,
    ws: WebSocketUpgrade,
) -> AppResult<Response> {
    ensure_same_origin(&headers, state.secure_cookies)?;
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

    Ok(ws.on_upgrade(move |socket| {
        desktop_browser_socket(state, instance_id, admin.username, socket)
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
    Ok(ws.on_upgrade(move |socket| desktop_agent_socket(state, query.session_id, socket)))
}

async fn desktop_browser_socket(
    state: AppState,
    instance_id: String,
    actor: String,
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

    let session_id = Uuid::new_v4().to_string();
    let (stream_token, token_hash) = new_stream_token();
    let (browser_tx, mut browser_rx) = mpsc::channel::<String>(32);
    let (frame_tx, mut frame_rx) = watch::channel::<Option<Arc<Vec<u8>>>>(None);
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
                actor: actor.clone(),
                agent_connection_id: agent.connection_id,
                token_hash,
                token_expires_at: now_ts() + TOKEN_TTL_SECONDS,
                token_claimed: false,
                browser_tx,
                frame_tx,
                agent_input_rx: Arc::new(tokio::sync::Mutex::new(Some(agent_input_rx))),
                close_tx,
            },
        );
    }

    if let Err(error) = sqlx::query(
        "INSERT INTO desktop_sessions(id, instance_id, actor, started_at) VALUES($1, $2, $3, $4)",
    )
    .bind(&session_id)
    .bind(&instance_id)
    .bind(&actor)
    .bind(now_ts())
    .execute(&state.db)
    .await
    {
        error!(?error, %session_id, "failed to create desktop session audit row");
        end_desktop_session(&state, &session_id, "database_error").await;
        send_single_message(
            socket,
            server_error("database_error", "无法创建远程桌面会话"),
        )
        .await;
        return;
    }
    if let Err(error) = write_action_log(
        &state.db,
        &actor,
        "desktop_start",
        &instance_id,
        &format!("启动远程桌面会话 {session_id}"),
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

    if agent
        .tx
        .send(AgentOutbound::DesktopOpen {
            session_id: session_id.clone(),
            stream_token,
            max_width: 1920,
            max_height: 1080,
            min_fps: 8,
            max_fps: 12,
            jpeg_quality: 70,
        })
        .is_err()
    {
        end_desktop_session(&state, &session_id, "agent_disconnected").await;
        send_single_message(socket, server_error("offline", "实例连接已断开")).await;
        return;
    }

    let (mut sender, mut receiver) = socket.split();
    if !send_text(&mut sender, &json!({"type": "opening"}).to_string()).await {
        end_desktop_session(&state, &session_id, "browser_disconnected").await;
        return;
    }

    info!(%session_id, %instance_id, "desktop browser websocket connected");
    let started = Instant::now();
    let mut last_activity = started;
    let mut last_inbound = started;
    let mut joined = false;
    let mut helper_timeout = Box::pin(tokio::time::sleep(HELPER_JOIN_TIMEOUT));
    let mut heartbeat = tokio::time::interval(HEARTBEAT_INTERVAL);
    heartbeat.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut reason = "browser_disconnected".to_string();

    loop {
        tokio::select! {
            incoming = receiver.next() => {
                let Some(incoming) = incoming else { break; };
                match incoming {
                    Ok(Message::Text(text)) => {
                        last_inbound = Instant::now();
                        match validate_browser_message(&text) {
                            Ok(is_activity) => {
                                if is_activity { last_activity = Instant::now(); }
                                let reliable = is_reliable_browser_message(&text);
                                let delivered = if reliable {
                                    tokio::time::timeout(Duration::from_secs(1), agent_input_tx.send(text.to_string())).await
                                        .is_ok_and(|result| result.is_ok())
                                } else {
                                    agent_input_tx.try_send(text.to_string()).is_ok()
                                };
                                if reliable && !delivered {
                                    let _ = send_text(&mut sender, &server_error("input_queue_overflow", "远程输入队列拥塞")).await;
                                    reason = "input_queue_overflow".to_string();
                                    break;
                                }
                            }
                            Err((code, message)) => {
                                let _ = send_text(&mut sender, &server_error(code, message)).await;
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
                        let _ = send_text(&mut sender, &server_error("invalid_message", "浏览器不得发送二进制数据")).await;
                        reason = "invalid_control_message".to_string();
                        break;
                    }
                }
            }
            message = browser_rx.recv() => {
                let Some(message) = message else { break; };
                let kind = message_type(&message);
                if kind.as_deref() == Some("ready") { joined = true; }
                let close_reason = (kind.as_deref() == Some("closed"))
                    .then(|| message_reason(&message).unwrap_or_else(|| "agent_closed".to_string()));
                if !send_text(&mut sender, &message).await { break; }
                if let Some(close_reason) = close_reason {
                    reason = close_reason;
                    break;
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
            changed = close_rx.changed() => {
                if changed.is_err() { break; }
                let close_reason = { close_rx.borrow_and_update().clone() };
                if let Some(close_reason) = close_reason {
                    let _ = send_text(&mut sender, &json!({"type":"closed", "reason":&close_reason}).to_string()).await;
                    reason = close_reason;
                    break;
                }
            }
            _ = &mut helper_timeout, if !joined => {
                let _ = send_text(&mut sender, &server_error("helper_timeout", "远程桌面启动超时")).await;
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
                    let _ = send_text(&mut sender, &json!({"type":"closed", "reason":"idle_timeout"}).to_string()).await;
                    reason = "idle_timeout".to_string();
                    break;
                }
                if now.duration_since(started) >= SESSION_TIMEOUT {
                    let _ = send_text(&mut sender, &json!({"type":"closed", "reason":"session_timeout"}).to_string()).await;
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

    end_desktop_session(&state, &session_id, &reason).await;
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
        let mut socket = socket;
        let _ = socket.close().await;
        return;
    };
    let Some(mut input_rx) = handle.agent_input_rx.lock().await.take() else {
        let mut socket = socket;
        let _ = socket.close().await;
        end_desktop_session(&state, &session_id, "duplicate_agent_data_channel").await;
        return;
    };
    let mut close_rx = handle.close_tx.subscribe();

    let (mut sender, mut receiver) = socket.split();
    let mut heartbeat = tokio::time::interval(HEARTBEAT_INTERVAL);
    heartbeat.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut last_inbound = Instant::now();
    let mut reason = "agent_data_disconnected".to_string();
    info!(%session_id, instance_id = %handle.instance_id, "desktop agent data websocket connected");

    loop {
        tokio::select! {
            incoming = receiver.next() => {
                let Some(incoming) = incoming else { break; };
                match incoming {
                    Ok(Message::Text(text)) => {
                        last_inbound = Instant::now();
                        match validate_agent_message(&text) {
                            Ok(()) => {
                                let close_reason = (message_type(&text).as_deref() == Some("closed"))
                                    .then(|| message_reason(&text).unwrap_or_else(|| "agent_closed".to_string()));
                                let delivered = tokio::time::timeout(
                                    Duration::from_secs(1),
                                    handle.browser_tx.send(text.to_string()),
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
                                let _ = handle.browser_tx.send(server_error(code, message)).await;
                                reason = "invalid_agent_message".to_string();
                                break;
                            }
                        }
                    }
                    Ok(Message::Binary(frame)) => {
                        last_inbound = Instant::now();
                        if let Err((code, message)) = validate_frame(&frame) {
                            let _ = handle.browser_tx.send(server_error(code, message)).await;
                            reason = "invalid_frame".to_string();
                            break;
                        }
                        handle.frame_tx.send_replace(Some(Arc::new(frame.to_vec())));
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
    if let Err(error) = sqlx::query(
        "UPDATE desktop_sessions SET ended_at = $1, end_reason = $2 WHERE id = $3 AND ended_at IS NULL",
    )
    .bind(now_ts())
    .bind(&reason)
    .bind(session_id)
    .execute(&state.db)
    .await
    {
        error!(?error, %session_id, "failed to finish desktop session audit row");
    }
    if let Err(error) = write_action_log(
        &state.db,
        &handle.actor,
        "desktop_end",
        &handle.instance_id,
        &format!("结束远程桌面会话 {session_id}：{reason}"),
    )
    .await
    {
        warn!(?error, %session_id, "failed to write desktop end action log");
    }
}

fn ensure_same_origin(headers: &HeaderMap, secure: bool) -> AppResult<()> {
    let origin = headers
        .get(header::ORIGIN)
        .and_then(|value| value.to_str().ok())
        .ok_or_else(|| AppError::new(StatusCode::FORBIDDEN, "缺少 Origin 请求头"))?;
    let host = headers
        .get(header::HOST)
        .and_then(|value| value.to_str().ok())
        .ok_or_else(|| AppError::new(StatusCode::FORBIDDEN, "缺少 Host 请求头"))?;
    let uri = origin
        .parse::<axum::http::Uri>()
        .map_err(|_| AppError::new(StatusCode::FORBIDDEN, "Origin 无效"))?;
    let expected_scheme = if secure { "https" } else { "http" };
    let valid_scheme = uri.scheme_str() == Some(expected_scheme);
    let origin_host = uri.authority().map(|authority| authority.as_str());
    if !valid_scheme || !origin_host.is_some_and(|value| value.eq_ignore_ascii_case(host)) {
        return Err(AppError::new(
            StatusCode::FORBIDDEN,
            "拒绝跨站 WebSocket 请求",
        ));
    }
    Ok(())
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

fn validate_frame(frame: &[u8]) -> Result<(), (&'static str, &'static str)> {
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

fn validate_browser_message(text: &str) -> Result<bool, (&'static str, &'static str)> {
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
        "release_all" | "secure_attention" => Ok(true),
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

fn validate_agent_message(text: &str) -> Result<(), (&'static str, &'static str)> {
    if text.len() > CONTROL_MESSAGE_MAX_BYTES {
        return Err(("message_too_large", "远程桌面状态消息过大"));
    }
    let value: Value =
        serde_json::from_str(text).map_err(|_| ("invalid_message", "远程桌面状态消息格式无效"))?;
    let kind = value
        .get("type")
        .and_then(Value::as_str)
        .ok_or(("invalid_message", "远程桌面状态消息缺少类型"))?;
    if matches!(
        kind,
        "ready" | "display" | "desktop_state" | "notice" | "paused" | "closed" | "error"
    ) {
        Ok(())
    } else {
        Err(("unknown_message", "未知的远程桌面状态消息"))
    }
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

    #[test]
    fn validates_omrd_jpeg_frame() {
        assert!(validate_frame(&valid_frame()).is_ok());
        let mut invalid = valid_frame();
        invalid[4] = 2;
        assert!(validate_frame(&invalid).is_err());
    }

    #[test]
    fn validates_browser_control_message_types() {
        assert_eq!(
            validate_browser_message(r#"{"type":"pointer_move","x":0.5,"y":1.0}"#),
            Ok(true)
        );
        assert_eq!(
            validate_browser_message(
                r#"{"type":"feedback","sequence":7,"fps":10.0,"decode_ms":4.2}"#
            ),
            Ok(false)
        );
        assert_eq!(
            validate_browser_message(r#"{"type":"secure_attention"}"#),
            Ok(true)
        );
        assert!(validate_browser_message(r#"{"type":"pointer_move","x":2,"y":0}"#).is_err());
        assert!(validate_browser_message(r#"{"type":"unknown"}"#).is_err());
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

    #[test]
    fn same_origin_requires_matching_host() {
        let mut headers = HeaderMap::new();
        headers.insert(header::HOST, "console.example.com".parse().unwrap());
        headers.insert(
            header::ORIGIN,
            "https://console.example.com".parse().unwrap(),
        );
        assert!(ensure_same_origin(&headers, true).is_ok());

        headers.insert(
            header::ORIGIN,
            "http://console.example.com".parse().unwrap(),
        );
        assert!(ensure_same_origin(&headers, true).is_err());
        assert!(ensure_same_origin(&headers, false).is_ok());

        headers.insert(header::ORIGIN, "https://evil.example".parse().unwrap());
        assert!(ensure_same_origin(&headers, true).is_err());
        headers.insert("sec-fetch-site", "same-origin".parse().unwrap());
        assert!(ensure_same_origin(&headers, true).is_err());
        headers.remove(header::ORIGIN);
        assert!(ensure_same_origin(&headers, true).is_err());
    }
}
