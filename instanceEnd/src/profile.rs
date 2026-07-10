use crate::models::HostProfile;
use std::sync::OnceLock;
use sysinfo::System;

const AGENT_VERSION: &str = env!("CARGO_PKG_VERSION");
static OPERATING_SYSTEM: OnceLock<String> = OnceLock::new();

pub fn host_profile() -> HostProfile {
    HostProfile {
        hostname: hostname::get()
            .ok()
            .and_then(|name| name.into_string().ok())
            .unwrap_or_else(|| "unknown-host".to_string()),
        os: operating_system(),
        arch: std::env::consts::ARCH.to_string(),
        agent_version: AGENT_VERSION.to_string(),
    }
}

fn operating_system() -> String {
    OPERATING_SYSTEM
        .get_or_init(|| normalize_os(&System::distribution_id()))
        .clone()
}

fn normalize_os(distribution_id: &str) -> String {
    let distribution_id = distribution_id.trim();
    if distribution_id.is_empty() {
        std::env::consts::OS.to_string()
    } else {
        distribution_id.to_ascii_lowercase()
    }
}

#[cfg(test)]
mod tests {
    use super::normalize_os;

    #[test]
    fn normalizes_distribution_id() {
        assert_eq!(normalize_os(" Ubuntu "), "ubuntu");
    }

    #[test]
    fn falls_back_when_distribution_id_is_empty() {
        assert_eq!(normalize_os("  "), std::env::consts::OS);
    }
}
