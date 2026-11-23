use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ViewerWindowMode {
    #[default]
    Shared,
    Separate,
}

impl ViewerWindowMode {
    pub(crate) const fn label(self) -> &'static str {
        match self {
            Self::Shared => "共享观看窗口",
            Self::Separate => "每台设备独立窗口",
        }
    }

    pub(crate) const fn can_share_with(self, other: Self) -> bool {
        matches!((self, other), (Self::Shared, Self::Shared))
    }

    fn path() -> Result<PathBuf> {
        Ok(
            PathBuf::from(std::env::var_os("LOCALAPPDATA").context("LOCALAPPDATA unavailable")?)
                .join("OpenUUYC")
                .join("viewer-preferences.json"),
        )
    }

    pub(crate) fn load() -> Result<Self> {
        Self::load_from(&Self::path()?)
    }

    pub(crate) fn save(self) -> Result<()> {
        self.save_to(&Self::path()?)
    }

    fn load_from(path: &Path) -> Result<Self> {
        let bytes = match std::fs::read(path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Self::default());
            }
            Err(error) => return Err(error.into()),
        };
        let saved: Preferences = serde_json::from_slice(&bytes).context("读取观看窗口设置失败")?;
        Ok(saved.window_mode)
    }

    fn save_to(self, path: &Path) -> Result<()> {
        use std::io::Write;
        let parent = path.parent().context("观看窗口设置目录无效")?;
        std::fs::create_dir_all(parent)?;
        let temporary = parent.join(format!(".viewer-preferences-{}.tmp", uuid::Uuid::new_v4()));
        let result = (|| -> Result<()> {
            let mut file = std::fs::OpenOptions::new()
                .create_new(true)
                .write(true)
                .open(&temporary)?;
            file.write_all(&serde_json::to_vec_pretty(&Preferences {
                window_mode: self,
            })?)?;
            file.sync_all()?;
            drop(file);
            std::fs::rename(&temporary, path)?;
            Ok(())
        })();
        if result.is_err() {
            let _ = std::fs::remove_file(&temporary);
        }
        result.context("保存观看窗口设置失败")
    }
}

#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
struct Preferences {
    window_mode: ViewerWindowMode,
}

#[cfg(test)]
mod tests {
    use super::ViewerWindowMode;
    use std::path::PathBuf;

    fn temp_path() -> PathBuf {
        std::env::current_dir()
            .unwrap()
            .join("target")
            .join(format!("viewer-preferences-{}.json", uuid::Uuid::new_v4()))
    }

    #[test]
    fn only_shared_viewers_can_join_the_same_workspace() {
        assert!(ViewerWindowMode::Shared.can_share_with(ViewerWindowMode::Shared));
        assert!(!ViewerWindowMode::Shared.can_share_with(ViewerWindowMode::Separate));
        assert!(!ViewerWindowMode::Separate.can_share_with(ViewerWindowMode::Shared));
        assert!(!ViewerWindowMode::Separate.can_share_with(ViewerWindowMode::Separate));
    }

    #[test]
    fn missing_preference_defaults_to_shared() {
        let path = temp_path();
        assert_eq!(
            ViewerWindowMode::load_from(&path).unwrap(),
            ViewerWindowMode::Shared
        );
    }

    #[test]
    fn window_mode_round_trips_through_preferences() {
        let path = temp_path();
        ViewerWindowMode::Separate.save_to(&path).unwrap();
        assert_eq!(
            ViewerWindowMode::load_from(&path).unwrap(),
            ViewerWindowMode::Separate
        );
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn old_empty_preference_object_defaults_to_shared() {
        let path = temp_path();
        std::fs::write(&path, b"{}").unwrap();
        assert_eq!(
            ViewerWindowMode::load_from(&path).unwrap(),
            ViewerWindowMode::Shared
        );
        let _ = std::fs::remove_file(path);
    }
}
