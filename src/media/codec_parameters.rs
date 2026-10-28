//! Receive-side parameter tracking, not a decoder configuration fallback.
//! These decisions run before PacketBuffer.

use crate::media::video_format::{
    self, BitReader, VideoFormatSignature, remove_emulation_prevention,
};
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
}

#[derive(Default)]
struct Pps {
    sps: i32,
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
    pub(crate) fn h264(
        &mut self,
        nalus: &mut Vec<NaluInfo>,
        first: bool,
    ) -> Result<Option<VideoFormatSignature>, &'static str> {
        let mut format = None;
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
                }
                _ => {}
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
        // The receive configuration has no H.265 out-of-band setter: parameter
        // sets are only inserted by the tracker.
        // Do not invent a sprop-vps/sps/pps wire capability or raw-NAL cache.
        Ok(format)
    }
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

// The metadata SPS parser consumes through temporal_mvp_enabled,
// including scaling lists and every RPS. Reading only width/height would
// accept a truncated SPS which must not be inserted under a valid SPS id.
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

/// Preserve video signal/colour information; the receive caller passes no
/// colour override. Rewrite only the DPB/reorder restriction.
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
