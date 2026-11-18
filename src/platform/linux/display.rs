//! Local displays: the geometry a viewed stream is sized for, and for the
//! host role the RandR topology, its recovery guard and the (absent) virtual
//! display driver.
use crate::media::LocalDisplayInfo;
use anyhow::{Context, Result, bail};
use display_info::DisplayInfo;

pub(crate) mod compositor;
pub(crate) mod recovery;
pub(crate) mod topology;
pub(crate) mod virtual_driver;
pub(crate) mod wayland;

pub(crate) use crate::media::capture::source_id;

/// The compositor's own layout, when there is one to describe it.
///
/// Never waits: a watcher thread follows the compositor and this returns its
/// latest snapshot (see [`wayland`]). Without one the caller keeps the display
/// server's rectangles, which is what it would use anyway.
pub(crate) fn compositor_layout() -> Option<std::sync::Arc<crate::platform::CompositorLayout>> {
    wayland::layout()
}

/// [`compositor_layout`], waiting for the first description to arrive.
///
/// For a caller that cannot go on without the layout and is not in anyone's hot
/// path: the watcher starts lazily, so the first snapshot of a process is only
/// published a round trip after the first ask.
pub(crate) fn compositor_layout_soon() -> Option<std::sync::Arc<crate::platform::CompositorLayout>>
{
    wayland::layout_soon()
}

pub fn detect_local_display() -> Result<LocalDisplayInfo> {
    let displays = DisplayInfo::all().context("failed to enumerate local displays")?;
    let display = displays
        .iter()
        .find(|display| display.is_primary)
        .or_else(|| {
            displays
                .iter()
                .max_by_key(|display| u64::from(display.width) * u64::from(display.height))
        })
        .context("no local display was detected")?;
    if display.width == 0 || display.height == 0 {
        bail!("local display reported an invalid resolution");
    }
    // Use the maximum refresh of active displays with a 30 Hz floor.
    // The primary display still supplies geometry; using
    // only its refresh would incorrectly limit viewing on a faster monitor.
    let refresh_hz = displays
        .iter()
        .filter(|display| display.frequency.is_finite() && display.frequency > 0.0)
        .map(|display| display.frequency.round() as u32)
        .max()
        .unwrap_or(30)
        .max(30);
    Ok(LocalDisplayInfo {
        width: display.width,
        height: display.height,
        refresh_hz,
    })
}

/// Current active display dimensions offered by the UU controller. This is
/// neither a list of supported physical modes nor a request to change one.
pub(crate) fn local_display_dimensions() -> Vec<(u32, u32)> {
    let mut modes = DisplayInfo::all()
        .unwrap_or_default()
        .into_iter()
        .filter(|d| d.width != 0 && d.height != 0)
        .map(|d| (d.width, d.height))
        .collect::<Vec<_>>();
    modes.sort_unstable();
    modes.dedup();
    if modes.is_empty() {
        modes.push((1920, 1080));
    }
    modes
}
