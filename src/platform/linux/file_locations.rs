//! User-owned file browser roots, from xdg-user-dirs (which records the
//! localized names) with the English defaults as the fallback.
use std::path::{Path, PathBuf};

/// One xdg user directory, if it exists.
pub(crate) fn user_dir(key: &str, default: &str) -> Option<PathBuf> {
    let home = PathBuf::from(std::env::var_os("HOME")?);
    user_dirs_entry(&home, key)
        .into_iter()
        .chain(Some(home.join(default)))
        .find(|path| path.is_absolute() && path.is_dir() && *path != home)
}

fn user_dirs_entry(home: &Path, key: &str) -> Option<PathBuf> {
    let config = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .unwrap_or_else(|| home.join(".config"));
    let text = std::fs::read_to_string(config.join("user-dirs.dirs")).ok()?;
    let value = text.lines().rev().find_map(|line| {
        line.trim()
            .strip_prefix(key)?
            .trim_start()
            .strip_prefix('=')
            .map(|value| value.trim().trim_matches('"').to_owned())
    })?;
    Some(match value.strip_prefix("$HOME/") {
        Some(relative) => home.join(relative),
        None => PathBuf::from(value),
    })
}

pub(crate) fn known_folders() -> Vec<(PathBuf, &'static str, &'static str)> {
    [
        ("XDG_DOCUMENTS_DIR", "Documents", "文档", "document"),
        ("XDG_DESKTOP_DIR", "Desktop", "桌面", "desktop"),
        ("XDG_PICTURES_DIR", "Pictures", "图片", "picture"),
        ("XDG_VIDEOS_DIR", "Videos", "视频", "video"),
        ("XDG_MUSIC_DIR", "Music", "音乐", "music"),
        ("XDG_DOWNLOAD_DIR", "Downloads", "下载", "download"),
    ]
    .into_iter()
    .filter_map(|(key, default, name, icon)| Some((user_dir(key, default)?, name, icon)))
    .collect()
}
