use std::{
    collections::{HashMap, VecDeque},
    fs,
    io::ErrorKind,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::UNIX_EPOCH,
};

use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    sync::mpsc,
    task::JoinHandle,
};
use uuid::Uuid;

use crate::{
    activity::ActivityTracker,
    models::{
        AgentInbound, FileEntry, FileEntryKind, FileErrorCode, FileListing, FileRequest,
        FileResponse, FileSystemRoot,
    },
};

pub const CAPABILITY: &str = "file_manager_v1";
pub const CHUNK_SIZE: usize = 256 * 1024;
const TRANSFER_WINDOW: usize = 4;
const FRAME_KIND_FILE_CHUNK_V1: u8 = 1;
const FRAME_HEADER_BYTES: usize = 1 + 16 + 8;

pub struct FileManager {
    outbound: mpsc::UnboundedSender<AgentInbound>,
    binary_outbound: mpsc::Sender<Vec<u8>>,
    activity: ActivityTracker,
    transfers: HashMap<String, ActiveTransfer>,
}

struct ActiveTransfer {
    control: TransferControl,
    task: JoinHandle<()>,
    active: Arc<AtomicBool>,
}

enum TransferControl {
    Upload(mpsc::Sender<UploadEvent>),
    Download(mpsc::Sender<u64>),
}

enum UploadEvent {
    Chunk { sequence: u64, data: Vec<u8> },
    Finish,
}

#[derive(Debug)]
struct FileFailure {
    code: FileErrorCode,
    message: String,
}

impl FileFailure {
    fn new(code: FileErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
}

struct UploadCleanup {
    path: PathBuf,
    committed: bool,
}

impl Drop for UploadCleanup {
    fn drop(&mut self) {
        if !self.committed {
            let _ = fs::remove_file(&self.path);
        }
    }
}

impl FileManager {
    pub fn new(
        outbound: mpsc::UnboundedSender<AgentInbound>,
        binary_outbound: mpsc::Sender<Vec<u8>>,
        activity: ActivityTracker,
    ) -> Self {
        Self {
            outbound,
            binary_outbound,
            activity,
            transfers: HashMap::new(),
        }
    }

    pub fn handle_request(&mut self, request_id: String, request: FileRequest) {
        self.prune();
        match request {
            FileRequest::UploadStart {
                parent,
                name,
                size_bytes,
                overwrite,
                max_bytes,
            } => self.start_upload(request_id, parent, name, size_bytes, overwrite, max_bytes),
            FileRequest::DownloadStart { path, max_bytes } => {
                self.start_download(request_id, path, max_bytes)
            }
            request => self.spawn_control_request(request_id, request),
        }
    }

    pub fn handle_binary(&mut self, frame: &[u8]) {
        self.prune();
        let Ok((request_id, sequence, data)) = decode_chunk_frame(frame) else {
            return;
        };
        let sender = self
            .transfers
            .get(&request_id)
            .and_then(|transfer| match &transfer.control {
                TransferControl::Upload(sender) => Some(sender.clone()),
                TransferControl::Download(_) => None,
            });
        let Some(sender) = sender else {
            return;
        };
        if data.len() > CHUNK_SIZE
            || sender
                .try_send(UploadEvent::Chunk { sequence, data })
                .is_err()
        {
            self.fail_and_cancel(
                &request_id,
                FileFailure::new(FileErrorCode::Busy, "文件上传数据超过允许的传输窗口"),
            );
        }
    }

    pub fn finish_upload(&mut self, request_id: &str) {
        self.prune();
        let sender = self
            .transfers
            .get(request_id)
            .and_then(|transfer| match &transfer.control {
                TransferControl::Upload(sender) => Some(sender.clone()),
                TransferControl::Download(_) => None,
            });
        if sender.is_some_and(|sender| sender.try_send(UploadEvent::Finish).is_err()) {
            self.fail_and_cancel(
                request_id,
                FileFailure::new(FileErrorCode::Busy, "文件上传尚未处理完毕"),
            );
        }
    }

    pub fn acknowledge_download(&mut self, request_id: &str, sequence: u64) {
        self.prune();
        let sender = self
            .transfers
            .get(request_id)
            .and_then(|transfer| match &transfer.control {
                TransferControl::Download(sender) => Some(sender.clone()),
                TransferControl::Upload(_) => None,
            });
        if sender.is_some_and(|sender| sender.try_send(sequence).is_err()) {
            self.fail_and_cancel(
                request_id,
                FileFailure::new(FileErrorCode::Busy, "文件下载确认窗口已满"),
            );
        }
    }

    pub fn cancel(&mut self, request_id: &str) {
        if let Some(transfer) = self.transfers.remove(request_id) {
            transfer.task.abort();
        }
    }

    pub fn close_all(&mut self) {
        for (_, transfer) in self.transfers.drain() {
            transfer.task.abort();
        }
    }

    fn prune(&mut self) {
        self.transfers
            .retain(|_, transfer| transfer.active.load(Ordering::SeqCst));
    }

    fn has_active_transfer(&self) -> bool {
        self.transfers
            .values()
            .any(|transfer| transfer.active.load(Ordering::SeqCst))
    }

    fn fail_and_cancel(&mut self, request_id: &str, failure: FileFailure) {
        self.cancel(request_id);
        send_failure(&self.outbound, request_id, failure);
    }

    fn spawn_control_request(&self, request_id: String, request: FileRequest) {
        let Some(activity_guard) = self.activity.try_enter() else {
            send_failure(
                &self.outbound,
                &request_id,
                FileFailure::new(FileErrorCode::Busy, "Agent 更新即将安装，请稍后再试"),
            );
            return;
        };
        let outbound = self.outbound.clone();
        tokio::spawn(async move {
            let result =
                tokio::task::spawn_blocking(move || execute_control_request(request)).await;
            drop(activity_guard);
            match result {
                Ok(Ok(response)) => send_response(&outbound, &request_id, response),
                Ok(Err(error)) => send_failure(&outbound, &request_id, error),
                Err(error) => send_failure(
                    &outbound,
                    &request_id,
                    FileFailure::new(FileErrorCode::Io, format!("文件操作任务异常结束：{error}")),
                ),
            }
        });
    }

    #[allow(clippy::too_many_arguments)]
    fn start_upload(
        &mut self,
        request_id: String,
        parent: String,
        name: String,
        size_bytes: u64,
        overwrite: bool,
        max_bytes: u64,
    ) {
        if self.has_active_transfer() {
            send_failure(
                &self.outbound,
                &request_id,
                FileFailure::new(FileErrorCode::Busy, "该实例已有文件正在传输"),
            );
            return;
        }
        if size_bytes > max_bytes {
            send_failure(
                &self.outbound,
                &request_id,
                FileFailure::new(FileErrorCode::TooLarge, "文件超过允许的大小上限"),
            );
            return;
        }
        let Some(activity_guard) = self.activity.try_enter() else {
            send_failure(
                &self.outbound,
                &request_id,
                FileFailure::new(FileErrorCode::Busy, "Agent 更新即将安装，请稍后再试"),
            );
            return;
        };

        let (control_tx, control_rx) = mpsc::channel(TRANSFER_WINDOW);
        let outbound = self.outbound.clone();
        let active = Arc::new(AtomicBool::new(true));
        let task_active = active.clone();
        let task_request_id = request_id.clone();
        let task = tokio::spawn(async move {
            let result = run_upload(
                &task_request_id,
                parent,
                name,
                size_bytes,
                overwrite,
                control_rx,
                &outbound,
            )
            .await;
            drop(activity_guard);
            if let Err(error) = result {
                send_failure(&outbound, &task_request_id, error);
            }
            task_active.store(false, Ordering::SeqCst);
        });
        self.transfers.insert(
            request_id,
            ActiveTransfer {
                control: TransferControl::Upload(control_tx),
                task,
                active,
            },
        );
    }

    fn start_download(&mut self, request_id: String, path: String, max_bytes: u64) {
        if self.has_active_transfer() {
            send_failure(
                &self.outbound,
                &request_id,
                FileFailure::new(FileErrorCode::Busy, "该实例已有文件正在传输"),
            );
            return;
        }
        let Some(activity_guard) = self.activity.try_enter() else {
            send_failure(
                &self.outbound,
                &request_id,
                FileFailure::new(FileErrorCode::Busy, "Agent 更新即将安装，请稍后再试"),
            );
            return;
        };

        let (ack_tx, ack_rx) = mpsc::channel(TRANSFER_WINDOW);
        let outbound = self.outbound.clone();
        let binary_outbound = self.binary_outbound.clone();
        let active = Arc::new(AtomicBool::new(true));
        let task_active = active.clone();
        let task_request_id = request_id.clone();
        let task = tokio::spawn(async move {
            let result = run_download(
                &task_request_id,
                path,
                max_bytes,
                ack_rx,
                &outbound,
                binary_outbound,
            )
            .await;
            drop(activity_guard);
            if let Err(error) = result {
                send_failure(&outbound, &task_request_id, error);
            }
            task_active.store(false, Ordering::SeqCst);
        });
        self.transfers.insert(
            request_id,
            ActiveTransfer {
                control: TransferControl::Download(ack_tx),
                task,
                active,
            },
        );
    }
}

fn execute_control_request(request: FileRequest) -> Result<FileResponse, FileFailure> {
    match request {
        FileRequest::Roots => Ok(FileResponse::Roots {
            roots: filesystem_roots(),
        }),
        FileRequest::List {
            path,
            offset,
            limit,
        } => Ok(FileResponse::Listing {
            listing: list_directory(&path, offset, limit)?,
        }),
        FileRequest::CreateDirectory { parent, name } => {
            let path = create_directory(&parent, &name)?;
            Ok(FileResponse::OperationComplete { path })
        }
        FileRequest::Move {
            source,
            destination_parent,
            name,
            overwrite,
        } => {
            let path = move_path(&source, &destination_parent, &name, overwrite)?;
            Ok(FileResponse::OperationComplete { path })
        }
        FileRequest::Delete { path, recursive } => {
            delete_path(&path, recursive)?;
            Ok(FileResponse::OperationComplete { path })
        }
        FileRequest::UploadStart { .. } | FileRequest::DownloadStart { .. } => Err(
            FileFailure::new(FileErrorCode::Unsupported, "无效的文件控制请求"),
        ),
    }
}

fn filesystem_roots() -> Vec<FileSystemRoot> {
    #[cfg(windows)]
    {
        let roots = (b'A'..=b'Z')
            .filter_map(|letter| {
                let path = format!("{}:\\", char::from(letter));
                Path::new(&path).exists().then(|| FileSystemRoot {
                    label: path.clone(),
                    path,
                })
            })
            .collect::<Vec<_>>();
        if roots.is_empty() {
            vec![FileSystemRoot {
                path: "C:\\".to_string(),
                label: "C:\\".to_string(),
            }]
        } else {
            roots
        }
    }

    #[cfg(not(windows))]
    {
        vec![FileSystemRoot {
            path: "/".to_string(),
            label: "/".to_string(),
        }]
    }
}

fn list_directory(path: &str, offset: u64, limit: u64) -> Result<FileListing, FileFailure> {
    let path = absolute_path(path)?;
    let metadata = fs::metadata(&path).map_err(|error| io_failure(error, "读取目录失败"))?;
    if !metadata.is_dir() {
        return Err(FileFailure::new(
            FileErrorCode::NotDirectory,
            "目标不是目录",
        ));
    }

    let mut entries = fs::read_dir(&path)
        .map_err(|error| io_failure(error, "读取目录失败"))?
        .map(|entry| {
            let entry = entry.map_err(|error| io_failure(error, "读取目录项失败"))?;
            file_entry(entry.path())
        })
        .collect::<Result<Vec<_>, _>>()?;
    entries.sort_by(|left, right| {
        entry_sort_rank(&left.kind)
            .cmp(&entry_sort_rank(&right.kind))
            .then_with(|| left.name.to_lowercase().cmp(&right.name.to_lowercase()))
            .then_with(|| left.name.cmp(&right.name))
    });

    let total = entries.len() as u64;
    let offset = offset.min(total);
    let limit = limit.clamp(1, 500);
    let end = offset.saturating_add(limit).min(total);
    let entries = entries[offset as usize..end as usize].to_vec();
    let parent = if is_root_path(&path) {
        None
    } else {
        path.parent().map(display_path)
    };
    Ok(FileListing {
        path: display_path(&path),
        parent,
        entries,
        offset,
        limit,
        total,
    })
}

fn file_entry(path: PathBuf) -> Result<FileEntry, FileFailure> {
    let metadata =
        fs::symlink_metadata(&path).map_err(|error| io_failure(error, "读取文件信息失败"))?;
    let file_type = metadata.file_type();
    let kind = if file_type.is_symlink() {
        FileEntryKind::Symlink
    } else if file_type.is_dir() {
        FileEntryKind::Directory
    } else if file_type.is_file() {
        FileEntryKind::File
    } else {
        FileEntryKind::Other
    };
    let modified_at = metadata
        .modified()
        .ok()
        .and_then(|value| value.duration_since(UNIX_EPOCH).ok())
        .and_then(|value| i64::try_from(value.as_secs()).ok());
    Ok(FileEntry {
        name: path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| display_path(&path)),
        path: display_path(&path),
        kind,
        size_bytes: if metadata.is_file() {
            metadata.len()
        } else {
            0
        },
        modified_at,
        readonly: metadata.permissions().readonly(),
    })
}

fn entry_sort_rank(kind: &FileEntryKind) -> u8 {
    match kind {
        FileEntryKind::Directory => 0,
        FileEntryKind::Symlink => 1,
        FileEntryKind::File => 2,
        FileEntryKind::Other => 3,
    }
}

fn create_directory(parent: &str, name: &str) -> Result<String, FileFailure> {
    let parent = require_directory(parent)?;
    validate_name(name)?;
    let path = parent.join(name);
    fs::create_dir(&path).map_err(|error| io_failure(error, "创建目录失败"))?;
    Ok(display_path(&path))
}

fn move_path(
    source: &str,
    destination_parent: &str,
    name: &str,
    overwrite: bool,
) -> Result<String, FileFailure> {
    let source = absolute_path(source)?;
    if is_root_path(&source) {
        return Err(FileFailure::new(
            FileErrorCode::InvalidPath,
            "文件系统根目录不能移动或重命名",
        ));
    }
    let source_metadata =
        fs::symlink_metadata(&source).map_err(|error| io_failure(error, "读取源文件失败"))?;
    let destination_parent = require_directory(destination_parent)?;
    validate_name(name)?;
    let destination = destination_parent.join(name);
    if source == destination {
        return Ok(display_path(&destination));
    }

    if source_metadata.is_dir() && !source_metadata.file_type().is_symlink() {
        let canonical_source =
            fs::canonicalize(&source).map_err(|error| io_failure(error, "读取源目录失败"))?;
        let canonical_parent = fs::canonicalize(&destination_parent)
            .map_err(|error| io_failure(error, "读取目标目录失败"))?;
        if canonical_parent.starts_with(&canonical_source) {
            return Err(FileFailure::new(
                FileErrorCode::InvalidPath,
                "目录不能移动到自身内部",
            ));
        }
    }

    let destination_metadata = fs::symlink_metadata(&destination).ok();
    if let Some(metadata) = &destination_metadata {
        if !overwrite {
            return Err(FileFailure::new(
                FileErrorCode::AlreadyExists,
                "目标位置已存在同名文件或目录",
            ));
        }
        if source_metadata.is_dir() || metadata.is_dir() {
            return Err(FileFailure::new(
                FileErrorCode::AlreadyExists,
                "目录不能覆盖已有目标",
            ));
        }
    }

    let backup = destination_metadata
        .map(|_| destination_parent.join(format!(".om-replace-{}.bak", Uuid::new_v4())));
    if let Some(backup) = &backup {
        fs::rename(&destination, backup)
            .map_err(|error| io_failure(error, "准备覆盖目标文件失败"))?;
    }
    if let Err(error) = fs::rename(&source, &destination) {
        if let Some(backup) = &backup {
            let _ = fs::rename(backup, &destination);
        }
        return Err(rename_failure(error));
    }
    if let Some(backup) = &backup {
        let _ = fs::remove_file(backup);
    }
    Ok(display_path(&destination))
}

fn delete_path(path: &str, recursive: bool) -> Result<(), FileFailure> {
    let path = absolute_path(path)?;
    if is_root_path(&path) {
        return Err(FileFailure::new(
            FileErrorCode::InvalidPath,
            "文件系统根目录不能删除",
        ));
    }
    let metadata =
        fs::symlink_metadata(&path).map_err(|error| io_failure(error, "读取待删除目标失败"))?;
    if metadata.is_dir() && !metadata.file_type().is_symlink() {
        if recursive {
            fs::remove_dir_all(&path).map_err(|error| io_failure(error, "删除目录失败"))
        } else {
            fs::remove_dir(&path).map_err(|error| io_failure(error, "删除目录失败"))
        }
    } else {
        fs::remove_file(&path).map_err(|error| io_failure(error, "删除文件失败"))
    }
}

async fn run_upload(
    request_id: &str,
    parent: String,
    name: String,
    expected_size: u64,
    overwrite: bool,
    mut events: mpsc::Receiver<UploadEvent>,
    outbound: &mpsc::UnboundedSender<AgentInbound>,
) -> Result<(), FileFailure> {
    let parent = require_directory_async(&parent).await?;
    validate_name(&name)?;
    let target = parent.join(&name);
    if let Ok(metadata) = tokio::fs::symlink_metadata(&target).await {
        if metadata.is_dir() {
            return Err(FileFailure::new(
                FileErrorCode::IsDirectory,
                "上传目标是目录",
            ));
        }
        if !overwrite {
            return Err(FileFailure::new(
                FileErrorCode::AlreadyExists,
                "目标位置已存在同名文件",
            ));
        }
    }

    let temporary = parent.join(format!(".om-upload-{request_id}.part"));
    let mut file = tokio::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temporary)
        .await
        .map_err(|error| io_failure(error, "创建上传临时文件失败"))?;
    let mut cleanup = UploadCleanup {
        path: temporary.clone(),
        committed: false,
    };
    send_response(
        outbound,
        request_id,
        FileResponse::UploadReady {
            path: display_path(&target),
        },
    );

    let mut next_sequence = 0_u64;
    let mut received = 0_u64;
    while let Some(event) = events.recv().await {
        match event {
            UploadEvent::Chunk { sequence, data } => {
                if sequence != next_sequence {
                    return Err(FileFailure::new(FileErrorCode::Io, "上传分块顺序无效"));
                }
                let data_len = data.len() as u64;
                if received.saturating_add(data_len) > expected_size {
                    return Err(FileFailure::new(
                        FileErrorCode::TooLarge,
                        "上传内容超过声明的文件大小",
                    ));
                }
                file.write_all(&data)
                    .await
                    .map_err(|error| io_failure(error, "写入上传文件失败"))?;
                received += data_len;
                send_response(
                    outbound,
                    request_id,
                    FileResponse::TransferAck {
                        sequence,
                        transferred_bytes: received,
                    },
                );
                next_sequence += 1;
            }
            UploadEvent::Finish => {
                if received != expected_size {
                    return Err(FileFailure::new(
                        FileErrorCode::Io,
                        "上传内容长度与声明大小不一致",
                    ));
                }
                file.flush()
                    .await
                    .map_err(|error| io_failure(error, "刷新上传文件失败"))?;
                file.sync_data()
                    .await
                    .map_err(|error| io_failure(error, "同步上传文件失败"))?;
                drop(file);
                commit_upload(&temporary, &target, overwrite).await?;
                cleanup.committed = true;
                send_response(
                    outbound,
                    request_id,
                    FileResponse::TransferComplete {
                        path: display_path(&target),
                        size_bytes: received,
                    },
                );
                return Ok(());
            }
        }
    }
    Err(FileFailure::new(FileErrorCode::Io, "上传连接已中断"))
}

async fn commit_upload(
    temporary: &Path,
    target: &Path,
    overwrite: bool,
) -> Result<(), FileFailure> {
    let existing = tokio::fs::symlink_metadata(target).await.ok();
    if let Some(metadata) = &existing {
        if metadata.is_dir() {
            return Err(FileFailure::new(
                FileErrorCode::IsDirectory,
                "上传目标是目录",
            ));
        }
        if !overwrite {
            return Err(FileFailure::new(
                FileErrorCode::AlreadyExists,
                "目标位置已存在同名文件",
            ));
        }
    }

    let backup = existing.map(|_| {
        target
            .parent()
            .unwrap_or_else(|| Path::new("."))
            .join(format!(".om-replace-{}.bak", Uuid::new_v4()))
    });
    if let Some(backup) = &backup {
        tokio::fs::rename(target, backup)
            .await
            .map_err(|error| io_failure(error, "准备覆盖目标文件失败"))?;
    }
    if let Err(error) = tokio::fs::rename(temporary, target).await {
        if let Some(backup) = &backup {
            let _ = tokio::fs::rename(backup, target).await;
        }
        return Err(rename_failure(error));
    }
    if let Some(backup) = &backup {
        let _ = tokio::fs::remove_file(backup).await;
    }
    Ok(())
}

async fn run_download(
    request_id: &str,
    path: String,
    max_bytes: u64,
    mut acknowledgements: mpsc::Receiver<u64>,
    outbound: &mpsc::UnboundedSender<AgentInbound>,
    binary_outbound: mpsc::Sender<Vec<u8>>,
) -> Result<(), FileFailure> {
    let path = absolute_path(&path)?;
    let metadata = tokio::fs::metadata(&path)
        .await
        .map_err(|error| io_failure(error, "读取下载文件失败"))?;
    if metadata.is_dir() {
        return Err(FileFailure::new(
            FileErrorCode::IsDirectory,
            "目录不能直接下载",
        ));
    }
    if !metadata.is_file() {
        return Err(FileFailure::new(
            FileErrorCode::Unsupported,
            "该文件类型不支持下载",
        ));
    }
    if metadata.len() > max_bytes {
        return Err(FileFailure::new(
            FileErrorCode::TooLarge,
            "文件超过允许的下载大小上限",
        ));
    }

    let size_bytes = metadata.len();
    let name = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "download".to_string());
    let mut file = tokio::fs::File::open(&path)
        .await
        .map_err(|error| io_failure(error, "打开下载文件失败"))?;
    send_response(
        outbound,
        request_id,
        FileResponse::DownloadReady {
            path: display_path(&path),
            name,
            size_bytes,
        },
    );

    let mut remaining = size_bytes;
    let mut sequence = 0_u64;
    let mut in_flight = VecDeque::new();
    while remaining > 0 || !in_flight.is_empty() {
        while remaining > 0 && in_flight.len() < TRANSFER_WINDOW {
            let next_len = usize::try_from(remaining.min(CHUNK_SIZE as u64)).unwrap_or(CHUNK_SIZE);
            let mut data = vec![0_u8; next_len];
            file.read_exact(&mut data)
                .await
                .map_err(|error| io_failure(error, "读取下载文件失败"))?;
            let frame = encode_chunk_frame(request_id, sequence, &data)?;
            binary_outbound
                .send(frame)
                .await
                .map_err(|_| FileFailure::new(FileErrorCode::Io, "文件下载连接已中断"))?;
            in_flight.push_back(sequence);
            sequence += 1;
            remaining -= data.len() as u64;
        }

        if let Some(expected) = in_flight.front().copied() {
            let received = acknowledgements
                .recv()
                .await
                .ok_or_else(|| FileFailure::new(FileErrorCode::Io, "文件下载连接已中断"))?;
            if received != expected {
                return Err(FileFailure::new(FileErrorCode::Io, "下载分块确认顺序无效"));
            }
            in_flight.pop_front();
        }
    }

    send_response(
        outbound,
        request_id,
        FileResponse::TransferComplete {
            path: display_path(&path),
            size_bytes,
        },
    );
    Ok(())
}

fn encode_chunk_frame(
    request_id: &str,
    sequence: u64,
    data: &[u8],
) -> Result<Vec<u8>, FileFailure> {
    if data.len() > CHUNK_SIZE {
        return Err(FileFailure::new(
            FileErrorCode::TooLarge,
            "文件分块超过协议上限",
        ));
    }
    let request_id = Uuid::parse_str(request_id)
        .map_err(|_| FileFailure::new(FileErrorCode::InvalidPath, "文件请求编号格式无效"))?;
    let mut frame = Vec::with_capacity(FRAME_HEADER_BYTES + data.len());
    frame.push(FRAME_KIND_FILE_CHUNK_V1);
    frame.extend_from_slice(request_id.as_bytes());
    frame.extend_from_slice(&sequence.to_be_bytes());
    frame.extend_from_slice(data);
    Ok(frame)
}

fn decode_chunk_frame(frame: &[u8]) -> Result<(String, u64, Vec<u8>), FileFailure> {
    if frame.len() < FRAME_HEADER_BYTES || frame[0] != FRAME_KIND_FILE_CHUNK_V1 {
        return Err(FileFailure::new(FileErrorCode::Io, "文件分块协议头无效"));
    }
    let request_id = Uuid::from_slice(&frame[1..17])
        .map_err(|_| FileFailure::new(FileErrorCode::Io, "文件分块请求编号无效"))?;
    let sequence = u64::from_be_bytes(
        frame[17..25]
            .try_into()
            .map_err(|_| FileFailure::new(FileErrorCode::Io, "文件分块序号无效"))?,
    );
    let data = frame[FRAME_HEADER_BYTES..].to_vec();
    if data.len() > CHUNK_SIZE {
        return Err(FileFailure::new(
            FileErrorCode::TooLarge,
            "文件分块超过协议上限",
        ));
    }
    Ok((request_id.to_string(), sequence, data))
}

fn absolute_path(value: &str) -> Result<PathBuf, FileFailure> {
    if value.is_empty() || value.contains('\0') {
        return Err(FileFailure::new(
            FileErrorCode::InvalidPath,
            "文件路径不能为空或包含空字符",
        ));
    }
    let path = PathBuf::from(value);
    if !path.is_absolute() {
        return Err(FileFailure::new(
            FileErrorCode::InvalidPath,
            "文件路径必须是绝对路径",
        ));
    }
    if path.components().any(|component| {
        matches!(
            component,
            std::path::Component::CurDir | std::path::Component::ParentDir
        )
    }) {
        return Err(FileFailure::new(
            FileErrorCode::InvalidPath,
            "文件路径不能包含 . 或 .. 路径段",
        ));
    }
    Ok(path)
}

fn require_directory(value: &str) -> Result<PathBuf, FileFailure> {
    let path = absolute_path(value)?;
    let metadata = fs::metadata(&path).map_err(|error| io_failure(error, "读取目录失败"))?;
    if !metadata.is_dir() {
        return Err(FileFailure::new(
            FileErrorCode::NotDirectory,
            "目标不是目录",
        ));
    }
    Ok(path)
}

async fn require_directory_async(value: &str) -> Result<PathBuf, FileFailure> {
    let path = absolute_path(value)?;
    let metadata = tokio::fs::metadata(&path)
        .await
        .map_err(|error| io_failure(error, "读取目录失败"))?;
    if !metadata.is_dir() {
        return Err(FileFailure::new(
            FileErrorCode::NotDirectory,
            "目标不是目录",
        ));
    }
    Ok(path)
}

fn validate_name(name: &str) -> Result<(), FileFailure> {
    let invalid = name.is_empty()
        || matches!(name, "." | "..")
        || name.contains('\0')
        || name.contains('/')
        || name.contains('\\');
    if invalid {
        return Err(FileFailure::new(
            FileErrorCode::InvalidPath,
            "文件名不能为空，也不能包含路径分隔符",
        ));
    }
    #[cfg(windows)]
    {
        let trimmed = name.trim_end_matches([' ', '.']);
        let stem = trimmed.split('.').next().unwrap_or_default();
        let reserved = ["CON", "PRN", "AUX", "NUL"]
            .into_iter()
            .any(|item| stem.eq_ignore_ascii_case(item))
            || (1..=9).any(|index| {
                stem.eq_ignore_ascii_case(&format!("COM{index}"))
                    || stem.eq_ignore_ascii_case(&format!("LPT{index}"))
            });
        if trimmed.is_empty()
            || trimmed != name
            || reserved
            || name.chars().any(|character| "<>:\"|?*".contains(character))
        {
            return Err(FileFailure::new(
                FileErrorCode::InvalidPath,
                "Windows 文件名格式无效",
            ));
        }
    }
    Ok(())
}

fn is_root_path(path: &Path) -> bool {
    if path.parent().is_none() {
        return true;
    }
    if fs::symlink_metadata(path).is_ok_and(|metadata| metadata.file_type().is_symlink()) {
        return false;
    }
    fs::canonicalize(path).is_ok_and(|canonical| canonical.parent().is_none())
}

fn display_path(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

fn rename_failure(error: std::io::Error) -> FileFailure {
    #[cfg(unix)]
    let crosses_devices = error.raw_os_error() == Some(libc::EXDEV);
    #[cfg(windows)]
    let crosses_devices = error.raw_os_error() == Some(17);
    #[cfg(not(any(unix, windows)))]
    let crosses_devices = false;

    if crosses_devices {
        FileFailure::new(FileErrorCode::Unsupported, "暂不支持跨磁盘移动")
    } else {
        io_failure(error, "移动或重命名失败")
    }
}

fn io_failure(error: std::io::Error, context: &str) -> FileFailure {
    let code = match error.kind() {
        ErrorKind::NotFound => FileErrorCode::NotFound,
        ErrorKind::PermissionDenied => FileErrorCode::PermissionDenied,
        ErrorKind::AlreadyExists | ErrorKind::DirectoryNotEmpty => FileErrorCode::AlreadyExists,
        ErrorKind::NotADirectory => FileErrorCode::NotDirectory,
        ErrorKind::IsADirectory => FileErrorCode::IsDirectory,
        ErrorKind::InvalidInput | ErrorKind::InvalidFilename => FileErrorCode::InvalidPath,
        ErrorKind::Unsupported => FileErrorCode::Unsupported,
        _ => FileErrorCode::Io,
    };
    FileFailure::new(code, format!("{context}：{error}"))
}

fn send_response(
    outbound: &mpsc::UnboundedSender<AgentInbound>,
    request_id: &str,
    response: FileResponse,
) {
    let _ = outbound.send(AgentInbound::FileResponse {
        request_id: request_id.to_string(),
        response,
    });
}

fn send_failure(
    outbound: &mpsc::UnboundedSender<AgentInbound>,
    request_id: &str,
    failure: FileFailure,
) {
    send_response(
        outbound,
        request_id,
        FileResponse::Error {
            code: failure.code,
            message: failure.message,
        },
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_directory() -> PathBuf {
        let path = std::env::temp_dir().join(format!("om-file-manager-{}", Uuid::new_v4()));
        fs::create_dir_all(&path).unwrap();
        path
    }

    #[test]
    fn lists_moves_and_recursively_deletes_unicode_entries() {
        let root = test_directory();
        let source = root.join("源文件.txt");
        fs::write(&source, b"payload").unwrap();
        let child = create_directory(root.to_str().unwrap(), "子目录").unwrap();
        let moved = move_path(source.to_str().unwrap(), &child, "新名称.txt", false).unwrap();

        let listing = list_directory(&child, 0, 200).unwrap();
        assert_eq!(listing.total, 1);
        assert_eq!(listing.entries[0].name, "新名称.txt");
        assert_eq!(fs::read(&moved).unwrap(), b"payload");

        delete_path(&child, true).unwrap();
        assert!(!Path::new(&child).exists());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn refuses_root_deletion_and_directory_overwrite() {
        let root_path = filesystem_roots().remove(0).path;
        assert_eq!(
            delete_path(&root_path, true).unwrap_err().code,
            FileErrorCode::InvalidPath
        );
        let root_alias = format!("{}{}.", root_path, std::path::MAIN_SEPARATOR);
        assert_eq!(
            delete_path(&root_alias, true).unwrap_err().code,
            FileErrorCode::InvalidPath
        );

        let root = test_directory();
        let left = root.join("left");
        let right = root.join("right");
        fs::create_dir(&left).unwrap();
        fs::create_dir(&right).unwrap();
        assert_eq!(
            move_path(
                left.to_str().unwrap(),
                root.to_str().unwrap(),
                "right",
                true,
            )
            .unwrap_err()
            .code,
            FileErrorCode::AlreadyExists
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn chunk_frame_round_trips_and_rejects_oversized_payloads() {
        let request_id = Uuid::new_v4().to_string();
        let frame = encode_chunk_frame(&request_id, 7, b"hello").unwrap();
        let decoded = decode_chunk_frame(&frame).unwrap();
        assert_eq!(decoded, (request_id, 7, b"hello".to_vec()));

        assert_eq!(
            encode_chunk_frame(&Uuid::new_v4().to_string(), 0, &vec![0; CHUNK_SIZE + 1])
                .unwrap_err()
                .code,
            FileErrorCode::TooLarge
        );
    }

    #[tokio::test]
    async fn streamed_upload_commits_only_after_all_chunks_arrive() {
        let root = test_directory();
        let request_id = Uuid::new_v4().to_string();
        let first = vec![7_u8; CHUNK_SIZE];
        let second = b"final-chunk".to_vec();
        let expected_size = (first.len() + second.len()) as u64;
        let (event_tx, event_rx) = mpsc::channel(TRANSFER_WINDOW);
        let (outbound_tx, mut outbound_rx) = mpsc::unbounded_channel();
        let task_request_id = request_id.clone();
        let task_root = display_path(&root);
        let task = tokio::spawn(async move {
            run_upload(
                &task_request_id,
                task_root,
                "payload.bin".to_string(),
                expected_size,
                false,
                event_rx,
                &outbound_tx,
            )
            .await
        });

        assert!(matches!(
            outbound_rx.recv().await,
            Some(AgentInbound::FileResponse {
                response: FileResponse::UploadReady { .. },
                ..
            })
        ));
        event_tx
            .send(UploadEvent::Chunk {
                sequence: 0,
                data: first.clone(),
            })
            .await
            .unwrap();
        event_tx
            .send(UploadEvent::Chunk {
                sequence: 1,
                data: second.clone(),
            })
            .await
            .unwrap();
        event_tx.send(UploadEvent::Finish).await.unwrap();

        let mut acknowledgements = 0;
        let mut completed = false;
        while let Some(message) = outbound_rx.recv().await {
            match message {
                AgentInbound::FileResponse {
                    response: FileResponse::TransferAck { .. },
                    ..
                } => acknowledgements += 1,
                AgentInbound::FileResponse {
                    response: FileResponse::TransferComplete { .. },
                    ..
                } => {
                    completed = true;
                    break;
                }
                _ => {}
            }
        }
        assert_eq!(acknowledgements, 2);
        assert!(completed);
        task.await.unwrap().unwrap();

        let mut expected = first;
        expected.extend_from_slice(&second);
        assert_eq!(fs::read(root.join("payload.bin")).unwrap(), expected);
        assert!(fs::read_dir(&root).unwrap().all(|entry| {
            !entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .ends_with(".part")
        }));
        fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn interrupted_upload_removes_partial_file() {
        let root = test_directory();
        let request_id = Uuid::new_v4().to_string();
        let (event_tx, event_rx) = mpsc::channel(TRANSFER_WINDOW);
        let (outbound_tx, mut outbound_rx) = mpsc::unbounded_channel();
        let task_request_id = request_id.clone();
        let task_root = display_path(&root);
        let task = tokio::spawn(async move {
            run_upload(
                &task_request_id,
                task_root,
                "partial.bin".to_string(),
                10,
                false,
                event_rx,
                &outbound_tx,
            )
            .await
        });

        let _ = outbound_rx.recv().await;
        event_tx
            .send(UploadEvent::Chunk {
                sequence: 0,
                data: b"short".to_vec(),
            })
            .await
            .unwrap();
        let _ = outbound_rx.recv().await;
        drop(event_tx);

        assert!(task.await.unwrap().is_err());
        assert!(!root.join("partial.bin").exists());
        assert!(fs::read_dir(&root).unwrap().all(|entry| {
            !entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .ends_with(".part")
        }));
        fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn streamed_download_obeys_acknowledgement_window_and_preserves_bytes() {
        let root = test_directory();
        let path = root.join("download.bin");
        let expected = (0..(CHUNK_SIZE * 2 + 17))
            .map(|index| (index % 251) as u8)
            .collect::<Vec<_>>();
        fs::write(&path, &expected).unwrap();
        let request_id = Uuid::new_v4().to_string();
        let (ack_tx, ack_rx) = mpsc::channel(TRANSFER_WINDOW);
        let (binary_tx, mut binary_rx) = mpsc::channel(TRANSFER_WINDOW);
        let (outbound_tx, mut outbound_rx) = mpsc::unbounded_channel();
        let task_request_id = request_id.clone();
        let task_path = display_path(&path);
        let task = tokio::spawn(async move {
            run_download(
                &task_request_id,
                task_path,
                u64::MAX,
                ack_rx,
                &outbound_tx,
                binary_tx,
            )
            .await
        });

        assert!(matches!(
            outbound_rx.recv().await,
            Some(AgentInbound::FileResponse {
                response: FileResponse::DownloadReady { .. },
                ..
            })
        ));
        let mut actual = Vec::new();
        for expected_sequence in 0..3 {
            let frame = binary_rx.recv().await.unwrap();
            let (frame_request_id, sequence, data) = decode_chunk_frame(&frame).unwrap();
            assert_eq!(frame_request_id, request_id);
            assert_eq!(sequence, expected_sequence);
            actual.extend_from_slice(&data);
            ack_tx.send(sequence).await.unwrap();
        }
        assert!(matches!(
            outbound_rx.recv().await,
            Some(AgentInbound::FileResponse {
                response: FileResponse::TransferComplete { .. },
                ..
            })
        ));
        task.await.unwrap().unwrap();
        assert_eq!(actual, expected);
        fs::remove_dir_all(root).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn deleting_symlink_does_not_delete_target() {
        use std::os::unix::fs::symlink;

        let root = test_directory();
        let target = root.join("target");
        let link = root.join("link");
        fs::create_dir(&target).unwrap();
        fs::write(target.join("keep.txt"), b"keep").unwrap();
        symlink(&target, &link).unwrap();

        delete_path(link.to_str().unwrap(), true).unwrap();
        assert!(target.join("keep.txt").exists());
        fs::remove_dir_all(root).unwrap();
    }
}
