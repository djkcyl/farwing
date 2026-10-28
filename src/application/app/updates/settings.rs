use crate::i18n::t;
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub(super) struct Settings {
    pub allow_prerelease: bool,
}
impl Settings {
    fn path() -> Result<PathBuf> {
        Ok(crate::product::local_data_dir()?.join("updates.json"))
    }
    pub fn load() -> Result<Self> {
        Self::load_from(&Self::path()?)
    }
    pub fn save(self) -> Result<()> {
        self.save_to(&Self::path()?)
    }
    pub(super) fn load_from(path: &Path) -> Result<Self> {
        Ok(crate::storage::read(path)
            .context(t().app.read_update_settings_failed)?
            .unwrap_or_default())
    }
    pub(super) fn save_to(self, path: &Path) -> Result<()> {
        crate::storage::write(path, &self).context(t().app.save_update_settings_failed)
    }
}
