//! Read-only display capability queries. The viewer-only macOS port does not
//! enumerate CoreAnimation displays yet, so it reports one unknown display at
//! the negotiated fallback rate.
use crate::protocol::capability::DisplayCapability;

pub(crate) fn capabilities(fallback_fps: u32) -> Vec<DisplayCapability> {
    vec![DisplayCapability {
        id: 0,
        fps: fallback_fps,
        kind: 0,
        hdr: -1,
    }]
}

#[allow(
    dead_code,
    reason = "Only a hardware presenter picks an HDR swap-chain colour space."
)]
pub(crate) fn monitor_is_hdr(_monitor: isize) -> bool {
    false
}
