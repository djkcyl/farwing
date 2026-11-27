//! Virtual display driver. macOS has no counterpart in this port, so the
//! feature reads as unavailable and a session never opens.
use anyhow::{Result, bail};

pub(crate) fn ready() -> bool {
    false
}

pub(crate) struct Session;
impl Session {
    pub(crate) fn open() -> Result<Self> {
        bail!("macOS 暂不支持虚拟屏")
    }
    pub(crate) fn create(&mut self, _width: u32, _height: u32, _hz: u32) -> Result<String> {
        bail!("macOS 暂不支持虚拟屏")
    }
    pub(crate) fn remove(&mut self, _connector: &str) -> Result<()> {
        bail!("macOS 暂不支持虚拟屏")
    }
    pub(crate) fn sync(&mut self) -> Result<Vec<String>> {
        Ok(Vec::new())
    }
    pub(crate) fn release_all(&mut self) {}
}

/// Why the feature is unavailable, as the settings view shows it.
#[derive(Clone)]
pub(crate) struct Availability {
    label: String,
    detail: Option<String>,
}

impl Availability {
    pub(crate) fn label(&self) -> &str {
        &self.label
    }

    pub(crate) fn detail(&self) -> Option<&str> {
        self.detail.as_deref()
    }
}

pub(crate) fn availability() -> Availability {
    Availability {
        label: "不可用".into(),
        detail: Some("macOS 暂不支持虚拟屏".into()),
    }
}
