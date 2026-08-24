use std::{
    fs::{self, File, OpenOptions},
    io::{self, Read, Write},
    path::{Path, PathBuf},
    thread,
    time::{Duration, Instant},
};

use anyhow::{Context, Result};
use directories::ProjectDirs;
use uuid::Uuid;

use crate::models::Identity;

const IDENTITY_CREATE_TIMEOUT: Duration = Duration::from_secs(5);
const IDENTITY_READ_RETRY: Duration = Duration::from_millis(10);
const CURRENT_CREDENTIAL_VERSION: u32 = 1;

pub fn load_or_create_identity(path: Option<PathBuf>) -> Result<Identity> {
    let configured = path.is_some();
    let path = identity_path(path)?;
    let path = prepare_identity_location(&path, configured)?;
    match read_identity(&path) {
        Ok(identity) => return finish_loaded_identity(&path, identity),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) if error.kind() == std::io::ErrorKind::InvalidData => {
            let identity = wait_for_created_identity(&path)
                .with_context(|| format!("failed to read identity file {}", path.display()))?;
            return finish_loaded_identity(&path, identity);
        }
        Err(error) => {
            return Err(error)
                .with_context(|| format!("failed to read identity file {}", path.display()));
        }
    }

    let identity = Identity {
        instance_id: Uuid::new_v4().to_string(),
        secret: Uuid::new_v4().to_string(),
        credential_version: CURRENT_CREDENTIAL_VERSION,
        previous_secret: None,
    };
    match create_identity_file(&path) {
        Ok(mut file) => {
            file.write_all(serde_json::to_string_pretty(&identity)?.as_bytes())?;
            file.sync_all()?;
            drop(file);
            protect_identity_file(&path)?;
            Ok(identity)
        }
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            let identity = wait_for_created_identity(&path).with_context(|| {
                format!(
                    "failed to read identity file created by another process {}",
                    path.display()
                )
            })?;
            finish_loaded_identity(&path, identity)
        }
        Err(error) => {
            Err(error).with_context(|| format!("failed to create identity file {}", path.display()))
        }
    }
}

fn create_identity_file(path: &Path) -> io::Result<File> {
    #[cfg(windows)]
    if crate::privileged_path::is_process_privileged() {
        return crate::windows_security::create_private_file(path);
    }
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
    }
    options.open(path)
}

fn prepare_identity_location(path: &Path, configured: bool) -> Result<PathBuf> {
    let path = if let Some(parent) = path.parent() {
        let parent = crate::privileged_path::prepare_configured_directory(
            parent,
            configured,
            "agent identity directory",
        )?;
        let file_name = path
            .file_name()
            .context("agent identity path has no file name")?;
        let path = parent.join(file_name);
        #[cfg(windows)]
        if is_system_identity_path(&path) {
            crate::windows_security::restrict_to_system_and_administrators(&parent)?;
        }
        path
    } else {
        path.to_owned()
    };
    protect_identity_file(&path)?;
    Ok(path)
}

fn protect_identity_file(path: &Path) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(_) => crate::privileged_path::validate_regular_file(path, "agent identity file")?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error.into()),
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
    }
    #[cfg(windows)]
    if crate::privileged_path::is_process_privileged() {
        crate::windows_security::restrict_to_system_and_administrators(path)?;
    }
    Ok(())
}

fn finish_loaded_identity(path: &Path, identity: Identity) -> Result<Identity> {
    protect_identity_file(path)?;
    #[cfg(windows)]
    if is_system_identity_path(path) && identity.credential_version < CURRENT_CREDENTIAL_VERSION {
        return rotate_system_identity(path, identity);
    }
    Ok(identity)
}

#[cfg(windows)]
fn is_system_identity_path(path: &std::path::Path) -> bool {
    crate::windows_security::program_data_directory()
        .ok()
        .map(|program_data| program_data.join("OperationMonitoring"))
        .is_some_and(|data_dir| path.starts_with(data_dir))
}

#[cfg(windows)]
fn rotate_system_identity(path: &std::path::Path, mut identity: Identity) -> Result<Identity> {
    let previous_secret = std::mem::replace(&mut identity.secret, Uuid::new_v4().to_string());
    identity.credential_version = CURRENT_CREDENTIAL_VERSION;
    identity.previous_secret = Some(previous_secret);
    write_system_identity(path, &identity)
        .with_context(|| format!("failed to rotate identity secret {}", path.display()))?;
    Ok(identity)
}

#[cfg(windows)]
pub fn complete_secret_rotation(path: Option<PathBuf>, identity: &mut Identity) -> Result<()> {
    if identity.previous_secret.is_none() {
        return Ok(());
    }
    let configured = path.is_some();
    let path = prepare_identity_location(&identity_path(path)?, configured)?;
    let mut completed = identity.clone();
    completed.previous_secret = None;
    write_system_identity(&path, &completed).with_context(|| {
        format!(
            "failed to finalize identity secret rotation {}",
            path.display()
        )
    })?;
    identity.previous_secret = None;
    Ok(())
}

#[cfg(windows)]
fn write_system_identity(path: &Path, identity: &Identity) -> Result<()> {
    let temporary = path.with_extension(format!("rotate-{}.tmp", Uuid::new_v4()));
    let result = (|| -> Result<()> {
        let mut file = create_identity_file(&temporary)?;
        file.write_all(serde_json::to_string_pretty(identity)?.as_bytes())?;
        file.sync_all()?;
        drop(file);
        protect_identity_file(&temporary)?;
        crate::windows_security::replace_file(&temporary, path)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

fn read_identity(path: &std::path::Path) -> std::io::Result<Identity> {
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW);
    }
    let mut file = options.open(path)?;
    if !file.metadata()?.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "identity path is not a regular file",
        ));
    }
    let mut content = String::new();
    file.read_to_string(&mut content)?;
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

pub(crate) fn identity_path(path: Option<PathBuf>) -> Result<PathBuf> {
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
    fn legacy_identity_defaults_to_the_migration_version() {
        let identity: Identity =
            serde_json::from_str(r#"{"instance_id":"instance-1","secret":"secret-1"}"#).unwrap();

        assert_eq!(identity.credential_version, 0);
        assert!(identity.previous_secret.is_none());
    }

    #[test]
    fn transition_secret_is_persisted_until_registration_succeeds() {
        let identity = Identity {
            instance_id: "instance-1".to_string(),
            secret: "new-secret".to_string(),
            credential_version: CURRENT_CREDENTIAL_VERSION,
            previous_secret: Some("old-secret".to_string()),
        };
        let serialized = serde_json::to_string(&identity).unwrap();
        let restored: Identity = serde_json::from_str(&serialized).unwrap();

        assert_eq!(restored.previous_secret.as_deref(), Some("old-secret"));
    }

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
    fn repairs_permissions_on_an_existing_identity() {
        let directory = std::env::temp_dir().join(format!("om-agent-identity-{}", Uuid::new_v4()));
        let path = directory.join("identity.json");
        fs::create_dir_all(&directory).unwrap();
        fs::write(
            &path,
            r#"{"instance_id":"existing-instance","secret":"existing-secret"}"#,
        )
        .unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();

        let identity = load_or_create_identity(Some(path.clone())).unwrap();

        assert_eq!(identity.instance_id, "existing-instance");
        assert_eq!(identity.secret, "existing-secret");
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
                thread::spawn(move || {
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
