//! Durable display recovery intent, separate from optional controller preferences.
use crate::platform::display::topology::SavedTopology;
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use std::{
    fs,
    io::{Read, Write},
    path::{Path, PathBuf},
};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(super) struct Virtual {
    pub guid: String,
    pub identity: Option<String>,
    pub width: u32,
    pub height: u32,
    pub hz: u32,
    pub dpi: u32,
    pub kind: i32,
    pub resolution_type: i32,
    pub modes: Vec<(u32, u32)>,
    pub layout: Option<crate::platform::display::topology::SavedTarget>,
}
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub(super) struct Preference {
    pub default_virtual: bool,
    pub super_enabled: bool,
    pub super_size: Option<(u32, u32)>,
    pub super_dpi: u32,
    pub super_resolution_type: i32,
    pub manual: Vec<Virtual>,
    pub physical: Vec<Physical>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub(super) struct Physical {
    pub identity: String,
    pub width: u32,
    pub height: u32,
    pub dpi: u32,
    pub resolution_type: i32,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub(super) struct Journal {
    /// None denotes a journal written before per-target mutation ownership.
    #[serde(default)]
    pub mode_changes: Option<Vec<super::recovery::ModeChange>>,
    pub owner_pid: u32,
    pub owner_birth: u64,
    pub retired: bool,
    pub token: String,
    pub baseline: SavedTopology,
    pub dpi: Vec<(String, u32)>,
    pub owned: Vec<Virtual>,
    pub super_baseline: Option<SavedTopology>,
    pub super_dpi: Vec<(String, u32)>,
    pub applied: Option<SavedTopology>,
    pub desired: Option<SavedTopology>,
    pub applied_dpi: Vec<(String, u32)>,
    pub desired_dpi: Vec<(String, u32)>,
}

pub(super) fn root() -> Result<PathBuf> {
    // The installed service keeps display state in its protected vault.
    if crate::platform::host_service::resident::is_owner()
        || crate::platform::host_service::vault::applies()?
    {
        return Ok(crate::platform::host_service::vault::root()?.join("displays"));
    }
    portable_root()
}
fn portable_root() -> Result<PathBuf> {
    Ok(crate::platform::paths::local_app_data()
        .context("本地显示配置目录不可用")?
        .join("OpenUUYC/displays"))
}
pub(super) fn transfer_preferences(to_service: bool) -> Result<()> {
    let portable = portable_root()?.join("preferences");
    let service = crate::platform::host_service::vault::root()?.join("displays/preferences");
    let (from, to) = if to_service {
        (portable, service)
    } else {
        (service, portable)
    };
    if !from.exists() {
        return Ok(());
    }
    use crate::platform::host_service::reject_reparse;
    reject_reparse(&from)?;
    for scope in fs::read_dir(from)? {
        let scope = scope?;
        let name = scope.file_name();
        let text = name.to_string_lossy();
        if text.len() != 64 || !text.bytes().all(|c| c.is_ascii_hexdigit()) {
            continue;
        }
        reject_reparse(&scope.path())?;
        if !scope.file_type()?.is_dir() {
            continue;
        }
        let destination = to.join(name);
        reject_reparse(&destination)?;
        for file in fs::read_dir(scope.path())? {
            let file = file?;
            let name = file.file_name();
            let text = name.to_string_lossy();
            if text.len() != 69
                || !text.ends_with(".json")
                || !text[..64].bytes().all(|c| c.is_ascii_hexdigit())
            {
                continue;
            }
            reject_reparse(&file.path())?;
            let target = destination.join(name);
            if to_service && target.exists() {
                continue;
            }
            if let Some(value) = read::<Preference>(&file.path())? {
                write(&target, &value)?;
            }
        }
    }
    Ok(())
}
pub(super) fn read<T: DeserializeOwned>(path: &Path) -> Result<Option<T>> {
    let file = match fs::File::open(path) {
        Ok(file) => file,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e.into()),
    };
    let mut bytes = Vec::new();
    file.take(1024 * 1024 + 1).read_to_end(&mut bytes)?;
    ensure!(bytes.len() <= 1024 * 1024, "显示配置文件过大");
    Ok(Some(
        serde_json::from_slice(&bytes).context("显示配置文件损坏")?,
    ))
}
pub(super) fn write<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    fs::create_dir_all(path.parent().context("显示配置路径无效")?)?;
    let temp = path.with_extension(format!("{}.tmp", uuid::Uuid::new_v4().simple()));
    let result = (|| {
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temp)?;
        file.write_all(&serde_json::to_vec(value)?)?;
        file.sync_all()?;
        drop(file);
        replace(&temp, path)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temp);
    }
    result
}

/// Move `temp` over `path` atomically and durably.
#[cfg(windows)]
fn replace(temp: &Path, path: &Path) -> Result<()> {
    use windows::{
        Win32::Storage::FileSystem::{
            MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH, MoveFileExW,
        },
        core::PCWSTR,
    };
    let wide = |p: &Path| {
        p.as_os_str()
            .to_string_lossy()
            .encode_utf16()
            .chain(Some(0))
            .collect::<Vec<_>>()
    };
    let from = wide(temp);
    let to = wide(path);
    unsafe {
        MoveFileExW(
            PCWSTR(from.as_ptr()),
            PCWSTR(to.as_ptr()),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    }?;
    Ok(())
}
/// Move `temp` over `path` atomically and durably: rename(2) replaces in one
/// step, and syncing the directory makes the new entry itself durable.
#[cfg(not(windows))]
fn replace(temp: &Path, path: &Path) -> Result<()> {
    fs::rename(temp, path)?;
    fs::File::open(path.parent().context("显示配置路径无效")?)?.sync_all()?;
    Ok(())
}
