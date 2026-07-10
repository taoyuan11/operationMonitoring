use std::path::PathBuf;

use clap::{Args, Parser, Subcommand};

#[derive(Parser, Debug)]
#[command(version, about = "Operation Monitoring agent")]
pub struct Cli {
    #[command(subcommand)]
    pub command: AgentCommand,
    #[command(flatten)]
    pub agent: AgentConfig,
    #[arg(long, hide = true, global = true)]
    pub daemon_child: bool,
}

#[derive(Subcommand, Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentCommand {
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
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
