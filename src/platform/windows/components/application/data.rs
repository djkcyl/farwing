//! Explicit full-uninstall data removal, scoped to this product and current user.
use super::*;

fn remove_tree(path: &Path) -> Result<()> {
    use windows::{
        Win32::{Foundation::*, Storage::FileSystem::*},
        core::PCWSTR,
    };
    let text = std::path::absolute(path)?.to_string_lossy().into_owned();
    let text = if text.starts_with(r"\\?\") {
        text
    } else if let Some(unc) = text.strip_prefix(r"\\") {
        format!(r"\\?\UNC\{unc}")
    } else {
        format!(r"\\?\{text}")
    };
    let wide: Vec<u16> = text.encode_utf16().chain(Some(0)).collect();
    // Deny DELETE sharing while enumerating. A user-writable directory cannot
    // be swapped for a junction under an elevated cleanup worker.
    let handle = match unsafe {
        CreateFileW(
            PCWSTR(wide.as_ptr()),
            FILE_READ_ATTRIBUTES.0,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            None,
            OPEN_EXISTING,
            FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT,
            None,
        )
    } {
        Ok(handle) => crate::platform::windows::host_service::pipe::Handle(handle),
        Err(error)
            if error.code() == ERROR_FILE_NOT_FOUND.to_hresult()
                || error.code() == ERROR_PATH_NOT_FOUND.to_hresult() =>
        {
            return Ok(());
        }
        Err(error) => return Err(error.into()),
    };
    let mut info = BY_HANDLE_FILE_INFORMATION::default();
    unsafe {
        GetFileInformationByHandle(handle.0, &mut info)?;
    }
    let directory = info.dwFileAttributes & FILE_ATTRIBUTE_DIRECTORY.0 != 0;
    let link = info.dwFileAttributes & FILE_ATTRIBUTE_REPARSE_POINT.0 != 0;
    if directory && !link {
        for entry in std::fs::read_dir(path)? {
            remove_tree(&entry?.path())?;
        }
    }
    drop(handle);
    // Removing a link unlinks only that name, never its target.
    if directory {
        std::fs::remove_dir(path)?;
    } else {
        std::fs::remove_file(path)?;
    }
    Ok(())
}
fn ensure_recovered(root: &Path) -> Result<()> {
    let directory = root.join("displays/recovery");
    if directory.exists() {
        reject_reparse(&directory)?;
        ensure!(
            std::fs::read_dir(directory)?.next().is_none(),
            "屏幕布局仍有待恢复记录，请先完成恢复再清除数据"
        );
    }
    Ok(())
}
pub(super) fn machine() -> Result<()> {
    let root = crate::platform::windows::host_service::vault::root()?;
    let directory = root.parent().context("本机数据目录无效")?;
    reject_reparse(directory)?;
    reject_reparse(&root)?;
    ensure_recovered(&root)?;
    crate::platform::windows::virtual_audio::Defaults::ensure_recovered()?;
    remove_tree(directory)
}
pub(super) fn user() -> Result<()> {
    // The default profile's root also contains `profiles\`, so removing it
    // purges every profile of this Windows user.
    let directory = crate::product::local_data_dir()?;
    reject_reparse(&directory)?;
    ensure_recovered(&directory)?;
    // Use the native keyring's metadata search; never fetch credential secrets.
    // Match only namespaces actually owned by this product, not arbitrary names
    // containing its branding or other applications' credential entries.
    keyring::Entry::store_status()
        .as_ref()
        .map_err(|_| anyhow::anyhow!("系统凭据库不可用"))?;
    let entries = keyring_core::Entry::search(&std::collections::HashMap::from([(
        "pattern",
        CREDENTIAL_PATTERN,
    )]))?;
    for entry in entries {
        entry.delete_credential()?;
    }
    remove_tree(&directory)
}
/// Credential targets are `<user>.<service>`; services come from
/// `product::keyring_service`, i.e. `farwing.<suffix>` for the default profile
/// and `farwing.<profile>.<suffix>` for the others (all profiles are purged).
const CREDENTIAL_PATTERN: &str = r"\.farwing\.(?:[a-z0-9][a-z0-9-]{0,31}\.)?(?:session|wallpaper|host\.(?:guest|update-resume)|(?:assist|viewing|audio|microphone|host|device-preferences)\.[0-9a-f]{64})$";

#[cfg(test)]
mod tests {
    #[test]
    fn credential_pattern() {
        let pattern = regex::Regex::new(super::CREDENTIAL_PATTERN).unwrap();
        let hash = "0123456789abcdef".repeat(4);
        for target in [
            "user.farwing.session".to_owned(),
            "user.farwing.dev.session".to_owned(),
            "key.farwing.wallpaper".to_owned(),
            "0b8f0a6e-1c1d-4a4e-9a9e-1f2a3b4c5d6e.farwing.host.guest".to_owned(),
            "scope.farwing.dev-2.host.update-resume".to_owned(),
            format!("device.farwing.viewing.{hash}"),
            format!("device.farwing.dev.device-preferences.{hash}"),
            format!("id.farwing.assist.{hash}"),
        ] {
            assert!(pattern.is_match(&target), "{target}");
        }
        for target in [
            "user.com.openuuyc.session".to_owned(),
            "user.openuuyc.session".to_owned(),
            "user.farwing.other".to_owned(),
            "user.farwing.Dev.session".to_owned(),
            format!("device.farwing.viewing.{hash}0"),
            "user.farwing.session.backup".to_owned(),
        ] {
            assert!(!pattern.is_match(&target), "{target}");
        }
    }
}
