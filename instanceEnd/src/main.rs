mod command;
mod config;
mod http;
mod identity;
mod lifecycle;
mod metrics;
mod models;
mod profile;
mod terminal;
mod time;
mod ws;

use anyhow::{Result, bail};
use clap::Parser;
use config::{AgentCommand, Cli};
use lifecycle::{run_agent, start, status, stop};

fn main() -> Result<()> {
    let cli = Cli::parse();
    if cli.daemon_child {
        if cli.command != AgentCommand::Start {
            bail!("invalid internal agent invocation");
        }
        return tokio::runtime::Runtime::new()?.block_on(run_agent(cli.agent));
    }

    match cli.command {
        AgentCommand::Start => start(&cli.agent),
        AgentCommand::Stop { timeout } => stop(&cli.agent, timeout),
        AgentCommand::Status => status(&cli.agent),
        AgentCommand::Log => tokio::runtime::Runtime::new()?.block_on(run_agent(cli.agent)),
    }
}
