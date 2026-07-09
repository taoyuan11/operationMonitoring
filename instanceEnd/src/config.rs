use std::path::PathBuf;

use clap::Parser;

#[derive(Parser, Debug, Clone)]
#[command(version, about = "Operation Monitoring agent")]
pub struct Cli {
    #[arg(long, env = "OM_SERVER", default_value = "http://127.0.0.1:13500")]
    pub server: String,
    #[arg(long, env = "OM_AGENT_ID_FILE")]
    pub identity_file: Option<PathBuf>,
    #[arg(long, env = "OM_REPORT_INTERVAL", default_value_t = 5)]
    pub report_interval: u64,
}
