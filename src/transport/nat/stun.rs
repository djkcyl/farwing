//! Minimal STUN Binding codec for NAT behaviour discovery (RFC 5389, RFC 5780).
//! Requests carry at most CHANGE-REQUEST; responses are read for the mapped
//! address, RESPONSE-ORIGIN, OTHER-ADDRESS and ERROR-CODE.
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};

const MAGIC_COOKIE: u32 = 0x2112_A442;
const HEADER_LEN: usize = 20;
const BINDING_REQUEST: u16 = 0x0001;
const BINDING_SUCCESS: u16 = 0x0101;
const BINDING_ERROR: u16 = 0x0111;

mod attr {
    pub const MAPPED_ADDRESS: u16 = 0x0001;
    pub const CHANGE_REQUEST: u16 = 0x0003;
    pub const ERROR_CODE: u16 = 0x0009;
    pub const XOR_MAPPED_ADDRESS: u16 = 0x0020;
    pub const RESPONSE_ORIGIN: u16 = 0x802B;
    pub const OTHER_ADDRESS: u16 = 0x802C;
}

/// CHANGE-REQUEST flags.
pub(super) const CHANGE_IP: u32 = 0x04;
pub(super) const CHANGE_PORT: u32 = 0x02;

pub(super) type TransactionId = [u8; 12];

/// A Binding request, with CHANGE-REQUEST when `change` is not zero.
pub(super) fn request(id: &TransactionId, change: u32) -> Vec<u8> {
    let length: u16 = if change == 0 { 0 } else { 8 };
    let mut message = Vec::with_capacity(HEADER_LEN + usize::from(length));
    message.extend_from_slice(&BINDING_REQUEST.to_be_bytes());
    message.extend_from_slice(&length.to_be_bytes());
    message.extend_from_slice(&MAGIC_COOKIE.to_be_bytes());
    message.extend_from_slice(id);
    if change != 0 {
        message.extend_from_slice(&attr::CHANGE_REQUEST.to_be_bytes());
        message.extend_from_slice(&4u16.to_be_bytes());
        message.extend_from_slice(&change.to_be_bytes());
    }
    message
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Reply {
    Success {
        mapped: SocketAddr,
        /// RESPONSE-ORIGIN: the server address that sent the response.
        origin: Option<SocketAddr>,
        /// OTHER-ADDRESS: the alternate IP and port of a complete RFC 5780 set.
        other: Option<SocketAddr>,
    },
    /// ERROR-CODE, e.g. 420 when the server has no alternate address for a
    /// CHANGE-REQUEST.
    Error(u16),
}

/// Parse a response to transaction `id`; anything else is `None`.
pub(super) fn parse(packet: &[u8], id: &TransactionId) -> Option<Reply> {
    if packet.len() < HEADER_LEN {
        return None;
    }
    let kind = u16::from_be_bytes([packet[0], packet[1]]);
    let length = usize::from(u16::from_be_bytes([packet[2], packet[3]]));
    if u32::from_be_bytes(packet[4..8].try_into().ok()?) != MAGIC_COOKIE
        || &packet[8..HEADER_LEN] != id
        || HEADER_LEN + length > packet.len()
        || !matches!(kind, BINDING_SUCCESS | BINDING_ERROR)
    {
        return None;
    }
    let body = &packet[HEADER_LEN..HEADER_LEN + length];
    let (mut xor_mapped, mut mapped, mut origin, mut other, mut error) =
        (None, None, None, None, None);
    let mut offset = 0;
    while offset + 4 <= body.len() {
        let typ = u16::from_be_bytes([body[offset], body[offset + 1]]);
        let len = usize::from(u16::from_be_bytes([body[offset + 2], body[offset + 3]]));
        let start = offset + 4;
        let value = body.get(start..start + len)?;
        match typ {
            attr::XOR_MAPPED_ADDRESS => xor_mapped = address(value, Some(id)),
            attr::MAPPED_ADDRESS => mapped = address(value, None),
            attr::RESPONSE_ORIGIN => origin = address(value, None),
            attr::OTHER_ADDRESS => other = address(value, None),
            attr::ERROR_CODE if value.len() >= 4 => {
                error = Some(u16::from(value[2] & 0x07) * 100 + u16::from(value[3]));
            }
            _ => {}
        }
        offset = start + len.div_ceil(4) * 4;
    }
    if kind == BINDING_ERROR {
        return Some(Reply::Error(error.unwrap_or(0)));
    }
    Some(Reply::Success {
        mapped: xor_mapped.or(mapped)?,
        origin,
        other,
    })
}

/// MAPPED-ADDRESS layout, XOR-ed with the cookie and transaction ID when
/// `xor` is given.
fn address(value: &[u8], xor: Option<&TransactionId>) -> Option<SocketAddr> {
    if value.len() < 4 {
        return None;
    }
    let mut port = u16::from_be_bytes([value[2], value[3]]);
    let cookie = MAGIC_COOKIE.to_be_bytes();
    if xor.is_some() {
        port ^= (MAGIC_COOKIE >> 16) as u16;
    }
    let ip = match (value[1], value.len()) {
        (0x01, 8) => {
            let mut octets: [u8; 4] = value[4..8].try_into().ok()?;
            if xor.is_some() {
                for (byte, mask) in octets.iter_mut().zip(cookie) {
                    *byte ^= mask;
                }
            }
            IpAddr::V4(Ipv4Addr::from(octets))
        }
        (0x02, 20) => {
            let mut octets: [u8; 16] = value[4..20].try_into().ok()?;
            if let Some(id) = xor {
                let mask = cookie.iter().chain(id.iter());
                for (byte, mask) in octets.iter_mut().zip(mask) {
                    *byte ^= mask;
                }
            }
            IpAddr::V6(Ipv6Addr::from(octets))
        }
        _ => return None,
    };
    Some(SocketAddr::new(ip, port))
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;

    /// A success response as the Farwing STUN endpoints send it.
    pub(crate) fn success(
        id: &TransactionId,
        mapped: SocketAddr,
        origin: SocketAddr,
        other: Option<SocketAddr>,
    ) -> Vec<u8> {
        let mut body = Vec::new();
        let mut put = |typ: u16, addr: SocketAddr, xor: bool| {
            let SocketAddr::V4(v4) = addr else {
                unreachable!("IPv4 test addresses")
            };
            let mut port = v4.port();
            let mut ip = v4.ip().octets();
            if xor {
                port ^= (MAGIC_COOKIE >> 16) as u16;
                for (byte, mask) in ip.iter_mut().zip(MAGIC_COOKIE.to_be_bytes()) {
                    *byte ^= mask;
                }
            }
            body.extend_from_slice(&typ.to_be_bytes());
            body.extend_from_slice(&8u16.to_be_bytes());
            body.extend_from_slice(&[0, 1]);
            body.extend_from_slice(&port.to_be_bytes());
            body.extend_from_slice(&ip);
        };
        put(attr::XOR_MAPPED_ADDRESS, mapped, true);
        put(attr::RESPONSE_ORIGIN, origin, false);
        if let Some(other) = other {
            put(attr::OTHER_ADDRESS, other, false);
        }
        let mut message = BINDING_SUCCESS.to_be_bytes().to_vec();
        message.extend_from_slice(&(body.len() as u16).to_be_bytes());
        message.extend_from_slice(&MAGIC_COOKIE.to_be_bytes());
        message.extend_from_slice(id);
        message.extend_from_slice(&body);
        message
    }

    #[test]
    fn request_layout() {
        let id = [7u8; 12];
        let plain = request(&id, 0);
        assert_eq!(plain.len(), 20);
        assert_eq!(&plain[..4], &[0, 1, 0, 0]);
        assert_eq!(&plain[4..8], &MAGIC_COOKIE.to_be_bytes());
        let change = request(&id, CHANGE_IP | CHANGE_PORT);
        assert_eq!(change.len(), 28);
        assert_eq!(&change[2..4], &[0, 8]);
        assert_eq!(&change[20..], &[0, 3, 0, 4, 0, 0, 0, 6]);
    }

    #[test]
    fn parses_success_and_error() {
        let id = [3u8; 12];
        let mapped: SocketAddr = "203.0.113.9:40001".parse().unwrap();
        let origin: SocketAddr = "198.51.100.1:3478".parse().unwrap();
        let other: SocketAddr = "198.51.100.2:3479".parse().unwrap();
        let packet = success(&id, mapped, origin, Some(other));
        assert_eq!(
            parse(&packet, &id),
            Some(Reply::Success {
                mapped,
                origin: Some(origin),
                other: Some(other)
            })
        );
        // Another transaction is ignored.
        assert_eq!(parse(&packet, &[4u8; 12]), None);
        let mut error = BINDING_ERROR.to_be_bytes().to_vec();
        error.extend_from_slice(&8u16.to_be_bytes());
        error.extend_from_slice(&MAGIC_COOKIE.to_be_bytes());
        error.extend_from_slice(&id);
        error.extend_from_slice(&[0, 9, 0, 4, 0, 0, 4, 20]);
        assert_eq!(parse(&error, &id), Some(Reply::Error(420)));
    }
}
