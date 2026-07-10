use std::{net::SocketAddr, path::PathBuf};

use clap::Parser;

#[derive(Parser, Debug)]
#[command(version, about = "Operation Monitoring backend")]
pub struct Cli {
    #[arg(long, env = "OM_BIND", default_value = "127.0.0.1:13500")]
    pub bind: SocketAddr,
    #[arg(
        long,
        env = "OM_DATABASE_URL",
        default_value = "sqlite://db/operation-monitoring.db"
    )]
    pub database_url: String,
    #[arg(long, env = "OM_ADMIN_PASSWORD", default_value = "admin123")]
    pub admin_password: String,
    #[arg(long, env = "OM_UPLOAD_DIR", default_value = "uploads")]
    pub upload_dir: PathBuf,
}
