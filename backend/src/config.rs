use std::{net::SocketAddr, path::PathBuf};

use clap::Parser;
use ipnet::IpNet;

const EXAMPLE_BOOTSTRAP_PASSWORD: &str = "replace-with-a-long-random-bootstrap-password";

#[derive(Parser, Debug)]
#[command(version, about = "Operation Monitoring backend")]
pub struct Cli {
    #[arg(long, env = "OM_BIND", default_value = "0.0.0.0:13500")]
    pub bind: SocketAddr,
    #[arg(
        long,
        env = "OM_DATABASE_URL",
        default_value = "postgresql://root@127.0.0.1:5432/operation_monitoring"
    )]
    pub database_url: String,
    #[arg(long, env = "OM_DATABASE_PASSWORD")]
    pub database_password: Option<String>,
    #[arg(long, env = "OM_ADMIN_PASSWORD")]
    pub admin_password: Option<String>,
    #[arg(long, env = "OM_AUTH_SECRET_KEY")]
    pub auth_secret_key: Option<String>,
    #[arg(long, env = "OM_AUTH_KEY_FILE", default_value = "auth/auth-secret.key")]
    pub auth_key_file: PathBuf,
    #[arg(long, env = "OM_SECURE_COOKIES", default_value_t = false)]
    pub secure_cookies: bool,
    #[arg(long, env = "OM_TRUST_PROXY_HEADERS", default_value_t = false)]
    pub trust_proxy_headers: bool,
    #[arg(
        long,
        env = "OM_TRUSTED_PROXY_CIDRS",
        value_delimiter = ',',
        default_value = "127.0.0.1/32,::1/128"
    )]
    pub trusted_proxy_cidrs: Vec<IpNet>,
    #[arg(long, env = "OM_ALLOW_LEGACY_AGENT_WS_AUTH", default_value_t = false)]
    pub allow_legacy_agent_ws_auth: bool,
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
    #[arg(
        long,
        env = "OM_FILE_TRANSFER_MAX_BYTES",
        default_value_t = 1024 * 1024 * 1024
    )]
    pub file_transfer_max_bytes: usize,
}

pub fn validate_bootstrap_password(password: &str) -> anyhow::Result<()> {
    const MIN_PASSWORD_BYTES: usize = 16;
    const MAX_PASSWORD_BYTES: usize = 1024;

    if password.len() < MIN_PASSWORD_BYTES {
        anyhow::bail!("OM_ADMIN_PASSWORD must contain at least {MIN_PASSWORD_BYTES} bytes");
    }
    if password.len() > MAX_PASSWORD_BYTES {
        anyhow::bail!("OM_ADMIN_PASSWORD must not exceed {MAX_PASSWORD_BYTES} bytes");
    }
    if password.chars().any(char::is_control) {
        anyhow::bail!("OM_ADMIN_PASSWORD must not contain control characters");
    }
    if password.trim() == EXAMPLE_BOOTSTRAP_PASSWORD {
        anyhow::bail!("OM_ADMIN_PASSWORD must not use the public example value");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use clap::Parser;

    use super::{Cli, EXAMPLE_BOOTSTRAP_PASSWORD, validate_bootstrap_password};

    #[test]
    fn bootstrap_password_requires_a_long_explicit_value() {
        assert!(validate_bootstrap_password("admin123").is_err());
        assert!(validate_bootstrap_password("long-random-passphrase").is_ok());
        assert!(validate_bootstrap_password("contains\ncontrol-value").is_err());
        assert!(validate_bootstrap_password(EXAMPLE_BOOTSTRAP_PASSWORD).is_err());
        assert!(validate_bootstrap_password(&format!("  {EXAMPLE_BOOTSTRAP_PASSWORD}  ")).is_err());
    }

    #[test]
    fn trusted_proxy_networks_parse_as_a_comma_separated_list() {
        let cli = Cli::try_parse_from([
            "backend",
            "--trusted-proxy-cidrs",
            "127.0.0.1/32,2001:db8::/32",
        ])
        .expect("parse trusted proxy networks");

        assert_eq!(cli.trusted_proxy_cidrs.len(), 2);
        assert!(
            cli.trusted_proxy_cidrs[0].contains(
                &"127.0.0.1"
                    .parse::<std::net::IpAddr>()
                    .expect("loopback IP")
            )
        );
        assert!(
            cli.trusted_proxy_cidrs[1].contains(
                &"2001:db8::1"
                    .parse::<std::net::IpAddr>()
                    .expect("IPv6 address")
            )
        );
    }
}
