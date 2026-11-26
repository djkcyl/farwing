//! Persistent defaults for this user's controller; no account secrets.
use super::ConnectionMediaOptions;
use anyhow::{Context, Result};
use std::io::Write;
use std::sync::atomic::{AtomicBool, Ordering};

static FULLSCREEN_FLOATING_BALL: AtomicBool = AtomicBool::new(false);

pub(crate) fn fullscreen_floating_ball() -> bool {
    FULLSCREEN_FLOATING_BALL.load(Ordering::Acquire)
}

fn path() -> Result<std::path::PathBuf> {
    Ok(crate::platform::paths::require_local_app_data()?
        .join("OpenUUYC")
        .join("media-preferences.json"))
}
pub fn load() -> Result<ConnectionMediaOptions> {
    let options = load_from(&path()?)?;
    FULLSCREEN_FLOATING_BALL.store(options.fullscreen_floating_ball, Ordering::Release);
    Ok(options)
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
    save_to(&path()?, options)?;
    FULLSCREEN_FLOATING_BALL.store(options.fullscreen_floating_ball, Ordering::Release);
    Ok(())
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fullscreen_ball_is_opt_in_and_survives_preference_round_trips() {
        let directory = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("temp")
            .join(format!("media-preferences-{}", uuid::Uuid::new_v4()));
        let path = directory.join("preferences.json");
        assert!(!load_from(&path).unwrap().fullscreen_floating_ball);
        std::fs::create_dir_all(&directory).unwrap();
        std::fs::write(&path, br#"{"frame_rate":"fps60"}"#).unwrap();
        let mut options = load_from(&path).unwrap();
        assert!(!options.fullscreen_floating_ball);
        assert_eq!(options.frame_rate, crate::media::FrameRateChoice::Fps60);
        for enabled in [true, false] {
            options.fullscreen_floating_ball = enabled;
            save_to(&path, options).unwrap();
            assert_eq!(load_from(&path).unwrap(), options);
        }
        std::fs::remove_dir_all(directory).unwrap();
    }
}
