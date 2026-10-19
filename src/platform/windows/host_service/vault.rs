//! Machine-available credentials with an explicit installing-user ACL.
//! DPAPI machine scope is encryption at rest, not an access-control boundary.
use super::pipe::Handle;
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
    ensure!(needed > 0 && needed < 65536, "用户身份长度无效");
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
            ensure!(bytes.len() < 4096, "服务所有者记录无效");
            let record: serde_json::Value = serde_json::from_slice(&bytes)?;
            Ok(Some(
                record
                    .get("owner")
                    .and_then(|v| v.as_str())
                    .context("服务所有者缺失")?
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
    Ok((own == owner || own == "S-1-5-18") && root()?.join("enabled").is_file())
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
        "请选择安装服务的 Windows 用户"
    );
    if let Some(existing) = self::owner()? {
        ensure!(existing == owner, "服务已由另一 Windows 用户管理");
    }
    let root = root()?;
    let parent = root.parent().context("服务目录无效")?;
    super::super::components::files::reject_reparse(parent)?;
    super::super::components::files::reject_reparse(&root)?;
    std::fs::create_dir_all(parent)?;
    super::super::components::files::secure_directory(parent)?;
    std::fs::create_dir_all(&root)?;
    set_acl(
        &root,
        &format!("O:BAG:BAD:P(A;OICI;FA;;;SY)(A;OICI;FA;;;BA)(A;OICI;FA;;;{owner})"),
    )?;
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
    ensure!(bytes.len() <= 1024 * 1024, "服务凭据记录过大");
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
    ensure!(value.len() <= 1024 * 1024, "服务凭据记录过大");
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
