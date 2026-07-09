use serde::{Deserialize, Serialize};
use sqlx::FromRow;

#[derive(Serialize)]
pub struct HealthResponse {
    pub status: &'static str,
    pub now: i64,
}

#[derive(Deserialize)]
pub struct LoginRequest {
    pub username: String,
    pub password: String,
}

#[derive(Serialize)]
pub struct LoginResponse {
    pub username: String,
    pub role: &'static str,
}

#[derive(Serialize)]
pub struct MeResponse {
    pub authenticated: bool,
    pub username: Option<String>,
}

#[derive(Deserialize)]
pub struct AgentRegisterRequest {
    pub instance_id: String,
    pub secret: String,
    pub hostname: String,
    pub os: String,
    pub arch: String,
    pub agent_version: String,
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
    pub remark: String,
    pub hostname: String,
    pub os: String,
    pub arch: String,
    pub agent_version: String,
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
    pub remark: String,
    pub hostname: String,
    pub os: String,
    pub arch: String,
    pub agent_version: String,
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
}

#[derive(Deserialize)]
pub struct UpdateInstanceRequest {
    pub name: Option<String>,
    pub region: Option<String>,
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
}

#[derive(Deserialize)]
pub struct SettingsRequest {
    pub retention_days: i64,
}

#[derive(Serialize)]
pub struct AppearanceResponse {
    pub background_image_url: Option<String>,
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
}

#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AgentOutbound {
    RunCommand { job_id: String, command: String },
    Ping { now: i64 },
}

#[derive(Serialize, Deserialize, Debug)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AgentInbound {
    Pong {
        now: i64,
    },
    CommandResult {
        job_id: String,
        exit_code: i64,
        output: String,
    },
}

#[derive(Debug, Clone)]
pub struct CommandOutcome {
    pub job_id: String,
    pub exit_code: i64,
    pub output: String,
}
