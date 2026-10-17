//! Linux OS and GPU backends: VA-API decoding, wgpu presentation, X11 capture
//! and display topology, software encoding for the host role. No
//! account/session ownership in this layer.
pub(crate) mod audio_thread;
pub(crate) mod capture;
pub(crate) mod cuda;
pub(crate) mod cursor_shape;
pub(crate) mod deadline;
pub(crate) mod decoder;
pub(crate) mod device_profile;
pub(crate) mod display;
pub(crate) mod display_hdr;
pub(crate) mod dmabuf;
pub(crate) mod drag_drop;
pub(crate) mod encoder;
pub(crate) mod file_locations;
mod gpu_convert;
pub(crate) mod graphics;
pub(crate) mod host_service;
pub(crate) mod input;
pub(crate) mod loopback;
pub(crate) mod notifications;
pub(crate) mod nvenc;
pub(crate) mod power_request;
mod software_encoder;
pub(crate) mod startup_notify;
pub(crate) mod surface;
pub(crate) mod system_power;
pub(crate) mod terminal;
pub(crate) mod transfer;
mod vaapi_encoder;
mod video_layer;
pub(crate) mod virtual_audio;
pub(crate) mod virtual_display;
pub(crate) mod wol;
