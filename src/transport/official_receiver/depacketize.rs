//! RTP payload and header-extension parsing.
use super::{ParsedVideoPacket, START_CODE, VideoCodecKind, VideoSendTiming};
use crate::media::codec_parameters::{NaluInfo, h264_nalu_info, h265_nalu_info, rewrite_h264_sps};
use std::time::Instant;
use webrtc::rtp::packet::Packet as RtpPacket;

pub(super) fn depacketize_h265(
    packet: &RtpPacket,
    received_at: Instant,
) -> Option<ParsedVideoPacket> {
    let payload = packet.payload.as_ref();
    if payload.len() < 2 {
        return None;
    }
    let kind = (payload[0] >> 1) & 63;
    if kind == 50 {
        return None;
    }
    let mut first = true;
    let mut output = Vec::new();
    let mut nalus = Vec::new();
    let mut packet_keyframe = false;
    if kind == 49 {
        if payload.len() < 3 {
            return None;
        }
        first = payload[2] & 0x80 != 0;
        let original = payload[2] & 63;
        packet_keyframe = (19..=21).contains(&original);
        if first {
            let mut unit = vec![(payload[0] & 0x81) | (original << 1), payload[1]];
            unit.extend_from_slice(&payload[3..]);
            nalus.push(h265_nalu_info(&unit, true));
            output.extend_from_slice(&START_CODE);
            output.extend(unit);
        } else {
            output.extend_from_slice(&payload[3..]);
        }
    } else {
        let mut units = Vec::new();
        if kind == 48 {
            if payload.len() < 5 {
                return None;
            }
            let mut offset = 2;
            while offset < payload.len() {
                let length =
                    u16::from_be_bytes(payload.get(offset..offset + 2)?.try_into().ok()?) as usize;
                offset += 2;
                if length < 2 {
                    return None;
                }
                units.push(payload.get(offset..offset + length)?);
                offset += length;
            }
        } else {
            units.push(payload);
        }
        for unit in units {
            let info = h265_nalu_info(unit, false);
            if (48..=50).contains(&info.kind) {
                return None;
            }
            packet_keyframe |= (19..=21).contains(&info.kind) || info.kind == 33;
            if nalus.len() < 10 {
                nalus.push(info);
            }
            output.extend_from_slice(&START_CODE);
            output.extend_from_slice(unit);
        }
    }
    let packet_idr = nalus.iter().any(|n| (19..=21).contains(&n.kind));
    parsed_packet(
        packet,
        VideoCodecKind::H265,
        first,
        packet_keyframe,
        packet_idr,
        nalus,
        output,
        received_at,
    )
}

#[allow(clippy::too_many_arguments)]
pub(super) fn parsed_packet(
    packet: &RtpPacket,
    codec: VideoCodecKind,
    first: bool,
    packet_keyframe: bool,
    packet_idr: bool,
    mut nalus: Vec<NaluInfo>,
    payload: Vec<u8>,
    received_at: Instant,
) -> Option<ParsedVideoPacket> {
    let sps_kind = if codec == VideoCodecKind::H264 { 7 } else { 33 };
    let pps_kind = if codec == VideoCodecKind::H264 { 8 } else { 34 };
    let parameter_format = nalus
        .iter()
        .filter(|n| n.kind == sps_kind)
        .filter_map(|n| n.format)
        .next_back();
    // The official RTP video header has one geometry slot per packet, which
    // all SPS entries in that packet copy into their tracker records.
    for nalu in &mut nalus {
        if nalu.kind == sps_kind {
            nalu.format = parameter_format;
        }
    }
    Some(ParsedVideoPacket {
        codec,
        sequence_number: packet.header.sequence_number,
        timestamp: packet.header.timestamp,
        marker: packet.header.marker,
        is_first_packet_in_frame: first,
        sps: nalus.iter().any(|n| n.kind == sps_kind && n.sps >= 0),
        pps: nalus.iter().any(|n| n.kind == pps_kind),
        // H.264 的 IDR 由 FU-A 原始类型（5）直接判定，避免关键帧首片早于
        // 接收窗口时漏判；H.265/AV1 仍按 NALU 类型。
        idr: packet_idr
            || nalus.iter().any(|n| {
                if codec == VideoCodecKind::H264 {
                    n.kind == 5
                } else {
                    (19..=21).contains(&n.kind)
                }
            }),
        packet_keyframe,
        nalus,
        parameter_format,
        color_space: None,
        rotation: None,
        content_type: None,
        video_capture_index: None,
        is_new_picture: None,
        video_timing: None,
        frame_sending_delay_ms: None,
        playout_delay: None,
        nack_count: 0,
        payload,
        received_at,
    })
}

pub(super) fn depacketize_av1(
    packet: &RtpPacket,
    received_at: Instant,
) -> Option<ParsedVideoPacket> {
    let h = *packet.payload.first()?;
    if h & 7 != 0 || h & 0x88 == 0x88 || packet.payload.len() < 2 {
        return None;
    }
    let mut payload = Vec::with_capacity(packet.payload.len() + 4);
    payload.extend_from_slice(&(packet.payload.len() as u32).to_le_bytes());
    payload.extend_from_slice(&packet.payload);
    let mut result = parsed_packet(
        packet,
        VideoCodecKind::Av1,
        h & 0x80 == 0,
        h & 8 != 0,
        h & 8 != 0,
        Vec::new(),
        payload,
        received_at,
    )?;
    result.idr = h & 8 != 0;
    Some(result)
}

pub(super) fn parse_video_timing(payload: &[u8]) -> Option<VideoSendTiming> {
    if payload.len() != 13 {
        return None;
    }
    let (flags, deltas) = (payload[0], &payload[1..]);
    if flags == u8::MAX {
        return None;
    }
    let read = |offset: usize| u16::from_be_bytes([deltas[offset], deltas[offset + 1]]);
    Some(VideoSendTiming {
        flags,
        encode_start_delta_ms: read(0),
        encode_finish_delta_ms: read(2),
        packetization_finish_delta_ms: read(4),
        pacer_exit_delta_ms: read(6),
        network_timestamp_delta_ms: read(8),
        network2_timestamp_delta_ms: read(10),
    })
}

pub(super) fn depacketize_h264(
    packet: &RtpPacket,
    received_at: Instant,
) -> Option<ParsedVideoPacket> {
    let payload = packet.payload.as_ref();
    let kind = *payload.first()? & 31;
    let mut first = true;
    let mut nalus = Vec::new();
    let mut output = Vec::new();
    let mut packet_keyframe = false;
    // libwebrtc 从 FU-A 的原始 NAL 类型判定关键帧，不依赖 S 位。部分 host
    // （macOS）的关键帧首片可能早于接收窗口，S=1 永远不会出现，只靠 NALU
    // 列表会漏掉整帧 IDR。
    let mut packet_idr = false;
    if kind == 28 {
        if payload.len() < 2 {
            return None;
        }
        first = payload[1] & 0x80 != 0;
        let original = payload[1] & 31;
        packet_keyframe = original == 5;
        packet_idr = original == 5;
        if first {
            let mut unit = vec![(payload[0] & 0xe0) | original];
            unit.extend_from_slice(&payload[2..]);
            nalus.push(h264_nalu_info(&unit, true));
            output.extend_from_slice(&START_CODE);
            output.extend(unit);
        } else {
            output.extend_from_slice(&payload[2..]);
        }
    } else {
        let mut units = Vec::new();
        if kind == 24 {
            if payload.len() < 4 {
                return None;
            }
            let mut offset = 1;
            while offset < payload.len() {
                let length =
                    u16::from_be_bytes(payload.get(offset..offset + 2)?.try_into().ok()?) as usize;
                offset += 2;
                if length == 0 {
                    return None;
                }
                units.push(payload.get(offset..offset + length)?);
                offset += length;
            }
        } else {
            units.push(payload);
        }
        let mut last_rewrite = None;
        for (index, unit) in units.iter().enumerate() {
            let info = h264_nalu_info(unit, false);
            if matches!(info.kind, 24 | 28) {
                return None;
            }
            packet_keyframe |= matches!(info.kind, 5 | 7);
            packet_idr |= info.kind == 5;
            if info.kind == 7
                && let Some(rewritten) = rewrite_h264_sps(unit)
            {
                last_rewrite = Some((index, rewritten));
            }
            if nalus.len() < 10 {
                nalus.push(info);
            }
        }
        // UU's STAP-A rewrite preserves the original packet prefix and
        // replaces the last SPS needing rewriting; do not chain replacements.
        for (index, unit) in units.iter().enumerate() {
            output.extend_from_slice(&START_CODE);
            if let Some((changed, bytes)) = last_rewrite.as_ref()
                && *changed == index
            {
                output.extend_from_slice(bytes);
            } else {
                output.extend_from_slice(unit);
            }
        }
    }
    parsed_packet(
        packet,
        VideoCodecKind::H264,
        first,
        packet_keyframe,
        packet_idr,
        nalus,
        output,
        received_at,
    )
}
