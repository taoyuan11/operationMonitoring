use anyhow::Result;

use crate::{
    config::Cli,
    metrics::MetricsCollector,
    models::{
        AgentRegisterRequest, AgentRegisterResponse, AgentReportRequest, HostProfile, Identity,
        MetricPayload,
    },
    profile::host_profile,
};

pub async fn report_loop(cli: Cli, identity: Identity, http_client: reqwest::Client) -> Result<()> {
    let mut collector = MetricsCollector::new();
    let interval = std::time::Duration::from_secs(cli.report_interval.max(1));

    loop {
        let profile = host_profile();
        match report_once(&cli, &identity, &profile, &http_client, collector.sample()).await {
            Ok(response) => {
                println!("report: {}", response.message);
                if response.disabled {
                    println!("instance disabled by backend; reporting paused");
                }
            }
            Err(error) => eprintln!("report failed: {error:#}"),
        }
        tokio::time::sleep(interval).await;
    }
}

pub async fn report_once(
    cli: &Cli,
    identity: &Identity,
    profile: &HostProfile,
    http_client: &reqwest::Client,
    metrics: MetricPayload,
) -> Result<AgentRegisterResponse> {
    let payload = AgentReportRequest {
        instance_id: identity.instance_id.clone(),
        secret: identity.secret.clone(),
        hostname: profile.hostname.clone(),
        os: profile.os.clone(),
        arch: profile.arch.clone(),
        agent_version: profile.agent_version.clone(),
        metrics,
    };
    let url = format!("{}/api/agent/report", cli.server.trim_end_matches('/'));
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

pub async fn register_once(
    cli: &Cli,
    identity: &Identity,
    http_client: &reqwest::Client,
) -> Result<AgentRegisterResponse> {
    let profile = host_profile();
    let payload = AgentRegisterRequest {
        instance_id: identity.instance_id.clone(),
        secret: identity.secret.clone(),
        hostname: profile.hostname,
        os: profile.os,
        arch: profile.arch,
        agent_version: profile.agent_version,
    };
    let url = format!("{}/api/agent/register", cli.server.trim_end_matches('/'));
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
