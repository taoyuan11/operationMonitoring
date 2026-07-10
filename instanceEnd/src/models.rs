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
}
