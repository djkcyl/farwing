//! Low-delay video coding for Farwing; AV1 software kernels require x86/x86_64.
//! One software session API; codec-specific syntax is shared with GPU adapters.
#![forbid(unsafe_code)]

pub mod decoder;
pub mod encoder;
mod format;
pub use format::{Codec, Format, PixelFormat};

/// Parsers needed by hardware decoders and RTP metadata, not software backends.
pub mod syntax {
    pub mod h264 {
        pub use farwing_h264::{headers, syntax::*};
    }
    pub use farwing_av1::bitstream as av1;
    pub use farwing_h265_syntax as hevc;
}
