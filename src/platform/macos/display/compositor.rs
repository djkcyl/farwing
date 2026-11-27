//! Compositor output takeover, viewer-only: macOS has no external compositor
//! to command, so every entry point reports that.
use anyhow::{Result, bail, ensure};

pub(crate) fn available() -> bool {
    false
}

pub(crate) fn ensure_available() -> Result<()> {
    ensure!(available(), "{}", unavailable_reason());
    Ok(())
}

pub(crate) fn unavailable_reason() -> String {
    "macOS 暂不支持接管新输出".to_owned()
}

pub(crate) fn set_mode(
    _connector: &str,
    _width: u32,
    _height: u32,
    _custom_hz: Option<u32>,
) -> Result<()> {
    ensure_available()
}

pub(crate) fn adopt(_connector: &str, _width: u32, _height: u32, _hz: u32) -> Result<()> {
    bail!("macOS 暂不支持接管新输出")
}
