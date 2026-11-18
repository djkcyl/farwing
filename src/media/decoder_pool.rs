//! UU fallback/configuration owner, using this project's platform decoders.
//! Official lifecycle: CF7D30/CF8240/CF87B0/CF8BC0, not a boolean HW->SW retry.

use bytes::Bytes;

use crate::media::VideoCodec;
use crate::media::decoder::{DecoderCandidate, NativeVideoDecoder};
use crate::media::decoder_result::VideoDecodeResult;
use crate::media::video_format::VideoFormatSignature;

struct Candidate {
    kind: DecoderCandidate,
    enabled: bool,
}

pub(crate) struct DecoderPool {
    decoder: Option<NativeVideoDecoder>,
    candidates: Vec<Candidate>,
    active: usize,
    codec: VideoCodec,
    format: Option<VideoFormatSignature>,
    width: u32,
    height: u32,
    frame_rate: u32,
    preference: crate::media::selection::DecoderPreference,
    extra_data: Bytes,
    error_count: u32,
    notification: crate::media::decode_api::DecoderNotification,
    callback_result: Option<VideoDecodeResult>,
    blocked_reason: Option<String>,

    software_slot: Option<std::rc::Rc<crate::media::decoder::software_slot::SoftwareSlot>>,

    writer: Option<crate::platform::surface::D3D11SurfaceWriter>,
}

pub(crate) struct DecoderTransition {
    /// A replaced/released decoder invalidates its inflight generation even if
    /// its replacement fails to initialize.
    pub replaced: bool,
    pub result: Option<VideoDecodeResult>,
}

impl DecoderPool {
    pub(crate) fn new(
        decoder: NativeVideoDecoder,
        codec: VideoCodec,
        width: u32,
        height: u32,
        frame_rate: u32,
        preference: crate::media::selection::DecoderPreference,
        extra_data: Bytes,
    ) -> Self {
        let candidates =
            DecoderCandidate::preferred(codec, preference, decoder.surface_writer().as_ref())
                .into_iter()
                .map(|kind| Candidate {
                    kind,
                    enabled: true,
                })
                .collect::<Vec<_>>();
        let active = candidates
            .iter()
            .position(|entry| entry.kind == decoder.candidate())
            .expect("opened decoder belongs to the configured candidate list");
        Self {
            software_slot: decoder.software_slot(),

            writer: decoder.surface_writer(),
            decoder: Some(decoder),
            candidates,
            active,
            codec,
            format: None,
            width,
            height,
            frame_rate,
            preference,
            extra_data,
            error_count: 0,
            notification: crate::media::decode_api::DecoderNotification::default(),
            callback_result: None,
            blocked_reason: None,
        }
    }

    pub(crate) fn label(&self) -> &str {
        self.decoder
            .as_ref()
            .map_or("等待可用解码器", NativeVideoDecoder::label)
    }

    pub(crate) fn decoder(&mut self) -> Option<&mut NativeVideoDecoder> {
        self.decoder.as_mut()
    }

    pub(crate) fn is_software(&self) -> bool {
        self.decoder
            .as_ref()
            .is_some_and(NativeVideoDecoder::is_software)
    }

    pub(crate) fn blocked_reason(&self) -> Option<&str> {
        self.blocked_reason.as_deref()
    }

    pub(crate) fn set_notification(
        &mut self,
        notification: crate::media::decode_api::DecoderNotification,
    ) {
        if let Some(decoder) = self.decoder.as_mut() {
            decoder.set_notification(notification.clone());
        }
        self.notification = notification;
    }

    pub(crate) fn callback_failed(&mut self, error: &anyhow::Error) {
        // Output errors change adapter state; the next Decode consumes that
        // state. They do not immediately recreate a decoder or send a PLI.
        self.callback_result = Some(
            if matches!(
                error.downcast_ref::<crate::media::decode_api::DecodeError>(),
                Some(
                    crate::media::decode_api::DecodeError::Unsupported
                        | crate::media::decode_api::DecodeError::HardwareFailure
                )
            ) {
                VideoDecodeResult::Fallback
            } else {
                VideoDecodeResult::RequestKeyframe
            },
        );
    }

    pub(crate) fn callback_result(&self) -> Option<VideoDecodeResult> {
        self.callback_result
    }

    pub(crate) fn prepare(
        &mut self,
        codec: VideoCodec,
        format: Option<VideoFormatSignature>,
        keyframe: bool,
        parameter_sets: Bytes,
    ) -> DecoderTransition {
        let codec_changed = codec != self.codec;
        // A new codec/configuration must never be seeded with the first stream's
        // VPS/SPS/PPS. Keep the latest keyframe parameters for backend replacement.
        if keyframe && (codec_changed || !parameter_sets.is_empty()) {
            self.extra_data = parameter_sets;
        }
        let format_changed = format.is_some_and(|next| {
            self.format
                .is_none_or(|current| !same_configuration(current, next))
        });
        if !codec_changed && !format_changed {
            // Cropping origin is decoder output metadata, not one of the six
            // Configure comparison fields in the UU wrapper.
            if format.is_some() {
                self.format = format;
            }
            return DecoderTransition {
                replaced: false,
                result: None,
            };
        }
        if !keyframe {
            return DecoderTransition {
                replaced: false,
                result: Some(VideoDecodeResult::RequestKeyframe),
            };
        }
        if codec_changed {
            self.codec = codec;
            self.candidates =
                DecoderCandidate::preferred(codec, self.preference, self.writer.as_ref())
                    .into_iter()
                    .map(|kind| Candidate {
                        kind,
                        enabled: true,
                    })
                    .collect();
            self.active = 0;
            self.format = None;
        }
        self.release();
        let width = format.map_or(self.width, |format| format.visible_width);
        let height = format.map_or(self.height, |format| format.visible_height);
        let initialized = self.select(width, height, format);
        if initialized {
            self.width = width;
            self.height = height;
            self.format = format;
            tracing::info!(
                ?codec,
                ?format,
                decoder = self.label(),
                "UU decoder configuration replaced"
            );
        }
        // Per-frame geometry reconfiguration retains candidate validity; only
        // external Configure or a new codec re-enables the full candidate set.
        DecoderTransition {
            replaced: true,
            result: (!initialized).then_some(VideoDecodeResult::Error),
        }
    }

    pub(crate) fn complete(
        &mut self,
        result: VideoDecodeResult,
        keyframe: bool,
        input_format: Option<VideoFormatSignature>,
    ) -> DecoderTransition {
        if result == VideoDecodeResult::Decoded {
            self.error_count = 0;
        } else if result.counts_towards_fallback(keyframe) {
            self.error_count = self.error_count.saturating_add(1);
        }
        if self.error_count < 5 && result != VideoDecodeResult::Fallback {
            return DecoderTransition {
                replaced: false,
                result: Some(result),
            };
        }
        let enabled_count = self.candidates.iter().filter(|entry| entry.enabled).count();
        let failed_index = self.active;
        let failed = self.candidates[self.active].kind;
        if enabled_count > 1 {
            self.candidates[self.active].enabled = false;
        }
        if let Some(format) = input_format {
            self.width = format.visible_width;
            self.height = format.visible_height;
            if let Some(current) = self.format.as_mut() {
                current.coded_width = format.coded_width;
                current.coded_height = format.coded_height;
                current.visible_width = format.visible_width;
                current.visible_height = format.visible_height;
            }
        }
        tracing::warn!(
            ?failed,
            ?result,
            errors = self.error_count,
            enabled_count,
            width = self.width,
            height = self.height,
            "UU decoder fallback transition"
        );
        self.release();
        let format = self.format.or(input_format);
        let mut initialized = self.select(self.width, self.height, format);
        if !initialized && enabled_count > 1 {
            // Other adapters may be alive but lack this stream's format. Only
            // after exhausting alternatives, recover the last usable candidate.
            self.candidates[failed_index].enabled = true;
            initialized = self.select_candidates(
                std::iter::once(failed_index),
                self.width,
                self.height,
                format,
            );
        }
        DecoderTransition {
            replaced: true,
            // CF87B0 initializes immediately but does not retry the failed frame.
            result: Some(if initialized {
                VideoDecodeResult::RequestKeyframe
            } else {
                VideoDecodeResult::Error
            }),
        }
    }

    fn release(&mut self) {
        self.decoder.take();
        self.error_count = 0;
        self.callback_result = None;
    }

    fn select(&mut self, width: u32, height: u32, format: Option<VideoFormatSignature>) -> bool {
        self.select_candidates(0..self.candidates.len(), width, height, format)
    }

    fn select_candidates(
        &mut self,
        indices: impl IntoIterator<Item = usize>,
        width: u32,
        height: u32,
        format: Option<VideoFormatSignature>,
    ) -> bool {
        self.error_count = 0;
        self.blocked_reason = None;
        for index in indices {
            let entry = &self.candidates[index];
            if !entry.enabled {
                continue;
            }
            // Match actual stream parameters before choosing a backend. In
            // particular H264 4:4:4 belongs to the software candidate, rather
            // than losing its first keyframe to a predictable hardware error.

            if let Some(format) = format {
                let supported = match entry.kind {
                    #[cfg(windows)]
                    DecoderCandidate::WindowsD3d11 { .. } => {
                        (format.chroma_format_idc == 1
                            || matches!(self.codec, VideoCodec::H265 | VideoCodec::Av1)
                                && format.chroma_format_idc == 3)
                            && format.bit_depth_luma == format.bit_depth_chroma
                            && (format.bit_depth_luma == 8
                                || matches!(self.codec, VideoCodec::H265 | VideoCodec::Av1)
                                    && format.bit_depth_luma == 10)
                    }
                    // The VA-API backend reads surfaces back as 8-bit NV12.
                    #[cfg(not(windows))]
                    DecoderCandidate::LinuxVaapi => {
                        format.chroma_format_idc == 1
                            && format.bit_depth_luma == 8
                            && format.bit_depth_chroma == 8
                    }
                    DecoderCandidate::Software => {
                        openuuyc_codec::Format {
                            codec: self.codec,
                            chroma: format.chroma_format_idc,
                            depth: format.bit_depth_luma,
                        }
                        .can_decode()
                            && format.bit_depth_luma == format.bit_depth_chroma
                    }
                };
                if !supported {
                    continue;
                }
            }

            match NativeVideoDecoder::open_candidate(
                entry.kind,
                self.codec,
                width,
                height,
                self.frame_rate,
                self.extra_data.clone(),
                self.writer.clone(),
                self.software_slot.clone(),
            ) {
                Ok(mut decoder) => {
                    decoder.describe_selection(
                        self.preference,
                        self.candidates.iter().any(|c| !c.enabled),
                    );
                    {
                        self.software_slot = decoder.software_slot();
                    }
                    decoder.set_notification(self.notification.clone());

                    if let Some(writer) = decoder.surface_writer() {
                        self.writer = Some(writer);
                    }
                    self.active = index;
                    self.decoder = Some(decoder);
                    return true;
                }
                Err(error) => {
                    if error.is::<crate::media::decoder::software_slot::SoftwarePlaybackBusy>() {
                        self.blocked_reason = Some(error.to_string());
                    }
                    tracing::warn!(candidate = ?entry.kind, %error,
                        width, height, "decoder candidate rejected current configuration");
                }
            }
        }

        {
            self.software_slot = None;
        }
        false
    }
}

fn same_configuration(a: VideoFormatSignature, b: VideoFormatSignature) -> bool {
    a.chroma_format_idc == b.chroma_format_idc
        && a.bit_depth_luma == b.bit_depth_luma
        && a.coded_width == b.coded_width
        && a.coded_height == b.coded_height
        && a.visible_width == b.visible_width
        && a.visible_height == b.visible_height
}
