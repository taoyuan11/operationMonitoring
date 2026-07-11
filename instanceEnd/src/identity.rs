use std::{
    fs::{self, OpenOptions},
    io::Write,
    path::PathBuf,
};

use anyhow::{Context, Result};
use directories::ProjectDirs;
use uuid::Uuid;

use crate::models::Identity;

pub fn load_or_create_identity(path: Option<PathBuf>) -> Result<Identity> {
    let path = identity_path(path)?;
    if path.exists() {
        let content = fs::read_to_string(&path)
            .with_context(|| format!("failed to read identity file {}", path.display()))?;
        return Ok(serde_json::from_str(&content)?);
    }

    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let identity = Identity {
        instance_id: Uuid::new_v4().to_string(),
        secret: Uuid::new_v4().to_string(),
    };
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options
        .open(&path)
        .with_context(|| format!("failed to create identity file {}", path.display()))?;
    file.write_all(serde_json::to_string_pretty(&identity)?.as_bytes())?;
    Ok(identity)
}

fn identity_path(path: Option<PathBuf>) -> Result<PathBuf> {
    if let Some(path) = path {
        return Ok(path);
    }
    if let Some(project_dirs) = ProjectDirs::from("com", "operation-monitoring", "agent") {
        return Ok(project_dirs.config_dir().join("identity.json"));
    }
    Ok(std::env::current_dir()?.join("agent_identity.json"))
}

#[cfg(all(test, unix))]
mod tests {
    use std::os::unix::fs::PermissionsExt;

    use super::*;

    #[test]
    fn creates_identity_with_owner_only_permissions() {
        let directory = std::env::temp_dir().join(format!("om-agent-identity-{}", Uuid::new_v4()));
        let path = directory.join("identity.json");

        load_or_create_identity(Some(path.clone())).unwrap();

        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        let _ = fs::remove_dir_all(directory);
    }
}
