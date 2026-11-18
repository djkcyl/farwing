//! Virtual displays. X11 drivers in general offer no way to add a monitor at
//! run time (only a few expose spare VIRTUAL outputs), so Linux has no driver
//! to open and the type below cannot be constructed. The host still asks, so
//! extended and super screens are refused with a reason instead of hidden.
use anyhow::{Result, bail};
use uuid::Uuid;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Output {
    pub adapter: u64,
    pub target: u32,
}
pub(crate) struct Watchdog {
    pub timeout: u32,
}

pub(crate) enum Driver {}
impl Driver {
    pub(crate) fn fallback(&self) -> Result<Option<Output>> {
        match *self {}
    }
    pub(crate) fn device_instance() -> Result<String> {
        bail!("Linux 被控端没有虚拟显示驱动")
    }
    pub(crate) fn open() -> Result<Self> {
        bail!("Linux 被控端没有虚拟显示驱动")
    }
    pub(crate) fn add(&self, _id: Uuid, _width: u32, _height: u32, _hz: u32) -> Result<Output> {
        match *self {}
    }
    pub(crate) fn remove(&self, _id: Uuid) -> Result<()> {
        match *self {}
    }
    pub(crate) fn render_adapter(&self, _adapter: u64) -> Result<()> {
        match *self {}
    }
    pub(crate) fn watchdog(&self) -> Result<Watchdog> {
        match *self {}
    }
    pub(crate) fn ping(&self) -> Result<()> {
        match *self {}
    }
}

/// Whether a failed removal means the display was already gone.
pub(crate) fn already_removed(_error: &anyhow::Error) -> bool {
    false
}
