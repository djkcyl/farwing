//! Publisher negotiation and format selection policy.
pub(crate) use crate::media::encoding::{
    Backend, Capability, Codec, Color, Format, QualityTarget, Rate,
};
use crate::media::video_color::VideoColorSpace;
use std::sync::atomic::{AtomicU8, AtomicU64, Ordering};

#[derive(Clone, Debug)]
pub(crate) struct Choice {
    pub capability: Capability,
    pub maximum: (u32, u32),
    format_maximum: (u32, u32),
    pub fps: u32,
    hardware_decode: bool,
}
impl Choice {
    pub fn maximum_for(&self, requested: Option<(u32, u32)>) -> (u32, u32) {
        // S543710: a complete RPC pair replaces the initial PB decoder size.
        // It cannot enlarge the real encoder/JSON format capability.
        requested.map_or(self.maximum, |size| {
            (
                size.0.min(self.format_maximum.0),
                size.1.min(self.format_maximum.1),
            )
        })
    }
}
#[derive(Debug)]
pub(crate) struct Negotiated {
    pub choices: Vec<Choice>,
    dual: crate::protocol::capability::DualCapability,
    codecs: AtomicU8,
    encoder_true_color: bool,
    sdr_10bit: bool,
    /// The receiver crops the padding of an aligned-only AV1 encoder.
    pub av1_visible_size: bool,
    pub preferred_gpu: Option<u64>,
    pub gpu_compatibility: bool,
    source_gpu: AtomicU64,
    requested_gpu: Option<crate::media::selection::GpuId>,
}
impl Negotiated {
    pub(crate) fn deferred() -> Self {
        Self {
            choices: Vec::new(),
            dual: crate::protocol::capability::DualCapability::negotiate(
                &Default::default(),
                Default::default(),
            ),
            codecs: AtomicU8::new(15),
            encoder_true_color: false,
            sdr_10bit: false,
            av1_visible_size: false,
            preferred_gpu: None,
            gpu_compatibility: false,
            source_gpu: AtomicU64::new(0),
            requested_gpu: None,
        }
    }
    pub(crate) fn for_track(&self) -> Self {
        Self {
            choices: self.choices.clone(),
            dual: self.dual.clone(),
            codecs: AtomicU8::new(15),
            encoder_true_color: self.encoder_true_color,
            sdr_10bit: self.sdr_10bit,
            av1_visible_size: self.av1_visible_size,
            preferred_gpu: self.preferred_gpu,
            gpu_compatibility: self.gpu_compatibility,
            source_gpu: AtomicU64::new(self.source_gpu.load(Ordering::Relaxed)),
            requested_gpu: self.requested_gpu,
        }
    }
    pub fn new(
        local: &[Capability],
        remote: &crate::protocol::capability::DeviceCapability,
        decoders: &[crate::features::stream_control::publisher::DecoderCapability],
        settings: super::EncodingSettings,
        source_gpu: u64,
    ) -> anyhow::Result<Self> {
        let requested_gpu = (settings.mode != super::EncoderMode::Software)
            .then_some(settings.gpu)
            .flatten();
        let local_wire = crate::protocol::capability::DeviceCapability {
            video_codec_capability: local.iter().map(Capability::wire).collect(),
            ..Default::default()
        };
        let mut dual =
            crate::protocol::capability::DualCapability::negotiate(&local_wire, remote.clone());
        dual.preferred_codec = remote
            .preferred_codec
            .filter(|c| matches!(c, 1 | 2 | 5))
            .or(settings.preferred_codec.preferred());
        let mut choices = Vec::new();
        for capability in local {
            let Some(row) = dual
                .exact(
                    capability.format.codec.wire(),
                    capability.format.chroma,
                    capability.format.depth >= 10,
                )
                .filter(|r| r.result == 0)
            else {
                continue;
            };
            for peer in &remote.video_codec_capability {
                if peer.video_codec != capability.format.codec.wire()
                    || peer.chroma_sampling != capability.format.chroma
                    || peer.bit_depth != capability.format.depth
                    || peer.width < 2
                    || peer.height < 2
                {
                    continue;
                }
                for decoder in decoders {
                    if decoder.codec != peer.video_codec
                        || decoder.chroma_sampling() != peer.chroma_sampling
                        || decoder.width < 2
                        || decoder.height < 2
                    {
                        continue;
                    }
                    choices.push(Choice {
                        hardware_decode: peer.codec_impl != 37,
                        capability: capability.clone(),
                        format_maximum: (
                            capability
                                .maximum
                                .0
                                .min(peer.width as u32)
                                .min(row.max_width as u32),
                            capability
                                .maximum
                                .1
                                .min(peer.height as u32)
                                .min(row.max_height as u32),
                        ),
                        maximum: (
                            capability
                                .maximum
                                .0
                                .min(peer.width as u32)
                                .min(decoder.width as u32)
                                .min(row.max_width as u32),
                            capability
                                .maximum
                                .1
                                .min(peer.height as u32)
                                .min(decoder.height as u32)
                                .min(row.max_height as u32),
                        ),
                        fps: decoder.maximum_fps().min(
                            if capability.backend == Backend::Software {
                                openuuyc_codec::encoder::maximum_fps(
                                    capability.format.codec.media(),
                                )
                            } else {
                                144
                            },
                        ),
                    });
                }
            }
        }
        if choices.is_empty() {
            tracing::warn!(local=?local_wire.video_codec_capability,remote=?remote.video_codec_capability,
                decoders=?decoders,intersection=?dual.frame_quality_capability,"host media capability intersection empty");
            anyhow::bail!("没有共同的画面编码/解码能力");
        }
        Ok(Self {
            choices,
            dual,
            codecs: AtomicU8::new(15),
            // S543170 computes this from local encoder caps before intersection.
            encoder_true_color: local.iter().any(|c| c.format.chroma == 3),
            sdr_10bit: remote.openuuyc_sdr_10bit,
            av1_visible_size: remote.openuuyc_av1_visible_size,
            preferred_gpu: crate::media::selection::resolve_gpu(requested_gpu),
            gpu_compatibility: settings.gpu_compatibility,
            source_gpu: AtomicU64::new(source_gpu),
            requested_gpu,
        })
    }
    pub fn encoder_true_color(&self) -> bool {
        self.encoder_true_color
    }
    pub fn source_gpu(&self, adapter: u64) {
        self.source_gpu.store(adapter, Ordering::Relaxed);
    }
    pub fn preference(&self) -> (Option<crate::media::selection::GpuId>, Option<i32>) {
        (self.requested_gpu, self.dual.preferred_codec)
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
            .dual
            .preferred_codec
            .is_some_and(|codec| codec != capability.format.codec.wire())
        {
            R::CodecFormat
        } else if self.preferred_gpu.is_some() || self.dual.preferred_codec.is_some() {
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
    pub fn maximum_quality(&self, format: Format, source: (u32, u32), maximum: (u32, u32)) -> i32 {
        let json_limit = self
            .dual
            .exact(format.codec.wire(), format.chroma, format.depth >= 10)
            .filter(|r| r.result == 0)
            .map_or(1, |r| r.max_frame_quality);
        (1..=json_limit)
            .filter(|&q| {
                let tier = super::parameters::dimensions(q);
                source.0.min(tier.0) <= maximum.0 && source.1.min(tier.1) <= maximum.1
            })
            .max()
            .unwrap_or(1)
    }
    pub fn apply(
        &self,
        config: &mut super::VideoConfig,
        codec: Option<Codec>,
        chroma: u8,
        hdr: bool,
        source: (u32, u32),
    ) -> anyhow::Result<()> {
        let mut dual = self.dual.clone();
        dual.frame_quality_capability.retain(|row| {
            Codec::from_wire(row.video_codec).is_some_and(|c| {
                self.permits_format(Format {
                    codec: c,
                    chroma: row.chroma_sampling,
                    depth: row.bit_depth,
                }) && (self.dual.preferred_codec.is_some()
                    || codec.is_none_or(|wanted| wanted == c))
            }) && self.choices.iter().any(|choice| {
                choice.capability.format.codec.wire() == row.video_codec
                    && choice.capability.format.chroma == row.chroma_sampling
                    && choice.capability.format.depth == row.bit_depth
                    && choice.fps >= 30
            })
        });
        let colors = if hdr {
            vec![Color::Hdr, Color::Sdr]
        } else {
            vec![Color::Sdr]
        };
        let sampling = if chroma == 3 {
            vec![3, 1]
        } else {
            vec![chroma]
        };
        let desired = if matches!(config.quality, 5 | 6) {
            source
        } else {
            crate::media::geometry::output_size(source.0, source.1, config.quality)
        };
        let mut selected = None;
        'color: for color in colors {
            for &chroma in &sampling {
                let mut fps = config.requested_fps;
                while fps >= 30 {
                    let mut eligible = dual.clone();
                    eligible.frame_quality_capability.retain(|row| {
                        self.choices.iter().any(|c| {
                            c.fps >= fps
                                && c.capability.format.codec.wire() == row.video_codec
                                && c.capability.format.chroma == row.chroma_sampling
                                && c.capability.format.depth == row.bit_depth
                        })
                    });
                    let mut best = None;
                    for depth in [10, 8] {
                        if !color.is_hdr() && depth == 10 && !self.sdr_10bit {
                            continue;
                        }
                        if color.is_hdr() && depth != 10 {
                            continue;
                        }
                        for choice in self.choices.iter().filter(|c| {
                            c.fps >= fps
                                && c.capability.format.chroma == chroma
                                && c.capability.format.depth == depth
                        }) {
                            let Some(row) = eligible
                                .exact(choice.capability.format.codec.wire(), chroma, depth == 10)
                                .filter(|r| r.result == 0)
                            else {
                                continue;
                            };
                            let maximum = choice.maximum_for(config.requested_maximum);
                            let limit = (
                                maximum.0.min(row.max_width as u32),
                                maximum.1.min(row.max_height as u32),
                            );
                            let size =
                                crate::media::geometry::fit_size(desired.0, desired.1, limit);
                            let score = crate::media::selection::candidate_rank(
                                size,
                                choice.capability.backend != Backend::Software
                                    && choice.hardware_decode,
                                Some(row.video_codec) == self.dual.preferred_codec,
                                fps,
                                choice.capability.adapter,
                                self.preferred_gpu,
                                self.source_gpu.load(Ordering::Relaxed),
                                row.video_codec,
                                depth,
                            );
                            if best.as_ref().is_none_or(|(old, _, _)| score > *old) {
                                best = Some((score, choice, limit));
                            }
                        }
                    }
                    if let Some((_, choice, maximum)) = best {
                        selected = Some((choice, fps, maximum, color));
                        break 'color;
                    }
                    fps = match fps {
                        144.. => 90,
                        90..=143 => 60,
                        60..=89 => 30,
                        _ => 0,
                    };
                }
            }
        }
        let (choice, fps, row_maximum, color) =
            selected.ok_or_else(|| anyhow::anyhow!("请求的画面格式没有共同能力"))?;
        config.color = color;
        config.format = choice.capability.format;
        let maximum = choice.maximum_for(config.requested_maximum);
        config.maximum = (maximum.0.min(row_maximum.0), maximum.1.min(row_maximum.1));
        config.maximum_fps = fps;
        config.fps = fps.min(config.fps_limit);
        config.maximum_quality = self.maximum_quality(config.format, source, config.maximum);
        config.auto_quality = config.auto_quality.min(config.maximum_quality);
        if !matches!(config.quality, 5 | 6) {
            config.quality = config.quality.min(config.maximum_quality);
        }
        Ok(())
    }
    pub fn candidate(
        &self,
        wanted: &super::VideoConfig,
        source: (u32, u32),
        source_gpu: u64,
        failed: &std::collections::HashSet<(u64, Backend, Format)>,
    ) -> Option<&Choice> {
        self.choices
            .iter()
            .filter(|c| {
                self.permits_format(c.capability.format)
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
                let maximum = c.maximum_for(wanted.requested_maximum);
                let desired =
                    crate::media::geometry::output_size(source.0, source.1, wanted.quality);
                let fitted = crate::media::geometry::fit_size(desired.0, desired.1, maximum);
                crate::media::selection::candidate_rank(
                    fitted,
                    c.capability.backend != Backend::Software && c.hardware_decode,
                    c.capability.format == wanted.format,
                    c.fps.min(wanted.maximum_fps),
                    c.capability.adapter,
                    self.preferred_gpu,
                    source_gpu,
                    c.capability.format.codec.wire(),
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
