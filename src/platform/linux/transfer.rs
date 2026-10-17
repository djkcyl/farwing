//! Cross-adapter frame delivery. Linux capture and encoding share the one CPU
//! path, so a transfer is never needed and cannot be created.
use super::capture::{Device, Frame};
use anyhow::{Result, bail};

pub(crate) struct Delivery {
    pub frame: Frame,
}

pub(crate) enum Transfer {}
impl Transfer {
    pub fn new(_source: &Device, _destination: &Device, _frame: &Frame) -> Result<Self> {
        bail!("Linux 采集与编码共用同一设备，不需要跨设备传输")
    }
    pub fn matches(&self, _source: &Device, _destination: &Device, _frame: &Frame) -> bool {
        match *self {}
    }
    pub fn copy(&mut self, _frame: &Frame, _active: impl Fn() -> bool) -> Result<Option<Delivery>> {
        match *self {}
    }
}
