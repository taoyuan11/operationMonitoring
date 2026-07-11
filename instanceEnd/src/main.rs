mod activity;
mod command;
mod config;
mod http;
mod identity;
mod install;
mod lifecycle;
mod metrics;
mod models;
mod profile;
mod terminal;
mod time;
mod update;
mod ws;

use anyhow::{Result, bail};
use clap::Parser;
use config::{AgentCommand, Cli};
use lifecycle::{run_agent, start, status, stop};

fn main() -> Result<()> {
    let cli = Cli::parse();
    if cli.command == AgentCommand::ServiceRun {
        return install::run_service(cli.agent);
    }

    if cli.daemon_child {
        if cli.command != AgentCommand::Start {
            bail!("invalid internal agent invocation");
        }
        return tokio::runtime::Runtime::new()?.block_on(run_agent(cli.agent));
    }

    match cli.command {
        AgentCommand::Install {
            non_interactive,
            yes,
        } => install::install(cli.agent, non_interactive, yes),
        AgentCommand::Uninstall { yes } => install::uninstall(yes),
        AgentCommand::Start => start(&cli.agent),
        AgentCommand::Stop { timeout } => stop(&cli.agent, timeout),
        AgentCommand::Status => status(&cli.agent),
        AgentCommand::Log => tokio::runtime::Runtime::new()?.block_on(run_agent(cli.agent)),
        AgentCommand::ServiceRun => unreachable!(),
        AgentCommand::ApplyUpdate { plan_file } => update::apply_update(&plan_file),
    }
}
