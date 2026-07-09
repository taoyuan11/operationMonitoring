mod command;
mod config;
mod http;
mod identity;
mod metrics;
mod models;
mod profile;
mod time;
mod ws;

use anyhow::Result;
use clap::Parser;
use config::Cli;
use http::report_loop;
use identity::load_or_create_identity;
use ws::agent_ws_loop;

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    let identity = load_or_create_identity(cli.identity_file.clone())?;
    println!("agent instance_id: {}", identity.instance_id);
    println!("server: {}", cli.server);

    let http_client = reqwest::Client::new();
    let report_task = tokio::spawn(report_loop(
        cli.clone(),
        identity.clone(),
        http_client.clone(),
    ));
    let ws_task = tokio::spawn(agent_ws_loop(cli, identity));

    let (report_result, ws_result) = tokio::join!(report_task, ws_task);
    report_result??;
    ws_result??;
    Ok(())
}
