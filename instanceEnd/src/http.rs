use std::time::Duration;

use anyhow::{Context, Result, bail};
use futures_util::StreamExt;
use serde::de::DeserializeOwned;

use crate::{
    config::AgentConfig,
    device_profile::collect_device_profile,
    models::{AgentRegisterRequest, AgentRegisterResponse, DeviceProfile, Identity},
    profile::host_profile,
    update::update_capability,
};

pub(crate) const MAX_AGENT_JSON_RESPONSE_BYTES: usize = 64 * 1024;
pub(crate) const AGENT_CONTROL_HTTP_TIMEOUT: Duration = Duration::from_secs(30);

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
    let url = config.server_endpoint()?.http_url("api/agent/register")?;
    let response = http_client
        .post(url)
        .timeout(AGENT_CONTROL_HTTP_TIMEOUT)
        .json(&payload)
        .send()
        .await?
        .error_for_status()?;
    bounded_json_response(
        response,
        MAX_AGENT_JSON_RESPONSE_BYTES,
        "agent registration response",
    )
    .await
}

pub(crate) async fn bounded_json_response<T>(
    response: reqwest::Response,
    limit: usize,
    description: &str,
) -> Result<T>
where
    T: DeserializeOwned,
{
    if response
        .content_length()
        .is_some_and(|length| length > u64::try_from(limit).unwrap_or(u64::MAX))
    {
        bail!("{description} exceeded the {limit}-byte limit");
    }

    let mut body = Vec::with_capacity(
        response
            .content_length()
            .and_then(|length| usize::try_from(length).ok())
            .unwrap_or_default()
            .min(limit),
    );
    let mut chunks = response.bytes_stream();
    while let Some(chunk) = chunks.next().await {
        append_bounded_response_chunk(&mut body, &chunk?, limit, description)?;
    }
    serde_json::from_slice(&body).with_context(|| format!("failed to decode {description}"))
}

fn append_bounded_response_chunk(
    body: &mut Vec<u8>,
    chunk: &[u8],
    limit: usize,
    description: &str,
) -> Result<()> {
    let next_length = body
        .len()
        .checked_add(chunk.len())
        .filter(|length| *length <= limit)
        .ok_or_else(|| anyhow::anyhow!("{description} exceeded the {limit}-byte limit"))?;
    body.reserve(next_length.saturating_sub(body.len()));
    body.extend_from_slice(chunk);
    Ok(())
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

    #[test]
    fn response_chunks_cannot_exceed_the_aggregate_limit() {
        let mut body = Vec::new();
        append_bounded_response_chunk(&mut body, b"1234", 8, "test response").unwrap();
        append_bounded_response_chunk(&mut body, b"5678", 8, "test response").unwrap();
        assert!(append_bounded_response_chunk(&mut body, b"9", 8, "test response").is_err());
        assert_eq!(body, b"12345678");
    }
}
