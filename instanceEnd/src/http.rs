use std::time::Duration;

use anyhow::Result;

use crate::{
    config::AgentConfig,
    device_profile::collect_device_profile,
    models::{AgentRegisterRequest, AgentRegisterResponse, DeviceProfile, Identity},
    profile::host_profile,
    update::update_capability,
};

pub async fn register_once(
    config: &AgentConfig,
    identity: &Identity,
    http_client: &reqwest::Client,
) -> Result<AgentRegisterResponse> {
    let profile = host_profile();
    let capability = update_capability();
    let device_profile =
        collect_device_profile_bounded(Duration::from_secs(10), collect_device_profile).await;
    let payload = AgentRegisterRequest {
        instance_id: identity.instance_id.clone(),
        secret: identity.secret.clone(),
        previous_secret: identity.previous_secret.clone(),
        hostname: profile.hostname,
        os: profile.os,
        arch: profile.arch,
        agent_version: profile.agent_version,
        package_type: capability.package_type,
        native_arch: capability.native_arch,
        update_privileged: Some(capability.update_privileged),
        device_profile,
    };
    let url = format!("{}/api/agent/register", config.server.trim_end_matches('/'));
    let response = http_client
        .post(url)
        .json(&payload)
        .send()
        .await?
        .error_for_status()?
        .json::<AgentRegisterResponse>()
        .await?;
    Ok(response)
}

async fn collect_device_profile_bounded<F>(timeout: Duration, collect: F) -> Option<DeviceProfile>
where
    F: FnOnce() -> DeviceProfile + Send + 'static,
{
    match tokio::time::timeout(timeout, tokio::task::spawn_blocking(collect)).await {
        Ok(Ok(profile)) => Some(profile),
        Ok(Err(error)) => {
            crate::logging::error(format_args!("device profile collection failed: {error:#}"));
            None
        }
        Err(_) => {
            crate::logging::error(format_args!(
                "device profile collection timed out; registering without it"
            ));
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::{DeviceCpuInfo, DeviceSystemInfo};

    fn empty_device_profile() -> DeviceProfile {
        DeviceProfile {
            schema_version: 1,
            collected_at: 1,
            system: DeviceSystemInfo {
                os_name: String::new(),
                os_version: String::new(),
                kernel_version: String::new(),
                architecture: String::new(),
            },
            cpu: DeviceCpuInfo {
                model: String::new(),
                vendor: String::new(),
                physical_cores: None,
                logical_cores: 0,
                frequency_mhz: None,
            },
            memory_total: 0,
            storage_total: 0,
            gpus: Vec::new(),
            disks: Vec::new(),
            network_interfaces: Vec::new(),
        }
    }

    #[tokio::test]
    async fn registration_degrades_when_device_collection_times_out() {
        let profile = collect_device_profile_bounded(Duration::from_millis(1), || {
            std::thread::sleep(Duration::from_millis(25));
            empty_device_profile()
        })
        .await;

        assert!(profile.is_none());
    }
}
