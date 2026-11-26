//! Receive-side parameter tracking, not a decoder configuration fallback.
//! UU streamer 4FA902 / 4FD8CC run these decisions before PacketBuffer.

use crate::media::video_format::{
    self, BitReader, VideoFormatSignature, remove_emulation_prevention,
};
use base64::Engine;
use std::collections::{HashMap, HashSet};

#[derive(Clone, Copy, Debug)]
pub(crate) struct NaluInfo {
    pub kind: u8,
    pub vps: i32,
    pub sps: i32,
    pub pps: i32,
    pub format: Option<VideoFormatSignature>,
}

impl NaluInfo {
    pub(crate) fn unknown(kind: u8) -> Self {
        Self {
            kind,
            vps: -1,
            sps: -1,
            pps: -1,
            format: None,
        }
    }
}

#[derive(Default)]
struct Sps {
    parent: i32,
    format: Option<VideoFormatSignature>,
    out_of_band: Option<Vec<u8>>,
    /// 带内（单 NAL 包）收到的裸 SPS；与 out_of_band 一样用于关键帧前拼接。
    in_band: Option<Vec<u8>>,
}

#[derive(Default)]
struct Pps {
    sps: i32,
    out_of_band: Option<Vec<u8>>,
    /// 带内（单 NAL 包）收到的裸 PPS。
    in_band: Option<Vec<u8>>,
}

#[derive(Default)]
pub(crate) struct ParameterTracker {
    h264_sps: HashMap<i32, Sps>,
    h264_pps: HashMap<i32, Pps>,
    h265_vps: HashSet<i32>,
    h265_sps: HashMap<i32, Sps>,
    h265_pps: HashMap<i32, Pps>,
}

impl ParameterTracker {
    pub(crate) fn install_h264_sprop(&mut self, fmtp: &str) {
        let Some(value) = fmtp.split(';').find_map(|part| {
            let (name, value) = part.trim().split_once('=')?;
            name.eq_ignore_ascii_case("sprop-parameter-sets")
                .then_some(value.trim())
        }) else {
            return;
        };
        let parsed = (|| {
            // UU's sprop parser consumes one comma-separated SPS/PPS pair.
            let (sps, pps) = value.split_once(',')?;
            let sps = base64::engine::general_purpose::STANDARD.decode(sps).ok()?;
            let pps = base64::engine::general_purpose::STANDARD.decode(pps).ok()?;
            if sps.first()? & 31 != 7 || pps.first()? & 31 != 8 {
                return None;
            }
            let header = h264_sps_header(&sps)?;
            let (pps_id, sps_id) = h264_pps_full_ids(&pps)?;
            Some((sps, pps, header.id, pps_id, sps_id))
        })();
        let Some((sps, pps, id, pps_id, parent)) = parsed else {
            tracing::warn!("ignoring malformed H.264 out-of-band parameter pair");
            return;
        };
        self.h264_sps.insert(
            id,
            Sps {
                format: video_format::parse_h264_sps(&sps),
                out_of_band: Some(sps),
                ..Default::default()
            },
        );
        self.h264_pps.insert(
            pps_id,
            Pps {
                sps: parent,
                out_of_band: Some(pps),
                in_band: None,
            },
        );
        tracing::debug!(
            sps_id = id,
            pps_id,
            "installed H.264 out-of-band parameter pair"
        );
    }

    /// 把载荷里带内下发的 SPS/PPS 裸 NAL（不含起始码）记入跟踪表。
    fn store_in_band_parameters(
        sps_map: &mut HashMap<i32, Sps>,
        pps_map: &mut HashMap<i32, Pps>,
        payload: &[u8],
    ) {
        for unit in annexb_units(payload) {
            match unit.first().map(|byte| byte & 31) {
                Some(7) => {
                    if let Some(header) = h264_sps_header(unit) {
                        sps_map.entry(header.id).or_default().in_band = Some(unit.to_vec());
                    }
                }
                Some(8) => {
                    if let Some((pps_id, sps_id)) = h264_pps_full_ids(unit) {
                        let entry = pps_map.entry(pps_id).or_default();
                        entry.in_band = Some(unit.to_vec());
                        entry.sps = sps_id;
                    }
                }
                _ => {}
            }
        }
    }

    /// 查找 PPS 对应的 SPS，并确认成对字节可用（带内或带外）。
    pub(crate) fn h264_pair_for(&self, pps_id: i32) -> Option<(i32, i32)> {
        let pps = self.h264_pps.get(&pps_id)?;
        if pps.in_band.is_none() && pps.out_of_band.is_none() {
            return None;
        }
        let sps = self.h264_sps.get(&pps.sps)?;
        if sps.in_band.is_none() && sps.out_of_band.is_none() {
            return None;
        }
        Some((pps.sps, pps_id))
    }

    /// SPS/PPS 成对的 annexb 字节（带起始码），用于帧级前拼接。
    pub(crate) fn h264_pair_annexb(&self, sps_id: i32, pps_id: i32) -> Option<Vec<u8>> {
        let sps = self.h264_sps.get(&sps_id)?;
        let sps = sps.in_band.as_ref().or_else(|| sps.out_of_band.as_ref())?;
        let pps = self.h264_pps.get(&pps_id)?;
        let pps = pps.in_band.as_ref().or_else(|| pps.out_of_band.as_ref())?;
        let mut output = Vec::with_capacity(sps.len() + pps.len() + 8);
        output.extend_from_slice(&[0, 0, 0, 1]);
        output.extend_from_slice(sps);
        output.extend_from_slice(&[0, 0, 0, 1]);
        output.extend_from_slice(pps);
        Some(output)
    }

    pub(crate) fn h264(
        &mut self,
        nalus: &mut Vec<NaluInfo>,
        first: bool,
        payload: &mut Vec<u8>,
    ) -> Result<Option<VideoFormatSignature>, &'static str> {
        // 先记录带内 SPS/PPS。部分 host（macOS）用单独的单 NAL 包下发参数集，
        // 关键帧本身不再携带；libwebrtc 接收端同样会把先收到的参数集拼到
        // 关键帧前面，否则解码器拿不到 extradata。
        // Only packets that carry one are scanned: this runs for every packet.
        if nalus.iter().any(|n| matches!(n.kind, 7 | 8)) {
            Self::store_in_band_parameters(&mut self.h264_sps, &mut self.h264_pps, payload);
        }
        let mut format = None;
        let mut prefix_pair = None;
        for nalu in nalus.iter() {
            match nalu.kind {
                7 => {
                    self.h264_sps.entry(nalu.sps).or_default().format = nalu.format;
                }
                8 => {
                    self.h264_pps.entry(nalu.pps).or_default().sps = nalu.sps;
                }
                5 if first => {
                    if nalu.pps == -1 {
                        return Err("H.264 IDR omitted PPS id");
                    }
                    let pps = self
                        .h264_pps
                        .get(&nalu.pps)
                        .ok_or("H.264 IDR references missing PPS")?;
                    let sps = self
                        .h264_sps
                        .get(&pps.sps)
                        .ok_or("H.264 PPS references missing SPS")?;
                    format = sps.format;
                    let packet_carries_parameters =
                        nalus.iter().any(|n| n.kind == 7) && nalus.iter().any(|n| n.kind == 8);
                    if !packet_carries_parameters
                        && (sps.in_band.is_some() || sps.out_of_band.is_some())
                        && (pps.in_band.is_some() || pps.out_of_band.is_some())
                    {
                        prefix_pair = Some((pps.sps, nalu.pps));
                    }
                }
                _ => {}
            }
        }
        if let Some((sps_id, pps_id)) = prefix_pair {
            let sps = self.h264_sps[&sps_id]
                .in_band
                .as_ref()
                .or(self.h264_sps[&sps_id].out_of_band.as_ref())
                .expect("checked stored SPS");
            let pps = self.h264_pps[&pps_id]
                .in_band
                .as_ref()
                .or(self.h264_pps[&pps_id].out_of_band.as_ref())
                .expect("checked stored PPS");
            let mut output = Vec::with_capacity(sps.len() + pps.len() + payload.len() + 8);
            output.extend_from_slice(&[0, 0, 0, 1]);
            output.extend_from_slice(sps);
            output.extend_from_slice(&[0, 0, 0, 1]);
            output.extend_from_slice(pps);
            output.append(payload);
            *payload = output;
            if nalus.len() + 2 <= 10 {
                let mut sps_info = NaluInfo::unknown(7);
                sps_info.sps = sps_id;
                sps_info.format = format;
                let mut pps_info = NaluInfo::unknown(8);
                pps_info.sps = sps_id;
                pps_info.pps = pps_id;
                nalus.extend([sps_info, pps_info]);
            }
        }
        Ok(format)
    }

    pub(crate) fn h265(
        &mut self,
        nalus: &[NaluInfo],
        first: bool,
    ) -> Result<Option<VideoFormatSignature>, &'static str> {
        let mut format = None;
        for nalu in nalus {
            match nalu.kind {
                32 => {
                    self.h265_vps.insert(nalu.vps);
                }
                33 => {
                    let sps = self.h265_sps.entry(nalu.sps).or_default();
                    sps.parent = nalu.vps;
                    sps.format = nalu.format;
                }
                34 => {
                    self.h265_pps.entry(nalu.pps).or_default().sps = nalu.sps;
                }
                19..=21 if first => {
                    if nalu.pps == -1 {
                        return Err("H.265 IRAP omitted PPS id");
                    }
                    let pps = self
                        .h265_pps
                        .get(&nalu.pps)
                        .ok_or("H.265 IRAP references missing PPS")?;
                    let sps = self
                        .h265_sps
                        .get(&pps.sps)
                        .ok_or("H.265 PPS references missing SPS")?;
                    if !self.h265_vps.contains(&sps.parent) {
                        return Err("H.265 SPS references missing VPS");
                    }
                    format = sps.format;
                }
                _ => {}
            }
        }
        // The shipped receive configuration has no H.265 out-of-band setter:
        // the three map insertion functions are only called from the tracker.
        // Do not invent a sprop-vps/sps/pps wire capability or raw-NAL cache.
        Ok(format)
    }
}

/// 按 Annex-B 起始码切分载荷，返回各 NAL 的裸字节（不含起始码）。
fn annexb_units(payload: &[u8]) -> Vec<&[u8]> {
    let mut units = Vec::new();
    let mut cursor = 0usize;
    while cursor + 3 <= payload.len() {
        let header_len = if payload[cursor..].starts_with(&[0, 0, 0, 1]) {
            4
        } else if payload[cursor..].starts_with(&[0, 0, 1]) {
            3
        } else {
            cursor += 1;
            continue;
        };
        let start = cursor + header_len;
        let mut end = start;
        while end + 3 <= payload.len() && !payload[end..].starts_with(&[0, 0, 1]) {
            end += 1;
        }
        if end + 3 > payload.len() {
            end = payload.len();
        }
        if start < end {
            units.push(&payload[start..end]);
        }
        cursor = end;
    }
    units
}

pub(crate) fn h264_nalu_info(unit: &[u8], fragmented: bool) -> NaluInfo {
    let kind = unit[0] & 31;
    let mut info = NaluInfo::unknown(kind);
    if fragmented {
        info.pps = slice_pps(&unit[1..], false, kind).unwrap_or(-1);
        return info;
    }
    match kind {
        1 | 5 => info.pps = slice_pps(&unit[1..], false, kind).unwrap_or(-1),
        7 => {
            if let Some(header) = h264_sps_header(unit) {
                info.sps = header.id;
                info.format = video_format::parse_h264_sps(unit);
            }
        }
        8 => {
            if let Some((pps, sps)) = parameter_pair(&unit[1..]) {
                info.pps = pps;
                info.sps = sps;
            }
        }
        _ => {}
    }
    info
}

pub(crate) fn h265_nalu_info(unit: &[u8], fragmented: bool) -> NaluInfo {
    let kind = (unit[0] >> 1) & 63;
    let mut info = NaluInfo::unknown(kind);
    if fragmented {
        info.pps = slice_pps(&unit[2..], true, kind).unwrap_or(-1);
        return info;
    }
    match kind {
        0 | 1 | 19..=21 => info.pps = slice_pps(&unit[2..], true, kind).unwrap_or(-1),
        32 => info.vps = unit.get(2).map_or(-1, |byte| i32::from(byte >> 4)),
        33 => {
            if let Some((vps, sps)) = h265_sps_ids(unit) {
                info.vps = vps;
                info.sps = sps;
                info.format = video_format::parse_h265_sps(unit);
            }
        }
        34 => {
            if let Some((pps, sps)) = parameter_pair(&unit[2..]) {
                info.pps = pps;
                info.sps = sps;
            }
        }
        _ => {}
    }
    info
}

fn parameter_pair(ebsp: &[u8]) -> Option<(i32, i32)> {
    let rbsp = remove_emulation_prevention(ebsp);
    let mut reader = BitReader::new(&rbsp);
    Some((reader.read_ue()? as i32, reader.read_ue()? as i32))
}

// The metadata SPS parser (71296E) consumes through temporal_mvp_enabled,
// including scaling lists and every RPS. Reading only width/height would
// accept a truncated SPS which UU does not insert under a valid SPS id.
fn h265_sps_ids(unit: &[u8]) -> Option<(i32, i32)> {
    let rbsp = remove_emulation_prevention(unit.get(2..)?);
    let mut r = BitReader::new(&rbsp);
    let vps = r.read_bits(4)? as i32;
    let layers = r.read_bits(3)? as usize;
    r.read_bit()?;
    video_format::skip_h265_profile_tier_level(&mut r, layers)?;
    let sps = r.read_ue()? as i32;
    let chroma = r.read_ue()?;
    if chroma == 3 {
        r.read_bit()?;
    }
    r.read_ue()?;
    r.read_ue()?;
    if r.read_bit()? != 0 {
        for _ in 0..4 {
            r.read_ue()?;
        }
    }
    r.read_ue()?;
    r.read_ue()?;
    let poc_bits = r.read_ue()?.checked_add(4)?;
    let first = if r.read_bit()? != 0 { 0 } else { layers };
    for _ in first..=layers {
        r.read_ue()?;
        r.read_ue()?;
        r.read_ue()?;
    }
    for _ in 0..6 {
        r.read_ue()?;
    }
    if r.read_bit()? != 0 && r.read_bit()? != 0 {
        for size_id in 0..4 {
            for _matrix in (0..6).step_by(if size_id == 3 { 3 } else { 1 }) {
                if r.read_bit()? != 0 {
                    if size_id >= 2 {
                        r.read_se()?;
                    }
                    for _ in 0..(if size_id == 0 { 16 } else { 64 }) {
                        r.read_se()?;
                    }
                } else {
                    r.read_ue()?;
                }
            }
        }
    }
    r.read_bit()?;
    r.read_bit()?;
    if r.read_bit()? != 0 {
        r.skip_bits(8)?;
        r.read_ue()?;
        r.read_ue()?;
        r.read_bit()?;
    }
    let sets = r.read_ue()?;
    let mut previous_delta_count = 0_u32;
    for index in 0..sets {
        if index != 0 && r.read_bit()? != 0 {
            r.read_bit()?;
            r.read_ue()?;
            let mut count = 0;
            for _ in 0..=previous_delta_count {
                let used = r.read_bit()? != 0;
                if used || r.read_bit()? != 0 {
                    count += 1;
                }
            }
            previous_delta_count = count;
        } else {
            let negatives = r.read_ue()?;
            let positives = r.read_ue()?;
            previous_delta_count = negatives.checked_add(positives)?;
            for _ in 0..previous_delta_count {
                r.read_ue()?;
                r.read_bit()?;
            }
        }
    }
    if r.read_bit()? != 0 {
        for _ in 0..r.read_ue()? {
            r.skip_bits(poc_bits.try_into().ok()?)?;
            r.read_bit()?;
        }
    }
    r.read_bit()?;
    Some((vps, sps))
}

fn slice_pps(ebsp: &[u8], hevc: bool, kind: u8) -> Option<i32> {
    let rbsp = remove_emulation_prevention(ebsp);
    let mut reader = BitReader::new(&rbsp);
    if hevc {
        reader.read_bit()?;
        if (16..=23).contains(&kind) {
            reader.read_bit()?;
        }
    } else {
        reader.read_ue()?;
        reader.read_ue()?;
    }
    Some(reader.read_ue()? as i32)
}

struct H264SpsHeader {
    id: i32,
    max_refs: u32,
    rbsp: Vec<u8>,
    vui_bit: usize,
    vui_present: bool,
}

fn h264_sps_header(unit: &[u8]) -> Option<H264SpsHeader> {
    let rbsp = remove_emulation_prevention(unit.get(1..)?);
    let mut reader = BitReader::new(&rbsp);
    let profile = reader.read_bits(8)?;
    reader.skip_bits(16)?;
    let id = reader.read_ue()? as i32;
    if matches!(
        profile,
        44 | 83 | 86 | 100 | 110 | 118 | 122 | 128 | 134 | 138 | 139 | 244
    ) {
        let chroma = reader.read_ue()?;
        if chroma == 3 {
            reader.read_bit()?;
        }
        reader.read_ue()?;
        reader.read_ue()?;
        reader.read_bit()?;
        if reader.read_bit()? != 0 {
            for i in 0..(if chroma == 3 { 12 } else { 8 }) {
                if reader.read_bit()? != 0 {
                    let mut last = 8_i32;
                    let mut next = 8_i32;
                    for _ in 0..(if i < 6 { 16 } else { 64 }) {
                        if next != 0 {
                            let delta = reader.read_se()?;
                            if !(-128..=127).contains(&delta) {
                                return None;
                            }
                            next = (last + delta) & 255;
                        }
                        if next != 0 {
                            last = next;
                        }
                    }
                }
            }
        }
    }
    if reader.read_ue()? > 28 {
        return None;
    }
    match reader.read_ue()? {
        0 => {
            if reader.read_ue()? > 28 {
                return None;
            }
        }
        1 => {
            reader.read_bit()?;
            reader.read_ue()?;
            reader.read_ue()?;
            for _ in 0..reader.read_ue()? {
                reader.read_ue()?;
            }
        }
        _ => {}
    }
    let max_refs = reader.read_ue()?;
    reader.read_bit()?;
    reader.read_ue()?;
    reader.read_ue()?;
    if reader.read_bit()? == 0 {
        reader.read_bit()?;
    }
    reader.read_bit()?;
    if reader.read_bit()? != 0 {
        for _ in 0..4 {
            reader.read_ue()?;
        }
    }
    let vui_bit = reader.position();
    let vui_present = reader.read_bit()? != 0;
    Some(H264SpsHeader {
        id,
        max_refs,
        rbsp,
        vui_bit,
        vui_present,
    })
}

fn h264_pps_full_ids(unit: &[u8]) -> Option<(i32, i32)> {
    let rbsp = remove_emulation_prevention(unit.get(1..)?);
    let mut r = BitReader::new(&rbsp);
    let pps = r.read_ue()? as i32;
    let sps = r.read_ue()? as i32;
    r.skip_bits(2)?;
    let groups = r.read_ue()?;
    if groups != 0 {
        match r.read_ue()? {
            0 => {
                for _ in 0..=groups {
                    r.read_ue()?;
                }
            }
            2 => {
                for _ in 0..=groups {
                    r.read_ue()?;
                    r.read_ue()?;
                }
            }
            3..=5 => {
                r.read_bit()?;
                r.read_ue()?;
            }
            6 => {
                let size = r.read_ue()?.checked_add(1)?;
                // 28EC70 bsr / xor -32 / add 34: floor(log2(groups))+2.
                let bits = 33 - groups.leading_zeros();
                let skip = u64::from(size) * u64::from(bits);
                if skip >= (1 << 31) {
                    return None;
                }
                r.skip_bits(skip.try_into().ok()?)?;
            }
            _ => {}
        }
    }
    r.read_ue()?;
    r.read_ue()?;
    r.skip_bits(3)?;
    if !(-26..=25).contains(&r.read_se()?) {
        return None;
    }
    r.read_ue()?;
    r.read_ue()?;
    r.skip_bits(3)?;
    Some((pps, sps))
}

/// Preserve video signal/colour information; the receive caller passes no
/// colour override. Rewrite only the official DPB/reorder restriction.
pub(crate) fn rewrite_h264_sps(unit: &[u8]) -> Option<Vec<u8>> {
    let sps = h264_sps_header(unit)?;
    let mut r = BitReader::new(&sps.rbsp);
    r.skip_bits(sps.vui_bit + 1)?;
    let mut edits = vec![(sps.vui_bit, sps.vui_bit + 1, vec![true])];
    if sps.vui_present {
        if r.read_bit()? != 0 && r.read_bits(8)? == 255 {
            r.skip_bits(32)?;
        }
        if r.read_bit()? != 0 {
            r.read_bit()?;
        }
        if r.read_bit()? != 0 {
            r.skip_bits(4)?;
            if r.read_bit()? != 0 {
                r.skip_bits(24)?;
            }
        }
        if r.read_bit()? != 0 {
            r.read_ue()?;
            r.read_ue()?;
        }
        if r.read_bit()? != 0 {
            r.skip_bits(65)?;
        }
        let nal_hrd = r.read_bit()? != 0;
        if nal_hrd {
            skip_hrd(&mut r)?;
        }
        let vcl_hrd = r.read_bit()? != 0;
        if vcl_hrd {
            skip_hrd(&mut r)?;
        }
        if nal_hrd || vcl_hrd {
            r.read_bit()?;
        }
        r.read_bit()?;
        let restriction_bit = r.position();
        if r.read_bit()? != 0 {
            r.read_bit()?;
            for _ in 0..4 {
                r.read_ue()?;
            }
            let start = r.position();
            let reorder = r.read_ue()?;
            let buffering = r.read_ue()?;
            if reorder == 0 && buffering <= sps.max_refs {
                return None;
            }
            let mut bits = ue_bits(0);
            bits.extend(ue_bits(sps.max_refs));
            edits.push((start, r.position(), bits));
        } else {
            let mut bits = vec![true];
            bits.extend(restriction_bits(sps.max_refs));
            edits.push((restriction_bit, restriction_bit + 1, bits));
        }
    } else {
        let mut bits = vec![false; 8];
        bits.push(true);
        bits.extend(restriction_bits(sps.max_refs));
        edits.push((r.position(), r.position(), bits));
    }
    let mut bits = Vec::new();
    let mut consumed = 0;
    for (start, end, replacement) in edits {
        copy_bits(&sps.rbsp, consumed, start, &mut bits);
        bits.extend(replacement);
        consumed = end;
    }
    copy_bits(&sps.rbsp, consumed, sps.rbsp.len() * 8, &mut bits);
    let mut rbsp = vec![0_u8; bits.len().div_ceil(8)];
    for (i, bit) in bits.into_iter().enumerate() {
        if bit {
            rbsp[i / 8] |= 0x80 >> (i % 8);
        }
    }
    let mut output = vec![unit[0]];
    let mut zeroes = 0;
    for byte in rbsp {
        if zeroes >= 2 && byte <= 3 {
            output.push(3);
            zeroes = 0;
        }
        output.push(byte);
        zeroes = if byte == 0 { zeroes + 1 } else { 0 };
    }
    Some(output)
}

fn skip_hrd(r: &mut BitReader<'_>) -> Option<()> {
    let count = r.read_ue()?;
    r.skip_bits(8)?;
    for _ in 0..=count {
        r.read_ue()?;
        r.read_ue()?;
        r.read_bit()?;
    }
    r.skip_bits(20)
}
fn restriction_bits(max_refs: u32) -> Vec<bool> {
    let mut bits = vec![true];
    for value in [2, 1, 16, 16, 0, max_refs] {
        bits.extend(ue_bits(value));
    }
    bits
}
fn ue_bits(value: u32) -> Vec<bool> {
    let code = u64::from(value) + 1;
    let width = (64 - code.leading_zeros()) as usize;
    let mut bits = vec![false; width - 1];
    for bit in (0..width).rev() {
        bits.push(code & (1 << bit) != 0);
    }
    bits
}
fn copy_bits(bytes: &[u8], start: usize, end: usize, output: &mut Vec<bool>) {
    output.extend((start..end).map(|bit| bytes[bit / 8] & (0x80 >> (bit % 8)) != 0));
}
