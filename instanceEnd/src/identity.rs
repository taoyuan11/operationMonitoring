use std::{
    fs::{self, OpenOptions},
    io::Write,
    path::PathBuf,
    thread,
    time::{Duration, Instant},
};

use anyhow::{Context, Result};
use directories::ProjectDirs;
use uuid::Uuid;

use crate::models::Identity;

const IDENTITY_CREATE_TIMEOUT: Duration = Duration::from_secs(5);
const IDENTITY_READ_RETRY: Duration = Duration::from_millis(10);

pub fn load_or_create_identity(path: Option<PathBuf>) -> Result<Identity> {
    let path = identity_path(path)?;
    match read_identity(&path) {
        Ok(identity) => return Ok(identity),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) if error.kind() == std::io::ErrorKind::InvalidData => {
            return wait_for_created_identity(&path)
                .with_context(|| format!("failed to read identity file {}", path.display()));
        }
        Err(error) => {
            return Err(error)
                .with_context(|| format!("failed to read identity file {}", path.display()));
        }
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
    match options.open(&path) {
        Ok(mut file) => {
            file.write_all(serde_json::to_string_pretty(&identity)?.as_bytes())?;
            file.sync_all()?;
            Ok(identity)
        }
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            wait_for_created_identity(&path).with_context(|| {
                format!(
                    "failed to read identity file created by another process {}",
                    path.display()
                )
            })
        }
        Err(error) => {
            Err(error).with_context(|| format!("failed to create identity file {}", path.display()))
        }
    }
}

fn read_identity(path: &std::path::Path) -> std::io::Result<Identity> {
    let content = fs::read_to_string(path)?;
    serde_json::from_str(&content)
        .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))
}

fn wait_for_created_identity(path: &std::path::Path) -> std::io::Result<Identity> {
    let started = Instant::now();
    loop {
        match read_identity(path) {
            Ok(identity) => return Ok(identity),
            Err(_) if started.elapsed() < IDENTITY_CREATE_TIMEOUT => {
                thread::sleep(IDENTITY_READ_RETRY);
            }
            Err(error) => return Err(error),
        }
    }
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
    use std::{
        os::unix::fs::PermissionsExt,
        sync::{Arc, Barrier},
    };

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

    #[test]
    fn concurrent_first_loads_share_the_created_identity() {
        let directory = std::env::temp_dir().join(format!("om-agent-identity-{}", Uuid::new_v4()));
        let path = directory.join("identity.json");
        let barrier = Arc::new(Barrier::new(8));
        let handles = (0..8)
            .map(|_| {
                let path = path.clone();
                let barrier = barrier.clone();
                std::thread::spawn(move || {
                    barrier.wait();
                    load_or_create_identity(Some(path)).unwrap()
                })
            })
            .collect::<Vec<_>>();

        let identities = handles
            .into_iter()
            .map(|handle| handle.join().unwrap())
            .collect::<Vec<_>>();
        assert!(identities.iter().all(|identity| {
            identity.instance_id == identities[0].instance_id
                && identity.secret == identities[0].secret
        }));
        assert_eq!(
            load_or_create_identity(Some(path)).unwrap().instance_id,
            identities[0].instance_id
        );

        let _ = fs::remove_dir_all(directory);
    }
}
