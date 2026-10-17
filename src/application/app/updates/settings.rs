use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::{
    io::Write,
    path::{Path, PathBuf},
};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub(super) struct Settings {
    pub allow_prerelease: bool,
}
impl Settings {
    fn path() -> Result<PathBuf> {
        Ok(crate::platform::paths::require_local_app_data()?
            .join("OpenUUYC")
            .join("updates.json"))
    }
    pub fn load() -> Result<Self> {
        Self::load_from(&Self::path()?)
    }
    pub fn save(self) -> Result<()> {
        self.save_to(&Self::path()?)
    }
    pub(super) fn load_from(path: &Path) -> Result<Self> {
        match std::fs::read(path) {
            Ok(bytes) => serde_json::from_slice(&bytes).context("读取更新设置失败"),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(error) => Err(error).context("读取更新设置失败"),
        }
    }
    pub(super) fn save_to(self, path: &Path) -> Result<()> {
        let parent = path.parent().context("更新设置目录无效")?;
        std::fs::create_dir_all(parent)?;
        let temporary = parent.join(format!(".updates-{}.tmp", uuid::Uuid::new_v4()));
        let result = (|| -> Result<()> {
            let mut file = std::fs::OpenOptions::new()
                .create_new(true)
                .write(true)
                .open(&temporary)?;
            file.write_all(&serde_json::to_vec_pretty(&self)?)?;
            file.sync_all()?;
            drop(file);
            std::fs::rename(&temporary, path)?;
            Ok(())
        })();
        if result.is_err() {
            let _ = std::fs::remove_file(&temporary);
        }
        result.context("保存更新设置失败")
    }
}
