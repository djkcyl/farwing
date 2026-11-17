//! Visible picture size for encoders that can only code aligned pictures.
//!
//! AV1 has no cropping syntax. AMD VCN4 (Radeon RX 7000, 780M) codes pictures
//! aligned to 64x16 and pads the right and bottom edges with black, so its
//! sequence header carries the padded size. The sender follows that sequence
//! header with a private metadata OBU naming the real size. Receivers that do
//! not know the declaration ignore it and present the padding.
use crate::bitstream::{
    obu::{Obu, Units},
    Sequence,
};

/// "Unregistered user private" metadata type (AV1 section 6.7.1).
const METADATA_TYPE: u8 = 31;
const MAGIC: [u8; 4] = *b"UUVS";
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
    if obu.kind != 5
        || p.len() != PAYLOAD
        || p[0] != METADATA_TYPE
        || p[1..5] != MAGIC
        || p[9] != 0x80
    {
        return None;
    }
    let size = (
        u32::from(u16::from_be_bytes([p[5], p[6]])),
        u32::from(u16::from_be_bytes([p[7], p[8]])),
    );
    (size.0 > 0 && size.1 > 0).then_some(size)
}

/// Declares `size` after the first sequence header when the encoder padded the
/// picture by less than the 64x16 hardware alignment. Any other stream,
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

/// Declaration carried by one temporal unit. `None`: the unit has no sequence
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

/// Presentable size of a `coded` picture whose padding is on the right and
/// bottom. A declaration that does not fit inside the picture is ignored.
pub fn clamp(visible: Option<(u32, u32)>, coded: (u32, u32)) -> (u32, u32) {
    match visible {
        Some((w, h)) if w <= coded.0 && h <= coded.1 => (w, h),
        _ => coded,
    }
}
