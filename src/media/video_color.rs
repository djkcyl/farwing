//! Color-space RTP extension and the renderer's SDR conversion policy.

pub(crate) const COLOR_SPACE_URI: &str = "http://www.webrtc.org/experiments/rtp-hdrext/color-space";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct VideoColorSpace {
    pub primaries: u8,
    pub transfer: u8,
    pub matrix: u8,
    pub range: u8,
    pub hdr_metadata: Option<HdrMetadata>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) struct HdrMetadata {
    pub max_luminance: u16,
    pub min_luminance: u16,
    pub chromaticity: [u16; 8],
    pub max_content_light_level: u16,
    pub max_frame_average_light_level: u16,
}

impl VideoColorSpace {
    pub(crate) fn parse(payload: &[u8]) -> Option<Self> {
        if !matches!(payload.len(), 4 | 28) {
            return None;
        }
        let [primaries, transfer, matrix, flags] = payload[..4].try_into().ok()?;
        // Native setters reject undefined H.273 IDs. Reserved flag bits are
        // ignored, but each two-bit chroma-siting value must be 0, 1 or 2.
        if !matches!(primaries, 1 | 2 | 4..=12 | 22)
            || !matches!(transfer, 1 | 2 | 4..=18)
            || !matches!(matrix, 0..=2 | 4..=14)
            || (flags & 3) == 3
            || ((flags >> 2) & 3) == 3
        {
            return None;
        }
        let hdr_metadata = if payload.len() == 28 {
            // Validate the optional mastering metadata as the native parser
            // does. Keep it with the frame for HDR/SDR output selection.
            let word = |index| u16::from_be_bytes([payload[index], payload[index + 1]]);
            if word(4) > 20_000
                || word(6) > 50_000
                || (8..24).step_by(2).any(|index| word(index) > 50_000)
                || word(24) > 20_000
                || word(26) > 20_000
            {
                return None;
            }
            Some(HdrMetadata {
                max_luminance: word(4),
                min_luminance: word(6),
                chromaticity: std::array::from_fn(|index| word(8 + index * 2)),
                max_content_light_level: word(24),
                max_frame_average_light_level: word(26),
            })
        } else {
            None
        };
        Some(Self {
            primaries,
            transfer,
            matrix,
            range: (flags >> 4) & 3,
            hdr_metadata,
        })
    }

    pub(crate) fn rendering(self) -> RenderColor {
        RenderColor {
            matrix: match self.matrix {
                1 => ColorMatrix::Bt709,
                9 => ColorMatrix::Bt2020,
                // Use BT.601 for unspecified/other matrices.
                _ => ColorMatrix::Bt601,
            },
            full_range: self.range == 2,
            hdr_peak_nits: (self.transfer == 16)
                .then(|| self.hdr_metadata.map_or(1000, |meta| meta.max_luminance)),
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum ColorMatrix {
    #[default]
    Bt601,
    Bt709,
    Bt2020,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct RenderColor {
    pub matrix: ColorMatrix,
    pub full_range: bool,
    /// SMPTE ST 2084 (PQ), as produced by the Windows HDR capture path.
    /// None means SDR, including ordinary 10-bit SDR fixtures.
    pub hdr_peak_nits: Option<u16>,
}

impl RenderColor {
    /// Affine YUV->RGB rows, for normalized NV12 or high-aligned P010 samples.

    pub(crate) fn transform(self, bit_depth: u8) -> [[f32; 4]; 3] {
        let (sample_scale, black, luma_span, center, chroma_span) =
            match (bit_depth, self.full_range) {
                (10, true) => (65_535.0 / 64.0, 0.0, 1023.0, 512.0, 1023.0),
                (10, false) => (65_535.0 / 64.0, 64.0, 876.0, 512.0, 896.0),
                (_, true) => (255.0, 0.0, 255.0, 128.0, 255.0),
                (_, false) => (255.0, 16.0, 219.0, 128.0, 224.0),
            };
        let (kr, kb) = match self.matrix {
            ColorMatrix::Bt601 => (0.299, 0.114),
            ColorMatrix::Bt709 => (0.2126, 0.0722),
            ColorMatrix::Bt2020 => (0.2627, 0.0593),
        };
        let kg = 1.0 - kr - kb;
        let red_cr = 2.0 * (1.0 - kr);
        let blue_cb = 2.0 * (1.0 - kb);
        let green_cb = -2.0 * kb * (1.0 - kb) / kg;
        let green_cr = -2.0 * kr * (1.0 - kr) / kg;
        let y = sample_scale / luma_span;
        let c = sample_scale / chroma_span;
        let y_offset = -black / luma_span;
        let c_offset = -center / chroma_span;
        [
            [y, 0.0, red_cr * c, y_offset + red_cr * c_offset],
            [
                y,
                green_cb * c,
                green_cr * c,
                y_offset + (green_cb + green_cr) * c_offset,
            ],
            [y, blue_cb * c, 0.0, y_offset + blue_cb * c_offset],
        ]
    }
}

#[derive(Default)]
pub(crate) struct VideoColorHistory {
    last: Option<VideoColorSpace>,
}

impl VideoColorHistory {
    /// Only the frame's last packet carries color. A valid extension replaces
    /// the cache; a keyframe without one clears it; deltas inherit it.
    pub(crate) fn receive(
        &mut self,
        last_packet: bool,
        keyframe: bool,
        extension: Option<&[u8]>,
    ) -> Option<VideoColorSpace> {
        if !last_packet {
            return None;
        }
        let parsed = extension.and_then(VideoColorSpace::parse);
        if parsed.is_some() || keyframe {
            if self.last != parsed {
                tracing::debug!(previous = ?self.last, current = ?parsed, keyframe, "received video color-space change");
            }
            self.last = parsed;
        }
        self.last
    }
}
