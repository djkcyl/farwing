//! Capture source identity; backend handles never enter this descriptor.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) struct Screen {
    pub id: i32,
    pub device_name: String,
    pub display_name: String,
    pub width: u32,
    pub height: u32,
    pub left: i32,
    pub top: i32,
    pub primary: bool,
    pub fps: u32,
    pub dpi_scale: Option<u32>,
    pub hdr: bool,
    pub adapter: u64,
    pub render_adapter: Option<u64>,
    pub identity: Option<String>,
}
/// What an encoder pipeline captures. Only screens so far; cameras and
/// windows join as further kinds (`docs/protocol/camera-design.md` §9.1).
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) enum Source {
    Screen(Screen),
}

impl Source {
    /// Identifies the source among the process's encoder pipelines: two
    /// members share a pipeline only if their sources have the same key.
    pub(crate) fn key(&self) -> &str {
        match self {
            Self::Screen(screen) => &screen.device_name,
        }
    }
    pub(crate) fn size(&self) -> (u32, u32) {
        match self {
            Self::Screen(screen) => (screen.width, screen.height),
        }
    }
    /// The highest frame rate it produces.
    pub(crate) fn fps(&self) -> u32 {
        match self {
            Self::Screen(screen) => screen.fps,
        }
    }
    pub(crate) fn hdr(&self) -> bool {
        match self {
            Self::Screen(screen) => screen.hdr,
        }
    }
    /// The adapter its frames are on.
    pub(crate) fn adapter(&self) -> u64 {
        match self {
            Self::Screen(screen) => screen.adapter,
        }
    }
    /// The adapter that renders it (an encoder there avoids a copy).
    pub(crate) fn render_adapter(&self) -> u64 {
        match self {
            Self::Screen(screen) => screen.render_adapter.unwrap_or(screen.adapter),
        }
    }
    pub(crate) fn screen(&self) -> Option<&Screen> {
        match self {
            Self::Screen(screen) => Some(screen),
        }
    }
    /// For diagnostics.
    pub(crate) fn label(&self) -> String {
        match self {
            Self::Screen(screen) => screen.label(),
        }
    }
}

#[derive(Debug)]
pub(crate) struct SourceGone;
impl std::fmt::Display for SourceGone {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(crate::i18n::t().session.capture_display_disconnected)
    }
}
impl std::error::Error for SourceGone {}

/// Ignore device paths returned as display names by some remote implementations.
/// Keep actual model names intact; identity and selection never use this label.
pub(crate) fn monitor_name(name: &str) -> Option<&str> {
    let name = name.trim();
    let source = name.strip_prefix(r"\\.\").unwrap_or(name);
    // display-info also returns "Unknown Display <handle>" when Windows has no name.
    let numbered_source = ["DISPLAY", "Unknown Display "].iter().any(|prefix| {
        source
            .get(..prefix.len())
            .is_some_and(|p| p.eq_ignore_ascii_case(prefix))
            && source
                .get(prefix.len()..)
                .is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
    });
    (!name.is_empty() && !numbered_source).then_some(name)
}
impl Screen {
    pub(crate) fn label(&self) -> String {
        monitor_name(&self.display_name)
            .map(str::to_owned)
            .unwrap_or_else(|| (crate::i18n::t().session.display_number)(i64::from(self.id) + 1))
    }
}
