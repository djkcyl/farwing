//! STUN/TURN attribute cursor rules.
//! These differ from the dependency's length-only attribute decoder.
use stun::agent::TransactionId;
use stun::attributes::{AttrType, RawAttribute};
use stun::message::{Message, MAGIC_COOKIE};

use crate::error::{Error, Result};

#[derive(Clone, Copy)]
pub enum Dialect {
    Stun,
    Turn,
    Ice,
}

/// Classifies a packet as an ICE STUN message. RTP timestamps can happen to
/// equal the magic cookie; the cookie alone is insufficient.
pub fn is_ice_stun(packet: &[u8]) -> bool {
    if packet.len() < 20 || packet.len() % 4 != 0 || packet[4..8] != MAGIC_COOKIE.to_be_bytes() {
        return false;
    }
    let typ = u16::from_be_bytes([packet[0], packet[1]]);
    if matches!(typ, 0x0200 | 0x0300 | 0x0310) {
        return true;
    }
    let length = packet.len();
    length >= 28
        && packet[length - 8..length - 4] == [0x80, 0x28, 0, 4]
        && packet[length - 4..]
            == stun::fingerprint::fingerprint_value(&packet[..length - 8]).to_be_bytes()
}

#[derive(Clone, Copy)]
enum Value {
    Address,
    U32,
    U64,
    Bytes,
    Error,
    U16List,
    Unknown,
}

fn value_type(typ: u16, dialect: Dialect) -> Value {
    if matches!(dialect, Dialect::Ice) {
        match typ {
            36 | 49153 | 49239 | 49264 => return Value::U32,
            37 => return Value::Bytes,
            32809 | 32810 => return Value::U64,
            _ => {}
        }
    }
    if matches!(dialect, Dialect::Turn) {
        match typ {
            12 | 13 | 25 => return Value::U32,
            18 | 22 => return Value::Address,
            19 | 24 | 26 | 34 => return Value::Bytes,
            _ => {}
        }
    }
    match typ {
        1 | 32 | 32803 => Value::Address,
        32808 | 65280 => Value::U32,
        6 | 8 | 20 | 21 | 32802 | 49240 => Value::Bytes,
        9 => Value::Error,
        10 | 49241 => Value::U16List,
        _ => Value::Unknown,
    }
}

pub fn decode(packet: &[u8], dialect: Dialect) -> Result<Message> {
    let invalid = || Error::ErrFailedToDecodeStun;
    if packet.len() < 20 {
        return Err(invalid());
    }
    let typ = u16::from_be_bytes([packet[0], packet[1]]);
    let length = usize::from(u16::from_be_bytes([packet[2], packet[3]]));
    // Every request emitted by this path has a 12-byte transaction ID.
    // Legacy 16-byte-ID replies cannot match these outstanding transactions.
    if typ > 0x3fff || length + 20 != packet.len() || packet[4..8] != MAGIC_COOKIE.to_be_bytes() {
        return Err(invalid());
    }
    let mut message = Message::new();
    message.typ.read_value(typ);
    message.length = length as u32;
    message.transaction_id = TransactionId(packet[8..20].try_into().map_err(|_| invalid())?);
    message.raw = packet.to_vec();
    let mut cursor = 20;
    while cursor < packet.len() {
        let header = packet.get(cursor..cursor + 4).ok_or_else(invalid)?;
        let typ = u16::from_be_bytes([header[0], header[1]]);
        let declared = usize::from(u16::from_be_bytes([header[2], header[3]]));
        cursor += 4;
        let value_type = value_type(typ, dialect);
        // Integer constructors set their own size before Read, even when the
        // incoming attribute header declared a different size.
        let length = match value_type {
            Value::U32 => 4,
            Value::U64 => 8,
            _ => declared,
        };
        let mut value = packet
            .get(cursor..cursor + length)
            .ok_or_else(invalid)?
            .to_vec();
        match value_type {
            Value::Address => {
                if !matches!((value.get(1), length), (Some(1), 8) | (Some(2), 20)) {
                    return Err(invalid());
                }
                // The reserved byte is consumed but not validated.
                // Normalize the attribute view only; HMAC uses original raw.
                value[0] = 0;
            }
            Value::Error => {
                if length < 4 {
                    return Err(invalid());
                }
                value[2] &= 7; // Reserved bits are ignored; the code still parses.
            }
            Value::U16List if length % 2 != 0 => return Err(invalid()),
            _ => {}
        }
        cursor += length;
        let padding = (4 - length % 4) % 4;
        let skip_required = matches!(value_type, Value::Unknown) && typ & 0x4000 == 0;
        if cursor + padding <= packet.len() {
            cursor += padding;
        } else if skip_required {
            return Err(invalid());
        }
        // A failed padding skip is ignored. A completely absent final pad is
        // accepted; a leftover partial pad fails the next attribute-header
        // read. Unknown skipped attributes require full pad.
        // Keep an attribute view for consumers; integrity.rs independently
        // scans original wire lengths instead of these normalized spans.
        message.attributes.0.push(RawAttribute {
            typ: AttrType(typ),
            length: length as u16,
            value,
        });
    }
    Ok(message)
}
