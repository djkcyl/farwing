//! Persistent defaults for this user's controller; no account secrets.
use super::ConnectionMediaOptions;
use anyhow::{Context, Result};
use std::io::Write;
fn path() -> Result<std::path::PathBuf> {
    Ok(crate::platform::paths::require_local_app_data()?
        .join("OpenUUYC")
        .join("media-preferences.json"))
}
pub fn load() -> Result<ConnectionMediaOptions> {
    load_from(&path()?)
}
pub(super) fn load_from(path: &std::path::Path) -> Result<ConnectionMediaOptions> {
    let bytes = match std::fs::read(path) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Default::default()),
        Err(e) => return Err(e.into()),
    };
    serde_json::from_slice(&bytes).context("读取编解码首选设置失败")
}
pub(crate) fn save(options: ConnectionMediaOptions) -> Result<()> {
    save_to(&path()?, options)
}
pub(super) fn save_to(path: &std::path::Path, options: ConnectionMediaOptions) -> Result<()> {
    options.validate()?;
    let parent = path.parent().context("媒体设置目录无效")?;
    std::fs::create_dir_all(parent)?;
    let options = ConnectionMediaOptions {
        audio_only: false,
        muted: false,
        ..options
    };
    let temp = parent.join(format!(".media-preferences-{}.tmp", uuid::Uuid::new_v4()));
    let result = (|| -> Result<()> {
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temp)?;
        file.write_all(&serde_json::to_vec_pretty(&options)?)?;
        file.sync_all()?;
        drop(file);
        std::fs::rename(&temp, path)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temp);
    }
    result.context("保存编解码首选设置失败")
}
