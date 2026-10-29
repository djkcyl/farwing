//! Machine-available credentials with an explicit installing-user ACL.
//! DPAPI machine scope is encryption at rest, not an access-control boundary.
use super::pipe::Handle;
use crate::i18n::t;
use anyhow::{Context, Result, ensure};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use windows::{
    Win32::{
        Foundation::*,
        Security::{Authorization::*, Cryptography::*, *},
        System::Threading::*,
    },
    core::{PCWSTR, PWSTR},
};

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(Some(0)).collect()
}
/// `%ProgramData%\Farwing\host`.
pub(crate) fn root() -> Result<PathBuf> {
    Ok(crate::product::program_data_dir()?.join("host"))
}
pub(crate) fn sid(pid: u32) -> Result<String> {
    let process = Handle(unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid)? });
    let mut token = HANDLE::default();
    unsafe {
        OpenProcessToken(process.0, TOKEN_QUERY, &mut token)?;
    }
    let token = Handle(token);
    token_sid(token.0)
}
pub(crate) fn token_sid(token: HANDLE) -> Result<String> {
    let mut needed = 0;
    unsafe {
        let _ = GetTokenInformation(token, TokenUser, None, 0, &mut needed);
    }
    ensure!(
        needed > 0 && needed < 65536,
        "{}",
        t().system.vault_invalid_identity_length
    );
    let mut buffer = vec![0usize; (needed as usize).div_ceil(size_of::<usize>())];
    unsafe {
        GetTokenInformation(
            token,
            TokenUser,
            Some(buffer.as_mut_ptr().cast()),
            needed,
            &mut needed,
        )?;
        let user = &*buffer.as_ptr().cast::<TOKEN_USER>();
        let mut text = PWSTR::null();
        ConvertSidToStringSidW(user.User.Sid, &mut text)?;
        let value = text.to_string();
        LocalFree(Some(HLOCAL(text.0.cast())));
        Ok(value?)
    }
}
pub(crate) fn owner() -> Result<Option<String>> {
    // This public SID is in the administrator-owned deployment, not in the
    // owner's writable credential directory; other users can select portable mode.
    match std::fs::read(super::install::directory()?.join("suite.json")) {
        Ok(bytes) => {
            ensure!(
                bytes.len() < 4096,
                "{}",
                t().system.vault_invalid_owner_record
            );
            let record: serde_json::Value = serde_json::from_slice(&bytes)?;
            Ok(Some(
                record
                    .get("owner")
                    .and_then(|v| v.as_str())
                    .context(t().system.vault_owner_missing)?
                    .to_owned(),
            ))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.into()),
    }
}
pub(crate) fn applies() -> Result<bool> {
    // The service vault belongs to the default profile; other profiles keep
    // their credentials, logs and records in their own portable stores.
    if !crate::product::is_default_profile() {
        return Ok(false);
    }
    let Some(owner) = owner()? else {
        return Ok(false);
    };
    let own = sid(std::process::id())?;
    Ok((own == owner || is_host_identity()?) && root()?.join("enabled").is_file())
}
/// The background host's own SID: a resource-manager SID (S-1-9) derived
/// from the service name, a group in its LocalService token, admitted to the
/// vault and the service's pipes. Only a process holding the TCB privilege
/// (the service) can put it in a token; Windows refuses `NT SERVICE` SIDs
/// as extra logon groups.
pub(crate) fn host_sid() -> Result<String> {
    let digest = Sha256::digest(format!("{}.Host", crate::product::SERVICE_NAME));
    let parts: Vec<String> = digest
        .chunks_exact(4)
        .take(4)
        .map(|chunk| u32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]).to_string())
        .collect();
    Ok(format!("S-1-9-{}", parts.join("-")))
}
/// This process is the background host: SYSTEM, or LocalService with the
/// host SID.
pub(crate) fn is_host_identity() -> Result<bool> {
    let own = sid(std::process::id())?;
    Ok(own == "S-1-5-18" || (own == "S-1-5-19" && has_host_sid(None)?))
}
/// An impersonation `token` (a pipe client's) is the background host's.
pub(crate) fn is_host_token(token: HANDLE) -> Result<bool> {
    let own = token_sid(token)?;
    Ok(own == "S-1-5-18" || (own == "S-1-5-19" && has_host_sid(Some(token))?))
}
fn has_host_sid(token: Option<HANDLE>) -> Result<bool> {
    let host: Vec<u16> = host_sid()?.encode_utf16().chain(Some(0)).collect();
    let mut psid = PSID::default();
    unsafe { ConvertStringSidToSidW(PCWSTR(host.as_ptr()), &mut psid)? };
    let mut member = windows::core::BOOL::default();
    let checked = unsafe { CheckTokenMembership(token, psid, &mut member) };
    unsafe {
        LocalFree(Some(HLOCAL(psid.0)));
    }
    checked?;
    Ok(member.as_bool())
}
/// The vault's access list: SYSTEM, administrators, the owner and the
/// background host's own SID. Applied at every service start.
pub(crate) fn admit_host() -> Result<()> {
    let (Some(owner), root) = (owner()?, root()?) else {
        return Ok(());
    };
    if !root.is_dir() {
        return Ok(());
    }
    set_acl(&root, &acl(&owner)?)
}
/// The background host's own data directory (its `%LOCALAPPDATA%`), beside
/// the vault: SYSTEM, administrators and the host SID only.
pub(crate) fn host_data() -> Result<PathBuf> {
    use super::super::components::files::{reject_reparse, secure_directory};
    let parent = root()?.parent().context(t().system.vault_invalid_directory)?.to_path_buf();
    let path = parent.join("resident");
    reject_reparse(&parent)?;
    reject_reparse(&path)?;
    if !parent.is_dir() {
        std::fs::create_dir_all(&parent)?;
        secure_directory(&parent)?;
    }
    std::fs::create_dir_all(&path)?;
    set_acl(
        &path,
        &format!("O:BAG:BAD:P(A;OICI;FA;;;SY)(A;OICI;FA;;;BA)(A;OICI;FA;;;{})", host_sid()?),
    )?;
    Ok(path)
}
fn acl(owner: &str) -> Result<String> {
    Ok(format!(
        "O:BAG:BAD:P(A;OICI;FA;;;SY)(A;OICI;FA;;;BA)(A;OICI;FA;;;{owner})(A;OICI;FA;;;{})",
        host_sid()?
    ))
}
pub(crate) fn create(owner: &str) -> Result<()> {
    // Parse, do not interpolate an unchecked SID into SDDL.
    let mut parsed = PSID::default();
    unsafe {
        ConvertStringSidToSidW(PCWSTR(wide(owner).as_ptr()), &mut parsed)?;
        LocalFree(Some(HLOCAL(parsed.0)));
    }
    ensure!(
        (owner.starts_with("S-1-5-21-") || owner.starts_with("S-1-12-1-"))
            && owner
                .bytes()
                .all(|c| c.is_ascii_digit() || c == b'S' || c == b'-'),
        "{}",
        t().system.vault_invalid_owner
    );
    if let Some(existing) = self::owner()? {
        ensure!(
            existing == owner,
            "{}",
            t().system.service_managed_by_other_user
        );
    }
    let root = root()?;
    let parent = root.parent().context(t().system.vault_invalid_directory)?;
    super::super::components::files::reject_reparse(parent)?;
    super::super::components::files::reject_reparse(&root)?;
    std::fs::create_dir_all(parent)?;
    super::super::components::files::secure_directory(parent)?;
    std::fs::create_dir_all(&root)?;
    set_acl(&root, &acl(owner)?)?;
    std::fs::write(root.join("owner.sid"), owner)?;
    Ok(())
}
fn set_acl(path: &Path, sddl: &str) -> Result<()> {
    unsafe {
        let mut descriptor = PSECURITY_DESCRIPTOR::default();
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            PCWSTR(wide(sddl).as_ptr()),
            SDDL_REVISION_1,
            &mut descriptor,
            None,
        )?;
        let result = (|| -> Result<()> {
            let (mut present, mut defaulted) = Default::default();
            let mut dacl = std::ptr::null_mut();
            let mut owner = PSID::default();
            GetSecurityDescriptorDacl(descriptor, &mut present, &mut dacl, &mut defaulted)?;
            GetSecurityDescriptorOwner(descriptor, &mut owner, &mut defaulted)?;
            SetNamedSecurityInfoW(
                PCWSTR(wide(&path.to_string_lossy()).as_ptr()),
                SE_FILE_OBJECT,
                DACL_SECURITY_INFORMATION
                    | PROTECTED_DACL_SECURITY_INFORMATION
                    | OWNER_SECURITY_INFORMATION,
                Some(owner),
                None,
                Some(dacl),
                None,
            )
            .ok()?;
            Ok(())
        })();
        LocalFree(Some(HLOCAL(descriptor.0)));
        result
    }
}
pub(crate) fn key(service: &str, account: &str) -> String {
    format!(
        "{:x}.secret",
        Sha256::digest(format!("{service}\0{account}"))
    )
}
pub(crate) fn read(key: &str) -> Result<Option<Vec<u8>>> {
    let path = root()?.join(key);
    super::super::components::files::reject_reparse(&path)?;
    let bytes = match std::fs::read(path) {
        Ok(v) => v,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e.into()),
    };
    ensure!(
        bytes.len() <= 1024 * 1024,
        "{}",
        t().system.vault_record_too_large
    );
    protect(&bytes, false).map(Some)
}
pub(crate) fn write(key: &str, value: Option<&[u8]>) -> Result<()> {
    let path = root()?.join(key);
    super::super::components::files::reject_reparse(&path)?;
    let Some(value) = value else {
        match std::fs::remove_file(path) {
            Ok(()) => (),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => (),
            Err(e) => return Err(e.into()),
        };
        return Ok(());
    };
    ensure!(
        value.len() <= 1024 * 1024,
        "{}",
        t().system.vault_record_too_large
    );
    let bytes = protect(value, true)?;
    let temp = path.with_extension(format!("{}.tmp", uuid::Uuid::new_v4().simple()));
    let result = (|| -> Result<()> {
        use std::io::Write;
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temp)?;
        file.write_all(&bytes)?;
        file.sync_all()?;
        drop(file);
        unsafe {
            windows::Win32::Storage::FileSystem::MoveFileExW(
                PCWSTR(wide(&temp.to_string_lossy()).as_ptr()),
                PCWSTR(wide(&path.to_string_lossy()).as_ptr()),
                windows::Win32::Storage::FileSystem::MOVEFILE_REPLACE_EXISTING
                    | windows::Win32::Storage::FileSystem::MOVEFILE_WRITE_THROUGH,
            )?;
        }
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(temp);
    }
    result
}
fn protect(bytes: &[u8], encrypt: bool) -> Result<Vec<u8>> {
    let input = CRYPT_INTEGER_BLOB {
        cbData: bytes.len() as u32,
        pbData: bytes.as_ptr().cast_mut(),
    };
    let mut output = CRYPT_INTEGER_BLOB::default();
    unsafe {
        if encrypt {
            CryptProtectData(
                &input,
                None,
                None,
                None,
                None,
                CRYPTPROTECT_LOCAL_MACHINE | CRYPTPROTECT_UI_FORBIDDEN,
                &mut output,
            )?;
        } else {
            CryptUnprotectData(
                &input,
                None,
                None,
                None,
                None,
                CRYPTPROTECT_UI_FORBIDDEN,
                &mut output,
            )?;
        }
        let result = std::slice::from_raw_parts(output.pbData, output.cbData as usize).to_vec();
        std::ptr::write_bytes(output.pbData, 0, output.cbData as usize);
        LocalFree(Some(HLOCAL(output.pbData.cast())));
        Ok(result)
    }
}
