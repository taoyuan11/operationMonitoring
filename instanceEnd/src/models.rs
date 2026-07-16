use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Identity {
    pub instance_id: String,
    pub secret: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct AgentRegisterRequest {
    pub instance_id: String,
    pub secret: String,
    pub hostname: String,
    pub os: String,
    pub arch: String,
    pub agent_version: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub package_type: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub native_arch: Option<String>,
    pub update_privileged: Option<bool>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct AgentRegisterResponse {
    pub approved: bool,
    pub disabled: bool,
    pub message: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MetricPayload {
    pub ts: i64,
    pub cpu_percent: f64,
    pub memory_used: i64,
    pub memory_total: i64,
    pub disk_used: i64,
    pub disk_total: i64,
    pub network_rx: i64,
    pub network_tx: i64,
    pub gpu_percent: Option<f64>,
    pub gpu_memory_used: Option<i64>,
    pub gpu_memory_total: Option<i64>,
    pub uptime_seconds: i64,
    pub load_average: Option<f64>,
}

#[derive(Debug, Clone)]
pub struct HostProfile {
    pub hostname: String,
    pub os: String,
    pub arch: String,
    pub agent_version: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct UpdateOffer {
    pub release_id: String,
    pub version: String,
    pub artifact_id: String,
    pub download_url: String,
    pub sha256: String,
    pub size_bytes: i64,
    pub package_type: String,
    pub native_arch: String,
    #[serde(default)]
    pub retry_count: i64,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum UpdateStatus {
    Waiting,
    Downloading,
    Verifying,
    WaitingIdle,
    Installing,
    AwaitingRestart,
    Succeeded,
    RollbackSucceeded,
    Failed,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum FileEntryKind {
    File,
    Directory,
    Symlink,
    Other,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct FileSystemRoot {
    pub path: String,
    pub label: String,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct FileEntry {
    pub name: String,
    pub path: String,
    pub kind: FileEntryKind,
    pub size_bytes: u64,
    pub modified_at: Option<i64>,
    pub readonly: bool,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct FileListing {
    pub path: String,
    pub parent: Option<String>,
    pub entries: Vec<FileEntry>,
    pub offset: u64,
    pub limit: u64,
    pub total: u64,
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum FileErrorCode {
    InvalidPath,
    NotFound,
    PermissionDenied,
    AlreadyExists,
    NotDirectory,
    IsDirectory,
    Busy,
    TooLarge,
    Unsupported,
    Io,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(tag = "operation", rename_all = "snake_case")]
pub enum FileRequest {
    Roots,
    List {
        path: String,
        offset: u64,
        limit: u64,
    },
    CreateDirectory {
        parent: String,
        name: String,
    },
    Move {
        source: String,
        destination_parent: String,
        name: String,
        overwrite: bool,
    },
    Delete {
        path: String,
        recursive: bool,
    },
    UploadStart {
        parent: String,
        name: String,
        size_bytes: u64,
        overwrite: bool,
        max_bytes: u64,
    },
    DownloadStart {
        path: String,
        max_bytes: u64,
    },
}

#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(tag = "result", rename_all = "snake_case")]
pub enum FileResponse {
    Roots {
        roots: Vec<FileSystemRoot>,
    },
    Listing {
        listing: FileListing,
    },
    OperationComplete {
        path: String,
    },
    UploadReady {
        path: String,
    },
    DownloadReady {
        path: String,
        name: String,
        size_bytes: u64,
    },
    TransferAck {
        sequence: u64,
        transferred_bytes: u64,
    },
    TransferComplete {
        path: String,
        size_bytes: u64,
    },
    Error {
        code: FileErrorCode,
        message: String,
    },
}

#[derive(Serialize, Deserialize, Debug)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AgentOutbound {
    RunCommand {
        job_id: String,
        command: String,
    },
    Ping {
        now: i64,
    },
    TerminalOpen {
        session_id: String,
        cols: u16,
        rows: u16,
    },
    TerminalInput {
        session_id: String,
        data: String,
    },
    TerminalResize {
        session_id: String,
        cols: u16,
        rows: u16,
    },
    TerminalClose {
        session_id: String,
    },
    FileRequest {
        request_id: String,
        request: FileRequest,
    },
    FileTransferFinish {
        request_id: String,
    },
    FileTransferAck {
        request_id: String,
        sequence: u64,
    },
    FileTransferCancel {
        request_id: String,
    },
    UpdateAvailable {
        release_id: String,
        version: String,
        artifact_id: String,
        download_url: String,
        sha256: String,
        size_bytes: i64,
        package_type: String,
        native_arch: String,
        #[serde(default)]
        retry_count: i64,
    },
    DesktopOpen {
        session_id: String,
        stream_token: String,
        max_width: u32,
        max_height: u32,
        min_fps: u8,
        max_fps: u8,
        jpeg_quality: u8,
    },
    DesktopClose {
        session_id: String,
        reason: String,
    },
}

#[derive(Serialize, Deserialize, Debug)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AgentInbound {
    Pong {
        now: i64,
    },
    Metrics {
        hostname: String,
        os: String,
        arch: String,
        agent_version: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        package_type: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        native_arch: Option<String>,
        update_privileged: Option<bool>,
        metrics: MetricPayload,
    },
    CommandResult {
        job_id: String,
        exit_code: i64,
        output: String,
    },
    TerminalOpened {
        session_id: String,
    },
    TerminalOutput {
        session_id: String,
        data: String,
    },
    TerminalClosed {
        session_id: String,
        exit_code: Option<i64>,
        reason: Option<String>,
    },
    FileResponse {
        request_id: String,
        response: FileResponse,
    },
    UpdateStatus {
        release_id: String,
        artifact_id: String,
        version: String,
        retry_count: i64,
        status: UpdateStatus,
        #[serde(skip_serializing_if = "Option::is_none")]
        message: Option<String>,
    },
    DesktopOpened {
        session_id: String,
    },
    DesktopClosed {
        session_id: String,
        reason: String,
    },
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn update_offer_matches_the_backend_websocket_shape() {
        let value = json!({
            "type": "update_available",
            "release_id": "release-1",
            "version": "1.2.3",
            "artifact_id": "artifact-1",
            "download_url": "/api/agent/update/artifacts/artifact-1/download",
            "sha256": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            "size_bytes": 42,
            "package_type": "standalone",
            "native_arch": "arm64",
            "retry_count": 2
        });

        let message: AgentOutbound = serde_json::from_value(value.clone()).unwrap();
        assert!(matches!(
            message,
            AgentOutbound::UpdateAvailable {
                version,
                retry_count: 2,
                ..
            } if version == "1.2.3"
        ));

        let mut legacy = value;
        legacy.as_object_mut().unwrap().remove("retry_count");
        assert!(matches!(
            serde_json::from_value::<AgentOutbound>(legacy).unwrap(),
            AgentOutbound::UpdateAvailable { retry_count: 0, .. }
        ));
    }

    #[test]
    fn update_status_matches_the_backend_websocket_shape() {
        let message = AgentInbound::UpdateStatus {
            release_id: "release-1".to_string(),
            artifact_id: "artifact-1".to_string(),
            version: "1.2.3".to_string(),
            retry_count: 2,
            status: UpdateStatus::AwaitingRestart,
            message: None,
        };

        assert_eq!(
            serde_json::to_value(message).unwrap(),
            json!({
                "type": "update_status",
                "release_id": "release-1",
                "artifact_id": "artifact-1",
                "version": "1.2.3",
                "retry_count": 2,
                "status": "awaiting_restart"
            })
        );
    }

    #[test]
    fn file_messages_use_stable_tagged_protocol_shapes() {
        let request = json!({
            "type": "file_request",
            "request_id": "request-1",
            "request": {
                "operation": "list",
                "path": "/srv",
                "offset": 0,
                "limit": 200
            }
        });
        assert!(matches!(
            serde_json::from_value::<AgentOutbound>(request).unwrap(),
            AgentOutbound::FileRequest {
                request: FileRequest::List { path, limit: 200, .. },
                ..
            } if path == "/srv"
        ));

        let response = AgentInbound::FileResponse {
            request_id: "request-1".to_string(),
            response: FileResponse::TransferAck {
                sequence: 3,
                transferred_bytes: 1024,
            },
        };
        assert_eq!(
            serde_json::to_value(response).unwrap(),
            json!({
                "type": "file_response",
                "request_id": "request-1",
                "response": {
                    "result": "transfer_ack",
                    "sequence": 3,
                    "transferred_bytes": 1024
                }
            })
        );
    }

    #[test]
    fn desktop_messages_use_stable_tagged_protocol_shapes() {
        let open = json!({
            "type": "desktop_open",
            "session_id": "desktop-1",
            "stream_token": "one-time-token",
            "max_width": 1920,
            "max_height": 1080,
            "min_fps": 8,
            "max_fps": 12,
            "jpeg_quality": 70
        });
        assert!(matches!(
            serde_json::from_value::<AgentOutbound>(open).unwrap(),
            AgentOutbound::DesktopOpen { session_id, jpeg_quality: 70, .. }
                if session_id == "desktop-1"
        ));

        assert_eq!(
            serde_json::to_value(AgentInbound::DesktopClosed {
                session_id: "desktop-1".to_string(),
                reason: "browser_disconnected".to_string(),
            })
            .unwrap(),
            json!({
                "type": "desktop_closed",
                "session_id": "desktop-1",
                "reason": "browser_disconnected"
            })
        );
    }
}
