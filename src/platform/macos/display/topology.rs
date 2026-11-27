//! Display topology, viewer-only: macOS never queries a topology, so every
//! entry point fails and the types keep the shape shared code compiles
//! against.
use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};

/// A display mode the server refused to apply.
#[derive(Debug)]
pub(crate) struct ModeTestRejected;
impl std::fmt::Display for ModeTestRejected {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("显示器拒绝该模式")
    }
}
impl std::error::Error for ModeTestRejected {}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Mode {
    pub width: u32,
    pub height: u32,
    pub hz: u32,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Dpi {
    pub current: u32,
    pub recommended: u32,
    pub supported: Vec<u32>,
}
/// Which virtual-display driver owns a target; none does on macOS.
#[derive(Clone, Debug, PartialEq, Eq)]
#[allow(dead_code)]
pub(crate) enum VirtualProvider {
    Uu,
    OpenUuyc,
}
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Target {
    pub identity: String,
    pub name: String,
    pub source: String,
    pub adapter: u64,
    pub adapter_path: String,
    pub target: u32,
    pub source_adapter: u64,
    pub source_id: u32,
    pub active: bool,
    pub available: bool,
    pub virtual_provider: Option<VirtualProvider>,
    pub modes: Vec<Mode>,
    pub preferred_mode: Option<(u32, u32)>,
    pub dpi: Option<Dpi>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct SavedTarget {
    pub identity: String,
    pub source_group: String,
    pub left: i32,
    pub top: i32,
    pub width: u32,
    pub height: u32,
    pub rotation: i32,
    pub refresh_numerator: u32,
    pub refresh_denominator: u32,
    pub scan_line_ordering: i32,
}
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct SavedTopology {
    pub targets: Vec<SavedTarget>,
}

/// An active output as the capture sees it.
#[derive(Clone, Debug)]
pub(crate) struct Monitor {
    pub identity: String,
    pub name: String,
    pub left: i32,
    pub top: i32,
    pub width: u32,
    pub height: u32,
    pub hz: Option<u32>,
    pub primary: bool,
}

pub(crate) fn identity(name: &str) -> String {
    format!("macos/{name}")
}

/// [`identity`] 的逆：输出名本身。
pub(crate) fn output_name(identity: &str) -> &str {
    identity.strip_prefix("macos/").unwrap_or(identity)
}

/// One consistent read of the display state; never constructed on macOS.
pub(crate) struct Topology {
    _never: std::convert::Infallible,
}

impl Topology {
    pub(crate) fn xwayland(&self) -> bool {
        false
    }

    pub(crate) fn query(_active_only: bool) -> Result<Self> {
        bail!("macOS host role is not supported")
    }

    pub(crate) fn snapshot(&self) -> Result<SavedTopology> {
        match self._never {}
    }

    pub(crate) fn metadata(&self) -> Result<Vec<Target>> {
        self.targets()
    }

    pub(crate) fn targets(&self) -> Result<Vec<Target>> {
        match self._never {}
    }

    pub(crate) fn monitors(&self) -> Vec<Monitor> {
        match self._never {}
    }
}

impl SavedTopology {
    /// Make exactly these outputs active with these modes and positions.
    pub(crate) fn apply(&self) -> Result<Vec<String>> {
        bail!("macOS host role is not supported")
    }
}

impl Target {
    pub(crate) fn revalidate(&self) -> Result<Self> {
        bail!("目标显示器已断开")
    }
    pub(crate) fn set_dpi(&self, _requested: u32, _active: impl Fn() -> bool) -> Result<()> {
        bail!("macOS 不支持按显示器调整缩放")
    }
    pub(crate) fn set_resolution(
        &self,
        _width: u32,
        _height: u32,
        _active: impl Fn() -> bool,
    ) -> Result<()> {
        bail!("macOS host role is not supported")
    }
}
