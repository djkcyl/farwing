//! Windows GUI ownership and shared application controls.
use anyhow::Result;

pub(crate) mod chrome;
use crate::platform::graphics as d3d11;

mod tray;
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
