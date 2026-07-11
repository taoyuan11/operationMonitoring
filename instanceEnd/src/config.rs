use std::path::PathBuf;

use clap::{Args, Parser, Subcommand};

#[derive(Parser, Debug)]
#[command(name = "om-agent", version, about = "Operation Monitoring agent")]
pub struct Cli {
    #[command(subcommand)]
    pub command: AgentCommand,
    #[command(flatten)]
    pub agent: AgentConfig,
    #[arg(long, hide = true, global = true)]
    pub daemon_child: bool,
}

#[derive(Subcommand, Debug, Clone, PartialEq, Eq)]
pub enum AgentCommand {
    /// Install the agent as a system service
    Install {
        /// Run without prompts; --server is required
        #[arg(long)]
        non_interactive: bool,
        /// Accept destructive or system-wide changes without confirmation
        #[arg(long, short = 'y')]
        yes: bool,
    },
    /// Remove the system service, executable, configuration, and data
    Uninstall {
        /// Confirm removal without prompting
        #[arg(long, short = 'y')]
        yes: bool,
    },
    /// Start the agent in the background
    Start,
    /// Stop the background agent
    Stop {
        /// Seconds to wait for the agent to exit
        #[arg(long, default_value_t = 10)]
        timeout: u64,
    },
    /// Show whether the agent is running
    Status,
    /// Run the agent in the foreground and print logs
    Log,
    #[command(name = "service-run", hide = true)]
    ServiceRun,
    #[command(name = "apply-update", hide = true)]
    ApplyUpdate {
        #[arg(long)]
        plan_file: PathBuf,
    },
}

#[derive(Args, Debug, Clone)]
pub struct AgentConfig {
    #[arg(
        long,
        env = "OM_SERVER",
        default_value = "http://127.0.0.1:13500",
        global = true
    )]
    pub server: String,
    #[arg(long, env = "OM_AGENT_ID_FILE", global = true)]
    pub identity_file: Option<PathBuf>,
    #[arg(long, env = "OM_REPORT_INTERVAL", default_value_t = 5, global = true)]
    pub report_interval: u64,
    /// Directory used for the process lock and control files
    #[arg(long, env = "OM_AGENT_STATE_DIR", global = true)]
    pub state_dir: Option<PathBuf>,
    /// File that receives background process output
    #[arg(long, env = "OM_AGENT_LOG_FILE", global = true)]
    pub log_file: Option<PathBuf>,
    /// Persistent directory used for downloaded packages and update state
    #[arg(long, env = "OM_AGENT_UPDATE_DIR", global = true)]
    pub update_dir: Option<PathBuf>,
}

impl AgentConfig {
    pub fn append_cli_args(&self, command: &mut std::process::Command) {
        command
            .arg("--server")
            .arg(&self.server)
            .arg("--report-interval")
            .arg(self.report_interval.to_string());
        if let Some(path) = &self.identity_file {
            command.arg("--identity-file").arg(path);
        }
        if let Some(path) = &self.state_dir {
            command.arg("--state-dir").arg(path);
        }
        if let Some(path) = &self.log_file {
            command.arg("--log-file").arg(path);
        }
        if let Some(path) = &self.update_dir {
            command.arg("--update-dir").arg(path);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    #[test]
    fn exposes_the_short_command_name() {
        assert_eq!(Cli::command().get_name(), "om-agent");
    }

    #[test]
    fn accepts_global_options_after_subcommand() {
        let cli = Cli::try_parse_from([
            "agent",
            "start",
            "--server",
            "http://monitor.example",
            "--report-interval",
            "9",
        ])
        .unwrap();

        assert_eq!(cli.command, AgentCommand::Start);
        assert_eq!(cli.agent.server, "http://monitor.example");
        assert_eq!(cli.agent.report_interval, 9);
    }

    #[test]
    fn parses_unattended_install_options() {
        let cli = Cli::try_parse_from([
            "agent",
            "install",
            "--non-interactive",
            "--yes",
            "--server",
            "https://monitor.example",
        ])
        .unwrap();

        assert_eq!(
            cli.command,
            AgentCommand::Install {
                non_interactive: true,
                yes: true,
            }
        );
        assert_eq!(cli.agent.server, "https://monitor.example");
    }

    #[test]
    fn stop_timeout_defaults_to_ten_seconds() {
        let cli = Cli::try_parse_from(["agent", "stop"]).unwrap();

        assert_eq!(cli.command, AgentCommand::Stop { timeout: 10 });
    }

    #[test]
    fn log_runs_in_the_foreground() {
        let cli = Cli::try_parse_from(["agent", "log"]).unwrap();

        assert_eq!(cli.command, AgentCommand::Log);
    }

    #[test]
    fn run_is_not_a_supported_command() {
        assert!(Cli::try_parse_from(["agent", "run"]).is_err());
    }

    #[test]
    fn accepts_a_persistent_update_directory() {
        let cli =
            Cli::try_parse_from(["agent", "log", "--update-dir", "/var/lib/om-agent/updates"])
                .unwrap();

        assert_eq!(
            cli.agent.update_dir,
            Some(PathBuf::from("/var/lib/om-agent/updates"))
        );
    }
}
