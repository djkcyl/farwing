//! Device capability exchange (session.start / session.setting) and the
//! ordinary-viewer format selection.
//!
//! Capability quality numbers are NOT capture-setting quality numbers.

use serde::{Deserialize, Serialize};

// Codec number of AV1 in capability and capture-setting messages. It is
// selected only when both peers list it; otherwise peers negotiate 1/2.
pub(crate) const AV1_CODEC_ID: i32 = 5;

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize)]
#[serde(default)]
pub(crate) struct CodecCapability {
    pub video_codec: i32,
    pub width: i32,
    pub height: i32,
    pub chroma_sampling: u8,
    pub bit_depth: u8,
    pub codec_impl: i32,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(default)]
pub(crate) struct DisplayCapability {
    pub id: i32,
    pub fps: u32,
    #[serde(rename = "type")]
    pub kind: i32,
    pub hdr: i32,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(default)]
pub(crate) struct DeviceCapability {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preferred_codec: Option<i32>,
    /// Decoder AND presentation support SDR10 independently from HDR.
    pub sdr_10bit: bool,
    /// Session this capability belongs to (empty for the controller's own).
    pub session_id: String,
    pub display_info: Vec<DisplayCapability>,
    pub video_codec_capability: Vec<CodecCapability>,
}

pub(crate) const QUALITY_DIMENSIONS: [(i32, i32); 4] =
    [(1280, 720), (1920, 1080), (2560, 1440), (3840, 2160)];

#[derive(Clone, Copy, Debug, Serialize)]
pub(crate) struct FrameQualityCapability {
    pub video_codec: i32,
    pub chroma_sampling: u8,
    pub bit_depth: u8,
    pub max_width: i32,
    pub max_height: i32,
    pub max_frame_quality: i32,
    pub result: i32,
}

impl FrameQualityCapability {
    fn valid(self) -> bool {
        self.has_dimensions() && self.result == 0
    }

    fn has_dimensions(self) -> bool {
        self.max_width != 0 && self.max_height != 0
    }
}

#[derive(Clone, Debug, Default, Serialize)]
pub(crate) struct DualCapability {
    #[serde(skip)]
    pub preferred_codec: Option<i32>,
    pub remote_display_info: Vec<DisplayCapability>,
    pub local_display_info: Vec<DisplayCapability>,
    pub frame_quality_capability: Vec<FrameQualityCapability>,
    #[serde(skip)]
    preferred_hardware: Vec<FrameQualityCapability>,
    /// The host's own preference, used when this side has none.
    #[serde(skip)]
    remote_preferred_codec: Option<i32>,
}

impl DualCapability {
    pub(crate) fn negotiate(local: &DeviceCapability, remote: DeviceCapability) -> Self {
        let mut rows = Vec::with_capacity(8);
        let mut preferred_hardware = Vec::new();
        for video_codec in [1, 2, AV1_CODEC_ID] {
            if video_codec == AV1_CODEC_ID
                && (!local
                    .video_codec_capability
                    .iter()
                    .any(|c| c.video_codec == AV1_CODEC_ID)
                    || !remote
                        .video_codec_capability
                        .iter()
                        .any(|c| c.video_codec == AV1_CODEC_ID))
            {
                continue;
            }
            for chroma_sampling in [1, 3] {
                for bit_depth in [8, 10] {
                    let maximum = |caps: &[CodecCapability]| {
                        caps.iter()
                            .filter(|cap| {
                                (cap.video_codec, cap.chroma_sampling, cap.bit_depth)
                                    == (video_codec, chroma_sampling, bit_depth)
                            })
                            .map(|cap| (cap.width, cap.height))
                            .max()
                    };
                    let encoder = maximum(&remote.video_codec_capability);
                    let decoder = maximum(&local.video_codec_capability);
                    let (quality, result) = match (encoder, decoder) {
                        (Some(encoder), Some(decoder)) => {
                            let limit = encoder.min(decoder);
                            match QUALITY_DIMENSIONS.iter().rposition(|size| *size <= limit) {
                                Some(index) => (index as i32 + 1, 0),
                                None => (-1, -3),
                            }
                        }
                        (Some(_), None) => (-1, -1),
                        (None, Some(_)) => (-1, -2),
                        (None, None) => (-1, -3),
                    };
                    // Emit the nominal tier, not the raw intersection.
                    // Failed rows have dimensions too; result must be checked.
                    let (max_width, max_height) = if quality > 0 {
                        QUALITY_DIMENSIONS[(quality - 1) as usize]
                    } else {
                        QUALITY_DIMENSIONS[1]
                    };
                    // Implementation 5 is the software encoder and 37 the
                    // software decoder. Keep this local preference out of the
                    // serialized capability contract.
                    let native_maximum = |caps: &[CodecCapability]| {
                        caps.iter()
                            .filter(|cap| {
                                (cap.video_codec, cap.chroma_sampling, cap.bit_depth)
                                    == (video_codec, chroma_sampling, bit_depth)
                                    && !matches!(cap.codec_impl, 5 | 37)
                            })
                            .map(|cap| (cap.width, cap.height))
                            .max()
                    };
                    if let (Some(a), Some(b)) = (
                        native_maximum(&local.video_codec_capability),
                        native_maximum(&remote.video_codec_capability),
                    ) {
                        if let Some(tier) = QUALITY_DIMENSIONS
                            .iter()
                            .rposition(|size| *size <= a.min(b))
                        {
                            let (max_width, max_height) = QUALITY_DIMENSIONS[tier];
                            preferred_hardware.push(FrameQualityCapability {
                                video_codec,
                                chroma_sampling,
                                bit_depth,
                                max_width,
                                max_height,
                                max_frame_quality: tier as i32 + 1,
                                result: 0,
                            });
                        }
                    }
                    rows.push(FrameQualityCapability {
                        video_codec,
                        chroma_sampling,
                        bit_depth,
                        max_width,
                        max_height,
                        max_frame_quality: quality,
                        result,
                    });
                }
            }
        }
        let remote_preferred_codec = remote.preferred_codec.filter(|c| matches!(c, 1 | 2 | 5));
        Self {
            preferred_codec: local
                .preferred_codec
                .filter(|c| matches!(c, 1 | 2 | 5))
                .or(remote_preferred_codec),
            remote_preferred_codec,
            remote_display_info: remote.display_info,
            local_display_info: local.display_info.clone(),
            frame_quality_capability: rows,
            preferred_hardware,
        }
    }

    /// Prefer `codec` (1, 2 or 5) from now on; `None` follows the host.
    pub(crate) fn prefer(&mut self, codec: Option<i32>) {
        self.preferred_codec = codec
            .filter(|c| matches!(c, 1 | 2 | 5))
            .or(self.remote_preferred_codec);
    }

    pub(crate) fn exact(
        &self,
        codec: i32,
        chroma: u8,
        hdr: bool,
    ) -> Option<FrameQualityCapability> {
        self.frame_quality_capability
            .iter()
            .rev()
            .copied()
            .find(|row| {
                row.video_codec == codec
                    && row.chroma_sampling == chroma
                    && (row.bit_depth >= 10) == hdr
            })
    }

    pub(crate) fn select(&self, chroma: u8, hdr: bool, quality: i32) -> FrameQualityCapability {
        let minimum = if matches!(quality, 0 | 5) { 0 } else { quality };
        let h264 = self.exact(1, chroma, hdr);
        let h265 = self.exact(2, chroma, hdr);
        let av1 = self.exact(AV1_CODEC_ID, chroma, hdr);
        let mut order = [AV1_CODEC_ID, 2, 1];
        order.sort_by_key(|codec| Some(*codec) != self.preferred_codec);
        for codec in order {
            if let Some(row) = self.preferred_hardware.iter().copied().find(|r| {
                r.video_codec == codec
                    && r.chroma_sampling == chroma
                    && (r.bit_depth >= 10) == hdr
                    && r.max_frame_quality >= minimum
                    && self
                        .exact(codec, chroma, hdr)
                        .is_some_and(|current| current.valid())
            }) {
                return row;
            }
        }
        for row in order
            .into_iter()
            .filter_map(|codec| self.exact(codec, chroma, hdr))
        {
            if row.valid() && row.max_frame_quality >= minimum {
                return row;
            }
        }
        [av1, h265, h264]
            .into_iter()
            .flatten()
            .filter(|r| r.valid())
            .max_by_key(|r| {
                (
                    r.max_frame_quality,
                    match r.video_codec {
                        5 => 3,
                        2 => 2,
                        _ => 1,
                    },
                )
            })
            .or_else(|| {
                [av1, h265, h264]
                    .into_iter()
                    .flatten()
                    .find(|r| r.has_dimensions())
            })
            .unwrap_or(FrameQualityCapability {
                video_codec: 2,
                chroma_sampling: 1,
                bit_depth: 8,
                max_width: 0,
                max_height: 0,
                max_frame_quality: 0,
                result: -3,
            })
    }
}
