//! Native OS/driver implementations. Product policy belongs to sessions/features.
//! Select the real backend here; no placeholder implementations for future OSes.
#[cfg(windows)]
pub(crate) mod windows;
#[cfg(windows)]
pub(crate) use windows::{
    capture, decoder, display, display_hdr, encoder, graphics, surface, swapchain, transfer,
    video_shader,
};
