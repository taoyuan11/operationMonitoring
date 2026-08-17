use std::{
    fs::File,
    io,
    os::windows::ffi::OsStrExt,
    os::windows::io::FromRawHandle,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, bail};
use windows::{
    Win32::{
        Foundation::{ERROR_SUCCESS, GENERIC_ALL, GENERIC_WRITE, HLOCAL, LocalFree},
        Security::{
            ACE_HEADER, ACE_INHERITED_OBJECT_TYPE_PRESENT, ACE_OBJECT_TYPE_PRESENT,
            ACL_SIZE_INFORMATION, AclSizeInformation,
            Authorization::{
                ConvertStringSecurityDescriptorToSecurityDescriptorW, GetEffectiveRightsFromAclW,
                GetNamedSecurityInfoW, MULTIPLE_TRUSTEE_OPERATION, SDDL_REVISION_1, SE_FILE_OBJECT,
                TRUSTEE_IS_SID, TRUSTEE_IS_WELL_KNOWN_GROUP, TRUSTEE_W,
            },
            CreateWellKnownSid, DACL_SECURITY_INFORMATION, GROUP_SECURITY_INFORMATION, GetAce,
            GetAclInformation, GetLengthSid, IsValidSid, IsWellKnownSid,
            OWNER_SECURITY_INFORMATION, PROTECTED_DACL_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR,
            PSID, SECURITY_ATTRIBUTES, SetFileSecurityW, WELL_KNOWN_SID_TYPE,
            WinAuthenticatedUserSid, WinBuiltinAdministratorsSid, WinBuiltinUsersSid,
            WinCreatorOwnerSid, WinLocalSystemSid, WinWorldSid,
        },
        Storage::FileSystem::{
            CREATE_NEW, CreateDirectoryW, CreateFileW, DELETE, FILE_ADD_FILE,
            FILE_ADD_SUBDIRECTORY, FILE_ATTRIBUTE_NORMAL, FILE_DELETE_CHILD, FILE_SHARE_MODE,
            FILE_WRITE_ATTRIBUTES, FILE_WRITE_EA, MOVEFILE_REPLACE_EXISTING,
            MOVEFILE_WRITE_THROUGH, MoveFileExW, WRITE_DAC, WRITE_OWNER,
        },
    },
    core::{PCWSTR, PWSTR},
};

pub fn restrict_to_system_and_administrators(path: &Path) -> Result<()> {
    let path_wide = path
        .as_os_str()
        .encode_wide()
        .chain(Some(0))
        .collect::<Vec<_>>();
    with_private_security_descriptor(|descriptor| {
        unsafe {
            SetFileSecurityW(
                PCWSTR(path_wide.as_ptr()),
                OWNER_SECURITY_INFORMATION
                    | GROUP_SECURITY_INFORMATION
                    | DACL_SECURITY_INFORMATION
                    | PROTECTED_DACL_SECURITY_INFORMATION,
                descriptor,
            )
        }
        .ok()
        .map_err(windows_error)
    })
    .with_context(|| format!("failed to protect {}", path.display()))
}

pub fn create_private_directory(path: &Path) -> io::Result<()> {
    let path_wide = path
        .as_os_str()
        .encode_wide()
        .chain(Some(0))
        .collect::<Vec<_>>();
    with_private_security_descriptor(|descriptor| {
        let attributes = security_attributes(descriptor);
        unsafe { CreateDirectoryW(PCWSTR(path_wide.as_ptr()), Some(&attributes)) }
            .map_err(windows_error)
    })
}

pub fn create_private_file(path: &Path) -> io::Result<File> {
    let path_wide = path
        .as_os_str()
        .encode_wide()
        .chain(Some(0))
        .collect::<Vec<_>>();
    with_private_security_descriptor(|descriptor| {
        let attributes = security_attributes(descriptor);
        let handle = unsafe {
            CreateFileW(
                PCWSTR(path_wide.as_ptr()),
                GENERIC_WRITE.0,
                FILE_SHARE_MODE(0),
                Some(&attributes),
                CREATE_NEW,
                FILE_ATTRIBUTE_NORMAL,
                None,
            )
        }
        .map_err(windows_error)?;
        Ok(unsafe { File::from_raw_handle(handle.0) })
    })
}

fn with_private_security_descriptor<T>(
    operation: impl FnOnce(PSECURITY_DESCRIPTOR) -> io::Result<T>,
) -> io::Result<T> {
    let sddl = "O:BAG:BAD:P(A;OICI;FA;;;SY)(A;OICI;FA;;;BA)"
        .encode_utf16()
        .chain(Some(0))
        .collect::<Vec<_>>();
    let mut descriptor = PSECURITY_DESCRIPTOR::default();
    unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            PCWSTR(sddl.as_ptr()),
            SDDL_REVISION_1,
            &mut descriptor,
            None,
        )
    }
    .map_err(windows_error)?;
    let result = operation(descriptor);
    unsafe {
        let _ = LocalFree(Some(HLOCAL(descriptor.0)));
    }
    result
}

fn windows_error(error: windows::core::Error) -> io::Error {
    let code = error.code().0 as u32;
    if code & 0xffff_0000 == 0x8007_0000 {
        io::Error::from_raw_os_error((code & 0xffff) as i32)
    } else {
        io::Error::other(error)
    }
}

fn security_attributes(descriptor: PSECURITY_DESCRIPTOR) -> SECURITY_ATTRIBUTES {
    SECURITY_ATTRIBUTES {
        nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: descriptor.0,
        bInheritHandle: false.into(),
    }
}

pub fn validate_privileged_directory(path: &Path) -> Result<()> {
    if is_trusted_windows_root(path) {
        return Ok(());
    }
    validate_privileged_security_descriptor(path)
}

pub fn validate_privileged_file(path: &Path) -> Result<()> {
    validate_privileged_security_descriptor(path)
}

fn validate_privileged_security_descriptor(path: &Path) -> Result<()> {
    let path_wide = path
        .as_os_str()
        .encode_wide()
        .chain(Some(0))
        .collect::<Vec<_>>();
    let mut owner = PSID::default();
    let mut dacl = std::ptr::null_mut();
    let mut descriptor = PSECURITY_DESCRIPTOR::default();
    let status = unsafe {
        GetNamedSecurityInfoW(
            PCWSTR(path_wide.as_ptr()),
            SE_FILE_OBJECT,
            OWNER_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION,
            Some(&mut owner),
            None,
            Some(&mut dacl),
            None,
            &mut descriptor,
        )
    };
    if status != ERROR_SUCCESS {
        return Err(io::Error::from_raw_os_error(status.0 as i32))
            .with_context(|| format!("failed to read security descriptor for {}", path.display()));
    }

    let result = (|| -> Result<()> {
        if owner.is_invalid()
            || !(unsafe { IsWellKnownSid(owner, WinLocalSystemSid).as_bool() }
                || unsafe { IsWellKnownSid(owner, WinBuiltinAdministratorsSid).as_bool() })
        {
            bail!(
                "{} is not owned by LocalSystem or Administrators",
                path.display()
            );
        }
        if dacl.is_null() {
            bail!("{} has an unrestricted Windows DACL", path.display());
        }

        let dangerous = FILE_ADD_FILE.0
            | FILE_ADD_SUBDIRECTORY.0
            | FILE_DELETE_CHILD.0
            | FILE_WRITE_EA.0
            | FILE_WRITE_ATTRIBUTES.0
            | DELETE.0
            | WRITE_DAC.0
            | WRITE_OWNER.0
            | GENERIC_WRITE.0
            | GENERIC_ALL.0;
        reject_untrusted_write_aces(dacl, dangerous, path)?;
        for sid_type in [WinWorldSid, WinAuthenticatedUserSid, WinBuiltinUsersSid] {
            if effective_rights(dacl, sid_type)? & dangerous != 0 {
                bail!(
                    "{} grants mutation rights to an unprivileged Windows group",
                    path.display()
                );
            }
        }
        Ok(())
    })();
    unsafe {
        let _ = LocalFree(Some(HLOCAL(descriptor.0)));
    }
    result
}

fn reject_untrusted_write_aces(
    dacl: *const windows::Win32::Security::ACL,
    dangerous: u32,
    path: &Path,
) -> Result<()> {
    let mut information = ACL_SIZE_INFORMATION::default();
    unsafe {
        GetAclInformation(
            dacl,
            &mut information as *mut _ as *mut _,
            std::mem::size_of::<ACL_SIZE_INFORMATION>() as u32,
            AclSizeInformation,
        )?;
    }

    for index in 0..information.AceCount {
        let mut raw_ace = std::ptr::null_mut();
        unsafe { GetAce(dacl, index, &mut raw_ace)? };
        if raw_ace.is_null() {
            bail!("{} contains a null Windows ACL entry", path.display());
        }
        let header = unsafe { &*(raw_ace.cast::<ACE_HEADER>()) };
        let ace_size = usize::from(header.AceSize);
        if ace_size < 8 {
            bail!("{} contains a malformed Windows ACL entry", path.display());
        }

        // ACCESS_ALLOWED_* ACEs all carry the access mask at offset four. The
        // callback and object forms use the same mask and differ only in where
        // their trustee SID begins.
        let mask = unsafe { std::ptr::read_unaligned(raw_ace.cast::<u8>().add(4).cast::<u32>()) };
        if mask & dangerous == 0 {
            continue;
        }
        let inherit_only = header.AceFlags & 0x08 != 0;

        let sid_offset = match header.AceType as u32 {
            0 | 9 => 8, // ACCESS_ALLOWED_ACE / callback ACE
            5 | 11 => {
                if ace_size < 12 {
                    bail!("{} contains a malformed object ACL entry", path.display());
                }
                let flags =
                    unsafe { std::ptr::read_unaligned(raw_ace.cast::<u8>().add(8).cast::<u32>()) };
                12 + if flags & ACE_OBJECT_TYPE_PRESENT.0 != 0 {
                    16
                } else {
                    0
                } + if flags & ACE_INHERITED_OBJECT_TYPE_PRESENT.0 != 0 {
                    16
                } else {
                    0
                }
            }
            // Compound ACEs are obsolete and have no safe representation for
            // this check; reject any write-bearing one conservatively.
            4 => bail!(
                "{} contains an unsupported writable compound ACL entry",
                path.display()
            ),
            _ => continue,
        };
        if sid_offset + std::mem::size_of::<u32>() > ace_size {
            bail!("{} contains a truncated Windows ACL entry", path.display());
        }
        let sid = PSID(unsafe { raw_ace.cast::<u8>().add(sid_offset).cast() });
        if !unsafe { IsValidSid(sid).as_bool() } {
            bail!("{} contains an invalid Windows ACL trustee", path.display());
        }
        if sid_offset + unsafe { GetLengthSid(sid) as usize } > ace_size
            || !is_trusted_sid(sid, inherit_only)
        {
            bail!(
                "{} grants mutation rights to an unprivileged Windows principal",
                path.display()
            );
        }
    }
    Ok(())
}

fn is_trusted_sid(sid: PSID, inherit_only: bool) -> bool {
    unsafe {
        IsWellKnownSid(sid, WinLocalSystemSid).as_bool()
            || IsWellKnownSid(sid, WinBuiltinAdministratorsSid).as_bool()
            || (inherit_only && IsWellKnownSid(sid, WinCreatorOwnerSid).as_bool())
    }
}

fn effective_rights(
    dacl: *const windows::Win32::Security::ACL,
    sid_type: WELL_KNOWN_SID_TYPE,
) -> Result<u32> {
    let mut sid = [0_u8; 68];
    let mut sid_size = sid.len() as u32;
    unsafe {
        CreateWellKnownSid(
            sid_type,
            None,
            Some(PSID(sid.as_mut_ptr().cast())),
            &mut sid_size,
        )?;
    }
    let trustee = TRUSTEE_W {
        pMultipleTrustee: std::ptr::null_mut(),
        MultipleTrusteeOperation: MULTIPLE_TRUSTEE_OPERATION(0),
        TrusteeForm: TRUSTEE_IS_SID,
        TrusteeType: TRUSTEE_IS_WELL_KNOWN_GROUP,
        ptstrName: PWSTR(sid.as_mut_ptr().cast()),
    };
    let mut rights = 0_u32;
    let status = unsafe { GetEffectiveRightsFromAclW(dacl, &trustee, &mut rights) };
    if status != ERROR_SUCCESS {
        return Err(io::Error::from_raw_os_error(status.0 as i32))
            .context("failed to evaluate Windows directory permissions");
    }
    Ok(rights)
}

fn is_trusted_windows_root(path: &Path) -> bool {
    if path.parent().is_none() {
        return true;
    }
    let normalized = path
        .to_string_lossy()
        .trim_end_matches(['\\', '/'])
        .to_ascii_lowercase();
    [
        "ProgramData",
        "ProgramFiles",
        "ProgramFiles(x86)",
        "SystemRoot",
    ]
    .into_iter()
    .filter_map(std::env::var_os)
    .map(PathBuf::from)
    .map(|path| {
        path.to_string_lossy()
            .trim_end_matches(['\\', '/'])
            .to_ascii_lowercase()
    })
    .any(|root| root == normalized)
}

pub fn replace_file(source: &Path, target: &Path) -> Result<()> {
    let source = source
        .as_os_str()
        .encode_wide()
        .chain(Some(0))
        .collect::<Vec<_>>();
    let target = target
        .as_os_str()
        .encode_wide()
        .chain(Some(0))
        .collect::<Vec<_>>();
    unsafe {
        MoveFileExW(
            PCWSTR(source.as_ptr()),
            PCWSTR(target.as_ptr()),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
        .context("failed to atomically replace protected file")
    }
}
