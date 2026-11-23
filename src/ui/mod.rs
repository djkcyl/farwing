//! Shared native controls and the Windows window backend.

#[cfg(windows)]
pub(crate) mod chrome;
#[cfg(windows)]
use crate::platform::graphics as d3d11;

#[cfg(windows)]
pub(crate) mod tray;
#[cfg(windows)]
pub(crate) mod window_manager;
#[cfg(windows)]
mod windows;

#[cfg(windows)]
mod app;
pub(crate) mod branding;
pub(crate) mod center_icon;
pub(crate) mod controls;
pub(crate) mod device_list;
pub(crate) mod device_visuals;
pub(crate) mod fonts;
pub(crate) mod login;
pub(crate) mod theme;
#[cfg(windows)]
pub(crate) mod video_textures;
#[cfg(windows)]
use app::{AppFactory, AppSession};
#[cfg(windows)]
pub(crate) use app::{App, WindowConfig};

#[cfg(windows)]
pub(crate) fn run(config: WindowConfig, factory: AppFactory) -> anyhow::Result<()> {
    windows::run(config, factory)
}
