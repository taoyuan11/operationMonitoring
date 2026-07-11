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
    UpdateStatus {
        release_id: String,
        artifact_id: String,
        version: String,
        retry_count: i64,
        status: UpdateStatus,
        #[serde(skip_serializing_if = "Option::is_none")]
        message: Option<String>,
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
}
