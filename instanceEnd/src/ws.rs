use std::time::Duration;

use anyhow::Result;
use futures_util::{SinkExt, StreamExt};
use tokio::sync::mpsc;
use tokio_tungstenite::{connect_async, tungstenite::Message};

use crate::{
    command::execute_command,
    config::AgentConfig,
    http::register_once,
    metrics::MetricsCollector,
    models::{AgentInbound, AgentOutbound, Identity},
    profile::host_profile,
    terminal::TerminalManager,
};

pub async fn agent_ws_loop(config: AgentConfig, identity: Identity) -> Result<()> {
    let http_client = reqwest::Client::new();
    loop {
        match register_once(&config, &identity, &http_client).await {
            Ok(response) if response.disabled => {
                println!("websocket paused: instance disabled");
                tokio::time::sleep(Duration::from_secs(10)).await;
                continue;
            }
            Ok(response) if !response.approved => {
                println!("websocket waiting for approval: {}", response.message);
                tokio::time::sleep(Duration::from_secs(10)).await;
                continue;
            }
            Ok(_) => {}
            Err(error) => {
                eprintln!("register before websocket failed: {error:#}");
                tokio::time::sleep(Duration::from_secs(10)).await;
                continue;
            }
        }

        let url = websocket_url(&config.server, &identity);
        println!("connecting websocket: {url}");
        match connect_async(&url).await {
            Ok((stream, _)) => {
                println!("websocket connected");
                if let Err(error) = handle_agent_socket(stream, &config).await {
                    eprintln!("websocket error: {error:#}");
                }
            }
            Err(error) => eprintln!("websocket connect failed: {error:#}"),
        }
        tokio::time::sleep(Duration::from_secs(5)).await;
    }
}

async fn handle_agent_socket(
    stream: tokio_tungstenite::WebSocketStream<
        tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
    >,
    config: &AgentConfig,
) -> Result<()> {
    let (mut write, mut read) = stream.split();
    let (outbound_tx, mut outbound_rx) = mpsc::unbounded_channel::<AgentInbound>();
    let mut terminals = TerminalManager::new(outbound_tx.clone());
    let mut collector = MetricsCollector::new();
    let mut report_interval =
        tokio::time::interval(Duration::from_secs(config.report_interval.max(1)));
    report_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    loop {
        tokio::select! {
            _ = report_interval.tick() => {
                let profile = host_profile();
                outbound_tx.send(AgentInbound::Metrics {
                    hostname: profile.hostname,
                    os: profile.os,
                    arch: profile.arch,
                    agent_version: profile.agent_version,
                    metrics: collector.sample(),
                })?;
            }
            outbound = outbound_rx.recv() => {
                let Some(outbound) = outbound else {
                    break;
                };
                let payload = serde_json::to_string(&outbound)?;
                write.send(Message::Text(payload.into())).await?;
            }
            incoming = read.next() => {
                let Some(incoming) = incoming else {
                    break;
                };
                match incoming? {
                    Message::Text(text) => {
                        let message = serde_json::from_str::<AgentOutbound>(&text)?;
                        match message {
                            AgentOutbound::Ping { now } => {
                                outbound_tx.send(AgentInbound::Pong { now })?;
                            }
                            AgentOutbound::RunCommand { job_id, command } => {
                                println!("running command job {job_id}: {command}");
                                let command_outbound = outbound_tx.clone();
                                tokio::spawn(async move {
                                    let (exit_code, output) = execute_command(&command).await;
                                    let _ = command_outbound.send(AgentInbound::CommandResult {
                                        job_id,
                                        exit_code,
                                        output,
                                    });
                                });
                            }
                            AgentOutbound::TerminalOpen { session_id, cols, rows } => {
                                terminals.open(session_id, cols, rows);
                            }
                            AgentOutbound::TerminalInput { session_id, data } => {
                                terminals.input(&session_id, &data);
                            }
                            AgentOutbound::TerminalResize { session_id, cols, rows } => {
                                terminals.resize(&session_id, cols, rows);
                            }
                            AgentOutbound::TerminalClose { session_id } => {
                                terminals.close(&session_id);
                            }
                        }
                    }
                    Message::Ping(data) => {
                        write.send(Message::Pong(data)).await?;
                    }
                    Message::Close(_) => break,
                    _ => {}
                }
            }
        }
    }

    terminals.close_all();
    Ok(())
}

fn websocket_url(server: &str, identity: &Identity) -> String {
    let trimmed = server.trim_end_matches('/');
    let base = if let Some(rest) = trimmed.strip_prefix("https://") {
        format!("wss://{rest}")
    } else if let Some(rest) = trimmed.strip_prefix("http://") {
        format!("ws://{rest}")
    } else {
        format!("ws://{trimmed}")
    };
    format!(
        "{base}/api/agent/ws?instance_id={}&secret={}",
        identity.instance_id, identity.secret
    )
}
