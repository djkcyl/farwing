//! Windows DXVA11 and software video decode contract.

#![allow(unsafe_code)]

mod error;
mod video;

pub use error::DecodeError;
pub use video::{DecoderMode, DecoderNotification, VideoDecoderConfig};
