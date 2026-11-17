//! Visible picture size for encoders that can only code aligned pictures.
//!
//! AV1 has no cropping syntax. AMD VCN4 (Radeon RX 7000, 780M) codes pictures
//! aligned to 64x16 and pads the right and bottom edges, so its sequence
//! header carries the padded size. The sender follows that sequence header
//! with a private metadata OBU naming the real size. Decoders that do not know
//! the declaration ignore the metadata and present the padding.
use crate::bitstream::{
    obu::{Obu, Units},
    Sequence,
};

/// "Unregistered user private" metadata type (AV1 section 6.7.1).
const METADATA_TYPE: u8 = 31;
const MAGIC: [u8; 4] = *b"FWVS";
const PAYLOAD: usize = 10;

fn obu(size: (u32, u32)) -> Option<[u8; PAYLOAD + 2]> {
    let (w, h) = (u16::try_from(size.0).ok()?, u16::try_from(size.1).ok()?);
    if w == 0 || h == 0 {
        return None;
    }
    let (w, h) = (w.to_be_bytes(), h.to_be_bytes());
    Some([
        5 << 3 | 2, // metadata OBU with a size field
        PAYLOAD as u8,
        METADATA_TYPE,
        MAGIC[0],
        MAGIC[1],
        MAGIC[2],
        MAGIC[3],
        w[0],
        w[1],
        h[0],
        h[1],
        0x80, // trailing bits
    ])
}

fn parse(obu: &Obu<'_>) -> Option<(u32, u32)> {
    let p = obu.payload;
    if obu.kind != 5 || p.len() != PAYLOAD || p[0] != METADATA_TYPE || p[1..5] != MAGIC || p[9] != 0x80 {
        return None;
    }
    let size = (
        u32::from(u16::from_be_bytes([p[5], p[6]])),
        u32::from(u16::from_be_bytes([p[7], p[8]])),
    );
    (size.0 > 0 && size.1 > 0).then_some(size)
}

/// Declares `size` after the first sequence header when the encoder padded
/// the picture by less than the 64x16 hardware alignment. Any other stream,
/// including one whose coded size already matches, is returned unchanged.
pub fn declare(mut data: Vec<u8>, size: (u32, u32)) -> Vec<u8> {
    let mut at = 0;
    let mut insert = None;
    if let Ok(units) = Units::new(&data) {
        for unit in units {
            let Ok(unit) = unit else { break };
            at += unit.bytes.len();
            if unit.kind != 1 {
                continue;
            }
            if let Ok(s) = Sequence::parse(unit) {
                let padded = (s.max_width, s.max_height) != size
                    && s.max_width >= size.0
                    && s.max_width - size.0 < 64
                    && s.max_height >= size.1
                    && s.max_height - size.1 < 16;
                insert = padded.then_some(at);
            }
            break;
        }
    }
    if let (Some(at), Some(obu)) = (insert, obu(size)) {
        data.splice(at..at, obu);
    }
    data
}

/// The declaration one temporal unit carries. `None`: it has no sequence
/// header, so the previous declaration still applies. `Some(None)`: its
/// sequence is presented at the coded size.
pub fn declared(data: &[u8]) -> Option<Option<(u32, u32)>> {
    let mut found = None;
    for unit in Units::new(data).ok()? {
        let Ok(unit) = unit else { break };
        if unit.kind == 1 {
            found = Some(None);
        } else if found.is_some() {
            if let Some(size) = parse(&unit) {
                found = Some(Some(size));
            }
        }
    }
    found
}

/// The presentable size of a `coded` picture whose padding is on the right
/// and bottom. A declaration that does not fit inside the picture is ignored.
pub fn clamp(visible: Option<(u32, u32)>, coded: (u32, u32)) -> (u32, u32) {
    match visible {
        Some((w, h)) if w <= coded.0 && h <= coded.1 => (w, h),
        _ => coded,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A temporal delimiter and a reduced still-picture sequence header for
    /// a `width` x `height` (8-bit 4:2:0) picture.
    fn stream(width: u32, height: u32) -> Vec<u8> {
        let mut bits = Vec::new();
        let mut put = |value: u32, n: u32| bits.extend((0..n).rev().map(|i| (value >> i) & 1 == 1));
        let (wb, hb) = (32 - (width - 1).leading_zeros(), 32 - (height - 1).leading_zeros());
        put(0, 3); // profile
        put(1, 1); // still picture
        put(1, 1); // reduced header
        put(0, 5); // level
        put(wb - 1, 4);
        put(hb - 1, 4);
        put(width - 1, wb);
        put(height - 1, hb);
        put(0, 3); // superblock size and intra filters
        put(0, 3); // super-resolution, CDEF, restoration
        put(0, 1); // 8-bit
        put(0, 1); // not monochrome
        put(0, 1); // no colour description
        put(0, 1); // range
        put(0, 2); // chroma sample position
        put(0, 1); // separate UV delta
        put(0, 1); // film grain
        put(1, 1); // trailing
        while bits.len() % 8 != 0 {
            bits.push(false);
        }
        let payload: Vec<u8> = bits.chunks(8).map(|c| c.iter().fold(0u8, |b, &bit| b << 1 | u8::from(bit))).collect();
        let mut out = vec![2 << 3 | 2, 0, 1 << 3 | 2, payload.len() as u8];
        out.extend(payload);
        out
    }

    #[test]
    fn padded_pictures_declare_their_size() {
        let coded = stream(1920, 1082);
        assert_eq!(declared(&coded), Some(None));
        let out = declare(coded.clone(), (1920, 1080));
        assert_eq!(out.len(), coded.len() + PAYLOAD + 2);
        assert_eq!(declared(&out), Some(Some((1920, 1080))));
        assert_eq!(clamp(Some((1920, 1080)), (1920, 1082)), (1920, 1080));
        // An exact size, or one beyond the alignment, stays as it is.
        assert_eq!(declare(coded.clone(), (1920, 1082)), coded);
        assert_eq!(declare(coded.clone(), (1280, 720)), coded);
        // A unit without a sequence header keeps the previous declaration.
        assert_eq!(declared(&[2 << 3 | 2, 0]), None);
        assert_eq!(clamp(Some((4000, 1080)), (1920, 1082)), (1920, 1082));
    }
}
