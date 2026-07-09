use std::time::Duration;

use anyhow::Result;
use futures_util::{SinkExt, StreamExt};
use tokio_tungstenite::{connect_async, tungstenite::Message};

use crate::{
    command::execute_command,
    config::Cli,
    http::register_once,
    models::{AgentInbound, AgentOutbound, Identity},
};

pub async fn agent_ws_loop(cli: Cli, identity: Identity) -> Result<()> {
    let http_client = reqwest::Client::new();
    loop {
        match register_once(&cli, &identity, &http_client).await {
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

        let url = websocket_url(&cli.server, &identity);
        println!("connecting websocket: {url}");
        match connect_async(&url).await {
            Ok((stream, _)) => {
                println!("websocket connected");
                if let Err(error) = handle_agent_socket(stream).await {
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
) -> Result<()> {
    let (mut write, mut read) = stream.split();
    while let Some(message) = read.next().await {
        match message? {
            Message::Text(text) => {
                let outbound = serde_json::from_str::<AgentOutbound>(&text)?;
                match outbound {
                    AgentOutbound::Ping { now } => {
                        let payload = serde_json::to_string(&AgentInbound::Pong { now })?;
                        write.send(Message::Text(payload.into())).await?;
                    }
                    AgentOutbound::RunCommand { job_id, command } => {
                        println!("running command job {job_id}: {command}");
                        let (exit_code, output) = execute_command(&command).await;
                        let payload = serde_json::to_string(&AgentInbound::CommandResult {
                            job_id,
                            exit_code,
                            output,
                        })?;
                        write.send(Message::Text(payload.into())).await?;
                    }
                }
            }
            Message::Close(_) => break,
            _ => {}
        }
    }
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
