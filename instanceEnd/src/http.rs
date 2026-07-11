use anyhow::Result;

use crate::{
    config::AgentConfig,
    models::{AgentRegisterRequest, AgentRegisterResponse, Identity},
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
    let payload = AgentRegisterRequest {
        instance_id: identity.instance_id.clone(),
        secret: identity.secret.clone(),
        hostname: profile.hostname,
        os: profile.os,
        arch: profile.arch,
        agent_version: profile.agent_version,
        package_type: capability.package_type,
        native_arch: capability.native_arch,
        update_privileged: Some(capability.update_privileged),
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
