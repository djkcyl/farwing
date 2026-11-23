//! Video decode errors and cancellation, plus Windows backend configuration.

#![allow(unsafe_code)]

mod error;
mod video;

pub use error::DecodeError;
pub use video::DecoderNotification;
#[cfg(windows)]
pub use video::{DecoderMode, VideoDecoderConfig};
