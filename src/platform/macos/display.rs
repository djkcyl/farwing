//! Local displays: the geometry a viewed stream is sized for, plus the display
//! topology modules the host role drives. macOS has no host role here, so the
//! topology never queries and the compositor/virtual-driver halves are
//! refused with a reason.
use crate::media::LocalDisplayInfo;
use anyhow::{Result, bail};

pub(crate) mod compositor;
pub(crate) mod recovery;
pub(crate) mod topology;
pub(crate) mod virtual_driver;

pub(crate) use crate::media::capture::source_id;

/// The compositor's own layout. macOS has no external compositor to read.
pub(crate) fn compositor_layout() -> Option<std::sync::Arc<crate::platform::CompositorLayout>> {
    None
}

/// [`compositor_layout`], waiting for the first description to arrive.
pub(crate) fn compositor_layout_soon() -> Option<std::sync::Arc<crate::platform::CompositorLayout>> {
    None
}

/// shortcut: no CoreGraphics enumeration yet; the viewer sizes itself from a
/// sane default until a real query lands.
pub fn detect_local_display() -> Result<LocalDisplayInfo> {
    Ok(LocalDisplayInfo {
        width: 1920,
        height: 1080,
        refresh_hz: 60,
    })
}

/// Current active display dimensions offered by the UU controller.
pub(crate) fn local_display_dimensions() -> Vec<(u32, u32)> {
    vec![(1920, 1080)]
}

// Keep the `bail!` import honest alongside the shortcut above.
#[allow(unused)]
fn _unused() -> Result<()> {
    bail!("macOS host role is not supported")
}
