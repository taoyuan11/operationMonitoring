use crate::models::HostProfile;

const AGENT_VERSION: &str = env!("CARGO_PKG_VERSION");

pub fn host_profile() -> HostProfile {
    HostProfile {
        hostname: hostname::get()
            .ok()
            .and_then(|name| name.into_string().ok())
            .unwrap_or_else(|| "unknown-host".to_string()),
        os: std::env::consts::OS.to_string(),
        arch: std::env::consts::ARCH.to_string(),
        agent_version: AGENT_VERSION.to_string(),
    }
}
