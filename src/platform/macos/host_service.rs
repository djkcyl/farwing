//! The unattended host service, as far as macOS has one: none. Viewer-only
//! port, so every answer is the portable mode's, as on Linux.
use anyhow::{Result, bail};
use std::path::PathBuf;

const NO_SERVICE: &str = "macOS 尚未提供后台被控服务";

/// Whether an install, update or uninstall is replacing the service now.
pub(crate) fn maintaining() -> bool {
    false
}

pub(crate) mod vault {
    use super::*;

    /// Credentials stay in the user's keychain.
    pub(crate) fn applies() -> Result<bool> {
        Ok(false)
    }
    pub(crate) fn root() -> Result<PathBuf> {
        bail!(NO_SERVICE)
    }
    pub(crate) fn owner() -> Result<Option<String>> {
        Ok(None)
    }
    pub(crate) fn sid(_pid: u32) -> Result<String> {
        bail!(NO_SERVICE)
    }
    pub(crate) fn key(service: &str, account: &str) -> String {
        use sha2::{Digest, Sha256};
        format!(
            "{:x}.secret",
            Sha256::digest(format!("{service}\0{account}"))
        )
    }
    pub(crate) fn read(_key: &str) -> Result<Option<Vec<u8>>> {
        bail!(NO_SERVICE)
    }
    pub(crate) fn write(_key: &str, _value: Option<&[u8]>) -> Result<()> {
        bail!(NO_SERVICE)
    }
}

pub(crate) mod resident {
    use super::*;
    pub(crate) use crate::session::resident::{Reply, Request, Snapshot};

    /// This process is never the service's resident account owner.
    pub(crate) fn is_owner() -> bool {
        false
    }
    /// The desktop client always owns the account itself.
    pub(crate) fn managed() -> bool {
        false
    }
    pub(crate) async fn request(_request: Request) -> Result<Reply> {
        bail!(NO_SERVICE)
    }
    pub(crate) fn call(_request: Request) -> Result<Reply> {
        bail!(NO_SERVICE)
    }
    /// Only asked of a managed client; with no service there is nothing to retire.
    pub(crate) async fn pause_for_exit() -> Result<()> {
        Ok(())
    }
}

/// Work the resident owner waits for before it reports itself paused. With no
/// service there is no owner waiting, so nothing is counted.
pub(crate) mod activity {
    pub(crate) struct Work;
    impl Work {
        pub fn new() -> Self {
            Self
        }
    }
}

/// 登录自启。macOS 的 LaunchAgent 接入尚未做，先报告不可用。
pub(crate) mod startup {
    use super::*;

    /// 两种机制都用不了时的原因，界面据此把开关置灰并说明。
    pub(crate) fn unavailable() -> Option<String> {
        Some("macOS 尚未提供登录自启动设置".to_owned())
    }

    #[derive(Clone, Copy, PartialEq, Eq, Debug)]
    pub(crate) enum State {
        Disabled,
        Enabled,
        #[allow(dead_code)]
        DisabledByWindows,
    }

    pub(crate) fn state() -> Result<State> {
        Ok(State::Disabled)
    }

    pub(crate) fn configure(_enabled: bool) -> Result<()> {
        bail!("macOS 尚未提供登录自启动设置")
    }

    pub(crate) fn set_image(_enabled: bool, _path: &std::path::Path) -> Result<()> {
        bail!("macOS 尚未提供登录自启动设置")
    }

    pub(crate) fn enabled() -> Result<bool> {
        Ok(false)
    }

    pub(crate) fn migrate() {}
}

/// Holds the per-user lock that lets only one process keep a device's
/// presence room; two owners would keep kicking each other off the server.
pub(crate) struct PresenceReservation(#[allow(dead_code)] std::fs::File);

/// Reserve the device's presence room, or `None` while another process of
/// this user holds it.
pub(crate) fn reserve_presence(device: &str) -> Result<Option<PresenceReservation>> {
    anyhow::ensure!(
        !device.is_empty()
            && device
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_'),
        "设备标识无效"
    );
    let base = crate::platform::paths::require_local_app_data()?;
    let directory = base.join("openuuyc");
    std::fs::create_dir_all(&directory)?;
    let path = directory.join(format!("presence.{device}.lock"));
    let file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&path)
        .with_context(|| format!("无法打开在线状态锁 {}", path.display()))?;
    match file.try_lock() {
        Ok(()) => Ok(Some(PresenceReservation(file))),
        Err(std::fs::TryLockError::WouldBlock) => Ok(None),
        Err(std::fs::TryLockError::Error(error)) => Err(error.into()),
    }
}

/// Refuse a path that is a symbolic link, the counterpart of the Windows
/// reparse-point check on service-owned directories.
pub(crate) fn reject_reparse(path: &std::path::Path) -> Result<()> {
    if let Ok(metadata) = std::fs::symlink_metadata(path) {
        anyhow::ensure!(!metadata.file_type().is_symlink(), "组件目录不可为符号链接");
    }
    Ok(())
}

use anyhow::Context as _;
