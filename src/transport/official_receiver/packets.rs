//! Packet buffering and frame assembly.
use super::{
    PACKET_BUFFER_INITIAL_SIZE, PACKET_BUFFER_MAX_SIZE, ParsedVideoPacket, VideoCodecKind,
    VideoSendTiming, ahead_of, sequence_at_or_ahead,
};
use crate::media::codec_parameters::ParameterTracker;
use crate::media::video_color::VideoColorSpace;
use crate::media::video_format::VideoFormatSignature;
use crate::transport::rtc::PlayoutDelay;
use std::collections::HashSet;
use std::time::{Duration, Instant};

/// 首包锚定时向后登记的「预窗口」大小；必须与
/// `crate::transport::rtc::feedback::RTP_STARTUP_NACK_WINDOW` 保持一致。
/// macOS host 的会话初始 STAP-A(SPS/PPS) + IDR 早于我们 attach，该窗口内
/// 的包由 feedback 侧的 pre-window NACK 补回。1080p 关键帧约 25-120 个
/// FU-A 片，窗口必须覆盖整帧（含 STAP-A 与 S=1 首片），24 太窄；
/// 128 条目 NACK ~600B 仍在单个 RTCP 包内。
const PRE_WINDOW_NACK: u16 = 128;
/// 会话起点（首包锚定）后允许「扣住前缀未到齐的帧」等待补传的宽限期。
/// 过期后不再组装缺了开头的帧，按常规缺口请求关键帧，避免 host 不重传
/// 时永久等待。
const PRE_WINDOW_GRACE: Duration = Duration::from_millis(2000);

pub(super) struct BufferedPacket {
    pub(super) packet: ParsedVideoPacket,
    pub(super) continuous: bool,
}

pub(super) struct EncodedFrame {
    pub(super) codec: VideoCodecKind,
    pub(super) id: i64,
    pub(super) references: Vec<i64>,
    pub(super) first_sequence_number: u16,
    pub(super) last_sequence_number: u16,
    pub(super) timestamp: u32,
    pub(super) keyframe: bool,
    pub(super) color_space: Option<VideoColorSpace>,
    pub(super) rotation: u16,
    pub(super) content_type: u8,
    pub(super) video_capture_index: Option<u16>,
    pub(super) is_new_picture: Option<bool>,
    pub(super) video_timing: Option<VideoSendTiming>,
    pub(super) frame_sending_delay_ms: Option<u16>,
    pub(super) playout_delay: Option<PlayoutDelay>,
    pub(super) nack_count: u8,
    pub(super) received_at: Instant,
    pub(super) last_received_at: Instant,
    pub(super) assembled_at: Instant,
    pub(super) data: Vec<u8>,
    pub(super) parameter_format: Option<VideoFormatSignature>,
}

pub(super) struct PacketBuffer {
    pub(super) buffer: Vec<Option<BufferedPacket>>,
    pub(super) first_sequence_number: u16,
    pub(super) first_packet_received: bool,
    pub(super) cleared_to_first_sequence: bool,
    pub(super) newest_inserted_sequence: Option<u16>,
    pub(super) missing_packets: HashSet<u16>,
    pub(super) received_padding: HashSet<u16>,
    pub(super) last_keyframe_first_sequence: Option<u16>,
    pub(super) parameters: ParameterTracker,
    malformed_av1: bool,
    /// 首包锚定时刻，用于 PRE_WINDOW_GRACE 宽限期判定。
    anchored_at: Option<Instant>,
    /// 被扣住（前缀等待 NACK 补传）的帧的 marker 序号；由 poll 重扫。
    pub(super) pending_rescan: Option<u16>,
}

pub(super) struct SequenceUnwrapper {
    pub(super) last: Option<i64>,
}

pub(super) struct PacketInsertResult {
    pub(super) frames: Vec<EncodedFrame>,
    pub(super) duplicate: bool,
    pub(super) cleared: bool,
    pub(super) parameter_rejected: bool,
}

impl PacketBuffer {
    pub(super) fn new() -> Self {
        let mut buffer = Vec::with_capacity(PACKET_BUFFER_INITIAL_SIZE);
        buffer.resize_with(PACKET_BUFFER_INITIAL_SIZE, || None);
        Self {
            buffer,
            first_sequence_number: 0,
            first_packet_received: false,
            cleared_to_first_sequence: false,
            newest_inserted_sequence: None,
            missing_packets: HashSet::new(),
            received_padding: HashSet::new(),
            last_keyframe_first_sequence: None,
            parameters: ParameterTracker::default(),
            malformed_av1: false,
            anchored_at: None,
            pending_rescan: None,
        }
    }

    pub(super) fn install_h264_sprop(&mut self, fmtp: &str) {
        self.parameters.install_h264_sprop(fmtp);
    }

    pub(super) fn prepare_parameters(&mut self, packet: &mut ParsedVideoPacket) -> bool {
        let tracked = match packet.codec {
            VideoCodecKind::Av1 => return true,
            VideoCodecKind::H264 => self.parameters.h264(
                &mut packet.nalus,
                packet.is_first_packet_in_frame,
                &mut packet.payload,
            ),
            VideoCodecKind::H265 => self
                .parameters
                .h265(&packet.nalus, packet.is_first_packet_in_frame),
        };
        match tracked {
            Ok(format) => packet.parameter_format = format.or(packet.parameter_format),
            Err(reason) => {
                tracing::debug!(
                    reason,
                    sequence_number = packet.sequence_number,
                    "request keyframe for missing codec parameter dependency"
                );
                return false;
            }
        }
        // 包级 prepend 可能给首片补上了 SPS/PPS NAL：重算标志位，
        // 帧组装时才不会重复前拼接。
        if packet.codec == VideoCodecKind::H264 {
            packet.sps = packet.nalus.iter().any(|n| n.kind == 7 && n.sps >= 0);
            packet.pps = packet.nalus.iter().any(|n| n.kind == 8);
        }
        true
    }

    pub(super) fn insert(&mut self, packet: ParsedVideoPacket) -> PacketInsertResult {
        let sequence_number = packet.sequence_number;
        if !self.first_packet_received {
            self.first_sequence_number = sequence_number;
            self.first_packet_received = true;
            self.anchored_at = Some(Instant::now());
            // 登记预窗口缺失：加入会话时早于首包的包（macOS host 的初始
            // STAP-A(SPS/PPS) + IDR 首片）由 feedback 侧 pre-window NACK
            // 补回；登记后 find_frames 才能识别「帧前缀未到齐」，避免把
            // 残缺帧当作完整关键帧发出（那会永久卡住首次解码）。首包本身
            // 就是关键帧开头时没有缺口，feedback 侧也不会去要。
            if !packet.starts_keyframe() {
                for offset in 1u16..=PRE_WINDOW_NACK {
                    self.missing_packets
                        .insert(sequence_number.wrapping_sub(offset));
                }
            }
        } else if ahead_of(self.first_sequence_number, sequence_number) {
            if self.cleared_to_first_sequence {
                return PacketInsertResult::empty();
            }
            self.first_sequence_number = sequence_number;
        }

        let mut index = sequence_number as usize % self.buffer.len();
        // UU's collision cleanup precedes duplicate detection and compares
        // raw uint16_t values (4F850B cmp/jnb), not AheadOf. Preserve that
        // branch even across sequence wrap; other sequence gates still use
        // their independently verified modular ordering.
        if self.buffer[index].as_ref().is_some_and(|stored| {
            self.last_keyframe_first_sequence
                .is_some_and(|keyframe| stored.packet.sequence_number < keyframe)
        }) {
            self.buffer[index] = None;
        }
        if self.buffer[index]
            .as_ref()
            .is_some_and(|stored| stored.packet.sequence_number == sequence_number)
        {
            let mut result = PacketInsertResult::empty();
            result.duplicate = true;
            return result;
        }
        while self.buffer[index].is_some() {
            let collided_sequence = self.buffer[index]
                .as_ref()
                .map(|stored| stored.packet.sequence_number);
            if !self.expand() {
                break;
            }
            index = sequence_number as usize % self.buffer.len();
            tracing::debug!(
                sequence_number,
                collided_sequence,
                buffer_size = self.buffer.len(),
                first_sequence_number = self.first_sequence_number,
                cleared_to_first_sequence = self.cleared_to_first_sequence,
                "PacketBuffer size expanded"
            );
        }
        if self.buffer[index].is_some() {
            let occupied_slots = self.buffer.iter().filter(|slot| slot.is_some()).count();
            tracing::warn!(
                sequence_number,
                occupied_sequence = self.buffer[index]
                    .as_ref()
                    .map(|stored| stored.packet.sequence_number),
                first_sequence_number = self.first_sequence_number,
                cleared_to_first_sequence = self.cleared_to_first_sequence,
                last_keyframe_first_sequence = self.last_keyframe_first_sequence,
                buffer_size = self.buffer.len(),
                occupied_slots,
                "Clear PacketBuffer and request key frame"
            );
            self.clear();
            return PacketInsertResult {
                frames: Vec::new(),
                duplicate: false,
                cleared: true,
                parameter_rejected: false,
            };
        }
        self.buffer[index] = Some(BufferedPacket {
            packet,
            continuous: false,
        });
        self.update_missing(sequence_number);
        let padding_age = (self.buffer.len() / 4).min(usize::from(u16::MAX)) as u16;
        let oldest_padding = sequence_number.wrapping_sub(padding_age);
        self.received_padding
            .retain(|padding| sequence_at_or_ahead(*padding, oldest_padding));
        PacketInsertResult {
            frames: self.find_frames(sequence_number),
            duplicate: false,
            cleared: false,
            parameter_rejected: std::mem::take(&mut self.malformed_av1),
        }
    }

    pub(super) fn insert_padding(&mut self, sequence_number: u16) -> PacketInsertResult {
        self.update_missing(sequence_number);
        self.received_padding.insert(sequence_number);
        PacketInsertResult {
            frames: self.find_frames(sequence_number.wrapping_add(1)),
            duplicate: false,
            cleared: false,
            parameter_rejected: std::mem::take(&mut self.malformed_av1),
        }
    }

    pub(super) fn expand(&mut self) -> bool {
        if self.buffer.len() == PACKET_BUFFER_MAX_SIZE {
            return false;
        }
        let new_size = (self.buffer.len() * 2).min(PACKET_BUFFER_MAX_SIZE);
        let mut expanded = Vec::with_capacity(new_size);
        expanded.resize_with(new_size, || None);
        for slot in &mut self.buffer {
            if let Some(packet) = slot.take() {
                let index = packet.packet.sequence_number as usize % new_size;
                expanded[index] = Some(packet);
            }
        }
        self.buffer = expanded;
        true
    }

    pub(super) fn clear(&mut self) {
        for slot in &mut self.buffer {
            *slot = None;
        }
        self.first_packet_received = false;
        self.cleared_to_first_sequence = false;
        self.newest_inserted_sequence = None;
        self.missing_packets.clear();
        self.received_padding.clear();
        self.last_keyframe_first_sequence = None;
        self.anchored_at = None;
        self.pending_rescan = None;
    }

    pub(super) fn clear_to(&mut self, sequence_number: u16) {
        if self.cleared_to_first_sequence && ahead_of(self.first_sequence_number, sequence_number) {
            return;
        }
        if !self.first_packet_received {
            return;
        }
        let target = sequence_number.wrapping_add(1);
        let forward_difference = target.wrapping_sub(self.first_sequence_number) as usize;
        let iterations = forward_difference.min(self.buffer.len());
        for _ in 0..iterations {
            let index = self.first_sequence_number as usize % self.buffer.len();
            if self.buffer[index]
                .as_ref()
                .is_some_and(|stored| ahead_of(target, stored.packet.sequence_number))
            {
                self.buffer[index] = None;
            }
            self.first_sequence_number = self.first_sequence_number.wrapping_add(1);
        }
        self.first_sequence_number = target;
        self.cleared_to_first_sequence = true;
        self.missing_packets
            .retain(|missing| sequence_at_or_ahead(*missing, target));
        self.received_padding
            .retain(|padding| sequence_at_or_ahead(*padding, target));
        // 被扣住的帧若整体落在清理前沿之前，其包槽已不可用，无需再重扫。
        if self
            .pending_rescan
            .is_some_and(|pending| ahead_of(target, pending))
        {
            self.pending_rescan = None;
        }
    }

    /// 重扫被扣住的帧（前缀等待 pre-window NACK 补传）。接收 worker 在每次
    /// poll 时调用：补全后释放，宽限期过后放弃（按常规缺口处理）。
    pub(super) fn rescan_pending(&mut self) -> Vec<EncodedFrame> {
        let Some(sequence_number) = self.pending_rescan else {
            return Vec::new();
        };
        if !self.in_startup_window() {
            // 宽限期已过：不再从帧尾回溯，残缺帧不会被组装，按常规缺口
            // 处理（请求关键帧）。
            self.pending_rescan = None;
            return Vec::new();
        }
        self.find_frames(sequence_number)
    }

    /// 会话起点的宽限期内、尚未组装出任何关键帧时，才允许为补传扣住帧
    /// 或从帧尾 marker 回溯。之后缺了首片的帧按常规缺口处理。
    fn in_startup_window(&self) -> bool {
        self.last_keyframe_first_sequence.is_none()
            && self
                .anchored_at
                .is_some_and(|anchored| anchored.elapsed() < PRE_WINDOW_GRACE)
    }

    pub(super) fn potential_new_frame(&self, sequence_number: u16) -> bool {
        let index = sequence_number as usize % self.buffer.len();
        let previous_index = if index == 0 {
            self.buffer.len() - 1
        } else {
            index - 1
        };
        let Some(entry) = self.buffer[index].as_ref() else {
            return false;
        };
        if entry.packet.sequence_number != sequence_number {
            return false;
        }
        if entry.packet.is_first_packet_in_frame {
            return true;
        }
        // 加入会话时可能错过关键帧的 S=1 首片（macOS host 的初始 IDR 常早于
        // 接收窗口），此时从帧尾 marker 包回溯也能拼出该帧，避免永久卡在
        // 「等关键帧」。只限会话起点：之后缺了开头的帧是普通丢包，组装
        // 出来只会把花屏带到下一个关键帧。
        if entry.packet.marker && self.in_startup_window() {
            return true;
        }
        let Some(previous) = self.buffer[previous_index].as_ref() else {
            return false;
        };
        previous.packet.sequence_number == sequence_number.wrapping_sub(1)
            && previous.packet.timestamp == entry.packet.timestamp
            && previous.continuous
    }

    pub(super) fn find_frames(&mut self, mut sequence_number: u16) -> Vec<EncodedFrame> {
        let mut frames = Vec::new();
        for _ in 0..self.buffer.len() {
            if self.received_padding.contains(&sequence_number) {
                sequence_number = sequence_number.wrapping_add(1);
                continue;
            }
            if !self.potential_new_frame(sequence_number) {
                break;
            }
            let index = sequence_number as usize % self.buffer.len();
            let is_last = {
                let entry = self.buffer[index].as_mut().expect("potential frame slot");
                entry.continuous = true;
                entry.packet.marker
            };
            if is_last {
                let mut start = sequence_number;
                let mut tested = 0_usize;
                loop {
                    tested += 1;
                    if tested == self.buffer.len() {
                        break;
                    }
                    let previous = start.wrapping_sub(1);
                    let previous_index = previous as usize % self.buffer.len();
                    let Some(entry) = self.buffer[previous_index].as_ref() else {
                        break;
                    };
                    if entry.packet.sequence_number != previous
                        || entry.packet.timestamp
                            != self.buffer[index]
                                .as_ref()
                                .expect("last frame packet")
                                .packet
                                .timestamp
                    {
                        break;
                    }
                    start = previous;
                }
                // 会话起点 STAP-A(SPS/PPS) + IDR 首片可能早于接收窗口
                // （macOS host）：若帧前缀未到齐（首包是 FU-A 续片，或关键帧
                // 缺参数集）且其前序包仍在缺失集合中（pre-window NACK 补传
                // 中），扣住本帧等待补全，由接收 worker 的 poll 重扫；宽限期
                // 过后放弃，按常规缺口请求关键帧，避免 host 不重传时永久卡住。
                let grace_live = self.in_startup_window();
                // 槽位缺席 = 该 seq 从未到达（已登记 missing，或落在 pre-window
                // 之外而从未被 NACK）。比 missing 集合判定更健壮：超出 NACK
                // 窗口的缺口同样能扣住帧；只有真正“帧头”（前槽是别的包的
                // 槽位）才放行。
                let boundary_seq = start.wrapping_sub(1);
                let boundary_absent = self
                    .buffer
                    .get(boundary_seq as usize % self.buffer.len())
                    .map(|slot| {
                        slot.as_ref()
                            .is_none_or(|entry| entry.packet.sequence_number != boundary_seq)
                    })
                    .unwrap_or(true);
                if grace_live && boundary_absent {
                    let truncated = !self.buffer[start as usize % self.buffer.len()]
                        .as_ref()
                        .expect("frame start slot")
                        .packet
                        .is_first_packet_in_frame;
                    let mut has_sps = false;
                    let mut has_pps = false;
                    let mut has_idr = false;
                    let mut probe = start;
                    let mut guard = 0;
                    let frame_end = sequence_number.wrapping_add(1);
                    while probe != frame_end && guard < self.buffer.len() {
                        guard += 1;
                        if let Some(entry) =
                            self.buffer[probe as usize % self.buffer.len()].as_ref()
                        {
                            has_sps |= entry.packet.sps;
                            has_pps |= entry.packet.pps;
                            has_idr |= entry.packet.idr;
                        }
                        probe = probe.wrapping_add(1);
                    }
                    let parameter_gap = match self.buffer[index]
                        .as_ref()
                        .expect("last frame packet")
                        .packet
                        .codec
                    {
                        VideoCodecKind::H264 => has_idr && (!has_sps || !has_pps),
                        VideoCodecKind::H265 => {
                            (has_sps || has_pps || has_idr) && (!has_sps || !has_pps)
                        }
                        VideoCodecKind::Av1 => false,
                    };
                    if truncated || parameter_gap {
                        self.pending_rescan = Some(sequence_number);
                        break;
                    }
                }
                let end = sequence_number.wrapping_add(1);
                let mut validate = start;
                while validate != end {
                    if self.buffer[validate as usize % self.buffer.len()]
                        .as_ref()
                        .is_some_and(|entry| entry.packet.nalus.len() > 9)
                    {
                        return frames;
                    }
                    validate = validate.wrapping_add(1);
                }
                let mut cursor = start;
                let mut data = Vec::new();
                let mut sps = false;
                let mut pps = false;
                let mut idr = false;
                let mut parameter_format = None;
                let mut color_space = None;
                let mut rotation = None;
                let mut content_type = None;
                let mut video_capture_index = None;
                let mut is_new_picture = None;
                let mut video_timing = None;
                let mut frame_sending_delay_ms = None;
                let mut playout_delay = None;
                let mut nack_count = 0;
                let mut frame_pps_id = None;
                let mut received_at: Option<Instant> = None;
                let mut last_received_at: Option<Instant> = None;
                let timestamp = self.buffer[index]
                    .as_ref()
                    .expect("last frame packet")
                    .packet
                    .timestamp;
                let codec = self.buffer[index]
                    .as_ref()
                    .expect("last frame packet")
                    .packet
                    .codec;
                while cursor != end {
                    if self.received_padding.remove(&cursor) {
                        cursor = cursor.wrapping_add(1);
                        continue;
                    }
                    let slot = cursor as usize % self.buffer.len();
                    let Some(packet) = self.buffer[slot].take() else {
                        break;
                    };
                    if cursor == start {
                        playout_delay = packet.packet.playout_delay;
                    }
                    nack_count = nack_count.max(packet.packet.nack_count);
                    sps |= packet.packet.sps;
                    pps |= packet.packet.pps;
                    idr |= packet.packet.idr;
                    frame_pps_id = frame_pps_id.or_else(|| {
                        packet
                            .packet
                            .nalus
                            .iter()
                            .find(|n| matches!(n.kind, 1 | 5) && n.pps >= 0)
                            .map(|n| n.pps)
                    });
                    parameter_format = packet.packet.parameter_format.or(parameter_format);
                    if packet.packet.marker {
                        color_space = packet.packet.color_space;
                    }
                    rotation = rotation.or(packet.packet.rotation);
                    content_type = content_type.or(packet.packet.content_type);
                    video_capture_index = video_capture_index.or(packet.packet.video_capture_index);
                    // First valid marker in sequence order, including a false
                    // value; never inherit the previous frame's marker.
                    is_new_picture = is_new_picture.or(packet.packet.is_new_picture);
                    video_timing = video_timing.or(packet.packet.video_timing);
                    frame_sending_delay_ms =
                        frame_sending_delay_ms.or(packet.packet.frame_sending_delay_ms);
                    received_at = Some(received_at.map_or(packet.packet.received_at, |current| {
                        current.min(packet.packet.received_at)
                    }));
                    last_received_at = Some(
                        last_received_at.map_or(packet.packet.received_at, |current| {
                            current.max(packet.packet.received_at)
                        }),
                    );
                    data.extend_from_slice(&packet.packet.payload);
                    cursor = cursor.wrapping_add(1);
                }
                if codec == VideoCodecKind::Av1 {
                    match crate::media::av1::assemble(&data) {
                        Ok(bytes) => {
                            parameter_format = crate::media::av1::format(&bytes);
                            data = bytes;
                        }
                        Err(error) => {
                            tracing::debug!(%error,"invalid AV1 RTP frame");
                            self.malformed_av1 = true;
                            sequence_number = sequence_number.wrapping_add(1);
                            continue;
                        }
                    }
                }
                let keyframe = match codec {
                    VideoCodecKind::H264 => idr,
                    VideoCodecKind::H265 => sps && pps && idr,
                    VideoCodecKind::Av1 => idr && parameter_format.is_some(),
                };
                // 帧级参数集补齐：关键帧首片丢失（NACK 恢复出的是截断帧，
                // 没有 S=1 片供包级 prepend 挂靠），但跟踪表已有 SPS/PPS
                // （重传的 STAP-A 或 sprop）时，把成对字节拼到帧首，
                // 让解码器仍能初始化。任一残留切片都可解析 pps_id。
                if keyframe
                    && codec == VideoCodecKind::H264
                    && (!sps || !pps)
                    && let Some(pps_id) = frame_pps_id
                    && let Some((sps_id, pps_id)) = self.parameters.h264_pair_for(pps_id)
                    && let Some(pair) = self.parameters.h264_pair_annexb(sps_id, pps_id)
                {
                    tracing::debug!(
                        ?start,
                        sps_id,
                        pps_id,
                        "keyframe missing parameter sets; prepending tracked pair"
                    );
                    let mut complete = Vec::with_capacity(pair.len() + data.len());
                    complete.extend_from_slice(&pair);
                    complete.append(&mut data);
                    data = complete;
                }
                if keyframe {
                    self.last_keyframe_first_sequence = Some(start);
                }
                frames.push(EncodedFrame {
                    codec,
                    id: 0,
                    references: Vec::new(),
                    first_sequence_number: start,
                    last_sequence_number: sequence_number,
                    timestamp,
                    keyframe,
                    color_space,
                    rotation: rotation.unwrap_or(0),
                    content_type: content_type.unwrap_or(0),
                    video_capture_index,
                    is_new_picture,
                    video_timing,
                    frame_sending_delay_ms,
                    playout_delay,
                    nack_count,
                    received_at: received_at.expect("complete frame has at least one packet"),
                    last_received_at: last_received_at
                        .expect("complete frame has at least one packet"),
                    assembled_at: Instant::now(),
                    data,
                    parameter_format,
                });
                self.missing_packets
                    .retain(|missing| *missing > sequence_number);
                if self.pending_rescan == Some(sequence_number) {
                    self.pending_rescan = None;
                }
            }
            sequence_number = sequence_number.wrapping_add(1);
        }
        frames
    }

    pub(super) fn update_missing(&mut self, sequence_number: u16) {
        let Some(mut newest) = self.newest_inserted_sequence else {
            self.newest_inserted_sequence = Some(sequence_number);
            return;
        };
        if ahead_of(sequence_number, newest) {
            let old = sequence_number.wrapping_sub(10_000);
            self.missing_packets
                .retain(|value| sequence_at_or_ahead(*value, old));
            if ahead_of(old, newest) {
                newest = old;
            }
            newest = newest.wrapping_add(1);
            while ahead_of(sequence_number, newest) {
                self.missing_packets.insert(newest);
                newest = newest.wrapping_add(1);
            }
            self.newest_inserted_sequence = Some(sequence_number);
        } else {
            self.missing_packets.remove(&sequence_number);
        }
    }
}

impl PacketInsertResult {
    pub(super) fn empty() -> Self {
        Self {
            frames: Vec::new(),
            duplicate: false,
            cleared: false,
            parameter_rejected: false,
        }
    }
}

impl SequenceUnwrapper {
    pub(super) fn unwrap(&mut self, value: u16) -> i64 {
        let value = i64::from(value);
        let Some(last) = self.last else {
            self.last = Some(value);
            return value;
        };
        let base = last & !0xffff;
        let candidates = [base + value, base + value - 65_536, base + value + 65_536];
        let unwrapped = *candidates
            .iter()
            .min_by_key(|candidate| (*candidate - last).abs())
            .expect("sequence candidates");
        self.last = Some(unwrapped);
        unwrapped
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use webrtc::rtp::{header::Header, packet::Packet};

    fn packet(
        sequence_number: u16,
        timestamp: u32,
        marker: bool,
        payload: &[u8],
    ) -> ParsedVideoPacket {
        let rtp = Packet {
            header: Header {
                sequence_number,
                timestamp,
                marker,
                ..Default::default()
            },
            payload: bytes::Bytes::copy_from_slice(payload),
        };
        super::super::depacketize::depacketize_h264(&rtp, Instant::now()).unwrap()
    }

    /// A P frame sent as three FU-A fragments (type 1), without its first.
    fn p_frame_tail(buffer: &mut PacketBuffer, start: u16, timestamp: u32) -> Vec<EncodedFrame> {
        let mut frames = buffer
            .insert(packet(
                start + 1,
                timestamp,
                false,
                &[0x7c, 0x01, 0xaa, 0xbb],
            ))
            .frames;
        frames.extend(
            buffer
                .insert(packet(
                    start + 2,
                    timestamp,
                    true,
                    &[0x7c, 0x41, 0xcc, 0xdd],
                ))
                .frames,
        );
        frames
    }

    #[test]
    fn mid_session_frame_missing_its_start_is_not_assembled() {
        let mut buffer = PacketBuffer::new();
        // A whole IDR slice in one packet starts the session.
        buffer.insert(packet(100, 1_000, true, &[0x65, 0x88, 0x84, 0x00, 0x10]));
        buffer.anchored_at = Some(Instant::now() - PRE_WINDOW_GRACE * 2);
        buffer.rescan_pending();
        assert!(p_frame_tail(&mut buffer, 101, 4_000).is_empty());
    }

    #[test]
    fn startup_frame_missing_its_start_waits_then_gives_up() {
        let mut buffer = PacketBuffer::new();
        assert!(p_frame_tail(&mut buffer, 101, 4_000).is_empty());
        assert!(
            buffer.pending_rescan.is_some(),
            "held for the pre-window NACK"
        );
        buffer.anchored_at = Some(Instant::now() - PRE_WINDOW_GRACE * 2);
        assert!(buffer.rescan_pending().is_empty());
        assert!(buffer.pending_rescan.is_none());
    }
}
