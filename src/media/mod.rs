//! Media ownership and module boundaries.

pub(crate) mod audio;
pub(crate) mod av1;
pub(crate) mod codec_parameters;
pub(crate) mod decoder;
pub(crate) mod decoder_pool;
pub(crate) mod decoder_result;
pub(crate) mod dependency_descriptor;
pub(crate) mod frame_meta;
pub(crate) mod microphone;
pub(crate) mod video_color;
pub(crate) mod video_format;

mod profile;
pub use profile::*;
pub mod preferences;
pub mod selection;

pub(crate) mod capture;
pub(crate) mod encoding;
pub(crate) mod geometry;
pub use crate::platform::display::detect_local_display;
pub(crate) use crate::platform::display::local_display_dimensions;

pub(crate) mod decode_api;
