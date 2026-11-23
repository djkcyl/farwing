#[cfg(windows)]
pub(crate) mod diagnostics;
#[cfg(windows)]
pub(crate) mod software_slot;

#[cfg(target_os = "linux")]
mod linux;
#[cfg(windows)]
mod windows;

#[cfg(target_os = "linux")]
pub(crate) use linux::{
    CpuVideoFrame, DecoderCandidate, NativeVideoDecoder, detect_native_decoder_support,
    prepare_native_decoder_support,
};
#[cfg(windows)]
pub(crate) use windows::{
    DecoderCandidate, NativeVideoDecoder, PROBE_SIZES, RenderSurface,
    detect_native_decoder_support, prepare_native_decoder_support,
};

#[cfg(target_os = "linux")]
pub(crate) type RenderSurface = CpuVideoFrame;

#[derive(Debug)]
pub(crate) struct DecodedFrame {
    pub pts: i64,
    pub width: u32,
    pub height: u32,
    pub surface: RenderSurface,
    pub ready_at: std::time::Instant,
}

#[derive(Default)]
pub(crate) struct DecodedBatch {
    pub frames: Vec<DecodedFrame>,
    pub input_error: Option<anyhow::Error>,
    pub output_issues: Vec<DecoderOutputIssue>,
}

pub(crate) enum DecoderOutputIssue {
    Dropped(i64),
    Failed {
        token: Option<i64>,
        error: anyhow::Error,
    },
}
