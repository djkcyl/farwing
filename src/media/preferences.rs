//! Persistent defaults for this profile's controller; no account secrets.
use super::ConnectionMediaOptions;
use anyhow::{Context, Result};

fn path() -> Result<std::path::PathBuf> {
    Ok(crate::product::local_data_dir()?.join("media-preferences.json"))
}
/// When the settings file last changed (`None`: no file).
pub(crate) fn modified() -> Option<std::time::SystemTime> {
    std::fs::metadata(path().ok()?).ok()?.modified().ok()
}

pub fn load() -> Result<ConnectionMediaOptions> {
    load_from(&path()?)
}
pub(super) fn load_from(path: &std::path::Path) -> Result<ConnectionMediaOptions> {
    Ok(crate::storage::read(path)
        .context(crate::i18n::t().session.media_settings_read_failed)?
        .unwrap_or_else(|| {
            #[cfg(target_os = "linux")]
            return ConnectionMediaOptions {
                frame_rate: super::FrameRateChoice::Fps30,
                ..Default::default()
            };
            #[cfg(windows)]
            ConnectionMediaOptions::default()
        }))
}
pub(crate) fn save(options: ConnectionMediaOptions) -> Result<()> {
    save_to(&path()?, options)
}
pub(super) fn save_to(path: &std::path::Path, options: ConnectionMediaOptions) -> Result<()> {
    options.validate()?;
    crate::storage::write(path, &options).context(crate::i18n::t().session.media_settings_save_failed)
}
