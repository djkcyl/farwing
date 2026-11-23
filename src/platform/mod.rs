//! Native OS/driver implementations. Product policy belongs to sessions/features.
//! Select the real backend here; no placeholder implementations for future OSes.
#[cfg(target_os = "linux")]
pub(crate) mod linux;
#[cfg(target_os = "linux")]
#[path = "windows/input/keyboard.rs"]
pub(crate) mod keyboard;
#[cfg(windows)]
pub(crate) use windows::input::keyboard;
#[cfg(windows)]
pub(crate) use windows::device_profile;
#[cfg(target_os = "linux")]
pub(crate) use linux::device_profile;

#[cfg(windows)]
pub(crate) mod windows;
#[cfg(windows)]
pub(crate) use windows::{
    camera, capture, decoder, display, display_hdr, encoder, graphics, source, surface, swapchain,
    transfer, video_shader,
};

pub(crate) fn display_supports_hdr() -> bool {
    #[cfg(windows)]
    return display_hdr::any_hdr();
    #[cfg(target_os = "linux")]
    return false;
}
