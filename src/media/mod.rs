//! Media ownership and module boundaries.

pub(crate) mod audio;
pub(crate) mod av1;
pub(crate) mod codec_parameters;
pub(crate) mod decoder;
pub(crate) mod decoder_pool;
pub(crate) mod decoder_result;
pub(crate) mod dependency_descriptor;
pub(crate) mod frame_crypto;
pub(crate) mod frame_meta;
pub(crate) mod microphone;
pub(crate) mod video_color;
pub(crate) mod video_format;

mod profile;
pub use profile::*;
pub mod preferences;
pub mod selection;

pub(crate) mod camera;
pub(crate) mod capture;
#[cfg(windows)]
pub(crate) mod encoding;
pub(crate) mod geometry;
#[cfg(windows)]
pub use crate::platform::display::detect_local_display;

pub(crate) mod decode_api;

/// Probe the local decoders in the background, before a viewer connects
/// with this decoding mode. `delay` lets a background start settle first.
pub fn prepare_decoder_support(mode: selection::ProcessingMode, delay: std::time::Duration) {
    decoder::prepare_native_decoder_support(mode, delay);
}
