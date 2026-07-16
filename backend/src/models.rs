use serde::{Deserialize, Serialize};
use sqlx::FromRow;

#[derive(Serialize)]
pub struct HealthResponse {
    pub status: &'static str,
    pub now: i64,
}

#[derive(Deserialize)]
pub struct AgentRegisterRequest {
    pub instance_id: String,
    pub secret: String,
    pub hostname: String,
    pub os: String,
    pub arch: String,
    pub agent_version: String,
    #[serde(default)]
    pub package_type: Option<String>,
    #[serde(default)]
    pub native_arch: Option<String>,
    #[serde(default)]
    pub update_privileged: Option<bool>,
}

#[derive(Serialize)]
pub struct AgentRegisterResponse {
    pub approved: bool,
    pub disabled: bool,
    pub message: String,
}

#[derive(Deserialize)]
pub struct AgentReportRequest {
    pub instance_id: String,
    pub secret: String,
    pub hostname: String,
    pub os: String,
    pub arch: String,
    pub agent_version: String,
    #[serde(default)]
    pub package_type: Option<String>,
    #[serde(default)]
    pub native_arch: Option<String>,
    #[serde(default)]
    pub update_privileged: Option<bool>,
    pub metrics: MetricPayload,
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

#[derive(Serialize, FromRow)]
pub struct InstanceRecord {
    pub id: String,
    pub secret: String,
    pub name: String,
    pub region: String,
    pub country_code: String,
    pub country: String,
    pub province_code: String,
    pub province: String,
    pub city: String,
    pub remark: String,
    pub hostname: String,
    pub os: String,
    pub arch: String,
    pub agent_version: String,
    pub package_type: String,
    pub native_arch: String,
    pub update_privileged: i64,
    pub approved: i64,
    pub disabled: i64,
    pub first_seen: i64,
    pub last_seen: Option<i64>,
}

#[derive(Serialize, FromRow)]
pub struct PendingInstance {
    pub id: String,
    pub hostname: String,
    pub os: String,
    pub arch: String,
    pub agent_version: String,
    pub package_type: String,
    pub native_arch: String,
    pub update_privileged: bool,
    pub first_seen: i64,
    pub last_seen: i64,
}

#[derive(FromRow)]
pub struct PendingInstanceSecret {
    pub id: String,
    pub secret: String,
    pub hostname: String,
    pub os: String,
    pub arch: String,
    pub agent_version: String,
    pub package_type: String,
    pub native_arch: String,
    pub update_privileged: i64,
    pub first_seen: i64,
    pub last_seen: i64,
}

#[derive(Serialize, FromRow)]
pub struct MetricRecord {
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

#[derive(Serialize)]
pub struct InstanceSummary {
    pub id: String,
    pub name: String,
    pub region: String,
    pub country_code: String,
    pub country: String,
    pub province_code: String,
    pub province: String,
    pub city: String,
    pub remark: String,
    pub hostname: String,
    pub os: String,
    pub arch: String,
    pub agent_version: String,
    pub capabilities: Vec<String>,
    pub online: bool,
    pub first_seen: i64,
    pub last_seen: Option<i64>,
    pub metrics: Option<MetricRecord>,
}

#[derive(Deserialize)]
pub struct MetricsQuery {
    pub from: Option<i64>,
    pub to: Option<i64>,
    pub limit: Option<i64>,
    pub bucket_seconds: Option<i64>,
}

#[derive(Deserialize)]
pub struct UpdateInstanceRequest {
    pub name: Option<String>,
    pub region: Option<String>,
    pub country_code: Option<String>,
    pub country: Option<String>,
    pub province_code: Option<String>,
    pub province: Option<String>,
    pub city: Option<String>,
    pub remark: Option<String>,
}

#[derive(FromRow)]
pub struct SettingsRow {
    pub value: String,
}

#[derive(Serialize)]
pub struct SettingsResponse {
    pub retention_days: i64,
    pub background_image_url: Option<String>,
    pub theme_mode: ThemeMode,
    pub accent_color: String,
}

#[derive(Deserialize)]
pub struct SettingsRequest {
    pub retention_days: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ThemeMode {
    Auto,
    Light,
    Dark,
}

#[derive(Deserialize)]
pub struct AppearanceSettingsRequest {
    pub theme_mode: ThemeMode,
    pub accent_color: String,
}

#[derive(Serialize)]
pub struct AppearanceResponse {
    pub background_image_url: Option<String>,
    pub theme_mode: ThemeMode,
    pub accent_color: String,
}

#[derive(Serialize, FromRow)]
pub struct CommandRecord {
    pub id: String,
    pub name: String,
    pub command: String,
    pub confirm_text: String,
    pub enabled: i64,
    pub created_at: i64,
}

#[derive(Deserialize)]
pub struct CreateCommandRequest {
    pub name: String,
    pub command: String,
    pub confirm_text: Option<String>,
}

#[derive(Serialize, FromRow)]
pub struct CommandJobRecord {
    pub id: String,
    pub command_id: Option<String>,
    pub instance_id: String,
    pub command: String,
    pub status: String,
    pub requested_by: String,
    pub created_at: i64,
    pub completed_at: Option<i64>,
    pub output: String,
    pub exit_code: Option<i64>,
}

#[derive(Serialize, FromRow)]
pub struct ActionLogRecord {
    pub id: String,
    pub actor: String,
    pub action: String,
    pub target: String,
    pub detail: String,
    pub created_at: i64,
}

#[derive(Deserialize)]
pub struct ListQuery {
    pub limit: Option<i64>,
}

#[derive(Deserialize)]
pub struct AgentWsQuery {
    pub instance_id: String,
    pub secret: String,
    #[serde(default)]
    pub capabilities: Option<String>,
}

#[derive(Deserialize)]
pub struct CreateAgentReleaseRequest {
    pub version: String,
    #[serde(default)]
    pub notes: String,
}

#[derive(Serialize, FromRow, Clone)]
pub struct AgentReleaseRecord {
    pub id: String,
    pub version: String,
    pub notes: String,
    pub status: String,
    pub created_at: i64,
    pub published_at: Option<i64>,
}

#[derive(Serialize, FromRow, Clone)]
pub struct AgentArtifactRecord {
    pub id: String,
    pub release_id: String,
    pub os: String,
    pub package_type: String,
    pub native_arch: String,
    pub file_name: String,
    pub size_bytes: i64,
    pub sha256: String,
    #[serde(skip_serializing)]
    pub storage_path: String,
    pub created_at: i64,
}

#[derive(Serialize, FromRow, Clone)]
pub struct AgentUpdateAttemptRecord {
    pub id: String,
    pub release_id: String,
    pub artifact_id: String,
    pub instance_id: String,
    pub from_version: String,
    pub target_version: String,
    pub status: String,
    pub message: String,
    pub retry_count: i64,
    pub created_at: i64,
    pub updated_at: i64,
    pub completed_at: Option<i64>,
}

#[derive(Serialize)]
pub struct AgentReleaseCoverage {
    pub eligible_instances: i64,
    pub covered_instances: i64,
    pub missing_artifact_instances: i64,
    pub unprivileged_instances: i64,
}

#[derive(Serialize)]
pub struct AgentReleaseDetail {
    #[serde(flatten)]
    pub release: AgentReleaseRecord,
    pub artifacts: Vec<AgentArtifactRecord>,
    pub attempts: Vec<AgentUpdateAttemptRecord>,
    pub coverage: AgentReleaseCoverage,
}

#[derive(Deserialize)]
pub struct UpdateAttemptsQuery {
    pub release_id: Option<String>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct AgentUpdateOffer {
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

#[derive(Serialize)]
pub struct AgentUpdateManifest {
    pub update: Option<AgentUpdateOffer>,
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

#[derive(Serialize, Deserialize, Debug, Clone)]
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
        #[serde(default)]
        package_type: Option<String>,
        #[serde(default)]
        native_arch: Option<String>,
        #[serde(default)]
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
        #[serde(default)]
        retry_count: i64,
        status: String,
        message: Option<String>,
    },
}

#[derive(Serialize, Deserialize, Debug)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum TerminalClientMessage {
    Input { data: String },
    Resize { cols: u16, rows: u16 },
}

#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum TerminalServerMessage {
    Opening,
    Ready,
    Output {
        data: String,
    },
    Closed {
        exit_code: Option<i64>,
        reason: Option<String>,
    },
    Error {
        message: String,
    },
}
