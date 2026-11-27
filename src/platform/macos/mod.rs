//! macOS backends.
//!
//! The viewer role is real (wgpu/Metal presentation, software decoding,
//! CoreAudio playback); the host role (capture, encoders, input injection,
//! virtual audio/display) has no macOS implementation yet, so those modules
//! are stubs that refuse with a reason. Signatures mirror the Linux side,
//! which the shared host pipeline compiles against.

pub(crate) mod audio_thread;
pub(crate) mod capture;
pub(crate) mod cursor_shape;
pub(crate) mod deadline;
pub(crate) mod decoder;
pub(crate) mod device_profile;
pub(crate) mod display;
pub(crate) mod display_hdr;
pub(crate) mod drag_drop;
pub(crate) mod encoder;
pub(crate) mod file_locations;
pub(crate) mod graphics;
pub(crate) mod host_service;
pub(crate) mod loopback;
pub(crate) mod notifications;
pub(crate) mod power_request;
pub(crate) mod surface;
pub(crate) mod system_power;
pub(crate) mod terminal;
pub(crate) mod transfer;
pub(crate) mod virtual_audio;
pub(crate) mod virtual_display;
pub(crate) mod wol;
mod video_layer;
