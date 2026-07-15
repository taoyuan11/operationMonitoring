use std::{collections::VecDeque, io, time::Duration};

use async_stream::stream;
use axum::{
    Json,
    body::{Body, Bytes},
    extract::{Path, Query, State},
    http::{HeaderMap, HeaderValue, StatusCode, header},
    response::Response,
};
use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;
use tracing::warn;
use uuid::Uuid;

use crate::{
    auth::require_admin,
    db::{get_instance, write_action_log},
    error::{AppError, AppResult},
    models::{
        AgentOutbound, FileErrorCode, FileListing, FileRequest, FileResponse, FileSystemRoot,
    },
    state::{AgentHandle, AppState, FileRequestEvent, PendingFileRequest},
};

const FILE_MANAGER_CAPABILITY: &str = "file_manager_v1";
const CHUNK_SIZE: usize = 256 * 1024;
const TRANSFER_WINDOW: usize = 4;
const FRAME_KIND_FILE_CHUNK_V1: u8 = 1;
const FRAME_HEADER_BYTES: usize = 1 + 16 + 8;
const CONTROL_TIMEOUT: Duration = Duration::from_secs(30);
const DELETE_TIMEOUT: Duration = Duration::from_secs(5 * 60);
const TRANSFER_IDLE_TIMEOUT: Duration = Duration::from_secs(60);

#[derive(Serialize)]
pub struct FileRootsResponse {
    roots: Vec<FileSystemRoot>,
    max_file_bytes: u64,
}

#[derive(Serialize)]
pub struct FileOperationResult {
    path: String,
}

#[derive(Deserialize)]
pub struct FileListQuery {
    path: String,
    #[serde(default)]
    offset: Option<u64>,
    #[serde(default)]
    limit: Option<u64>,
}

#[derive(Deserialize)]
pub struct CreateDirectoryRequest {
    parent: String,
    name: String,
}

#[derive(Deserialize)]
pub struct MoveFileRequest {
    source: String,
    destination_parent: String,
    name: String,
    #[serde(default)]
    overwrite: bool,
}

#[derive(Deserialize)]
pub struct DeleteFileRequest {
    path: String,
    #[serde(default)]
    recursive: bool,
}

#[derive(Deserialize)]
pub struct UploadFileQuery {
    parent: String,
    name: String,
    #[serde(default)]
    overwrite: bool,
}

#[derive(Deserialize)]
pub struct DownloadFileQuery {
    path: String,
}

struct RequestCleanup {
    state: AppState,
    instance_id: String,
    connection_id: Uuid,
    request_id: String,
    transfer: bool,
    cancel_on_drop: bool,
    released: bool,
}

impl RequestCleanup {
    async fn release(&mut self) {
        if self.released {
            return;
        }
        self.cancel_on_drop = false;
        self.state
            .file_requests
            .write()
            .await
            .remove(&self.request_id);
        if self.transfer {
            let mut active = self.state.active_file_transfers.write().await;
            if active
                .get(&self.instance_id)
                .is_some_and(|active_request| active_request == &self.request_id)
            {
                active.remove(&self.instance_id);
            }
        }
        self.released = true;
    }
}

impl Drop for RequestCleanup {
    fn drop(&mut self) {
        if self.released {
            return;
        }
        let state = self.state.clone();
        let instance_id = self.instance_id.clone();
        let connection_id = self.connection_id;
        let request_id = self.request_id.clone();
        let transfer = self.transfer;
        let cancel = self.cancel_on_drop;
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            return;
        };
        runtime.spawn(async move {
            state.file_requests.write().await.remove(&request_id);
            if transfer {
                let mut active = state.active_file_transfers.write().await;
                if active
                    .get(&instance_id)
                    .is_some_and(|active_request| active_request == &request_id)
                {
                    active.remove(&instance_id);
                }
            }
            if cancel {
                let agent = state
                    .agents
                    .read()
                    .await
                    .get(&instance_id)
                    .filter(|agent| agent.connection_id == connection_id)
                    .cloned();
                if let Some(agent) = agent {
                    let _ = agent
                        .tx
                        .send(AgentOutbound::FileTransferCancel { request_id });
                }
            }
        });
    }
}

pub async fn admin_file_roots(
    State(state): State<AppState>,
    Path(instance_id): Path<String>,
    headers: HeaderMap,
) -> AppResult<Json<FileRootsResponse>> {
    require_admin(&state, &headers).await?;
    let response =
        dispatch_control(&state, &instance_id, FileRequest::Roots, CONTROL_TIMEOUT).await?;
    let FileResponse::Roots { roots } = response else {
        return Err(unexpected_response());
    };
    Ok(Json(FileRootsResponse {
        roots,
        max_file_bytes: state.file_transfer_max_bytes as u64,
    }))
}

pub async fn admin_list_files(
    State(state): State<AppState>,
    Path(instance_id): Path<String>,
    Query(query): Query<FileListQuery>,
    headers: HeaderMap,
) -> AppResult<Json<FileListing>> {
    require_admin(&state, &headers).await?;
    let response = dispatch_control(
        &state,
        &instance_id,
        FileRequest::List {
            path: query.path,
            offset: query.offset.unwrap_or(0),
            limit: query.limit.unwrap_or(200).clamp(1, 500),
        },
        CONTROL_TIMEOUT,
    )
    .await?;
    let FileResponse::Listing { listing } = response else {
        return Err(unexpected_response());
    };
    Ok(Json(listing))
}

pub async fn admin_create_directory(
    State(state): State<AppState>,
    Path(instance_id): Path<String>,
    headers: HeaderMap,
    Json(payload): Json<CreateDirectoryRequest>,
) -> AppResult<Json<FileOperationResult>> {
    let admin = require_admin(&state, &headers).await?;
    let response = dispatch_control(
        &state,
        &instance_id,
        FileRequest::CreateDirectory {
            parent: payload.parent,
            name: payload.name,
        },
        CONTROL_TIMEOUT,
    )
    .await?;
    let FileResponse::OperationComplete { path } = response else {
        return Err(unexpected_response());
    };
    write_action_log(
        &state.db,
        &admin.username,
        "create_instance_directory",
        &instance_id,
        &format!("创建目录 {path}"),
    )
    .await?;
    Ok(Json(FileOperationResult { path }))
}

pub async fn admin_move_file(
    State(state): State<AppState>,
    Path(instance_id): Path<String>,
    headers: HeaderMap,
    Json(payload): Json<MoveFileRequest>,
) -> AppResult<Json<FileOperationResult>> {
    let admin = require_admin(&state, &headers).await?;
    let source = payload.source.clone();
    let response = dispatch_control(
        &state,
        &instance_id,
        FileRequest::Move {
            source: payload.source,
            destination_parent: payload.destination_parent,
            name: payload.name,
            overwrite: payload.overwrite,
        },
        CONTROL_TIMEOUT,
    )
    .await?;
    let FileResponse::OperationComplete { path } = response else {
        return Err(unexpected_response());
    };
    write_action_log(
        &state.db,
        &admin.username,
        "move_instance_file",
        &instance_id,
        &format!("移动或重命名 {source} -> {path}"),
    )
    .await?;
    Ok(Json(FileOperationResult { path }))
}

pub async fn admin_delete_file(
    State(state): State<AppState>,
    Path(instance_id): Path<String>,
    headers: HeaderMap,
    Json(payload): Json<DeleteFileRequest>,
) -> AppResult<Json<FileOperationResult>> {
    let admin = require_admin(&state, &headers).await?;
    let response = dispatch_control(
        &state,
        &instance_id,
        FileRequest::Delete {
            path: payload.path,
            recursive: payload.recursive,
        },
        if payload.recursive {
            DELETE_TIMEOUT
        } else {
            CONTROL_TIMEOUT
        },
    )
    .await?;
    let FileResponse::OperationComplete { path } = response else {
        return Err(unexpected_response());
    };
    write_action_log(
        &state.db,
        &admin.username,
        "delete_instance_file",
        &instance_id,
        &format!(
            "永久删除 {}{}",
            path,
            if payload.recursive {
                "（递归）"
            } else {
                ""
            }
        ),
    )
    .await?;
    Ok(Json(FileOperationResult { path }))
}

pub async fn admin_upload_file(
    State(state): State<AppState>,
    Path(instance_id): Path<String>,
    Query(query): Query<UploadFileQuery>,
    headers: HeaderMap,
    body: Body,
) -> AppResult<Json<FileOperationResult>> {
    let admin = require_admin(&state, &headers).await?;
    let size_bytes = content_length(&headers)?;
    if size_bytes > state.file_transfer_max_bytes as u64 {
        return Err(AppError::new(
            StatusCode::PAYLOAD_TOO_LARGE,
            "文件超过允许的上传大小上限",
        ));
    }

    let (agent, mut events, mut cleanup) = register_request(&state, &instance_id, true).await?;
    let request_id = cleanup.request_id.clone();
    agent
        .tx
        .send(AgentOutbound::FileRequest {
            request_id: request_id.clone(),
            request: FileRequest::UploadStart {
                parent: query.parent,
                name: query.name,
                size_bytes,
                overwrite: query.overwrite,
                max_bytes: state.file_transfer_max_bytes as u64,
            },
        })
        .map_err(|_| disconnected_error())?;
    match wait_response(&mut events, CONTROL_TIMEOUT).await? {
        FileResponse::UploadReady { .. } => {}
        response => return Err(response_error_or_unexpected(response)),
    }

    let mut sequence = 0_u64;
    let mut transferred = 0_u64;
    let mut in_flight = VecDeque::new();
    let mut body = body.into_data_stream();
    while let Some(chunk) = body.next().await {
        let chunk = chunk.map_err(|error| {
            AppError::new(
                StatusCode::BAD_REQUEST,
                format!("读取上传内容失败：{error}"),
            )
        })?;
        let mut offset = 0;
        while offset < chunk.len() {
            while in_flight.len() >= TRANSFER_WINDOW {
                wait_upload_ack(&mut events, &mut in_flight).await?;
            }
            let end = (offset + CHUNK_SIZE).min(chunk.len());
            let data = &chunk[offset..end];
            transferred = transferred.saturating_add(data.len() as u64);
            if transferred > size_bytes || transferred > state.file_transfer_max_bytes as u64 {
                return Err(AppError::new(
                    StatusCode::PAYLOAD_TOO_LARGE,
                    "上传内容超过声明的文件大小",
                ));
            }
            agent
                .binary_tx
                .send(encode_chunk_frame(&request_id, sequence, data)?)
                .await
                .map_err(|_| disconnected_error())?;
            in_flight.push_back((sequence, transferred));
            sequence += 1;
            offset = end;
        }
    }
    if transferred != size_bytes {
        return Err(AppError::new(
            StatusCode::BAD_REQUEST,
            "上传内容长度与 Content-Length 不一致",
        ));
    }
    while !in_flight.is_empty() {
        wait_upload_ack(&mut events, &mut in_flight).await?;
    }

    agent
        .tx
        .send(AgentOutbound::FileTransferFinish {
            request_id: request_id.clone(),
        })
        .map_err(|_| disconnected_error())?;
    let response = wait_response(&mut events, TRANSFER_IDLE_TIMEOUT).await?;
    let FileResponse::TransferComplete {
        path,
        size_bytes: completed_size,
    } = response
    else {
        return Err(response_error_or_unexpected(response));
    };
    if completed_size != size_bytes {
        return Err(AppError::new(
            StatusCode::BAD_GATEWAY,
            "Agent 返回的上传文件大小不一致",
        ));
    }
    cleanup.release().await;
    write_action_log(
        &state.db,
        &admin.username,
        "upload_instance_file",
        &instance_id,
        &format!("上传文件 {path}（{size_bytes} 字节）"),
    )
    .await?;
    Ok(Json(FileOperationResult { path }))
}

pub async fn admin_download_file(
    State(state): State<AppState>,
    Path(instance_id): Path<String>,
    Query(query): Query<DownloadFileQuery>,
    headers: HeaderMap,
) -> AppResult<Response> {
    let admin = require_admin(&state, &headers).await?;
    let requested_path = query.path.clone();
    let (agent, mut events, cleanup) = register_request(&state, &instance_id, true).await?;
    let request_id = cleanup.request_id.clone();
    agent
        .tx
        .send(AgentOutbound::FileRequest {
            request_id: request_id.clone(),
            request: FileRequest::DownloadStart {
                path: query.path,
                max_bytes: state.file_transfer_max_bytes as u64,
            },
        })
        .map_err(|_| disconnected_error())?;
    let response = wait_response(&mut events, CONTROL_TIMEOUT).await?;
    let FileResponse::DownloadReady {
        path: _,
        name,
        size_bytes,
    } = response
    else {
        return Err(response_error_or_unexpected(response));
    };
    if size_bytes > state.file_transfer_max_bytes as u64 {
        return Err(AppError::new(
            StatusCode::PAYLOAD_TOO_LARGE,
            "文件超过允许的下载大小上限",
        ));
    }
    write_action_log(
        &state.db,
        &admin.username,
        "download_instance_file",
        &instance_id,
        &format!("下载文件 {requested_path}（{size_bytes} 字节）"),
    )
    .await?;

    let stream_agent = agent.clone();
    let stream_request_id = request_id.clone();
    let stream = stream! {
        let mut cleanup = cleanup;
        let mut expected_sequence = 0_u64;
        let mut transferred = 0_u64;
        loop {
            let event = match tokio::time::timeout(TRANSFER_IDLE_TIMEOUT, events.recv()).await {
                Ok(Some(event)) => event,
                Ok(None) => {
                    yield Err::<Bytes, io::Error>(io::Error::other("文件下载连接已关闭"));
                    break;
                }
                Err(_) => {
                    yield Err::<Bytes, io::Error>(io::Error::other("文件下载等待数据超时"));
                    break;
                }
            };
            match event {
                FileRequestEvent::Chunk { sequence, data } => {
                    if sequence != expected_sequence {
                        yield Err::<Bytes, io::Error>(io::Error::other("文件下载分块顺序无效"));
                        break;
                    }
                    transferred = transferred.saturating_add(data.len() as u64);
                    if transferred > size_bytes {
                        yield Err::<Bytes, io::Error>(io::Error::other("文件下载内容超过声明大小"));
                        break;
                    }
                    expected_sequence += 1;
                    yield Ok::<Bytes, io::Error>(Bytes::from(data));
                    if stream_agent
                        .tx
                        .send(AgentOutbound::FileTransferAck {
                            request_id: stream_request_id.clone(),
                            sequence,
                        })
                        .is_err()
                    {
                        yield Err::<Bytes, io::Error>(io::Error::other("实例连接已断开"));
                        break;
                    }
                }
                FileRequestEvent::Response(FileResponse::TransferComplete {
                    size_bytes: completed_size,
                    ..
                }) => {
                    if completed_size != size_bytes || transferred != size_bytes {
                        yield Err::<Bytes, io::Error>(io::Error::other("文件下载内容长度不一致"));
                        break;
                    }
                    cleanup.release().await;
                    break;
                }
                FileRequestEvent::Response(FileResponse::Error { message, .. }) => {
                    yield Err::<Bytes, io::Error>(io::Error::other(message));
                    break;
                }
                FileRequestEvent::Disconnected => {
                    yield Err::<Bytes, io::Error>(io::Error::other("实例连接已断开"));
                    break;
                }
                FileRequestEvent::Response(_) => {
                    yield Err::<Bytes, io::Error>(io::Error::other("收到无效的文件下载响应"));
                    break;
                }
            }
        }
    };

    let disposition = content_disposition(&name)?;
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "application/octet-stream")
        .header(header::CONTENT_LENGTH, size_bytes)
        .header(header::CONTENT_DISPOSITION, disposition)
        .header(header::CACHE_CONTROL, "no-store")
        .body(Body::from_stream(stream))
        .map_err(|error| {
            AppError::new(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("创建下载响应失败：{error}"),
            )
        })
}

pub async fn handle_agent_file_response(
    state: &AppState,
    instance_id: &str,
    connection_id: Uuid,
    request_id: &str,
    response: FileResponse,
) {
    let pending = state
        .file_requests
        .read()
        .await
        .get(request_id)
        .filter(|pending| {
            pending.instance_id == instance_id && pending.agent_connection_id == connection_id
        })
        .cloned();
    if let Some(pending) = pending {
        let _ = pending.tx.try_send(FileRequestEvent::Response(response));
    }
}

pub async fn handle_agent_file_binary(
    state: &AppState,
    instance_id: &str,
    connection_id: Uuid,
    frame: &[u8],
) {
    let Ok((request_id, sequence, data)) = decode_chunk_frame(frame) else {
        warn!(%instance_id, "ignored invalid agent file chunk");
        return;
    };
    let pending = state
        .file_requests
        .read()
        .await
        .get(&request_id)
        .filter(|pending| {
            pending.instance_id == instance_id && pending.agent_connection_id == connection_id
        })
        .cloned();
    if let Some(pending) = pending
        && pending
            .tx
            .try_send(FileRequestEvent::Chunk { sequence, data })
            .is_err()
        && let Some(agent) = state.agents.read().await.get(instance_id).cloned()
    {
        let _ = agent
            .tx
            .send(AgentOutbound::FileTransferCancel { request_id });
    }
}

pub async fn close_connection_file_requests(
    state: &AppState,
    instance_id: &str,
    connection_id: Uuid,
) {
    let disconnected = {
        let mut requests = state.file_requests.write().await;
        let ids = requests
            .iter()
            .filter(|(_, pending)| {
                pending.instance_id == instance_id && pending.agent_connection_id == connection_id
            })
            .map(|(id, _)| id.clone())
            .collect::<Vec<_>>();
        ids.into_iter()
            .filter_map(|id| requests.remove(&id))
            .collect::<Vec<_>>()
    };
    for pending in disconnected {
        let _ = pending.tx.try_send(FileRequestEvent::Disconnected);
    }
}

async fn dispatch_control(
    state: &AppState,
    instance_id: &str,
    request: FileRequest,
    timeout: Duration,
) -> AppResult<FileResponse> {
    let (agent, mut events, mut cleanup) = register_request(state, instance_id, false).await?;
    let request_id = cleanup.request_id.clone();
    agent
        .tx
        .send(AgentOutbound::FileRequest {
            request_id,
            request,
        })
        .map_err(|_| disconnected_error())?;
    let response = wait_response(&mut events, timeout).await?;
    cleanup.release().await;
    Ok(response)
}

async fn register_request(
    state: &AppState,
    instance_id: &str,
    transfer: bool,
) -> AppResult<(
    AgentHandle,
    mpsc::Receiver<FileRequestEvent>,
    RequestCleanup,
)> {
    get_instance(&state.db, instance_id).await?;
    let agent = state
        .agents
        .read()
        .await
        .get(instance_id)
        .cloned()
        .ok_or_else(|| AppError::new(StatusCode::CONFLICT, "实例不在线"))?;
    if !agent
        .capabilities
        .iter()
        .any(|capability| capability == FILE_MANAGER_CAPABILITY)
    {
        return Err(AppError::new(
            StatusCode::CONFLICT,
            "当前 Agent 版本不支持文件管理，请先更新 Agent",
        ));
    }

    let request_id = Uuid::new_v4().to_string();
    if transfer {
        let mut active = state.active_file_transfers.write().await;
        if active.contains_key(instance_id) {
            return Err(AppError::new(
                StatusCode::CONFLICT,
                "该实例已有文件正在传输",
            ));
        }
        active.insert(instance_id.to_string(), request_id.clone());
    }

    let (tx, rx) = mpsc::channel(TRANSFER_WINDOW * 2 + 4);
    state.file_requests.write().await.insert(
        request_id.clone(),
        PendingFileRequest {
            instance_id: instance_id.to_string(),
            agent_connection_id: agent.connection_id,
            tx,
        },
    );
    let cleanup = RequestCleanup {
        state: state.clone(),
        instance_id: instance_id.to_string(),
        connection_id: agent.connection_id,
        request_id,
        transfer,
        cancel_on_drop: transfer,
        released: false,
    };
    Ok((agent, rx, cleanup))
}

async fn wait_response(
    events: &mut mpsc::Receiver<FileRequestEvent>,
    timeout: Duration,
) -> AppResult<FileResponse> {
    let event = tokio::time::timeout(timeout, events.recv())
        .await
        .map_err(|_| AppError::new(StatusCode::GATEWAY_TIMEOUT, "等待 Agent 文件响应超时"))?
        .ok_or_else(disconnected_error)?;
    match event {
        FileRequestEvent::Response(FileResponse::Error { code, message }) => {
            Err(file_error(code, message))
        }
        FileRequestEvent::Response(response) => Ok(response),
        FileRequestEvent::Disconnected => Err(disconnected_error()),
        FileRequestEvent::Chunk { .. } => Err(unexpected_response()),
    }
}

async fn wait_upload_ack(
    events: &mut mpsc::Receiver<FileRequestEvent>,
    in_flight: &mut VecDeque<(u64, u64)>,
) -> AppResult<()> {
    let response = wait_response(events, TRANSFER_IDLE_TIMEOUT).await?;
    match response {
        FileResponse::TransferAck {
            sequence,
            transferred_bytes,
        } => {
            let (expected_sequence, expected_bytes) =
                in_flight.pop_front().ok_or_else(unexpected_response)?;
            if sequence != expected_sequence || transferred_bytes != expected_bytes {
                return Err(AppError::new(
                    StatusCode::BAD_GATEWAY,
                    "Agent 返回的上传分块确认顺序或字节数无效",
                ));
            }
            Ok(())
        }
        response => Err(response_error_or_unexpected(response)),
    }
}

fn response_error_or_unexpected(response: FileResponse) -> AppError {
    match response {
        FileResponse::Error { code, message } => file_error(code, message),
        _ => unexpected_response(),
    }
}

fn file_error(code: FileErrorCode, message: String) -> AppError {
    let status = match code {
        FileErrorCode::InvalidPath | FileErrorCode::NotDirectory | FileErrorCode::IsDirectory => {
            StatusCode::BAD_REQUEST
        }
        FileErrorCode::NotFound => StatusCode::NOT_FOUND,
        FileErrorCode::PermissionDenied => StatusCode::FORBIDDEN,
        FileErrorCode::AlreadyExists | FileErrorCode::Busy => StatusCode::CONFLICT,
        FileErrorCode::TooLarge => StatusCode::PAYLOAD_TOO_LARGE,
        FileErrorCode::Unsupported => StatusCode::UNPROCESSABLE_ENTITY,
        FileErrorCode::Io => StatusCode::BAD_GATEWAY,
    };
    AppError::new(status, message)
}

fn unexpected_response() -> AppError {
    AppError::new(StatusCode::BAD_GATEWAY, "Agent 返回了无效的文件响应")
}

fn disconnected_error() -> AppError {
    AppError::new(StatusCode::CONFLICT, "实例连接已断开")
}

fn content_length(headers: &HeaderMap) -> AppResult<u64> {
    headers
        .get(header::CONTENT_LENGTH)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<u64>().ok())
        .ok_or_else(|| {
            AppError::new(
                StatusCode::LENGTH_REQUIRED,
                "上传文件必须提供有效的 Content-Length",
            )
        })
}

fn content_disposition(name: &str) -> AppResult<HeaderValue> {
    let fallback = name
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || matches!(character, '.' | '-' | '_') {
                character
            } else {
                '_'
            }
        })
        .collect::<String>();
    let fallback = if fallback.is_empty() {
        "download".to_string()
    } else {
        fallback
    };
    HeaderValue::from_str(&format!(
        "attachment; filename=\"{fallback}\"; filename*=UTF-8''{}",
        urlencoding::encode(name)
    ))
    .map_err(|_| AppError::new(StatusCode::BAD_GATEWAY, "下载文件名格式无效"))
}

fn encode_chunk_frame(request_id: &str, sequence: u64, data: &[u8]) -> AppResult<Vec<u8>> {
    if data.len() > CHUNK_SIZE {
        return Err(AppError::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            "文件分块超过协议上限",
        ));
    }
    let request_id = Uuid::parse_str(request_id).map_err(|_| unexpected_response())?;
    let mut frame = Vec::with_capacity(FRAME_HEADER_BYTES + data.len());
    frame.push(FRAME_KIND_FILE_CHUNK_V1);
    frame.extend_from_slice(request_id.as_bytes());
    frame.extend_from_slice(&sequence.to_be_bytes());
    frame.extend_from_slice(data);
    Ok(frame)
}

fn decode_chunk_frame(frame: &[u8]) -> Result<(String, u64, Vec<u8>), ()> {
    if frame.len() < FRAME_HEADER_BYTES
        || frame[0] != FRAME_KIND_FILE_CHUNK_V1
        || frame.len() - FRAME_HEADER_BYTES > CHUNK_SIZE
    {
        return Err(());
    }
    let request_id = Uuid::from_slice(&frame[1..17]).map_err(|_| ())?;
    let sequence = u64::from_be_bytes(frame[17..25].try_into().map_err(|_| ())?);
    Ok((
        request_id.to_string(),
        sequence,
        frame[FRAME_HEADER_BYTES..].to_vec(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn binary_file_frames_round_trip() {
        let request_id = Uuid::new_v4().to_string();
        let frame = encode_chunk_frame(&request_id, 9, b"payload").unwrap();
        assert_eq!(
            decode_chunk_frame(&frame).unwrap(),
            (request_id, 9, b"payload".to_vec())
        );
    }

    #[test]
    fn file_errors_map_to_stable_http_statuses() {
        assert_eq!(
            file_error(FileErrorCode::PermissionDenied, "denied".to_string()).status,
            StatusCode::FORBIDDEN
        );
        assert_eq!(
            file_error(FileErrorCode::AlreadyExists, "exists".to_string()).status,
            StatusCode::CONFLICT
        );
        assert_eq!(
            file_error(FileErrorCode::TooLarge, "large".to_string()).status,
            StatusCode::PAYLOAD_TOO_LARGE
        );
    }
}
