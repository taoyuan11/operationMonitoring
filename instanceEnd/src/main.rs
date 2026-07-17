mod activity;
mod command;
mod config;
mod file_manager;
mod http;
mod identity;
mod install;
mod lifecycle;
mod logging;
mod metrics;
mod models;
mod profile;
mod remote_desktop;
mod terminal;
mod time;
mod update;
mod ws;

use anyhow::{Context, Result, bail};
use clap::Parser;
use config::{AgentCommand, Cli};
use lifecycle::{follow_logs, run_agent, start, status, stop};

fn main() {
    if let Err(error) = run() {
        logging::error(format_args!("{error:#}"));
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    let cli = Cli::parse();
    if cli.command == AgentCommand::ServiceRun {
        init_agent_logging(&cli.agent)?;
        return install::run_service(cli.agent);
    }

    if cli.daemon_child {
        if cli.command != AgentCommand::Start {
            bail!("invalid internal agent invocation");
        }
        init_agent_logging(&cli.agent)?;
        return tokio::runtime::Runtime::new()?.block_on(run_agent(cli.agent));
    }

    match cli.command {
        AgentCommand::Install {
            non_interactive,
            yes,
        } => install::install(cli.agent, non_interactive, yes),
        AgentCommand::Uninstall { yes } => install::uninstall(cli.agent, yes),
        AgentCommand::Start => start(&cli.agent),
        AgentCommand::Stop { timeout } => stop(&cli.agent, timeout),
        AgentCommand::Status => status(&cli.agent),
        AgentCommand::Log => tokio::runtime::Runtime::new()?.block_on(follow_logs(&cli.agent)),
        AgentCommand::ServiceRun => unreachable!(),
        AgentCommand::ApplyUpdate { plan_file } => {
            let parent = plan_file
                .parent()
                .context("update plan path has no parent directory")?;
            logging::init(
                &parent.join("updater.log"),
                cli.agent.log_max_bytes,
                cli.agent.log_history,
            )?;
            update::apply_update(&plan_file)
        }
        AgentCommand::DesktopHelper {
            pipe,
            max_width,
            max_height,
            min_fps,
            max_fps,
            jpeg_quality,
            system_helper,
        } => remote_desktop::run_helper(remote_desktop::DesktopOptions {
            pipe,
            max_width,
            max_height,
            min_fps,
            max_fps,
            jpeg_quality,
            system_helper,
        }),
    }
}

fn init_agent_logging(config: &config::AgentConfig) -> Result<()> {
    logging::init(
        &lifecycle::log_file(config)?,
        config.log_max_bytes,
        config.log_history,
    )
}
