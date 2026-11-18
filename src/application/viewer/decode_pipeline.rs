//! Decoder generation, format changes, completion feedback and frame delivery.
use super::{
    DecodedVideoFrame, FrameQueue, FrameWake, OFFICIAL_DECODER_INFLIGHT_LIMIT, mutex_lock,
};
use crate::diagnostics::performance::PerformanceMonitor;
use crate::media::VideoCodec;
use crate::media::decoder::{DecodedBatch, DecodedFrame, DecoderOutputIssue, NativeVideoDecoder};
use crate::media::decoder_pool::DecoderPool;
use crate::media::decoder_result::VideoDecodeResult;
use crate::media::video_color::RenderColor;
use crate::media::video_format::{VideoFormatSignature, parse_stream_format};
use crate::transport::rtc::{EncodedVideoFrame, FrameSenderTiming, VideoReceiverFeedback};
use anyhow::Result;
use bytes::Bytes;
use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;
use tokio::sync::{mpsc, oneshot};

pub(super) struct FrameTiming {
    pub(super) is_new_picture: Option<bool>,
    pub(super) color: RenderColor,
    pub(super) rtp_timestamp: u32,
    pub(super) received_at: Instant,
    pub(super) assembled_at: Instant,
    pub(super) submitted_at: Instant,
    pub(super) rotation: u16,
    pub(super) keyframe: bool,
    pub(super) sender_timing: FrameSenderTiming,
}

pub(super) struct DecodedForwardContext<'a> {
    pub(super) frame_queue: &'a Mutex<Option<DecodedVideoFrame>>,
    pub(super) frame_wake: &'a FrameWake,
    pub(super) performance: &'a PerformanceMonitor,
    pub(super) receiver_feedback: &'a mpsc::UnboundedSender<VideoReceiverFeedback>,
    pub(super) inflight: &'a mut HashMap<i64, u32>,
}

#[derive(Clone, Copy, Debug)]
pub(super) struct DecoderCutoverDecision {
    pub(super) drop_frame: bool,
    pub(super) request_keyframe: bool,
    pub(super) reset_decoder: bool,
    pub(super) hard_reset: bool,
    pub(super) source_changed: bool,
    pub(super) content_changed: bool,
    pub(super) resolution_changed: bool,
    pub(super) pressure_recovery: bool,
}

pub(super) struct DecoderCutoverState {
    pub(super) waiting_for_keyframe: bool,
    pub(super) pressure_recovery: bool,
    pub(super) current_source_id: Option<u16>,
    pub(super) current_content_type: u8,
    pub(super) current_width: u32,
    pub(super) current_height: u32,
    pub(super) generation: u32,
}

impl DecoderCutoverState {
    pub(super) const fn new() -> Self {
        Self {
            waiting_for_keyframe: false,
            pressure_recovery: false,
            current_source_id: None,
            current_content_type: 0,
            current_width: 0,
            current_height: 0,
            generation: 0,
        }
    }

    pub(super) fn evaluate(
        &mut self,
        frame: &EncodedVideoFrame,
        format: Option<VideoFormatSignature>,
    ) -> DecoderCutoverDecision {
        if self.pressure_recovery && !frame.keyframe {
            return DecoderCutoverDecision {
                drop_frame: true,
                request_keyframe: false,
                reset_decoder: false,
                hard_reset: false,
                source_changed: false,
                content_changed: false,
                resolution_changed: false,
                pressure_recovery: true,
            };
        }

        let source_changed = frame
            .video_capture_index
            .zip(self.current_source_id)
            .is_some_and(|(source, previous)| source != previous);
        let content_changed =
            frame.content_type != 0 && frame.content_type != self.current_content_type;
        let (next_width, next_height) =
            format.map_or((0, 0), |format| (format.coded_width, format.coded_height));
        let resolution_changed = frame.keyframe
            && next_width != 0
            && next_height != 0
            && self.current_width != 0
            && self.current_height != 0
            && (next_width != self.current_width || next_height != self.current_height);
        let cutover = frame.keyframe || source_changed || content_changed;

        if self.waiting_for_keyframe {
            if !frame.keyframe {
                return DecoderCutoverDecision {
                    drop_frame: true,
                    request_keyframe: false,
                    reset_decoder: false,
                    hard_reset: false,
                    source_changed,
                    content_changed,
                    resolution_changed,
                    pressure_recovery: self.pressure_recovery,
                };
            }
            self.waiting_for_keyframe = false;
        } else if !frame.keyframe && cutover {
            self.waiting_for_keyframe = true;
            return DecoderCutoverDecision {
                drop_frame: true,
                request_keyframe: true,
                reset_decoder: false,
                hard_reset: false,
                source_changed,
                content_changed,
                resolution_changed,
                pressure_recovery: self.pressure_recovery,
            };
        }

        let pressure_recovery = self.pressure_recovery;
        let hard_reset = pressure_recovery
            || source_changed
            || (self.current_content_type != 0 && content_changed)
            || resolution_changed;
        let reset_decoder = frame.keyframe && (self.pressure_recovery || cutover);
        if reset_decoder {
            self.generation = self.generation.wrapping_add(1);
            self.pressure_recovery = false;
        }
        if content_changed {
            self.current_content_type = frame.content_type;
        }
        if let Some(source) = frame.video_capture_index {
            self.current_source_id = Some(source);
        }
        if frame.keyframe && next_width != 0 && next_height != 0 {
            self.current_width = next_width;
            self.current_height = next_height;
        }

        DecoderCutoverDecision {
            drop_frame: false,
            request_keyframe: false,
            reset_decoder,
            hard_reset,
            source_changed,
            content_changed,
            resolution_changed,
            pressure_recovery,
        }
    }

    pub(super) fn replace_instance(&mut self) {
        let generation = self.generation.wrapping_add(1);
        *self = Self::new();
        self.generation = generation;
    }

    pub(super) fn note_inflight_pressure(&mut self, inflight: usize, keyframe: bool) -> bool {
        if keyframe || self.pressure_recovery || inflight <= OFFICIAL_DECODER_INFLIGHT_LIMIT {
            return false;
        }
        self.pressure_recovery = true;
        true
    }

    pub(super) const fn token(&self, frame_index: u32) -> i64 {
        ((self.generation as u64) << 32 | frame_index as u64) as i64
    }
}

pub(super) struct DecodeActivity {
    pub(super) software: Arc<AtomicBool>,
    pub(super) enabled: Arc<AtomicBool>,
    pub(super) pause_epoch: Arc<std::sync::atomic::AtomicU64>,
    pub(super) idle: tokio::sync::watch::Receiver<u64>,
}

#[derive(Clone)]
pub(super) struct DecoderConfig {
    pub(super) codec: VideoCodec,
    pub(super) width: u32,
    pub(super) height: u32,
    pub(super) frame_rate: u32,
    pub(super) decoder: crate::media::selection::DecoderPreference,
    pub(super) software_decode: Arc<AtomicBool>,
    pub(super) decode_enabled: Arc<AtomicBool>,
    pub(super) decode_idle: tokio::sync::watch::Sender<u64>,
    pub(super) pause_epoch: Arc<std::sync::atomic::AtomicU64>,

    pub(super) surface_writer: Option<crate::platform::surface::D3D11SurfaceWriter>,
}

#[allow(clippy::too_many_arguments)]
pub(super) fn decoder_manager(
    config: DecoderConfig,
    mut video_source: mpsc::UnboundedReceiver<EncodedVideoFrame>,
    frame_queue: FrameQueue,
    frame_wake: FrameWake,
    performance: PerformanceMonitor,
    shutdown: Arc<AtomicBool>,
    fatal_error: Arc<Mutex<Option<String>>>,
    receiver_feedback: mpsc::UnboundedSender<VideoReceiverFeedback>,
    mut startup_sender: Option<oneshot::Sender<Result<(), String>>>,
) {
    if shutdown.load(Ordering::Acquire) {
        return;
    }
    tracing::debug!(codec = ?config.codec, decoder = ?config.decoder,
        "native decoder initialization deferred until the first frame's parameter sets");
    let mut active_codec = config.codec;
    // Only log the format when it actually changes, not once per frame.
    let mut last_format_label: Option<String> = None;
    // Open against real SPS/PPS/VPS and coded dimensions. The local display
    // is only a startup hint, not the compressed stream's allocation geometry.
    let mut pool: Option<DecoderPool> = None;
    let first_open_attempt = std::time::Instant::now();
    let max_open_wait = std::time::Duration::from_secs(3);
    let notification = crate::media::decode_api::DecoderNotification::new(Arc::clone(&shutdown));
    let mut timings = VecDeque::<FrameTiming>::new();
    let mut inflight = HashMap::<i64, u32>::new();
    let mut cutover_state = DecoderCutoverState::new();
    let mut next_decoder_frame_index = 0_u32;

    'decode: while !shutdown.load(Ordering::Acquire) {
        if !config.decode_enabled.load(Ordering::Acquire) {
            let epoch = config.pause_epoch.load(Ordering::Acquire);
            let acknowledged = *config.decode_idle.borrow();
            if acknowledged != epoch {
                pool.take(); // Close the CPU decoder before relinquishing its global slot.
                timings.clear();
                inflight.clear();
                cutover_state.replace_instance();
                mutex_lock(&frame_queue).take();
                config.decode_idle.send_replace(epoch);
            }
            match video_source.try_recv() {
                Ok(frame) => {
                    frame.completion.complete(VideoDecodeResult::Decoded);
                }
                Err(mpsc::error::TryRecvError::Empty) => std::thread::park(),
                Err(mpsc::error::TryRecvError::Disconnected) => break 'decode,
            }
            continue 'decode;
        }
        // Keep decoding reference pictures even when presentation is waiting
        // for the display. Only the latest ready picture retains a display
        // lease, so a slow renderer cannot fill the pool or stall admissions.
        if let Some(current) = pool.as_mut() {
            let output = current
                .decoder()
                .map_or_else(DecodedBatch::default, NativeVideoDecoder::poll);
            process_decoded_batch(
                output,
                current,
                &mut timings,
                DecodedForwardContext {
                    frame_queue: &frame_queue,
                    frame_wake: &frame_wake,
                    performance: &performance,
                    receiver_feedback: &receiver_feedback,
                    inflight: &mut inflight,
                },
            );
        }
        if shutdown.load(Ordering::Acquire) {
            break;
        }
        performance.set_decoder_queue_frames(video_source.len());
        let frame = match video_source.try_recv() {
            Ok(frame) => frame,
            Err(mpsc::error::TryRecvError::Empty) => {
                std::thread::park();
                // A backend output wake is useful without a new RTP packet.
                continue 'decode;
            }
            Err(mpsc::error::TryRecvError::Disconnected) => break 'decode,
        };
        if shutdown.load(Ordering::Acquire) {
            break;
        }
        let completion = frame.completion.clone();
        let submitted_at = Instant::now();
        let frame_id = frame.frame_id;
        let keyframe = frame.keyframe;
        let timestamp = frame.rtp_timestamp;
        let format = frame
            .parameter_format
            .or_else(|| parse_stream_format(frame.codec, &frame.data));
        if pool.is_none() {
            if startup_sender.is_none() && !keyframe {
                completion.complete(VideoDecodeResult::RequestKeyframe);
                continue 'decode;
            }
            let extra = extract_parameter_sets(frame.codec, &frame.data);
            let pool_extra = extra.clone();
            if extra.is_none() && first_open_attempt.elapsed() < max_open_wait {
                tracing::debug!(codec = ?frame.codec, elapsed_ms = first_open_attempt.elapsed().as_millis(),
                    "first frame has no parameter sets yet; deferring decoder open");
                continue 'decode;
            }
            let (width, height) = stream_geometry(&config, format);
            let opened = open_decoder_with_metadata(
                &config,
                frame.codec,
                width,
                height,
                extra,
                config.surface_writer.clone(),
            );
            match opened {
                Ok(decoder) => {
                    config
                        .software_decode
                        .store(decoder.is_software(), Ordering::Release);
                    tracing::info!(
                        decoder = decoder.label(),
                        codec = ?frame.codec,
                        width,
                        height,
                        frame_rate = config.frame_rate,
                        "native in-process decoder opened on the first frame"
                    );
                    performance.set_decoder(decoder.label());
                    if let Some(sender) = startup_sender.take() {
                        let _ = sender.send(Ok(()));
                    }
                    let opened_pool = DecoderPool::new(
                        decoder,
                        frame.codec,
                        width,
                        height,
                        config.frame_rate,
                        config.decoder,
                        pool_extra.unwrap_or_default(),
                    );
                    pool = Some(opened_pool);
                }
                Err(error) => {
                    let message = format!("{error:#}");
                    tracing::error!(codec = ?frame.codec, width, height,
                        "native decoder open failed after the first frame arrived");
                    *mutex_lock(&fatal_error) = Some(message.clone());
                    if let Some(sender) = startup_sender.take() {
                        let _ = sender.send(Err(message));
                    }
                    return;
                }
            }
        }
        let pool = pool.as_mut().expect("decoder opened before admission");
        pool.set_notification(notification.clone());
        let render_color = frame
            .color_space
            .map(|color| color.rendering())
            .unwrap_or_default();
        // Bit depth controls sample precision, not the transfer function.
        // Preserve explicit SDR/PQ metadata through to the renderer.
        timings.push_back(FrameTiming {
            is_new_picture: frame.is_new_picture,
            color: render_color,
            rtp_timestamp: timestamp,
            received_at: frame.received_at,
            assembled_at: frame.assembled_at,
            submitted_at,
            rotation: frame.rotation,
            keyframe,
            sender_timing: frame.sender_timing,
        });

        let parameters = if keyframe {
            extract_parameter_sets(frame.codec, &frame.data).unwrap_or_default()
        } else {
            Bytes::new()
        };
        let prepared = pool.prepare(frame.codec, format, keyframe, parameters);
        config
            .software_decode
            .store(pool.is_software(), Ordering::Release);
        if let Some(reason) = pool.blocked_reason() {
            *mutex_lock(&fatal_error) = Some(reason.to_owned());
            break 'decode;
        }
        if prepared.replaced {
            cutover_state.replace_instance();
            inflight.clear();
            performance.set_decoder(pool.label());
        }
        if let Some(result) = prepared.result {
            if !result.accepted() {
                timings.clear();
            }
            completion.complete(result);
            continue;
        }
        if let Some(callback_result) = pool.callback_result() {
            let transition = pool.complete(callback_result, keyframe, format);
            config
                .software_decode
                .store(pool.is_software(), Ordering::Release);
            if let Some(reason) = pool.blocked_reason() {
                *mutex_lock(&fatal_error) = Some(reason.to_owned());
                break 'decode;
            }
            if transition.replaced {
                cutover_state.replace_instance();
                inflight.clear();
                performance.set_decoder(pool.label());
            }
            let result = transition
                .result
                .expect("callback state has a Decode result");
            if !result.accepted() {
                timings.clear();
            }
            completion.complete(result);
            continue;
        }
        if frame.codec != active_codec {
            active_codec = frame.codec;
            last_format_label = None;
            performance.set_video_codec(match active_codec {
                VideoCodec::H264 => "H.264/AVC",
                VideoCodec::H265 => "H.265/HEVC",
                VideoCodec::Av1 => "AV1",
            });
        }
        if let Some(format) = format {
            let label = video_format_label(active_codec, format);
            if last_format_label.as_deref() != Some(label.as_str()) {
                tracing::info!(format = %label, "video format in use");
                last_format_label = Some(label.clone());
            }
            performance.set_video_format(label);
        }

        let cutover = cutover_state.evaluate(&frame, format);
        let mut result = if cutover.drop_frame {
            if cutover.request_keyframe {
                VideoDecodeResult::RequestKeyframe
            } else {
                VideoDecodeResult::Decoded
            }
        } else {
            VideoDecodeResult::Decoded
        };
        if cutover.reset_decoder {
            tracing::debug!(
                hard_reset = cutover.hard_reset,
                source_changed = cutover.source_changed,
                content_changed = cutover.content_changed,
                resolution_changed = cutover.resolution_changed,
                pressure_recovery = cutover.pressure_recovery,
                "UU decoder keyframe cutover"
            );
            inflight.clear();
            // The adapter advances generation before flush. Generic RTP timing
            // records remain until output retires the prefix, or Decode fails.
            let reset = pool.decoder().map_or_else(
                || Err(crate::media::decode_api::DecodeError::NoBackend.into()),
                |decoder| decoder.reset_for_keyframe(),
            );
            if let Err(error) = reset {
                tracing::warn!(%error, hard_reset = cutover.hard_reset, "decoder cutover reset failed");
                result = VideoDecodeResult::Fallback;
            }
        }

        if !cutover.drop_frame && result == VideoDecodeResult::Decoded {
            let decode_token = cutover_state.token(next_decoder_frame_index);
            next_decoder_frame_index = next_decoder_frame_index.wrapping_add(1);
            inflight.insert(decode_token, timestamp);
            tracing::trace!(frame_id, rtp_timestamp = timestamp, decode_token,
                codec = ?frame.codec, bytes = frame.data.len(), "submitting admitted video frame");
            let decoded = pool.decoder().map_or_else(
                || DecodedBatch {
                    input_error: Some(crate::media::decode_api::DecodeError::NoBackend.into()),
                    ..Default::default()
                },
                |decoder| decoder.push(frame, decode_token),
            );
            if shutdown.load(Ordering::Acquire) {
                break;
            }
            let input_error = process_decoded_batch(
                decoded,
                pool,
                &mut timings,
                DecodedForwardContext {
                    frame_queue: &frame_queue,
                    frame_wake: &frame_wake,
                    performance: &performance,
                    receiver_feedback: &receiver_feedback,
                    inflight: &mut inflight,
                },
            );
            if let Some(error) = input_error {
                inflight.remove(&decode_token);
                result = VideoDecodeResult::from_error(&error);
                tracing::warn!(%error, ?result, frame_id, keyframe, "native decoder input failed");
            } else if inflight.contains_key(&decode_token)
                && cutover_state.note_inflight_pressure(inflight.len(), keyframe)
            {
                result = VideoDecodeResult::RequestKeyframe;
                tracing::warn!(
                    inflight = inflight.len(),
                    "UU decoder inflight pressure requests keyframe"
                );
            }
        }
        if shutdown.load(Ordering::Acquire) {
            break;
        }
        let transition = pool.complete(result, keyframe, format);
        config
            .software_decode
            .store(pool.is_software(), Ordering::Release);
        if let Some(reason) = pool.blocked_reason() {
            *mutex_lock(&fatal_error) = Some(reason.to_owned());
            break 'decode;
        }
        if transition.replaced {
            cutover_state.replace_instance();
            inflight.clear();
            performance.set_decoder(pool.label());
        }
        let result = transition
            .result
            .expect("Decode completion always has a result");
        if !result.accepted() {
            timings.clear();
        }
        completion.complete(result);
    }
    performance.set_decoder_queue_frames(0);
}

/// Extract Annex-B parameter sets (H.264 SPS/PPS, H.265 VPS/SPS/PPS) from an
/// assembled frame, preserving start codes. The first complete keyframe
/// supplies these independently of the selected decoder backend.
pub(super) fn extract_parameter_sets(codec: VideoCodec, data: &[u8]) -> Option<Bytes> {
    if codec == VideoCodec::Av1 {
        return crate::media::av1::parameters(data);
    }
    fn nal_start(data: &[u8], at: usize) -> bool {
        at + 3 <= data.len() && data[at] == 0 && data[at + 1] == 0 && data[at + 2] == 1
            || at + 4 <= data.len()
                && data[at] == 0
                && data[at + 1] == 0
                && data[at + 2] == 0
                && data[at + 3] == 1
    }
    let mut out = Vec::new();
    let mut cursor = 0usize;
    let mut has_sps = false;
    let mut has_pps = false;
    while cursor + 3 <= data.len() {
        if !nal_start(data, cursor) {
            cursor += 1;
            continue;
        }
        let payload = cursor + if data[cursor + 2] == 1 { 3 } else { 4 };
        let mut end = payload;
        while end < data.len() && !nal_start(data, end) {
            end += 1;
        }
        let nal = &data[payload..end];
        if let Some(&header) = nal.first() {
            let keep = match codec {
                VideoCodec::H264 => matches!(header & 0x1f, 7 | 8),
                VideoCodec::H265 => matches!((header >> 1) & 0x3f, 32..=34),
                VideoCodec::Av1 => false,
            };
            if keep {
                out.extend_from_slice(&data[cursor..end]);
                if codec == VideoCodec::H264 {
                    match header & 0x1f {
                        7 => has_sps = true,
                        8 => has_pps = true,
                        _ => {}
                    }
                }
            }
        }
        cursor = end;
    }
    // H.264 需要 SPS 与 PPS 都齐备才算完整参数集：部分 host（macOS）
    // 用单独的单 NAL 包分别下发二者，只拿到一半时打开解码器会失败。
    if codec == VideoCodec::H264 && !(has_sps && has_pps) {
        return None;
    }
    (!out.is_empty()).then(|| Bytes::from(out))
}

/// Real coded geometry from the parsed stream format; local display size is only
/// a fallback for a not-yet-described stream.
pub(super) fn stream_geometry(
    config: &DecoderConfig,
    format: Option<VideoFormatSignature>,
) -> (u32, u32) {
    format
        .and_then(|format| {
            (format.coded_width > 0 && format.coded_height > 0)
                .then_some((format.coded_width, format.coded_height))
        })
        .unwrap_or((config.width, config.height))
}

#[allow(clippy::too_many_arguments)]
pub(super) fn open_decoder_with_metadata(
    config: &DecoderConfig,
    codec: VideoCodec,
    width: u32,
    height: u32,
    extra_data: Option<Bytes>,
    surface_writer: Option<crate::platform::surface::D3D11SurfaceWriter>,
) -> Result<NativeVideoDecoder> {
    tracing::debug!(
        ?codec,
        width,
        height,
        has_parameter_sets = extra_data.is_some(),
        "opening native decoder with first-frame metadata"
    );
    let extra = extra_data.unwrap_or_default();

    let opened = match surface_writer {
        Some(surface_writer) => NativeVideoDecoder::open_with_surface_writer(
            codec,
            width,
            height,
            config.frame_rate,
            config.decoder,
            extra,
            surface_writer,
        ),
        None => NativeVideoDecoder::open(
            codec,
            width,
            height,
            config.frame_rate,
            config.decoder,
            extra,
        ),
    };

    opened
}

pub(super) fn video_format_label(codec: VideoCodec, format: VideoFormatSignature) -> String {
    let codec_kind = codec;
    let codec = match codec {
        VideoCodec::H264 => "H.264/AVC",
        VideoCodec::H265 => "H.265/HEVC",
        VideoCodec::Av1 => "AV1",
    };
    let chroma = match format.chroma_format_idc {
        0 => "4:0:0",
        1 => "4:2:0",
        2 => "4:2:2",
        3 => "4:4:4",
        _ => "未知色度",
    };
    let coded = if format.coded_width != format.visible_width
        || format.coded_height != format.visible_height
    {
        format!(" · 编码 {}×{}", format.coded_width, format.coded_height)
    } else {
        String::new()
    };
    format!(
        "{codec} · {}×{}{} · {chroma} · {}-bit · {}",
        format.visible_width,
        format.visible_height,
        coded,
        format.bit_depth_luma,
        profile_name(codec_kind, format.profile_idc)
    )
}

/// The profile decides which hardware decode path a backend can ask for, so it
/// belongs next to the resolution in every diagnostic.
fn profile_name(codec: VideoCodec, profile_idc: u8) -> String {
    let name = match (codec, profile_idc) {
        (VideoCodec::H264, 66) => "Baseline",
        (VideoCodec::H264, 77) => "Main",
        (VideoCodec::H264, 88) => "Extended",
        (VideoCodec::H264, 100) => "High",
        (VideoCodec::H264, 110) => "High 10",
        (VideoCodec::H264, 122) => "High 4:2:2",
        (VideoCodec::H264, 244) => "High 4:4:4",
        (VideoCodec::H265, 1) => "Main",
        (VideoCodec::H265, 2) => "Main 10",
        (VideoCodec::H265, 3) => "Main Still Picture",
        (VideoCodec::H265, 4) => "Range Extensions",
        _ => return format!("profile {profile_idc}"),
    };
    format!("{name} profile")
}

pub(super) fn process_decoded_batch(
    batch: DecodedBatch,
    pool: &mut DecoderPool,
    timings: &mut VecDeque<FrameTiming>,
    mut context: DecodedForwardContext<'_>,
) -> Option<anyhow::Error> {
    // A later callback failure never rolls back successful earlier output.
    forward_decoded_frames(batch.frames, timings, &mut context, pool);
    for issue in batch.output_issues {
        match issue {
            DecoderOutputIssue::Dropped(token) => {
                if context.inflight.remove(&token).is_some() {
                    tracing::debug!(token, "backend explicitly dropped a decoded input");
                }
            }
            DecoderOutputIssue::Failed { token, error } => {
                if let Some(token) = token
                    && context.inflight.remove(&token).is_none()
                {
                    tracing::debug!(token, %error, "discarding stale decoder error callback");
                    continue;
                }
                tracing::warn!(?token, %error, "native decoder output failed");
                pool.callback_failed(&error);
            }
        }
    }
    batch.input_error
}

pub(super) fn forward_decoded_frames(
    decoded: Vec<DecodedFrame>,
    timings: &mut VecDeque<FrameTiming>,
    context: &mut DecodedForwardContext<'_>,
    pool: &mut DecoderPool,
) {
    for image in decoded {
        let Some(timestamp) = context.inflight.remove(&image.pts) else {
            tracing::debug!(
                decode_token = image.pts,
                "dropping stale decoder generation callback"
            );
            continue;
        };
        let Some(timing) = timing_for_timestamp(timings, timestamp) else {
            tracing::debug!(
                decode_token = image.pts,
                "dropping stale or unknown decoder callback"
            );
            continue;
        };
        let surface = match image
            .surface
            .prepare(image.width, image.height, timing.color)
        {
            Ok(surface) => surface,
            Err(error) => {
                tracing::warn!(decode_token = image.pts, %error, "native decoded color conversion failed");
                pool.callback_failed(&error);
                continue;
            }
        };
        let decoded_at = image.ready_at;
        let _ = context
            .receiver_feedback
            .send(VideoReceiverFeedback::DecodeTiming {
                duration: decoded_at.saturating_duration_since(timing.submitted_at),
                finished_at: decoded_at,
            });
        context.performance.record_decoded_frame(
            decoded_at,
            image.width,
            image.height,
            timing.keyframe,
            decoded_at.saturating_duration_since(timing.received_at),
        );
        // Hidden tabs have no presentation queue. Keep decoder feedback alive
        // during the official short capture grace period, releasing surfaces.
        if !context.frame_wake.visible.load(Ordering::Acquire) {
            continue;
        }
        let mut queue = mutex_lock(context.frame_queue);
        let replaced = queue.replace(DecodedVideoFrame {
            is_new_picture: timing.is_new_picture,
            width: image.width,
            height: image.height,
            surface,
            color: timing.color,
            received_at: timing.received_at,
            decoded_at,
            assembly_delay: timing.assembled_at.duration_since(timing.received_at),
            input_queue_delay: timing.submitted_at.duration_since(timing.assembled_at),
            decode_pipeline_delay: decoded_at.duration_since(timing.submitted_at),
            rotation: timing.rotation,
            sender_timing: timing.sender_timing,
        });
        context.performance.set_presentation_queue_frames(1);
        drop(queue);
        if replaced.is_some() {
            context.performance.record_dropped_present_frame();
        }
        drop(replaced);
        context.frame_wake.notify();
    }
}

pub(super) fn timing_for_timestamp(
    timings: &mut VecDeque<FrameTiming>,
    timestamp: u32,
) -> Option<FrameTiming> {
    // 4F5EA0 removes the older RTP prefix, not an arbitrary matched vector slot.
    while let Some(front) = timings.front() {
        let delta = front.rtp_timestamp.wrapping_sub(timestamp);
        if delta == 0 {
            return timings.pop_front();
        }
        let newer = if delta == 0x8000_0000 {
            front.rtp_timestamp > timestamp
        } else {
            (delta as i32) > 0
        };
        if newer {
            break;
        }
        timings.pop_front();
    }
    None
}
