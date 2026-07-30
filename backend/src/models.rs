use std::collections::BTreeMap;

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
    #[serde(default)]
    pub previous_secret: Option<String>,
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
    pub latency_ms: Option<f64>,
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
    #[serde(default)]
    pub secret: Option<String>,
    #[serde(default)]
    pub capabilities: Option<String>,
}

#[derive(Deserialize)]
pub struct DesktopAgentWsQuery {
    pub session_id: String,
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
    pub status: String,
    pub published_at: Option<i64>,
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
        #[serde(default)]
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
    DesktopOpened {
        session_id: String,
    },
    DesktopClosed {
        session_id: String,
        reason: String,
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
        #[serde(default)]
        retry_count: i64,
        status: String,
        message: Option<String>,
    },
}

fn default_docker_log_tail() -> u32 {
    200
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
