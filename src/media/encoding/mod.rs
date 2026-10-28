//! Sending formats are negotiated independently from the SDP codec family.
use crate::media::video_color::{HdrMetadata, VideoColorSpace};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub(crate) enum Codec {
    H264,
    H265,
    Av1,
}
impl Codec {
    pub fn from_mime(value: &str) -> Option<Self> {
        [Self::H264, Self::H265, Self::Av1]
            .into_iter()
            .find(|codec| value.eq_ignore_ascii_case(codec.mime()))
    }
    pub fn media(self) -> crate::media::VideoCodec {
        match self {
            Self::H264 => crate::media::VideoCodec::H264,
            Self::H265 => crate::media::VideoCodec::H265,
            Self::Av1 => crate::media::VideoCodec::Av1,
        }
    }
    pub fn session(self) -> farwing_protocol::session::Codec {
        match self {
            Self::H264 => farwing_protocol::session::Codec::H264,
            Self::H265 => farwing_protocol::session::Codec::Hevc,
            Self::Av1 => farwing_protocol::session::Codec::Av1,
        }
    }
    pub fn of(codec: farwing_protocol::session::Codec) -> Self {
        match codec {
            farwing_protocol::session::Codec::H264 => Self::H264,
            farwing_protocol::session::Codec::Hevc => Self::H265,
            farwing_protocol::session::Codec::Av1 => Self::Av1,
        }
    }
    /// The preference among codecs that do the same job: AV1, then HEVC.
    pub fn rank(self) -> u8 {
        match self {
            Self::Av1 => 3,
            Self::H265 => 2,
            Self::H264 => 1,
        }
    }
    pub fn mime(self) -> &'static str {
        match self {
            Self::H264 => "video/H264",
            Self::H265 => "video/H265",
            Self::Av1 => "video/AV1",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub(crate) struct Format {
    pub codec: Codec,
    pub chroma: u8,
    pub depth: u8,
}
impl Format {
    pub(crate) fn software(self) -> farwing_codec::Format {
        farwing_codec::Format {
            codec: self.codec.media(),
            chroma: self.chroma,
            depth: self.depth,
        }
    }

    pub const AVC: Self = Self {
        codec: Codec::H264,
        chroma: 1,
        depth: 8,
    };
    pub fn valid(self) -> bool {
        matches!(self.chroma, 1 | 3) && matches!(self.depth, 8 | 10)
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum Color {
    #[default]
    Sdr,
    Hdr,
}
impl Color {
    pub fn is_hdr(self) -> bool {
        self == Self::Hdr
    }
    pub fn space(self, metadata: Option<HdrMetadata>) -> VideoColorSpace {
        if self == Self::Hdr {
            VideoColorSpace {
                primaries: 9,
                transfer: 16,
                matrix: 9,
                range: 2,
                hdr_metadata: metadata,
            }
        } else {
            VideoColorSpace {
                primaries: 6,
                transfer: 4,
                matrix: 6,
                range: 1,
                hdr_metadata: None,
            }
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub(crate) enum Backend {
    Nvidia,
    Amd,
    Intel,
    Software,
}
impl Backend {
    pub fn name(self) -> &'static str {
        match self {
            Self::Nvidia => "NVIDIA NVENC",
            Self::Amd => "AMD AMF",
            Self::Intel => "INTEL QSV",
            Self::Software => "Farwing Rust",
        }
    }
    pub fn accepts(self, format: Format) -> bool {
        if !format.valid() {
            return false;
        }
        // NVENC and AMF expose Main profile in the current APIs. oneVPL has
        // High profile, but only a successful Query/Init and real output probe
        // can establish support on the selected Intel device.
        if format.codec == Codec::Av1
            && format.chroma == 3
            && matches!(self, Self::Nvidia | Self::Amd)
        {
            return false;
        }
        match self {
            // HEVC 44410 uses same-GPU planar CUDA input; AVC10 has no consumer.
            Self::Nvidia => !(format.depth == 10 && format.codec == Codec::H264),
            Self::Software => format.software().can_encode(),
            Self::Amd | Self::Intel => true,
        }
    }
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub(crate) struct Capability {
    pub adapter: u64,
    pub backend: Backend,
    pub format: Format,
    pub maximum: (u32, u32),
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct QualityTarget {
    /// Selected quality/custom ceiling, independent of congestion feedback.
    pub bitrate: u32,
    /// Negotiated frame-rate limit, not the observed input frame rate.
    pub fps: u32,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Rate {
    pub target: u32,
    pub peak: u32,
    pub fps: u32,
    /// The picture's size class; a change restarts at a keyframe.
    pub tier: crate::media::geometry::Tier,
    pub quality_target: QualityTarget,
    /// Cinema mode: a rate-control window of about a second lets busy
    /// scenes borrow bits from quiet ones, at the cost of latency.
    pub smooth: bool,
}

pub(crate) struct Encoded {
    pub data: Vec<u8>,
    pub keyframe: bool,
    pub timestamp_100ns: i64,
    pub is_new: bool,
    pub timing: Option<FrameTiming>,
    pub format: Format,
    pub color: crate::media::video_color::VideoColorSpace,
    /// Frame identity for `frame-meta`; set by the capture thread, not the
    /// encoder backend.
    pub meta: Option<crate::media::frame_meta::FrameMeta>,
    /// Long-term references as the backend actually coded them.
    pub ltr: LtrOutput,
}

/// What one frame should do with references
/// (`docs/protocol/rfi-ltr-brief.md` §6.8).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct FrameControl {
    pub keyframe: bool,
    /// Keep this frame as long-term reference `slot`.
    pub mark_ltr: Option<u8>,
    /// Predict this frame only from long-term reference `slot`.
    pub use_ltr: Option<u8>,
}

/// Long-term references of a coded frame, as reported by the encoder.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct LtrOutput {
    pub marked: Option<u8>,
    pub used: Option<u8>,
}
#[derive(Clone, Copy)]
pub(crate) struct FrameTiming {
    pub captured: std::time::Instant,
    pub encode_started: std::time::Instant,
    pub encode_finished: std::time::Instant,
}

pub(crate) mod rate;
