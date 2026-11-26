//! Per-user storage roots.
//!
//! Windows keeps application state under `%LOCALAPPDATA%`; the Linux
//! equivalent is the XDG data directory. Everything that used to read the
//! environment variable directly goes through here, so both platforms lay
//! their files out the same way underneath one root.
use anyhow::{Context, Result};
use std::path::PathBuf;

/// The per-user directory this client writes its own state into.
pub(crate) fn local_app_data() -> Option<PathBuf> {
    #[cfg(windows)]
    {
        std::env::var_os("LOCALAPPDATA")
            .map(PathBuf::from)
            .filter(|path| path.is_absolute())
    }
    #[cfg(not(windows))]
    {
        std::env::var_os("XDG_DATA_HOME")
            .map(PathBuf::from)
            .filter(|path| path.is_absolute())
            .or_else(|| {
                std::env::var_os("HOME")
                    .map(PathBuf::from)
                    .filter(|path| path.is_absolute())
                    .map(|home| home.join(".local/share"))
            })
    }
}

pub(crate) fn require_local_app_data() -> Result<PathBuf> {
    #[cfg(windows)]
    const MISSING: &str = "LOCALAPPDATA must be an absolute directory";
    #[cfg(not(windows))]
    const MISSING: &str = "无法确定用户数据目录（需要 HOME 或 XDG_DATA_HOME）";
    local_app_data().context(MISSING)
}
