use std::{net::SocketAddr, path::PathBuf};

use clap::Parser;

#[derive(Parser, Debug)]
#[command(version, about = "Operation Monitoring backend")]
pub struct Cli {
    #[arg(long, env = "OM_BIND", default_value = "0.0.0.0:13500")]
    pub bind: SocketAddr,
    #[arg(
        long,
        env = "OM_DATABASE_URL",
        default_value = "sqlite://db/operation-monitoring.db"
    )]
    pub database_url: String,
    #[arg(long, env = "OM_ADMIN_PASSWORD", default_value = "admin123")]
    pub admin_password: String,
    #[arg(long, env = "OM_AUTH_SECRET_KEY")]
    pub auth_secret_key: Option<String>,
    #[arg(long, env = "OM_AUTH_KEY_FILE", default_value = "db/auth-secret.key")]
    pub auth_key_file: PathBuf,
    #[arg(long, env = "OM_SECURE_COOKIES", default_value_t = false)]
    pub secure_cookies: bool,
    #[arg(long, default_value_t = false)]
    pub reset_admin_auth: bool,
    #[arg(long)]
    pub confirm_reset_admin_auth: Option<String>,
    #[arg(long, env = "OM_UPLOAD_DIR", default_value = "uploads")]
    pub upload_dir: PathBuf,
    #[arg(long, env = "OM_UPDATE_DIR", default_value = "updates")]
    pub update_dir: PathBuf,
    #[arg(
        long,
        env = "OM_AGENT_PACKAGE_MAX_BYTES",
        default_value_t = 256 * 1024 * 1024
    )]
    pub agent_package_max_bytes: usize,
}
