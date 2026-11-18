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
#[derive(Debug, thiserror::Error)]
#[error("当前连接的显示器已断开")]
pub(crate) struct SourceGone;

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
            .unwrap_or_else(|| format!("显示器 {}", i64::from(self.id) + 1))
    }
}

/// A protocol source ID stays tied to the monitor interface for this process.
/// Enumeration order and OS display names can change after a hotplug.
pub(crate) fn source_id(identity: &str) -> anyhow::Result<i32> {
    use anyhow::Context as _;
    use std::collections::HashMap;
    use std::sync::{Mutex, OnceLock};
    static IDS: OnceLock<Mutex<HashMap<String, i32>>> = OnceLock::new();
    let mut ids = IDS
        .get_or_init(Default::default)
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    if let Some(id) = ids.get(identity) {
        return Ok(*id);
    }
    let id = i32::try_from(ids.len()).context("显示器标识已耗尽")?;
    ids.insert(identity.to_owned(), id);
    Ok(id)
}
