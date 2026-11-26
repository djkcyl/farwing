//! Desktop GUI ownership and shared application controls.
use anyhow::Result;

pub(crate) mod chrome;
pub(crate) mod gfx;

#[cfg(windows)]
mod tray;
#[cfg(target_os = "linux")]
#[path = "tray_linux.rs"]
mod tray;

/// 已有实例的唤起通道。Windows 用注册窗口消息（见 instance.rs），其余平台没有
/// 对应的窗口属性，Linux 用 unix socket。
#[cfg(target_os = "linux")]
#[path = "control_linux.rs"]
pub(crate) mod control;

/// SIGTERM/SIGINT 转成一次正常退出，服务被 `systemctl stop` 时才有收尾。
#[cfg(target_os = "linux")]
#[path = "shutdown_linux.rs"]
pub(crate) mod shutdown;

pub(crate) mod window_manager;
mod windows;

mod app;
pub(crate) mod branding;
pub(crate) mod controls;
pub(crate) mod fonts;
pub(crate) mod theme;
use app::AppFactory;

use app::AppSession;
pub(crate) use app::{App, WindowConfig};

pub(crate) fn run(config: WindowConfig, factory: AppFactory) -> Result<()> {
    windows::run(config, factory)
}
