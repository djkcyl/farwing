//! Desktop sound capture, viewer-only: macOS has no loopback backend in this
//! port, so devices never enumerate and a capture never opens.
use crate::media::audio::sender::BLOCK;
use anyhow::{Result, bail};
use std::time::Instant;

pub(crate) type Samples = std::sync::Arc<dyn Fn([f32; BLOCK * 2], Instant) + Send + Sync>;

const UNSUPPORTED: &str = "macOS 桌面声音采集尚未支持";

/// A playback device.
pub(crate) struct Endpoint {
    pub id: String,
    description: String,
}
impl Endpoint {
    pub fn name(&self) -> String {
        self.description.clone()
    }
}

#[derive(Default)]
pub(crate) struct Change {
    pub refresh: bool,
    pub rebuild: bool,
}

pub(crate) struct Devices;
impl Devices {
    pub fn new() -> Result<Self> {
        bail!(UNSUPPORTED)
    }
    pub fn changes(&self, _selected: Option<&str>) -> Change {
        Change::default()
    }
    pub fn endpoint(&self, _selected: Option<&str>) -> Result<Option<Endpoint>> {
        Ok(None)
    }
    pub fn list(&self) -> Result<Vec<Endpoint>> {
        Ok(Vec::new())
    }
}

pub(crate) struct Capture {
    _never: std::convert::Infallible,
}

pub(crate) fn open(_endpoint: &Endpoint, _samples: Samples) -> Result<Capture> {
    bail!(UNSUPPORTED)
}

impl Capture {
    pub fn producing(&self) -> bool {
        match self._never {}
    }
    pub fn poll(&mut self) -> Result<()> {
        match self._never {}
    }
}
