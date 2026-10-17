//! Active decoder configuration and cancellation.

#![forbid(unsafe_code)]

use crate::media::VideoCodec;

/// Decoder selected by the candidate owner; this layer never falls back.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
#[non_exhaustive]
pub enum DecoderMode {
    /// Return frames owned by the platform decoder (D3D11 textures on
    /// Windows, VA-API surfaces read back on Linux).
    #[default]
    Hardware,
    /// Decode with the selected Rust software core.
    Software,
}

/// Parameters for opening a video decoder session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VideoDecoderConfig {
    /// Input video codec.
    pub codec: VideoCodec,
    /// Expected width (may be refined from bitstream).
    pub width: u32,
    /// Expected height (may be refined from bitstream).
    pub height: u32,
    /// Explicit hardware or software implementation.
    pub mode: DecoderMode,
    /// Owning graphics device for hardware decoding, where the platform
    /// decoder shares one with the renderer (D3D11 on Windows).
    pub gpu_device: Option<crate::platform::decoder::GpuDevice>,
    /// Annex-B codec parameter sets; may be empty until the first keyframe.
    pub extra_data: bytes::Bytes,
}

/// Shared decoder cancellation, independent of an executor.
#[derive(Clone)]
pub struct DecoderNotification {
    cancelled: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

impl DecoderNotification {
    pub fn new(cancelled: std::sync::Arc<std::sync::atomic::AtomicBool>) -> Self {
        Self { cancelled }
    }

    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(std::sync::atomic::Ordering::Acquire)
    }

    pub(crate) fn shared_cancellation(&self) -> std::sync::Arc<std::sync::atomic::AtomicBool> {
        self.cancelled.clone()
    }

    pub(crate) fn cancellation(&self) -> &std::sync::atomic::AtomicBool {
        &self.cancelled
    }
}

impl Default for DecoderNotification {
    fn default() -> Self {
        Self::new(std::sync::Arc::new(std::sync::atomic::AtomicBool::new(
            false,
        )))
    }
}
