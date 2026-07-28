use std::collections::BTreeMap;

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

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum DockerStatusState {
    NotInstalled,
    DaemonUnreachable,
    PermissionDenied,
    UnsupportedVersion,
    Ready,
    Error,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DockerStatus {
    pub state: DockerStatusState,
    pub cli_version: Option<String>,
    pub engine_version: Option<String>,
    pub api_version: Option<String>,
    pub compose_version: Option<String>,
    pub message: Option<String>,
    pub checked_at: i64,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum DockerErrorCode {
    InvalidRequest,
    NotFound,
    PermissionDenied,
    NotInstalled,
    DaemonUnavailable,
    UnsupportedVersion,
    Busy,
    Conflict,
    Timeout,
    Cancelled,
    OutputTooLarge,
    CommandFailed,
    Unsupported,
    Internal,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DockerError {
    pub code: DockerErrorCode,
    pub message: String,
    pub retryable: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i64>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum DockerPortProtocol {
    Tcp,
    Udp,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DockerPortSpec {
    pub host_ip: Option<String>,
    pub host_port: Option<u16>,
    pub container_port: u16,
    pub protocol: DockerPortProtocol,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum DockerMountKind {
    Volume,
    Bind,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DockerMountSpec {
    pub kind: DockerMountKind,
    pub source: String,
    pub target: String,
    #[serde(default)]
    pub read_only: bool,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum DockerRestartPolicy {
    No,
    Always,
    UnlessStopped,
    OnFailure,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct DockerContainerCreateSpec {
    pub name: Option<String>,
    pub image: String,
    #[serde(default)]
    pub command: Vec<String>,
    #[serde(default)]
    pub environment: BTreeMap<String, String>,
    #[serde(default)]
    pub ports: Vec<DockerPortSpec>,
    #[serde(default)]
    pub mounts: Vec<DockerMountSpec>,
    pub network: Option<String>,
    pub restart_policy: Option<DockerRestartPolicy>,
    pub restart_max_retries: Option<u32>,
    pub cpus: Option<f64>,
    pub memory_bytes: Option<u64>,
    #[serde(default)]
    pub confirm_bind_write: bool,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum DockerContainerAction {
    Start,
    Stop,
    Restart,
    Kill,
    Pause,
    Unpause,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DockerNetworkCreateSpec {
    pub name: String,
    pub driver: Option<String>,
    #[serde(default)]
    pub internal: bool,
    #[serde(default)]
    pub attachable: bool,
    #[serde(default)]
    pub labels: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DockerVolumeCreateSpec {
    pub name: String,
    pub driver: Option<String>,
    #[serde(default)]
    pub labels: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DockerComposeTarget {
    pub project: Option<String>,
    #[serde(default)]
    pub files: Vec<String>,
    #[serde(default)]
    pub profiles: Vec<String>,
    #[serde(default)]
    pub services: Vec<String>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum DockerComposeAction {
    Pull,
    Up,
    Start,
    Stop,
    Restart,
    Down,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DockerComposeServiceSummary {
    pub name: String,
    pub image: Option<String>,
    pub ports: Vec<String>,
    pub mounts: Vec<String>,
    pub networks: Vec<String>,
    pub profiles: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DockerComposeConfigSummary {
    pub service_count: u32,
    pub network_count: u32,
    pub volume_count: u32,
    pub config_count: u32,
    pub secret_count: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DockerComposeValidation {
    pub project: Option<String>,
    pub services: Vec<String>,
    pub service_summaries: Vec<DockerComposeServiceSummary>,
    pub config_summary: DockerComposeConfigSummary,
    pub warnings: Vec<String>,
    pub config_digest: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "operation", rename_all = "snake_case")]
pub enum DockerRequest {
    ContainerList {
        #[serde(default)]
        all: bool,
    },
    ContainerInspect {
        container: String,
    },
    ContainerCreate {
        spec: DockerContainerCreateSpec,
    },
    ContainerAction {
        container: String,
        action: DockerContainerAction,
        timeout_seconds: Option<u64>,
    },
    ContainerRename {
        container: String,
        name: String,
    },
    ContainerRemove {
        container: String,
        #[serde(default)]
        force: bool,
        #[serde(default)]
        remove_volumes: bool,
    },
    ContainerStats {
        container: Option<String>,
    },
    ImageList {
        #[serde(default)]
        all: bool,
    },
    ImageInspect {
        image: String,
    },
    ImagePull {
        image: String,
    },
    ImageTag {
        source: String,
        target: String,
    },
    ImageRemove {
        image: String,
        #[serde(default)]
        force: bool,
    },
    ImagePrune,
    NetworkList,
    NetworkInspect {
        network: String,
    },
    NetworkCreate {
        spec: DockerNetworkCreateSpec,
    },
    NetworkConnect {
        network: String,
        container: String,
        #[serde(default)]
        aliases: Vec<String>,
    },
    NetworkDisconnect {
        network: String,
        container: String,
        #[serde(default)]
        force: bool,
    },
    NetworkRemove {
        network: String,
    },
    NetworkPrune,
    VolumeList,
    VolumeInspect {
        volume: String,
    },
    VolumeCreate {
        spec: DockerVolumeCreateSpec,
    },
    VolumeRemove {
        volume: String,
        #[serde(default)]
        force: bool,
    },
    VolumePrune,
    ComposeList,
    ComposeInspect {
        target: DockerComposeTarget,
    },
    ComposeValidate {
        target: DockerComposeTarget,
    },
    ComposeDeploy {
        target: DockerComposeTarget,
        config_digest: String,
        #[serde(default)]
        confirm_high_risk: bool,
    },
    ComposeAction {
        target: DockerComposeTarget,
        action: DockerComposeAction,
        config_digest: String,
        #[serde(default)]
        remove_volumes: bool,
        #[serde(default)]
        confirm_high_risk: bool,
    },
    SystemDf,
    SystemPrune {
        #[serde(default)]
        containers: bool,
        #[serde(default)]
        images: bool,
        #[serde(default)]
        networks: bool,
        #[serde(default)]
        volumes: bool,
        #[serde(default)]
        all_images: bool,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "result", rename_all = "snake_case")]
pub enum DockerResponse {
    Data { data: serde_json::Value },
    OperationComplete { data: serde_json::Value },
    ComposeValidation { validation: DockerComposeValidation },
    Error { error: DockerError },
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
    DockerRequest {
        request_id: String,
        request: DockerRequest,
    },
    DockerCancel {
        request_id: String,
    },
    DockerLogStart {
        request_id: String,
        container: String,
        #[serde(default = "default_docker_log_tail")]
        tail: u32,
        #[serde(default)]
        follow: bool,
        since: Option<String>,
    },
    DockerLogCancel {
        request_id: String,
    },
    DockerExecOpen {
        session_id: String,
        container: String,
        shell: String,
        cols: u16,
        rows: u16,
    },
    DockerExecInput {
        session_id: String,
        data: String,
    },
    DockerExecResize {
        session_id: String,
        cols: u16,
        rows: u16,
    },
    DockerExecClose {
        session_id: String,
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
        #[serde(skip_serializing_if = "Option::is_none")]
        docker_status: Option<DockerStatus>,
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
    DockerResponse {
        request_id: String,
        response: DockerResponse,
    },
    DockerLogChunk {
        request_id: String,
        sequence: u64,
        data: String,
        cursor: Option<String>,
    },
    DockerLogClosed {
        request_id: String,
        error: Option<DockerError>,
    },
    DockerExecOpened {
        session_id: String,
    },
    DockerExecOutput {
        session_id: String,
        data: String,
    },
    DockerExecClosed {
        session_id: String,
        exit_code: Option<i64>,
        reason: Option<String>,
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

fn default_docker_log_tail() -> u32 {
    200
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
    fn docker_messages_use_stable_tagged_protocol_shapes() {
        let request = json!({
            "type": "docker_request",
            "request_id": "docker-1",
            "request": {
                "operation": "container_action",
                "container": "web",
                "action": "restart",
                "timeout_seconds": 15
            }
        });
        assert!(matches!(
            serde_json::from_value::<AgentOutbound>(request).unwrap(),
            AgentOutbound::DockerRequest {
                request: DockerRequest::ContainerAction {
                    container,
                    action: DockerContainerAction::Restart,
                    timeout_seconds: Some(15),
                },
                ..
            } if container == "web"
        ));

        let response = AgentInbound::DockerResponse {
            request_id: "docker-1".to_string(),
            response: DockerResponse::Error {
                error: DockerError {
                    code: DockerErrorCode::DaemonUnavailable,
                    message: "daemon unavailable".to_string(),
                    retryable: true,
                    exit_code: Some(1),
                },
            },
        };
        assert_eq!(
            serde_json::to_value(response).unwrap(),
            json!({
                "type": "docker_response",
                "request_id": "docker-1",
                "response": {
                    "result": "error",
                    "error": {
                        "code": "daemon_unavailable",
                        "message": "daemon unavailable",
                        "retryable": true,
                        "exit_code": 1
                    }
                }
            })
        );

        assert_eq!(
            serde_json::to_value(AgentInbound::DockerLogChunk {
                request_id: "logs-1".to_string(),
                sequence: 2,
                data: "line\n".to_string(),
                cursor: Some("2026-07-27T01:02:03Z".to_string()),
            })
            .unwrap(),
            json!({
                "type": "docker_log_chunk",
                "request_id": "logs-1",
                "sequence": 2,
                "data": "line\n",
                "cursor": "2026-07-27T01:02:03Z"
            })
        );

        let compose_action = json!({
            "type": "docker_request",
            "request_id": "compose-1",
            "request": {
                "operation": "compose_action",
                "target": {
                    "project": "sample",
                    "files": ["/srv/sample/compose.yaml"],
                    "profiles": [],
                    "services": ["web"]
                },
                "action": "up",
                "config_digest": "sha256:abc",
                "remove_volumes": false,
                "confirm_high_risk": true
            }
        });
        assert!(matches!(
            serde_json::from_value::<AgentOutbound>(compose_action).unwrap(),
            AgentOutbound::DockerRequest {
                request: DockerRequest::ComposeAction {
                    action: DockerComposeAction::Up,
                    config_digest,
                    ..
                },
                ..
            } if config_digest == "sha256:abc"
        ));

        let validation = DockerComposeValidation {
            project: Some("sample".to_string()),
            services: vec!["web".to_string()],
            service_summaries: vec![DockerComposeServiceSummary {
                name: "web".to_string(),
                image: Some("nginx:alpine".to_string()),
                ports: vec!["8080:80/tcp".to_string()],
                mounts: Vec::new(),
                networks: vec!["default".to_string()],
                profiles: Vec::new(),
            }],
            config_summary: DockerComposeConfigSummary {
                service_count: 1,
                network_count: 1,
                volume_count: 0,
                config_count: 0,
                secret_count: 0,
            },
            warnings: Vec::new(),
            config_digest: "sha256:abc".to_string(),
        };
        assert_eq!(
            serde_json::to_value(DockerResponse::ComposeValidation { validation }).unwrap(),
            json!({
                "result": "compose_validation",
                "validation": {
                    "project": "sample",
                    "services": ["web"],
                    "service_summaries": [{
                        "name": "web",
                        "image": "nginx:alpine",
                        "ports": ["8080:80/tcp"],
                        "mounts": [],
                        "networks": ["default"],
                        "profiles": []
                    }],
                    "config_summary": {
                        "service_count": 1,
                        "network_count": 1,
                        "volume_count": 0,
                        "config_count": 0,
                        "secret_count": 0
                    },
                    "warnings": [],
                    "config_digest": "sha256:abc"
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
