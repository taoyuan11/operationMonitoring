use std::{fs, path::PathBuf};

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
    fs::write(&path, serde_json::to_string_pretty(&identity)?)?;
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
