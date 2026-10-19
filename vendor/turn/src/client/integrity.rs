//! Integrity is checked against original wire offsets, not the normalized
//! attribute lengths of the typed parser.
use hmac::{Hmac, Mac};
use sha1::Sha1;
use stun::{
    attributes::{AttrType, ATTR_MESSAGE_INTEGRITY},
    message::Message,
};

pub const GOOG_INTEGRITY_32: AttrType = AttrType(0xC060);

pub fn verify(message: &Message, key: &[u8]) -> bool {
    let (typ, size) = if message.contains(ATTR_MESSAGE_INTEGRITY) {
        (ATTR_MESSAGE_INTEGRITY, 20)
    } else if message.contains(GOOG_INTEGRITY_32) {
        (GOOG_INTEGRITY_32, 4)
    } else {
        return false;
    };
    let raw = &message.raw;
    if raw.len() < 24
        || raw.len() % 4 != 0
        || usize::from(u16::from_be_bytes([raw[2], raw[3]])) + 20 != raw.len()
    {
        return false;
    }
    let mut cursor = 20;
    while let Some(header) = raw.get(cursor..cursor + 4) {
        let attribute = u16::from_be_bytes([header[0], header[1]]);
        let length = usize::from(u16::from_be_bytes([header[2], header[3]]));
        let end = cursor + 4 + length;
        if attribute == typ.0 {
            if length != size || end > raw.len() {
                return false;
            }
            let mut prefix = raw[..cursor].to_vec();
            prefix[2..4].copy_from_slice(&((end - 20) as u16).to_be_bytes());
            let Ok(mut mac) = Hmac::<Sha1>::new_from_slice(key) else {
                return false;
            };
            mac.update(&prefix);
            return mac.verify_truncated_left(&raw[cursor + 4..end]).is_ok();
        }
        cursor = end + (4 - length % 4) % 4;
    }
    false
}

pub fn append_goog_integrity(raw: &mut Vec<u8>, key: &[u8]) {
    // Callers construct a complete 20-byte header plus aligned attributes.
    let length = raw.len() + 8 - 20;
    raw[2..4].copy_from_slice(&(length as u16).to_be_bytes());
    let mut mac = Hmac::<Sha1>::new_from_slice(key).expect("HMAC accepts arbitrary key lengths");
    mac.update(raw);
    let tag = mac.finalize().into_bytes();
    raw.extend_from_slice(&[0xc0, 0x60, 0, 4]);
    raw.extend_from_slice(&tag[..4]);
}
