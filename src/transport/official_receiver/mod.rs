//! Official-compatible UU/WebRTC video receive state machine.
//!
//! Live depacketization, packet/reference buffering and decode scheduling.

use crate::media::codec_parameters::NaluInfo;
use crate::media::decoder_result::VideoDecodeResult;
use crate::media::video_color::{VideoColorHistory, VideoColorSpace};
use crate::media::video_format::VideoFormatSignature;
use crate::transport::rtc::PlayoutDelay;
use crate::transport::timing::{DecodeSchedule, PlayoutSchedule, VideoPlayoutTiming};
use anyhow::Result;
use depacketize::{depacketize_h264, depacketize_h265, parse_video_timing};
use packets::{EncodedFrame, PacketBuffer, PacketInsertResult};
use references::{FrameBuffer, ReferenceResult, SeqNumOnlyRefFinder, rtp_timestamp_ahead_of};
use std::collections::{BTreeMap, HashMap};
use std::time::{Duration, Instant};
use webrtc::rtp::packet::Packet as RtpPacket;

const PACKET_BUFFER_INITIAL_SIZE: usize = 2_048;
const PACKET_BUFFER_MAX_SIZE: usize = 16_384;
const FRAME_BUFFER_MAX_SIZE: usize = 800;
const DECODED_HISTORY_SIZE: usize = 1 << 13;
const DECODER_ADMISSION_TOKENS: usize = 1;
const MAX_STASHED_FRAMES: usize = 100;
const START_CODE: [u8; 4] = [0, 0, 0, 1];

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum VideoCodecKind {
    H264,
    H265,
    Av1,
}

/// WebRTC/UU video-timing header extension. All six values are milliseconds
/// from the frame capture timestamp. A flags value of 0xff invalidates the
/// whole extension; UU currently also uses bit 2 in addition to upstream's
/// timer/size bits.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct VideoSendTiming {
    pub flags: u8,
    pub encode_start_delta_ms: u16,
    pub encode_finish_delta_ms: u16,
    pub packetization_finish_delta_ms: u16,
    pub pacer_exit_delta_ms: u16,
    pub network_timestamp_delta_ms: u16,
    pub network2_timestamp_delta_ms: u16,
}

pub(crate) struct ParsedVideoPacket {
    codec: VideoCodecKind,
    sequence_number: u16,
    timestamp: u32,
    marker: bool,
    is_first_packet_in_frame: bool,
    sps: bool,
    pps: bool,
    idr: bool,
    packet_keyframe: bool,
    nalus: Vec<NaluInfo>,
    parameter_format: Option<VideoFormatSignature>,
    color_space: Option<VideoColorSpace>,
    rotation: Option<u16>,
    content_type: Option<u8>,
    video_capture_index: Option<u16>,
    is_new_picture: Option<bool>,
    video_timing: Option<VideoSendTiming>,
    frame_sending_delay_ms: Option<u16>,
    playout_delay: Option<PlayoutDelay>,
    nack_count: u8,
    payload: Vec<u8>,
    received_at: Instant,
}

impl ParsedVideoPacket {
    pub(crate) fn set_receive_timing(&mut self, delay: Option<PlayoutDelay>, nack_count: u8) {
        self.playout_delay = delay;
        self.nack_count = nack_count;
    }

    pub(crate) fn sequence_number(&self) -> u16 {
        self.sequence_number
    }

    pub(crate) fn starts_keyframe(&self) -> bool {
        self.is_first_packet_in_frame && self.packet_keyframe
    }
}

pub(crate) struct ReceivedVideoFrame {
    pub codec: VideoCodecKind,
    pub frame_id: i64,
    pub data: Vec<u8>,
    pub parameter_format: Option<VideoFormatSignature>,
    pub rtp_timestamp: u32,
    pub keyframe: bool,
    pub color_space: Option<VideoColorSpace>,
    pub rotation: u16,
    pub content_type: u8,
    pub video_capture_index: Option<u16>,
    pub is_new_picture: Option<bool>,
    pub video_timing: Option<VideoSendTiming>,
    pub frame_sending_delay_ms: Option<u16>,
    pub received_at: Instant,
    pub last_received_at: Instant,
    pub assembled_at: Instant,
    pub schedule: PlayoutSchedule,
}

#[derive(Default)]
pub(crate) struct ReceiverResult {
    pub frames: Vec<ReceivedVideoFrame>,
    pub request_keyframe: bool,
    pub clear_nack: bool,
    pub accepted_packet: bool,
    pub continuous_sequence: Option<u16>,
    pub predecode_drops: usize,
    pub frame_buffer_frames: usize,
}

#[derive(Clone)]
pub(crate) struct VideoHeaderExtensions {
    pub orientation: Option<u8>,
    pub content_type: Option<u8>,
    pub capture_index: Option<u8>,
    pub is_new_picture: Option<u8>,
    pub timing: Option<u8>,
    pub sending_delay: Option<u8>,
    pub color_space: Option<u8>,
}

pub(crate) struct OfficialVideoReceiver {
    packet_buffer: PacketBuffer,
    reference_finder: SeqNumOnlyRefFinder,
    frame_buffer: FrameBuffer,
    in_flight: HashMap<i64, InFlightFrame>,
    decoder_requires_keyframe: bool,
    has_decoded_frame: bool,
    last_decode_keyframe_request: Option<Instant>,
    last_sequence_by_frame: BTreeMap<i64, u16>,
    pending_continuous_sequence: Option<u16>,
    pending_predecode_drops: usize,
    timing: VideoPlayoutTiming,
    scheduled: Option<(u32, DecodeSchedule)>,
    decode_timeout: ReceiveDecodeTimeout,
    extensions: VideoHeaderExtensions,
    color_history: VideoColorHistory,
    current_codec: Option<VideoCodecKind>,
    last_assembled_timestamp: Option<u32>,
    last_completed_picture_id: i64,
    has_received_frame: bool,
}

struct InFlightFrame {
    last_sequence: u16,
    released_at: Instant,
    request_due: bool,
    previously_required_keyframe: bool,
}

impl OfficialVideoReceiver {
    pub(crate) fn reset_after_pause(&mut self) {
        let codec = match self.current_codec.unwrap_or(VideoCodecKind::H264) {
            VideoCodecKind::H264 => "video/H264",
            VideoCodecKind::H265 => "video/H265",
            VideoCodecKind::Av1 => "video/AV1",
        };
        let anchor = self
            .last_completed_picture_id
            .saturating_add(i64::from(u16::MAX));
        let mut next = Self::new(codec, self.extensions.clone(), "").expect("known video codec");
        next.reference_finder = SeqNumOnlyRefFinder::with_unwrap_anchor(anchor);
        next.last_completed_picture_id = anchor;
        *self = next;
    }
    pub(crate) fn new(
        mime_type: &str,
        extensions: VideoHeaderExtensions,
        codec_fmtp: &str,
    ) -> Result<Self> {
        let codec = if mime_type.eq_ignore_ascii_case("video/H264") {
            VideoCodecKind::H264
        } else if mime_type.eq_ignore_ascii_case("video/AV1") {
            VideoCodecKind::Av1
        } else if mime_type.eq_ignore_ascii_case("video/H265")
            || mime_type.eq_ignore_ascii_case("video/HEVC")
        {
            VideoCodecKind::H265
        } else {
            anyhow::bail!("unsupported official receiver codec {mime_type}");
        };
        let mut packet_buffer = PacketBuffer::new();
        if matches!(codec, VideoCodecKind::H264) {
            packet_buffer.install_h264_sprop(codec_fmtp);
        }
        Ok(Self {
            packet_buffer,
            reference_finder: SeqNumOnlyRefFinder::new(),
            frame_buffer: FrameBuffer::new(),
            in_flight: HashMap::new(),
            decoder_requires_keyframe: true,
            has_decoded_frame: false,
            last_decode_keyframe_request: None,
            last_sequence_by_frame: BTreeMap::new(),
            pending_continuous_sequence: None,
            pending_predecode_drops: 0,
            timing: VideoPlayoutTiming::new(Instant::now()),
            scheduled: None,
            decode_timeout: ReceiveDecodeTimeout::new(Instant::now()),
            extensions,
            color_history: VideoColorHistory::default(),
            current_codec: None,
            last_assembled_timestamp: None,
            last_completed_picture_id: -1,
            has_received_frame: false,
        })
    }

    pub(crate) fn configure_codec(&mut self, codec: VideoCodecKind, fmtp: &str) {
        if matches!(codec, VideoCodecKind::H264) {
            self.packet_buffer.install_h264_sprop(fmtp);
        }
    }

    pub(crate) fn parse_video_packet(
        &mut self,
        packet: &RtpPacket,
        received_at: Instant,
        codec: VideoCodecKind,
    ) -> Option<ParsedVideoPacket> {
        // RTP::Unmarshal already removes trailing padding. P=1 can still
        // carry a valid video payload (UU ReceivePacket, 0x18032D3F8).
        if packet.payload.is_empty() {
            return None;
        }
        let parsed = match codec {
            VideoCodecKind::H264 => depacketize_h264(packet, received_at),
            VideoCodecKind::H265 => depacketize_h265(packet, received_at),
            VideoCodecKind::Av1 => depacketize::depacketize_av1(packet, received_at),
        };
        let mut parsed = parsed?;
        let color_extension = self
            .extensions
            .color_space
            .and_then(|id| packet.header.get_extension(id));
        parsed.color_space = self.color_history.receive(
            packet.header.marker,
            parsed.packet_keyframe,
            color_extension.as_deref(),
        );
        // UU 的发送端在写 CVO 之前会把角度做一次 90↔270 互换（libstreamer 的
        // streamer::capture::ReverseRotation），它自己的客户端按相反的约定解释，两边自洽。
        // 我们按 WebRTC 的顺时针方向渲染，照抄上报值会让转过 90° 或 270° 的被控屏画面
        // 整体差 180°（0° 和 180° 取反后不变），所以这里把角度反过来。
        parsed.rotation = self
            .extensions
            .orientation
            .and_then(|id| packet.header.get_extension(id))
            .and_then(|payload| payload.first().copied())
            .map(|value| (360 - u16::from(value & 0x03) * 90) % 360);
        parsed.content_type = self
            .extensions
            .content_type
            .and_then(|id| packet.header.get_extension(id))
            .and_then(|payload| match payload.as_ref() {
                [value] if value & 0x80 == 0 => Some(value & 0x03),
                _ => None,
            });
        parsed.video_capture_index = self
            .extensions
            .capture_index
            .and_then(|id| packet.header.get_extension(id))
            .and_then(|payload| match payload.as_ref() {
                [high, low] => Some(u16::from_be_bytes([*high, *low])),
                _ => None,
            });
        parsed.video_timing = self
            .extensions
            .timing
            .and_then(|id| packet.header.get_extension(id))
            .and_then(|payload| parse_video_timing(payload.as_ref()));
        parsed.is_new_picture = self
            .extensions
            .is_new_picture
            .and_then(|id| packet.header.get_extension(id))
            .and_then(|payload| match payload.as_ref() {
                [value] => Some(*value != 0),
                _ => None,
            });
        parsed.frame_sending_delay_ms = self
            .extensions
            .sending_delay
            .and_then(|id| packet.header.get_extension(id))
            .and_then(|payload| match payload.as_ref() {
                [high, low] => Some(u16::from_be_bytes([*high, *low])),
                _ => None,
            });
        Some(parsed)
    }

    /// Parameter tracking precedes the per-packet RTCP flush in the official receiver.
    pub(crate) fn prepare_video_packet(&mut self, parsed: &mut ParsedVideoPacket) -> bool {
        self.packet_buffer.prepare_parameters(parsed)
    }

    pub(crate) fn receive_prepared(&mut self, parsed: ParsedVideoPacket) -> ReceiverResult {
        let insert = self.packet_buffer.insert(parsed);
        self.accept_insert(insert)
    }

    /// The caller has already flushed the parameter tracker's keyframe request.
    pub(crate) fn parameter_packet_rejected(&mut self) -> ReceiverResult {
        let mut result = self.accept_insert(PacketInsertResult {
            parameter_rejected: true,
            ..PacketInsertResult::empty()
        });
        result.request_keyframe = false;
        result
    }

    pub(crate) fn receive_padding(&mut self, sequence_number: u16) -> ReceiverResult {
        let references = self.reference_finder.padding_received(sequence_number);
        self.insert_reference_result(references);
        let insert = self.packet_buffer.insert_padding(sequence_number);
        self.accept_insert(insert)
    }

    pub(crate) fn decoder_finished(
        &mut self,
        frame_id: i64,
        result: VideoDecodeResult,
    ) -> ReceiverResult {
        let Some(frame) = self.in_flight.remove(&frame_id) else {
            return ReceiverResult::default();
        };
        let accepted = result.accepted();
        let request_keyframe = if accepted {
            result == VideoDecodeResult::RequestKeyframe
        } else {
            frame.request_due || !self.has_decoded_frame || !frame.previously_required_keyframe
        };
        self.decoder_requires_keyframe = !accepted;
        self.frame_buffer.keyframe_required = !accepted;
        if accepted {
            self.has_decoded_frame = true;
            self.packet_buffer.clear_to(frame.last_sequence);
            self.reference_finder.clear_to(frame.last_sequence);
            self.last_sequence_by_frame
                .retain(|stored_id, _| *stored_id > frame_id);
        }
        if request_keyframe {
            self.last_decode_keyframe_request = Some(frame.released_at);
            self.decode_timeout.request_count = self.decode_timeout.request_count.saturating_add(1);
        }
        self.decode_timeout.update_waits();
        self.decode_timeout
            .start_next(self.decoder_requires_keyframe, Instant::now());
        // The official worker applies the complete Decode result before it
        // requests the next frame. There is no intermediate admission message.
        let frames = self.take_ready_frames(false);
        ReceiverResult {
            frames,
            request_keyframe,
            clear_nack: request_keyframe,
            accepted_packet: false,
            continuous_sequence: self.pending_continuous_sequence.take(),
            predecode_drops: std::mem::take(&mut self.pending_predecode_drops),
            frame_buffer_frames: self.frame_buffer.frames.len(),
        }
    }

    pub(crate) fn record_decode(&mut self, duration: Duration, finished_at: Instant) {
        self.timing.record_decode(duration, finished_at);
        tracing::trace!(
            decode_p95_ms = self.timing.decode_estimate().as_millis(),
            "UU receive-stream decode timing sample"
        );
    }

    pub(crate) fn next_deadline(&self) -> Instant {
        self.scheduled
            .map_or(self.decode_timeout.deadline, |(_, scheduled)| {
                scheduled.deadline.min(self.decode_timeout.deadline)
            })
    }

    pub(crate) fn poll(&mut self, active: bool, receiving_keyframe: bool) -> ReceiverResult {
        let now = Instant::now();
        // Decode release precedes a timeout at the same instant, as its deadline
        // is normally clamped to one millisecond before the timeout.
        let mut frames = self.take_ready_frames(true);
        let mut request_keyframe = false;
        // 会话起点被扣住的帧（帧前缀等待 pre-window NACK 补传）每次 poll
        // 重扫：补全后先于后续帧释放；宽限期过后放弃。
        if self.packet_buffer.pending_rescan.is_some() {
            let insert = PacketInsertResult {
                frames: self.packet_buffer.rescan_pending(),
                duplicate: false,
                cleared: false,
                parameter_rejected: false,
            };
            if !insert.frames.is_empty() {
                let mut rescan = self.accept_insert(insert);
                let scheduled = std::mem::take(&mut frames);
                rescan.frames.extend(scheduled);
                request_keyframe |= rescan.request_keyframe;
                frames = rescan.frames;
            }
        }
        if now >= self.decode_timeout.deadline {
            self.decode_timeout.advance(now);
            request_keyframe = active && !receiving_keyframe;
            if request_keyframe {
                self.last_decode_keyframe_request = Some(now);
                self.decode_timeout.request_count =
                    self.decode_timeout.request_count.saturating_add(1);
            }
            self.decode_timeout.update_waits();
            self.decode_timeout
                .start_next(self.decoder_requires_keyframe, now);
        }
        ReceiverResult {
            frames,
            request_keyframe,
            clear_nack: request_keyframe,
            predecode_drops: std::mem::take(&mut self.pending_predecode_drops),
            frame_buffer_frames: self.frame_buffer.frames.len(),
            ..ReceiverResult::default()
        }
    }

    fn accept_insert(&mut self, insert: PacketInsertResult) -> ReceiverResult {
        let mut request_keyframe = insert.cleared || insert.parameter_rejected;
        for frame in insert.frames {
            request_keyframe |= self.insert_assembled_frame(frame);
        }
        let frames = self.take_ready_frames(false);
        ReceiverResult {
            frames,
            request_keyframe,
            clear_nack: false,
            accepted_packet: !insert.parameter_rejected,
            continuous_sequence: self.pending_continuous_sequence.take(),
            predecode_drops: std::mem::take(&mut self.pending_predecode_drops),
            frame_buffer_frames: self.frame_buffer.frames.len(),
        }
    }

    fn insert_assembled_frame(&mut self, frame: EncodedFrame) -> bool {
        let request_keyframe = !self.has_received_frame && !frame.keyframe;
        self.has_received_frame = true;

        if let Some(current_codec) = self.current_codec {
            if current_codec != frame.codec {
                let newer = self
                    .last_assembled_timestamp
                    .is_none_or(|last| rtp_timestamp_ahead_of(frame.timestamp, last));
                if !newer {
                    return request_keyframe;
                }
                self.reference_finder = SeqNumOnlyRefFinder::with_unwrap_anchor(
                    self.last_completed_picture_id + i64::from(u16::MAX),
                );
                self.current_codec = Some(frame.codec);
            }
        } else {
            self.current_codec = Some(frame.codec);
        }
        if self
            .last_assembled_timestamp
            .is_none_or(|last| rtp_timestamp_ahead_of(frame.timestamp, last))
        {
            self.last_assembled_timestamp = Some(frame.timestamp);
        }

        let references = self.reference_finder.manage(frame);
        self.insert_reference_result(references);
        request_keyframe
    }

    fn insert_reference_result(&mut self, result: ReferenceResult) {
        for frame in result.frames {
            if let Some(delay) = frame.playout_delay {
                self.timing.set_playout_delay(delay);
            }
            let frame_id = frame.id;
            let timestamp = frame.timestamp;
            let received_at = frame.last_received_at;
            let nack_delayed = frame.nack_count > 0;
            let was_present = self.frame_buffer.frames.contains_key(&frame_id);
            self.last_completed_picture_id = self.last_completed_picture_id.max(frame_id);
            self.last_sequence_by_frame
                .insert(frame_id, frame.last_sequence_number);
            if let Some(continuous_id) = self.frame_buffer.insert(frame)
                && let Some(sequence_number) =
                    self.last_sequence_by_frame.get(&continuous_id).copied()
            {
                self.pending_continuous_sequence = Some(sequence_number);
            }
            if !was_present && self.frame_buffer.frames.contains_key(&frame_id) && !nack_delayed {
                self.timing.incoming_timestamp(timestamp, received_at);
            }
        }
        // FrameBuffer can reject, evict or clear frames without a successful
        // Decode. Keep sequence metadata only while a consumer still owns it;
        // otherwise a stalled/failing decoder grows this map without bound.
        // The last continuous ID may already have left FrameBuffer, but its
        // sequence is still needed when insert() reports that same frontier.
        self.last_sequence_by_frame.retain(|id, _| {
            self.frame_buffer.frames.contains_key(id)
                || self.in_flight.contains_key(id)
                || self.frame_buffer.last_continuous_frame_id == Some(*id)
        });
    }

    fn take_ready_frames(&mut self, release_scheduled: bool) -> Vec<ReceivedVideoFrame> {
        let mut output = Vec::new();
        while self.in_flight.len() < DECODER_ADMISSION_TOKENS {
            let Some((next_id, last_id)) = self.frame_buffer.next_and_last_decodable() else {
                self.scheduled = None;
                break;
            };
            let next = &self.frame_buffer.frames[&next_id].frame;
            let timestamp = next.timestamp;
            let now = Instant::now();
            let schedule = if self.frame_buffer.keyframe_required {
                self.scheduled = None;
                if !next.references.is_empty() {
                    self.pending_predecode_drops += self.frame_buffer.drop_next_decodable();
                    continue;
                }
                self.timing.keyframe_schedule(timestamp, now)
            } else if let Some((scheduled_timestamp, schedule)) = self.scheduled
                && scheduled_timestamp == timestamp
            {
                if !release_scheduled || schedule.deadline > now {
                    break;
                }
                schedule
            } else {
                let last_timestamp = self.frame_buffer.frames[&last_id].frame.timestamp;
                let Some(schedule) = self.timing.decode_schedule(
                    timestamp,
                    last_timestamp,
                    self.frame_buffer.frames.len(),
                    now,
                    self.decode_timeout.deadline.saturating_duration_since(now),
                ) else {
                    self.scheduled = None;
                    self.pending_predecode_drops += self.frame_buffer.drop_next_decodable();
                    continue;
                };
                self.scheduled = Some((timestamp, schedule));
                // 3304B0 posts even a zero-delay callback. Do not decode inline
                // during packet insertion, or replace this timestamp's task
                // just because another independently decodable frame arrives.
                break;
            };
            self.scheduled = None;
            let Some(frame) = self.frame_buffer.extract_next_decodable() else {
                break;
            };
            self.decode_timeout.frame_released(now, frame.keyframe);
            let request_wait = Duration::from_secs(u64::from(
                self.decode_timeout.request_count.saturating_add(1).min(3),
            ));
            let request_due = self
                .last_decode_keyframe_request
                .is_none_or(|last| now >= last + request_wait);
            let schedule = self.timing.frame_released(
                frame.timestamp,
                frame.last_received_at,
                frame.data.len(),
                frame.nack_count > 0,
                schedule,
                now,
            );
            self.in_flight.insert(
                frame.id,
                InFlightFrame {
                    last_sequence: frame.last_sequence_number,
                    released_at: now,
                    request_due,
                    previously_required_keyframe: self.decoder_requires_keyframe,
                },
            );
            output.push(ReceivedVideoFrame {
                codec: frame.codec,
                frame_id: frame.id,
                data: frame.data,
                parameter_format: frame.parameter_format,
                rtp_timestamp: frame.timestamp,
                keyframe: frame.keyframe,
                color_space: frame.color_space,
                rotation: frame.rotation,
                content_type: frame.content_type,
                video_capture_index: frame.video_capture_index,
                is_new_picture: frame.is_new_picture,
                video_timing: frame.video_timing,
                frame_sending_delay_ms: frame.frame_sending_delay_ms,
                received_at: frame.received_at,
                last_received_at: frame.last_received_at,
                assembled_at: frame.assembled_at,
                schedule,
            });
        }
        output
    }
}

/// UU TimeoutTracker for this receiver's negotiated 1000ms NACK history:
/// initial keyframe/frame limits are 3s; decode-return and timeout callbacks
/// select min(1s * (keyframe request count + 1), 3s). No 50ms polling loop.
struct ReceiveDecodeTimeout {
    deadline: Instant,
    keyframe_wait: Duration,
    frame_wait: Duration,
    waiting_keyframe: bool,
    request_count: u32,
}

impl ReceiveDecodeTimeout {
    fn new(now: Instant) -> Self {
        Self {
            deadline: now + Duration::from_secs(3),
            keyframe_wait: Duration::from_secs(3),
            frame_wait: Duration::from_secs(3),
            waiting_keyframe: true,
            request_count: 0,
        }
    }

    fn update_waits(&mut self) {
        let wait = Duration::from_secs(u64::from(self.request_count.saturating_add(1).min(3)));
        self.keyframe_wait = wait;
        self.frame_wait = wait;
    }

    fn start_next(&mut self, keyframe: bool, now: Instant) {
        if keyframe {
            self.waiting_keyframe = true;
            self.deadline = self.deadline.min(now + self.keyframe_wait);
        }
    }

    fn frame_released(&mut self, now: Instant, keyframe: bool) {
        self.waiting_keyframe = false;
        self.deadline = now + self.frame_wait;
        if keyframe {
            self.request_count = 0;
        }
    }

    fn advance(&mut self, now: Instant) {
        self.deadline = now
            + if self.waiting_keyframe {
                self.keyframe_wait
            } else {
                self.frame_wait
            };
    }
}

fn ahead_of(newer: u16, older: u16) -> bool {
    let distance = newer.wrapping_sub(older);
    distance != 0 && (distance < 0x8000 || (distance == 0x8000 && newer > older))
}

fn sequence_at_or_ahead(value: u16, reference: u16) -> bool {
    value == reference || ahead_of(value, reference)
}

mod depacketize;
mod packets;
mod references;
