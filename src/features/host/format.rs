//! Publisher negotiation and format selection policy.
pub(crate) use crate::media::encoding::{
    Backend, Capability, Codec, Color, Format, QualityTarget, Rate,
};
use super::parameters::Tier;
use crate::media::video_color::VideoColorSpace;
use crate::i18n::t;
use std::sync::atomic::{AtomicU8, AtomicU64, Ordering};

/// One way to send a format: an encoder of this host and a decoder of the
/// controller that both take it.
#[derive(Clone, Debug)]
pub(crate) struct Choice {
    pub capability: Capability,
    /// The largest picture both sides take.
    pub maximum: (u32, u32),
    pub fps: u32,
    hardware_decode: bool,
}
#[derive(Debug)]
pub(crate) struct Negotiated {
    pub choices: Vec<Choice>,
    /// The codec the host prefers when the controller has no preference.
    preferred_codec: Option<Codec>,
    codecs: AtomicU8,
    pub preferred_gpu: Option<u64>,
    pub gpu_compatibility: bool,
    /// The controller presents AV1 at its declared visible size (the
    /// padding of an aligned-only encoder left out).
    pub av1_visible_size: bool,
    source_gpu: AtomicU64,
    requested_gpu: Option<crate::media::selection::GpuId>,
}
impl Negotiated {
    pub(crate) fn deferred() -> Self {
        Self {
            choices: Vec::new(),
            preferred_codec: None,
            codecs: AtomicU8::new(15),
            preferred_gpu: None,
            gpu_compatibility: false,
            av1_visible_size: false,
            source_gpu: AtomicU64::new(0),
            requested_gpu: None,
        }
    }
    pub(crate) fn for_track(&self) -> Self {
        Self {
            choices: self.choices.clone(),
            preferred_codec: self.preferred_codec,
            codecs: AtomicU8::new(15),
            preferred_gpu: self.preferred_gpu,
            gpu_compatibility: self.gpu_compatibility,
            av1_visible_size: self.av1_visible_size,
            source_gpu: AtomicU64::new(self.source_gpu.load(Ordering::Relaxed)),
            requested_gpu: self.requested_gpu,
        }
    }
    /// Every format an encoder of this host (`local`) and a decoder of the
    /// controller (`decoders`) share.
    pub fn new(
        local: &[Capability],
        decoders: &[farwing_protocol::session::Decoder],
        settings: super::EncodingSettings,
        source_gpu: u64,
    ) -> anyhow::Result<Self> {
        let requested_gpu = (settings.mode != super::EncoderMode::Software)
            .then_some(settings.gpu)
            .flatten();
        let mut choices = Vec::new();
        for capability in local {
            let format = capability.format;
            for decoder in decoders {
                if Codec::of(decoder.codec) != format.codec
                    || decoder.chroma444 != (format.chroma == 3)
                    || decoder.bit10 != (format.depth == 10)
                    || decoder.width < 2
                    || decoder.height < 2
                {
                    continue;
                }
                choices.push(Choice {
                    hardware_decode: decoder.hardware,
                    capability: capability.clone(),
                    maximum: (
                        capability.maximum.0.min(decoder.width),
                        capability.maximum.1.min(decoder.height),
                    ),
                    fps: if capability.backend == Backend::Software {
                        farwing_codec::encoder::maximum_fps(format.codec.media())
                    } else {
                        144
                    },
                });
            }
        }
        if choices.is_empty() {
            tracing::warn!(?local, ?decoders, "host media capability intersection empty");
            anyhow::bail!("{}", t().features.host_no_common_format);
        }
        Ok(Self {
            choices,
            preferred_codec: settings.preferred_codec.preferred().map(Codec::of),
            codecs: AtomicU8::new(15),
            preferred_gpu: crate::media::selection::resolve_gpu(requested_gpu),
            gpu_compatibility: settings.gpu_compatibility,
            av1_visible_size: decoders
                .iter()
                .filter(|d| Codec::of(d.codec) == Codec::Av1)
                .all(|d| d.av1_visible_size),
            source_gpu: AtomicU64::new(source_gpu),
            requested_gpu,
        })
    }
    pub fn source_gpu(&self, adapter: u64) {
        self.source_gpu.store(adapter, Ordering::Relaxed);
    }
    pub fn preference(&self) -> (Option<crate::media::selection::GpuId>, Option<farwing_protocol::session::Codec>) {
        (self.requested_gpu, self.preferred_codec.map(Codec::session))
    }
    pub fn selection_reason(
        &self,
        capability: &Capability,
        recovered: bool,
    ) -> crate::media::selection::SelectionReason {
        use crate::media::selection::SelectionReason as R;
        if recovered {
            R::FailedCandidate
        } else if self.requested_gpu.is_some() && self.preferred_gpu.is_none() {
            R::MissingGpu
        } else if self
            .preferred_gpu
            .is_some_and(|gpu| gpu != capability.adapter)
        {
            R::GpuFormat
        } else if self
            .preferred_codec
            .is_some_and(|codec| codec != capability.format.codec)
        {
            R::CodecFormat
        } else if self.preferred_gpu.is_some() || self.preferred_codec.is_some() {
            R::Preferred
        } else {
            R::Automatic
        }
    }
    pub fn bind_codecs(
        &self,
        codecs: &[webrtc::rtp_transceiver::rtp_codec::RTCRtpCodecParameters],
    ) {
        let bits = codecs.iter().fold(0, |bits, codec| {
            bits | match Codec::from_mime(&codec.capability.mime_type) {
                Some(Codec::H264) => 1,
                Some(Codec::H265) => 2,
                Some(Codec::Av1) => {
                    match crate::media::av1::rtp_profile(&codec.capability.sdp_fmtp_line) {
                        Some(0) => 4,
                        Some(1 | 2) => 12,
                        _ => 0,
                    }
                }
                None => 0,
            }
        });
        self.codecs.store(bits, Ordering::Release);
    }
    pub fn permits_codec(&self, codec: Codec) -> bool {
        self.codecs.load(Ordering::Acquire)
            & match codec {
                Codec::H264 => 1,
                Codec::H265 => 2,
                Codec::Av1 => 12,
            }
            != 0
    }
    pub fn permits_format(&self, format: Format) -> bool {
        if format.codec == Codec::Av1 {
            self.codecs.load(Ordering::Acquire) & if format.chroma == 3 { 8 } else { 4 } != 0
        } else {
            self.permits_codec(format.codec)
        }
    }
    /// The highest tier whose picture of `source` fits `maximum`.
    pub fn maximum_tier(&self, source: (u32, u32), maximum: (u32, u32)) -> Tier {
        Tier::ALL
            .into_iter()
            .filter(|&tier| {
                let output = crate::media::geometry::output_size(source.0, source.1, tier);
                crate::media::geometry::fits_within(output, maximum)
            })
            .max()
            .unwrap_or(Tier::P720)
    }
    /// What this participant can be sent: codecs, 4:4:4, HDR and the
    /// highest tier.
    pub fn limits(&self) -> crate::features::stream_control::wire::StreamLimits {
        use crate::features::stream_control::wire::{Codec as WireCodec, Quality, StreamLimits};
        let usable: Vec<_> = self
            .choices
            .iter()
            .filter(|c| self.permits_format(c.capability.format))
            .collect();
        let mut codecs: Vec<_> = usable
            .iter()
            .map(|c| WireCodec::of(Some(c.capability.format.codec.session())) as i32)
            .collect();
        codecs.sort_unstable();
        codecs.dedup();
        let largest = usable
            .iter()
            .map(|c| c.maximum)
            .max_by_key(|(w, h)| u64::from(*w) * u64::from(*h))
            .unwrap_or_default();
        let tier = Tier::ALL
            .into_iter()
            .filter(|tier| crate::media::geometry::fits_within(tier.size(), largest))
            .max()
            .unwrap_or(Tier::P720);
        StreamLimits {
            codecs,
            chroma444: usable.iter().any(|c| c.capability.format.chroma == 3),
            hdr: usable.iter().any(|c| c.capability.format.depth == 10),
            max_quality: [Quality::Low, Quality::Standard, Quality::High, Quality::Ultra]
                [tier as usize] as i32,
        }
    }
    /// Pick the format for `config`: `codec` if set, 4:4:4 (`chroma` 3) and
    /// HDR when asked and possible, at the highest frame rate up to the
    /// requested one.
    pub fn apply(
        &self,
        config: &mut super::VideoConfig,
        codec: Option<Codec>,
        chroma: u8,
        hdr: bool,
        source: (u32, u32),
    ) -> anyhow::Result<()> {
        let colors: &[Color] = if hdr {
            &[Color::Hdr, Color::Sdr]
        } else {
            &[Color::Sdr]
        };
        let samplings: &[u8] = if chroma == 3 { &[3, 1] } else { &[1] };
        let desired = match config.quality {
            super::parameters::Quality::Tier(tier) => {
                crate::media::geometry::output_size(source.0, source.1, tier)
            }
            super::parameters::Quality::Auto | super::parameters::Quality::Custom => source,
        };
        let mut selected = None;
        'color: for &color in colors {
            for &chroma in samplings {
                let mut fps = config.requested_fps;
                while fps >= 30 {
                    let best = self
                        .choices
                        .iter()
                        .filter(|c| {
                            let format = c.capability.format;
                            c.fps >= fps
                                && format.chroma == chroma
                                && (!color.is_hdr() || format.depth == 10)
                                && codec.is_none_or(|wanted| wanted == format.codec)
                                && self.permits_format(format)
                        })
                        .max_by_key(|c| {
                            let size = crate::media::geometry::fit_size(desired.0, desired.1, c.maximum);
                            crate::media::selection::candidate_rank(
                                size,
                                c.capability.backend != Backend::Software && c.hardware_decode,
                                Some(c.capability.format.codec) == self.preferred_codec,
                                fps,
                                c.capability.adapter,
                                self.preferred_gpu,
                                self.source_gpu.load(Ordering::Relaxed),
                                c.capability.format.codec,
                                c.capability.format.depth,
                            )
                        });
                    if let Some(choice) = best {
                        selected = Some((choice, fps, color));
                        break 'color;
                    }
                    // The next common rate below: 120, 90, 60, 30.
                    fps = [120, 90, 60, 30].into_iter().find(|&rate| rate < fps).unwrap_or(0);
                }
            }
        }
        let (choice, fps, color) =
            selected.ok_or_else(|| anyhow::anyhow!(t().features.host_requested_format_unsupported))?;
        config.color = color;
        config.format = choice.capability.format;
        config.maximum = choice.maximum;
        config.maximum_fps = fps;
        config.fps = fps.min(config.fps_limit);
        config.maximum_tier = self.maximum_tier(source, config.maximum);
        config.auto_tier = config.auto_tier.min(config.maximum_tier);
        if let super::parameters::Quality::Tier(tier) = config.quality {
            config.quality = super::parameters::Quality::Tier(tier.min(config.maximum_tier));
        }
        Ok(())
    }
    /// [`Self::apply`] with the codec the controller asked for (else the
    /// host's preference), or any codec where that one is not available (on
    /// this screen's GPU, or among the codecs the connection negotiated).
    pub fn apply_requested(
        &self,
        config: &mut super::VideoConfig,
        chroma: u8,
        hdr: bool,
        source: (u32, u32),
    ) -> anyhow::Result<()> {
        if let Some(codec) = config.requested_codec.or(self.preferred_codec)
            && self.apply(config, Some(codec), chroma, hdr, source).is_ok()
        {
            return Ok(());
        }
        self.apply(config, None, chroma, hdr, source)
    }
    /// The encoder for `wanted`; with `exact`, only one producing exactly
    /// `wanted.format` (the shared viewer stream).
    pub fn candidate(
        &self,
        wanted: &super::VideoConfig,
        source: (u32, u32),
        source_gpu: u64,
        failed: &std::collections::HashSet<(u64, Backend, Format)>,
        exact: bool,
    ) -> Option<&Choice> {
        self.choices
            .iter()
            .filter(|c| {
                (!exact || c.capability.format == wanted.format)
                    && self.permits_format(c.capability.format)
                    && c.capability.format.chroma == wanted.format.chroma
                    && (c.capability.format.depth == wanted.format.depth
                        || (wanted.format.depth == 10 && c.capability.format.depth == 8))
                    && !failed.contains(&(
                        c.capability.adapter,
                        c.capability.backend,
                        c.capability.format,
                    ))
            })
            .max_by_key(|c| {
                let desired = crate::media::geometry::output_size(
                    source.0,
                    source.1,
                    wanted.quality.tier(wanted.auto_tier),
                );
                let fitted = crate::media::geometry::fit_size(desired.0, desired.1, c.maximum);
                crate::media::selection::candidate_rank(
                    fitted,
                    c.capability.backend != Backend::Software && c.hardware_decode,
                    c.capability.format == wanted.format,
                    c.fps.min(wanted.maximum_fps),
                    c.capability.adapter,
                    self.preferred_gpu,
                    source_gpu,
                    c.capability.format.codec,
                    c.capability.format.depth,
                )
            })
    }
    pub fn supports_codec(&self, codec: Codec) -> bool {
        self.choices
            .iter()
            .any(|c| c.capability.format.codec == codec)
    }
    pub fn apply_source(
        &self,
        config: &mut super::VideoConfig,
        source: (u32, u32),
        hdr_available: bool,
    ) -> anyhow::Result<()> {
        if config.color.is_hdr() && !hdr_available {
            let chroma = config.format.chroma;
            self.apply(config, None, chroma, false, source)?;
        }
        Ok(())
    }
}

pub(crate) fn color_extension(color: VideoColorSpace) -> Vec<u8> {
    let mut bytes = vec![
        color.primaries,
        color.transfer,
        color.matrix,
        color.range << 4,
    ];
    if let Some(metadata) = color.hdr_metadata {
        for value in [metadata.max_luminance, metadata.min_luminance]
            .into_iter()
            .chain(metadata.chromaticity)
            .chain([
                metadata.max_content_light_level,
                metadata.max_frame_average_light_level,
            ])
        {
            bytes.extend_from_slice(&value.to_be_bytes());
        }
    }
    bytes
}

#[cfg(test)]
mod tests {
    use super::*;
    use farwing_protocol::session;

    fn encoder(codec: Codec, chroma: u8, depth: u8) -> Capability {
        Capability {
            adapter: 1,
            backend: Backend::Nvidia,
            format: Format { codec, chroma, depth },
            maximum: (3840, 2160),
        }
    }
    fn decoder(codec: session::Codec, chroma444: bool, bit10: bool) -> session::Decoder {
        session::Decoder {
            codec,
            chroma444,
            bit10,
            width: 2560,
            height: 1440,
            hardware: true,
            av1_visible_size: true,
        }
    }

    #[test]
    fn formats_both_sides_take_set_the_choice() {
        let negotiated = Negotiated::new(
            &[
                encoder(Codec::H265, 1, 8),
                encoder(Codec::H265, 3, 8),
                encoder(Codec::H265, 1, 10),
                encoder(Codec::Av1, 1, 8),
            ],
            &[
                decoder(session::Codec::Hevc, false, false),
                decoder(session::Codec::Hevc, true, false),
                decoder(session::Codec::Av1, false, false),
            ],
            Default::default(),
            1,
        )
        .unwrap();
        let limits = negotiated.limits();
        assert!(limits.chroma444, "HEVC 4:4:4 on both sides");
        assert!(!limits.hdr, "no 10-bit decoder");
        assert_eq!(limits.max_quality(), crate::features::stream_control::wire::Quality::High);
        let mut config = super::super::VideoConfig {
            requested_fps: 60,
            fps_limit: 60,
            ..Default::default()
        };
        // 4:4:4 and HDR asked: 4:4:4 where possible, SDR without a 10-bit
        // decoder; the requested codec wins over the preference order.
        config.requested_codec = Some(Codec::H265);
        negotiated
            .apply_requested(&mut config, 3, true, (2560, 1440))
            .unwrap();
        assert_eq!(config.format, Format { codec: Codec::H265, chroma: 3, depth: 8 });
        assert!(!config.color.is_hdr());
        assert_eq!(config.maximum, (2560, 1440));
        // Without a request AV1 ranks first.
        config.requested_codec = None;
        negotiated
            .apply_requested(&mut config, 1, false, (2560, 1440))
            .unwrap();
        assert_eq!(config.format.codec, Codec::Av1);
    }
}
