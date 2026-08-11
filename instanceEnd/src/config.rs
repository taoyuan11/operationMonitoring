use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use clap::{Args, Parser, Subcommand};
use reqwest::Url;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServerEndpoint {
    base: Url,
}

impl ServerEndpoint {
    pub fn parse(value: &str) -> Result<Self> {
        let parsed = Url::parse(value.trim()).context("invalid server URL")?;
        if !matches!(parsed.scheme(), "http" | "https") || parsed.host_str().is_none() {
            bail!("server URL must be an absolute HTTP or HTTPS URL");
        }
        if !parsed.username().is_empty() || parsed.password().is_some() {
            bail!("server URL must not contain user information");
        }
        if parsed.query().is_some() || parsed.fragment().is_some() {
            bail!("server URL must not contain a query or fragment");
        }

        let mut normalized = parsed.as_str().trim_end_matches('/').to_owned();
        normalized.push('/');
        Ok(Self {
            base: Url::parse(&normalized).context("failed to normalize server URL")?,
        })
    }

    pub fn normalized_server(&self) -> String {
        self.base.as_str().trim_end_matches('/').to_owned()
    }

    pub fn http_url(&self, relative_path: &str) -> Result<Url> {
        if relative_path.is_empty()
            || relative_path.starts_with('/')
            || relative_path.contains(['?', '#'])
            || relative_path
                .split('/')
                .any(|segment| segment.is_empty() || matches!(segment, "." | ".."))
        {
            bail!("server API path must be relative");
        }
        self.base
            .join(relative_path)
            .context("failed to construct server API URL")
    }

    pub fn websocket_url(&self, relative_path: &str) -> Result<Url> {
        let mut url = self.http_url(relative_path)?;
        let scheme = if self.is_https() { "wss" } else { "ws" };
        url.set_scheme(scheme)
            .map_err(|_| anyhow::anyhow!("failed to construct server WebSocket URL"))?;
        Ok(url)
    }

    pub fn is_https(&self) -> bool {
        self.base.scheme() == "https"
    }
}

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
    /// Show existing agent logs and follow new output
    Log,
    /// Force-install a local agent package when automatic updates are unavailable
    Update {
        /// Path to the replacement om-agent executable
        package: PathBuf,
    },
    #[command(name = "service-run", hide = true)]
    ServiceRun,
    #[command(name = "apply-update", hide = true)]
    ApplyUpdate {
        #[arg(long)]
        plan_file: PathBuf,
    },
    #[command(name = "desktop-helper", hide = true)]
    DesktopHelper {
        #[arg(long)]
        pipe: String,
        #[arg(long, default_value_t = 1920)]
        max_width: u32,
        #[arg(long, default_value_t = 1080)]
        max_height: u32,
        #[arg(long, default_value_t = 8)]
        min_fps: u8,
        #[arg(long, default_value_t = 12)]
        max_fps: u8,
        #[arg(long, default_value_t = 70)]
        jpeg_quality: u8,
        #[arg(long)]
        audio_codec: Option<String>,
        #[arg(long, hide = true)]
        system_helper: bool,
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
    /// Maximum size of one log file before rotation
    #[arg(
        long,
        env = "OM_AGENT_LOG_MAX_BYTES",
        default_value_t = 10 * 1024 * 1024,
        value_parser = clap::value_parser!(u64).range(1..),
        global = true
    )]
    pub log_max_bytes: u64,
    /// Number of rotated log files to retain
    #[arg(long, env = "OM_AGENT_LOG_HISTORY", default_value_t = 3, global = true)]
    pub log_history: usize,
    /// Persistent directory used for downloaded packages and update state
    #[arg(long, env = "OM_AGENT_UPDATE_DIR", global = true)]
    pub update_dir: Option<PathBuf>,
}

impl AgentConfig {
    pub fn server_endpoint(&self) -> Result<ServerEndpoint> {
        ServerEndpoint::parse(&self.server)
    }

    pub fn normalize_server(&mut self) -> Result<()> {
        self.server = self.server_endpoint()?.normalized_server();
        Ok(())
    }

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
        command
            .arg("--log-max-bytes")
            .arg(self.log_max_bytes.to_string())
            .arg("--log-history")
            .arg(self.log_history.to_string());
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
    fn parses_the_log_follow_command() {
        let cli = Cli::try_parse_from(["agent", "log"]).unwrap();

        assert_eq!(cli.command, AgentCommand::Log);
    }

    #[test]
    fn parses_a_forced_update_package_path() {
        let cli = Cli::try_parse_from(["agent", "update", "/tmp/om-agent.next"]).unwrap();

        assert_eq!(
            cli.command,
            AgentCommand::Update {
                package: PathBuf::from("/tmp/om-agent.next")
            }
        );
    }

    #[test]
    fn parses_log_rotation_options() {
        let cli = Cli::try_parse_from([
            "agent",
            "start",
            "--log-max-bytes",
            "2048",
            "--log-history",
            "0",
        ])
        .unwrap();

        assert_eq!(cli.agent.log_max_bytes, 2048);
        assert_eq!(cli.agent.log_history, 0);
    }

    #[test]
    fn rejects_zero_log_size() {
        assert!(Cli::try_parse_from(["agent", "start", "--log-max-bytes", "0"]).is_err());
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

    #[test]
    fn normalizes_http_and_https_server_urls() {
        let http = ServerEndpoint::parse(" HTTP://Monitor.Example:80/base/// ").unwrap();
        assert_eq!(http.normalized_server(), "http://monitor.example/base");
        assert_eq!(
            http.http_url("api/agent/register").unwrap().as_str(),
            "http://monitor.example/base/api/agent/register"
        );
        assert_eq!(
            http.websocket_url("api/agent/ws").unwrap().as_str(),
            "ws://monitor.example/base/api/agent/ws"
        );

        let https = ServerEndpoint::parse("HTTPS://monitor.example/root").unwrap();
        assert!(https.is_https());
        assert_eq!(
            https.websocket_url("api/agent/ws").unwrap().as_str(),
            "wss://monitor.example/root/api/agent/ws"
        );
    }

    #[test]
    fn rejects_ambiguous_or_credentialed_server_urls() {
        for server in [
            "monitor.example",
            "ftp://monitor.example",
            "https://user@monitor.example",
            "https://monitor.example?path=/ignored",
            "https://monitor.example/#fragment",
        ] {
            assert!(ServerEndpoint::parse(server).is_err(), "accepted {server}");
        }
    }
}
