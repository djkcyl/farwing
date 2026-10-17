//! AV1 low-overhead OBUs and AOM RTP payloads. No decoder or GPU ownership.
use anyhow::{Context, Result, ensure};
use bytes::Bytes;
use openuuyc_codec::syntax::av1::{Chroma, Sequence, obu::Units};

pub(crate) fn leb(data: &[u8], at: &mut usize) -> Result<usize> {
    Ok(openuuyc_codec::syntax::av1::obu::leb128(data, at)?)
}
fn put_leb(mut value: usize, out: &mut Vec<u8>) {
    while value >= 128 {
        out.push(value as u8 | 128);
        value >>= 7;
    }
    out.push(value as u8);
}

pub(crate) fn units(data: &[u8]) -> Result<Units<'_>> {
    Ok(Units::new(data)?)
}

pub(crate) fn parameters(data: &[u8]) -> Option<Bytes> {
    let mut sequence = None;
    for unit in units(data).ok()? {
        let unit = unit.ok()?;
        if unit.kind == 1 && sequence.is_none() {
            sequence = Some(unit.bytes);
        }
    }
    sequence.map(Bytes::copy_from_slice)
}
pub(crate) fn format(data: &[u8]) -> Option<super::video_format::VideoFormatSignature> {
    // This runs on the receive task. Metadata parsing borrows the input and
    // needs neither a full frame parser nor copied sequence-header storage.
    let mut sequence = None;
    for unit in units(data).ok()? {
        let unit = unit.ok()?;
        if unit.kind == 1 && sequence.is_none() {
            sequence = Some(Sequence::parse(unit).ok()?);
        }
    }
    let s = sequence?;
    let w = s.max_width;
    let h = s.max_height;
    Some(super::video_format::VideoFormatSignature {
        profile_idc: s.profile,
        coded_width: w,
        coded_height: h,
        visible_width: w,
        visible_height: h,
        crop_left: 0,
        crop_top: 0,
        chroma_format_idc: match s.chroma {
            Chroma::Monochrome => 0,
            Chroma::Yuv420 => 1,
            Chroma::Yuv422 => 2,
            Chroma::Yuv444 => 3,
        },
        bit_depth_luma: s.depth,
        bit_depth_chroma: s.depth,
    })
}

/// W=0 aggregation keeps sequence/frame headers together when they fit. Every
/// OBU element carries its own LEB128 length; fragments never cross frames.
pub(crate) fn payloads(mtu: usize, data: &[u8], key: bool) -> Result<Vec<Bytes>> {
    ensure!(mtu > 4, "AV1 RTP payload limit too small");
    let mut packets = Vec::new();
    let mut packet = vec![if key { 8 } else { 0 }];
    for unit in units(data)? {
        let unit = unit?.bytes;
        let typ = (unit[0] >> 3) & 15;
        if matches!(typ, 2 | 15) {
            continue;
        }
        let header_len = 1 + usize::from(unit[0] & 4 != 0);
        let mut at = header_len;
        let size = leb(unit, &mut at)?;
        let mut wire = Vec::with_capacity(header_len + size);
        wire.extend_from_slice(&unit[..header_len]);
        wire[0] &= !2;
        wire.extend_from_slice(&unit[at..]);
        let mut offset = 0;
        while offset < wire.len() {
            if mtu - packet.len() < 2 {
                packets.push(Bytes::from(packet));
                packet = vec![0];
            }
            if packet.len() == 1 && offset > 0 {
                packet[0] |= 0x80;
            }
            let space = mtu - packet.len();
            let mut take = (wire.len() - offset).min(space - 1);
            loop {
                let len = if take == 0 {
                    1
                } else {
                    (usize::BITS - take.leading_zeros()).div_ceil(7) as usize
                };
                if take + len <= space {
                    break;
                }
                take -= 1;
            }
            ensure!(take > 0, "AV1 RTP fragment capacity");
            put_leb(take, &mut packet);
            packet.extend_from_slice(&wire[offset..offset + take]);
            offset += take;
            if offset < wire.len() {
                packet[0] |= 0x40;
                packets.push(Bytes::from(packet));
                packet = vec![0x80];
            }
        }
    }
    if packet.len() > 1 {
        packets.push(Bytes::from(packet));
    }
    ensure!(!packets.is_empty(), "empty AV1 picture");
    Ok(packets)
}

/// Called only after the sequence-aware packet buffer has assembled the frame.
/// Input records contain a LE u32 length followed by the original RTP payload.
pub(crate) fn assemble(records: &[u8]) -> Result<Vec<u8>> {
    ensure!(records.len() <= 64 * 1024 * 1024, "AV1 frame too large");
    let mut at = 0;
    let mut pending = Vec::new();
    let mut out = Vec::new();
    let mut fragment = false;
    let mut first = true;
    while at < records.len() {
        let n = u32::from_le_bytes(
            records
                .get(at..at + 4)
                .context("AV1 record length")?
                .try_into()?,
        ) as usize;
        at += 4;
        let p = records.get(at..at + n).context("AV1 record truncated")?;
        at += n;
        let h = *p.first().context("empty AV1 RTP")?;
        ensure!(
            h & 7 == 0 && (h & 0x80 != 0) == fragment && (h & 8 == 0 || first && !fragment),
            "broken AV1 fragment chain"
        );
        first = false;
        let count = (h >> 4) & 3;
        let mut pos = 1;
        let mut element = 0;
        while pos < p.len() {
            element += 1;
            let n = if count != 0 && element == count {
                p.len() - pos
            } else {
                leb(p, &mut pos)?
            };
            ensure!(n > 0, "empty AV1 RTP element");
            let end = pos
                .checked_add(n)
                .filter(|e| *e <= p.len())
                .context("invalid AV1 RTP length")?;
            ensure!(pending.len() + n <= 64 * 1024 * 1024, "AV1 OBU too large");
            pending.extend_from_slice(&p[pos..end]);
            pos = end;
            fragment = pos == p.len() && h & 0x40 != 0;
            if !fragment {
                let header = pending[0];
                ensure!(header & 0x81 == 0, "invalid AV1 OBU");
                let header_len = 1 + usize::from(header & 4 != 0);
                ensure!(pending.len() >= header_len, "AV1 extension truncated");
                if header_len == 2 {
                    ensure!(pending[1] & 7 == 0, "invalid AV1 extension");
                }
                let mut start = header_len;
                if header & 2 != 0 {
                    let n = leb(&pending, &mut start)?;
                    ensure!(n == pending.len() - start, "AV1 size mismatch");
                }
                let typ = (header >> 3) & 15;
                if !matches!(typ, 2 | 15) {
                    out.push(header | 2);
                    out.extend_from_slice(&pending[1..header_len]);
                    put_leb(pending.len() - start, &mut out);
                    out.extend_from_slice(&pending[start..]);
                }
                pending.clear();
            }
        }
        ensure!(
            element > 0 && (count == 0 || count == element),
            "invalid AV1 aggregation count"
        );
    }
    ensure!(
        !fragment && pending.is_empty() && !out.is_empty(),
        "incomplete AV1 picture"
    );
    for unit in units(&out)? {
        unit?;
    }
    Ok(out)
}

/// RTP profile defaults to Main when omitted. Reject invalid or conflicting
/// declarations instead of accidentally treating them as the 4:4:4 payload.
pub(crate) fn rtp_profile(fmtp: &str) -> Option<u8> {
    let mut profile = None;
    for part in fmtp.split(';') {
        let Some((key, value)) = part.trim().split_once('=') else {
            continue;
        };
        if key.trim().eq_ignore_ascii_case("profile") {
            let value = value.trim().parse::<u8>().ok()?;
            if value > 2 || profile.is_some_and(|old| old != value) {
                return None;
            }
            profile = Some(value);
        }
    }
    Some(profile.unwrap_or(0))
}
