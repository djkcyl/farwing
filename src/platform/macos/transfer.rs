//! Cross-adapter frame delivery, viewer-only build: there is no host capture
//! or encoding on macOS yet, so a transfer can never be created.
use super::capture::{Device, Frame};
use anyhow::{Result, bail};

pub(crate) struct Delivery {
    pub frame: Frame,
}

pub(crate) enum Transfer {}
impl Transfer {
    pub fn new(_source: &Device, _destination: &Device, _frame: &Frame) -> Result<Self> {
        bail!("macOS host role is not supported")
    }
    pub fn matches(&self, _source: &Device, _destination: &Device, _frame: &Frame) -> bool {
        match *self {}
    }
    pub fn copy(&mut self, _frame: &Frame, _active: impl Fn() -> bool) -> Result<Option<Delivery>> {
        match *self {}
    }
}
