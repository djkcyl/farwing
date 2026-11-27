//! User-owned file browser roots. macOS has no xdg-user-dirs, so the English
//! defaults inside the home directory are the localized names.
use std::path::PathBuf;

/// One home subdirectory of `default`, if it exists.
pub(crate) fn user_dir(_key: &str, default: &str) -> Option<PathBuf> {
    let home = PathBuf::from(std::env::var_os("HOME")?);
    let path = home.join(default);
    (path.is_absolute() && path.is_dir() && path != home).then_some(path)
}

pub(crate) fn known_folders() -> Vec<(PathBuf, &'static str, &'static str)> {
    [
        ("", "Documents", "文档", "document"),
        ("", "Desktop", "桌面", "desktop"),
        ("", "Pictures", "图片", "picture"),
        ("", "Movies", "视频", "video"),
        ("", "Music", "音乐", "music"),
        ("", "Downloads", "下载", "download"),
    ]
    .into_iter()
    .filter_map(|(key, default, name, icon)| Some((user_dir(key, default)?, name, icon)))
    .collect()
}
