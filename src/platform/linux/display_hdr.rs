//! Read-only display capability queries for Linux desktops.
//! Neither X11 nor Wayland exposes a portable HDR mode today, so every display
//! reports "unknown" rather than claiming SDR or HDR it cannot verify.
use crate::protocol::capability::DisplayCapability;

pub(crate) fn capabilities(fallback_fps: u32) -> Vec<DisplayCapability> {
    let displays = display_info::DisplayInfo::all()
        .ok()
        .filter(|displays| !displays.is_empty())
        .map(|displays| {
            displays
                .into_iter()
                .enumerate()
                .map(|(index, display)| DisplayCapability {
                    id: index as i32,
                    fps: refresh_hz(display.frequency).unwrap_or(fallback_fps),
                    kind: 0,
                    hdr: -1,
                })
                .collect::<Vec<_>>()
        });
    displays.unwrap_or_else(|| {
        vec![DisplayCapability {
            id: 0,
            fps: fallback_fps,
            kind: 0,
            hdr: -1,
        }]
    })
}

/// A zero or absurd refresh rate means the compositor did not report one.
fn refresh_hz(frequency: f32) -> Option<u32> {
    (frequency.is_finite() && (1.0..=1000.0).contains(&frequency)).then(|| frequency.round() as u32)
}

#[allow(
    dead_code,
    reason = "Only the Windows presenter picks an HDR swap-chain colour space."
)]
pub(crate) fn monitor_is_hdr(_monitor: isize) -> bool {
    false
}
