use std::{
    ffi::OsString,
    fs, io,
    path::{Component, Path, PathBuf},
};

use anyhow::{Context, Result, bail};

pub fn is_process_privileged() -> bool {
    is_process_privileged_impl()
}

pub fn prepare_configured_directory(
    path: &Path,
    configured: bool,
    description: &str,
) -> Result<PathBuf> {
    let enforce = configured && is_process_privileged();
    let path = absolute_path(path, description, enforce)?;
    if !enforce {
        fs::create_dir_all(&path)
            .with_context(|| format!("failed to create {description} {}", path.display()))?;
        return Ok(path);
    }

    let (existing, missing) = nearest_existing_ancestor(&path, description)?;
    validate_directory_chain(&existing, description)?;
    let mut directory = existing.clone();
    for component in missing.iter().rev() {
        directory.push(component);
        match create_private_directory(&directory) {
            Ok(()) => protect_created_directory(&directory)?,
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                validate_directory_chain(&directory, description)?;
            }
            Err(error) => {
                return Err(error).with_context(|| {
                    format!("failed to create {description} {}", directory.display())
                });
            }
        }
    }
    validate_directory_chain(&path, description)?;
    fs::canonicalize(&path)
        .with_context(|| format!("failed to resolve {description} {}", path.display()))
}

#[cfg(windows)]
fn create_private_directory(path: &Path) -> io::Result<()> {
    crate::windows_security::create_private_directory(path)
}

#[cfg(not(windows))]
fn create_private_directory(path: &Path) -> io::Result<()> {
    let mut builder = fs::DirBuilder::new();
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;

        builder.mode(0o700);
    }
    builder.create(path)
}

pub fn validate_configured_directory(
    path: &Path,
    configured: bool,
    description: &str,
) -> Result<()> {
    if configured && is_process_privileged() {
        validate_directory_chain(&absolute_path(path, description, true)?, description)?;
    }
    Ok(())
}

pub fn validate_regular_file(path: &Path, description: &str) -> Result<()> {
    let metadata = fs::symlink_metadata(path)
        .with_context(|| format!("failed to inspect {description} {}", path.display()))?;
    if metadata.file_type().is_symlink() || !metadata.file_type().is_file() {
        bail!(
            "{description} {} must be a regular file, not a link",
            path.display()
        );
    }
    if is_process_privileged() {
        let path = absolute_path(path, description, true)?;
        let parent = path
            .parent()
            .ok_or_else(|| anyhow::anyhow!("{description} has no parent directory"))?;
        validate_directory_chain(parent, description)?;
        validate_privileged_file_metadata(&metadata, &path, description)?;
    }
    Ok(())
}

fn absolute_path(path: &Path, description: &str, reject_parent: bool) -> Result<PathBuf> {
    if reject_parent
        && path
            .components()
            .any(|component| matches!(component, Component::ParentDir))
    {
        bail!("{description} must not contain '..': {}", path.display());
    }
    let path = if path.is_absolute() {
        path.to_owned()
    } else {
        std::env::current_dir()?.join(path)
    };
    Ok(path.components().collect())
}

fn nearest_existing_ancestor(path: &Path, description: &str) -> Result<(PathBuf, Vec<OsString>)> {
    let mut existing = path.to_owned();
    let mut missing = Vec::new();
    loop {
        match fs::symlink_metadata(&existing) {
            Ok(_) => return Ok((existing, missing)),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                let name = existing.file_name().ok_or_else(|| {
                    anyhow::anyhow!("{description} has no existing ancestor: {}", path.display())
                })?;
                missing.push(name.to_owned());
                existing = existing
                    .parent()
                    .ok_or_else(|| {
                        anyhow::anyhow!(
                            "{description} has no existing ancestor: {}",
                            path.display()
                        )
                    })?
                    .to_owned();
            }
            Err(error) => {
                return Err(error).with_context(|| {
                    format!("failed to inspect {description} {}", existing.display())
                });
            }
        }
    }
}

#[cfg(unix)]
fn validate_directory_chain(path: &Path, description: &str) -> Result<()> {
    use std::os::unix::fs::MetadataExt;

    for ancestor in path.ancestors().collect::<Vec<_>>().into_iter().rev() {
        let metadata = fs::symlink_metadata(ancestor).with_context(|| {
            format!(
                "failed to inspect {description} ancestor {}",
                ancestor.display()
            )
        })?;
        if metadata.file_type().is_symlink() {
            if metadata.uid() != 0 {
                bail!(
                    "{description} ancestor {} is a link not owned by root",
                    ancestor.display()
                );
            }
            continue;
        }
        if !metadata.is_dir() {
            bail!(
                "{description} ancestor {} is not a directory",
                ancestor.display()
            );
        }
        if metadata.uid() != 0 {
            bail!(
                "{description} ancestor {} is not owned by root",
                ancestor.display()
            );
        }
        if metadata.mode() & 0o022 != 0 {
            bail!(
                "{description} ancestor {} is writable by group or other users",
                ancestor.display()
            );
        }
    }

    let canonical = fs::canonicalize(path)
        .with_context(|| format!("failed to resolve {description} {}", path.display()))?;
    if canonical != path {
        for ancestor in canonical.ancestors().collect::<Vec<_>>().into_iter().rev() {
            let metadata = fs::symlink_metadata(ancestor).with_context(|| {
                format!(
                    "failed to inspect resolved {description} {}",
                    ancestor.display()
                )
            })?;
            if !metadata.is_dir() || metadata.uid() != 0 || metadata.mode() & 0o022 != 0 {
                bail!(
                    "resolved {description} ancestor {} is not root-owned and protected",
                    ancestor.display()
                );
            }
        }
    }
    Ok(())
}

#[cfg(windows)]
fn validate_directory_chain(path: &Path, description: &str) -> Result<()> {
    use std::os::windows::fs::MetadataExt;
    use windows::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT;

    for ancestor in path.ancestors().collect::<Vec<_>>().into_iter().rev() {
        let metadata = fs::symlink_metadata(ancestor).with_context(|| {
            format!(
                "failed to inspect {description} ancestor {}",
                ancestor.display()
            )
        })?;
        if !metadata.is_dir() {
            bail!(
                "{description} ancestor {} is not a directory",
                ancestor.display()
            );
        }
        if metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT.0 != 0 {
            bail!(
                "{description} ancestor {} is a reparse point",
                ancestor.display()
            );
        }
        crate::windows_security::validate_privileged_directory(ancestor).with_context(|| {
            format!(
                "{description} ancestor {} is writable by an unprivileged account",
                ancestor.display()
            )
        })?;
    }
    Ok(())
}

#[cfg(not(any(unix, windows)))]
fn validate_directory_chain(_path: &Path, _description: &str) -> Result<()> {
    Ok(())
}

#[cfg(unix)]
fn validate_privileged_file_metadata(
    metadata: &fs::Metadata,
    path: &Path,
    description: &str,
) -> Result<()> {
    use std::os::unix::fs::MetadataExt;

    if metadata.uid() != 0 || metadata.mode() & 0o022 != 0 || metadata.nlink() != 1 {
        bail!(
            "{description} {} must be root-owned, non-writable by other users, and have one link",
            path.display()
        );
    }
    Ok(())
}

#[cfg(windows)]
fn validate_privileged_file_metadata(
    metadata: &fs::Metadata,
    path: &Path,
    description: &str,
) -> Result<()> {
    use std::os::windows::fs::MetadataExt;
    use windows::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT;

    if metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT.0 != 0 {
        bail!("{description} {} is a reparse point", path.display());
    }
    crate::windows_security::validate_privileged_file(path)
}

#[cfg(not(any(unix, windows)))]
fn validate_privileged_file_metadata(
    _metadata: &fs::Metadata,
    _path: &Path,
    _description: &str,
) -> Result<()> {
    Ok(())
}

#[cfg(unix)]
fn protect_created_directory(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;

    fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    Ok(())
}

#[cfg(windows)]
fn protect_created_directory(path: &Path) -> Result<()> {
    crate::windows_security::restrict_to_system_and_administrators(path)
}

#[cfg(not(any(unix, windows)))]
fn protect_created_directory(_path: &Path) -> Result<()> {
    Ok(())
}

#[cfg(unix)]
fn is_process_privileged_impl() -> bool {
    unsafe { libc::geteuid() == 0 }
}

#[cfg(windows)]
fn is_process_privileged_impl() -> bool {
    use std::mem::{size_of, zeroed};
    use windows_sys::Win32::{
        Foundation::CloseHandle,
        Security::{GetTokenInformation, TOKEN_ELEVATION, TOKEN_QUERY, TokenElevation},
        System::Threading::{GetCurrentProcess, OpenProcessToken},
    };

    unsafe {
        let mut token = std::ptr::null_mut();
        if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) == 0 {
            return false;
        }
        let mut elevation: TOKEN_ELEVATION = zeroed();
        let mut returned = 0_u32;
        let elevated = GetTokenInformation(
            token,
            TokenElevation,
            &mut elevation as *mut _ as *mut _,
            size_of::<TOKEN_ELEVATION>() as u32,
            &mut returned,
        ) != 0
            && elevation.TokenIsElevated != 0;
        CloseHandle(token);
        elevated
    }
}

#[cfg(not(any(unix, windows)))]
fn is_process_privileged_impl() -> bool {
    false
}

#[cfg(all(test, unix))]
mod tests {
    use std::os::unix::fs::symlink;

    use super::*;

    #[test]
    fn regular_file_validation_rejects_symbolic_links() {
        let directory =
            std::env::temp_dir().join(format!("om-agent-privileged-path-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&directory).unwrap();
        let target = directory.join("target");
        let link = directory.join("link");
        fs::write(&target, "data").unwrap();
        symlink(&target, &link).unwrap();

        assert!(validate_regular_file(&link, "test file").is_err());

        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn privileged_paths_reject_parent_components() {
        assert!(absolute_path(Path::new("../state"), "test directory", true).is_err());
        assert!(absolute_path(Path::new("../state"), "test directory", false).is_ok());
    }
}
