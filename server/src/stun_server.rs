//! STUN Binding responder for NAT behaviour discovery (RFC 5389 / RFC 5780)
//! on the extra `[[turn.stun]]` endpoints of the builtin mode.
//!
//! - Binding success responses carry XOR-MAPPED-ADDRESS, RESPONSE-ORIGIN (the
//!   public address of the socket that sends the response) and FINGERPRINT.
//! - OTHER-ADDRESS is only sent by an endpoint that is part of a complete
//!   RFC 5780 set: another public IP serving the same two ports (A:p, A:q,
//!   B:p, B:q). A server with a single IP must not send it (RFC 5780 7.4),
//!   otherwise standard clients misclassify the mapping behaviour.
//! - CHANGE-REQUEST is answered from the endpoint that differs as requested
//!   (preferring the one consistent with OTHER-ADDRESS). If there is none, the
//!   request gets 420 (Unknown Attribute) listing CHANGE-REQUEST (RFC 5780
//!   section 6). Unlike RFC 5780, "change port" is also honoured on a single
//!   IP with two ports: standard clients never ask for it there, and Farwing
//!   clients can use it to detect port-dependent filtering.
//! - RESPONSE-PORT and PADDING are not supported (420): both redirect or
//!   inflate responses. Other unknown comprehension-required attributes get
//!   420 too (RFC 5389 7.3.1). Requests without the magic cookie, other
//!   methods, indications, responses and malformed messages are dropped.
//! - Responses are rate limited per source IP (IPv6: per /64).
//!
//! The primary builtin TURN socket belongs to the `turn` crate and does not
//! take part in OTHER-ADDRESS or CHANGE-REQUEST.

use crate::config::TurnConfig;
use crate::ratelimit::TokenBucket;
use anyhow::{Context, Result};
use parking_lot::Mutex;
use std::collections::HashMap;
use std::net::{IpAddr, Ipv6Addr, SocketAddr};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::net::UdpSocket;
use tokio::task::JoinHandle;

const MAGIC_COOKIE: u32 = 0x2112_A442;
const HEADER_LEN: usize = 20;
const FINGERPRINT_XOR: u32 = 0x5354_554E;

/// Message types of the Binding method.
pub const BINDING_REQUEST: u16 = 0x0001;
pub const BINDING_SUCCESS: u16 = 0x0101;
pub const BINDING_ERROR: u16 = 0x0111;

/// Attribute types.
pub mod attr {
    pub const CHANGE_REQUEST: u16 = 0x0003;
    pub const USERNAME: u16 = 0x0006;
    pub const MESSAGE_INTEGRITY: u16 = 0x0008;
    pub const ERROR_CODE: u16 = 0x0009;
    pub const UNKNOWN_ATTRIBUTES: u16 = 0x000A;
    pub const MESSAGE_INTEGRITY_SHA256: u16 = 0x001C;
    pub const XOR_MAPPED_ADDRESS: u16 = 0x0020;
    pub const PRIORITY: u16 = 0x0024;
    pub const USE_CANDIDATE: u16 = 0x0025;
    pub const PADDING: u16 = 0x0026;
    pub const RESPONSE_PORT: u16 = 0x0027;
    pub const FINGERPRINT: u16 = 0x8028;
    pub const RESPONSE_ORIGIN: u16 = 0x802B;
    pub const OTHER_ADDRESS: u16 = 0x802C;
}

/// CHANGE-REQUEST flags.
pub const CHANGE_IP: u32 = 0x04;
pub const CHANGE_PORT: u32 = 0x02;

/// Responses per second and burst per source IP (IPv6: per /64).
pub const RATE_PER_S: u32 = 20;
pub const RATE_BURST: u32 = 40;
/// Sources tracked by the limiter; new sources are dropped while it is full.
const MAX_TRACKED: usize = 65_536;
/// Unknown attributes listed in one 420 response.
const MAX_UNKNOWN: usize = 16;
/// Receive buffer; longer datagrams fail the length check and are dropped.
const MAX_PACKET: usize = 1500;

const CRC_TABLE: [u32; 256] = {
    let mut table = [0u32; 256];
    let mut i = 0;
    while i < 256 {
        let mut c = i as u32;
        let mut k = 0;
        while k < 8 {
            c = if c & 1 != 0 { 0xEDB8_8320 ^ (c >> 1) } else { c >> 1 };
            k += 1;
        }
        table[i] = c;
        i += 1;
    }
    table
};

/// CRC-32 (ISO-HDLC), as used by FINGERPRINT.
fn crc32(data: &[u8]) -> u32 {
    let mut c = !0u32;
    for &b in data {
        c = CRC_TABLE[((c ^ u32::from(b)) & 0xFF) as usize] ^ (c >> 8);
    }
    !c
}

/// CHANGE-REQUEST of a Binding request.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Change {
    pub ip: bool,
    pub port: bool,
}

/// A Binding request worth answering.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BindingRequest {
    pub transaction_id: [u8; 12],
    pub change: Change,
    /// Comprehension-required attributes this server does not support.
    pub unknown: Vec<u16>,
}

/// Parse a datagram. `None` means drop it silently: not an RFC 5389 STUN
/// message, not a Binding request, malformed, or a wrong FINGERPRINT.
pub fn parse_binding_request(packet: &[u8]) -> Option<BindingRequest> {
    if packet.len() < HEADER_LEN {
        return None;
    }
    let msg_type = u16::from_be_bytes([packet[0], packet[1]]);
    let length = usize::from(u16::from_be_bytes([packet[2], packet[3]]));
    let cookie = u32::from_be_bytes([packet[4], packet[5], packet[6], packet[7]]);
    // The type check also covers the two leading zero bits.
    if msg_type != BINDING_REQUEST
        || cookie != MAGIC_COOKIE
        || length % 4 != 0
        || HEADER_LEN + length != packet.len()
    {
        return None;
    }
    let mut req = BindingRequest {
        transaction_id: packet[8..HEADER_LEN].try_into().ok()?,
        change: Change::default(),
        unknown: Vec::new(),
    };
    let mut after_integrity = false;
    let mut offset = HEADER_LEN;
    while offset < packet.len() {
        if packet.len() - offset < 4 {
            return None;
        }
        let typ = u16::from_be_bytes([packet[offset], packet[offset + 1]]);
        let len = usize::from(u16::from_be_bytes([packet[offset + 2], packet[offset + 3]]));
        let start = offset + 4;
        let next = start + len.div_ceil(4) * 4;
        if next > packet.len() {
            return None;
        }
        let value = &packet[start..start + len];
        if typ == attr::FINGERPRINT {
            // Last attribute, CRC over everything before it.
            let expected = crc32(&packet[..offset]) ^ FINGERPRINT_XOR;
            if next != packet.len() || value != expected.to_be_bytes() {
                return None;
            }
        } else if !after_integrity {
            // RFC 5389 15.4: only FINGERPRINT counts after MESSAGE-INTEGRITY.
            match typ {
                attr::CHANGE_REQUEST => {
                    let flags = u32::from_be_bytes(value.try_into().ok()?);
                    req.change = Change {
                        ip: flags & CHANGE_IP != 0,
                        port: flags & CHANGE_PORT != 0,
                    };
                }
                attr::MESSAGE_INTEGRITY | attr::MESSAGE_INTEGRITY_SHA256 => after_integrity = true,
                // No authentication for Binding; ICE attributes are harmless.
                attr::USERNAME | attr::PRIORITY | attr::USE_CANDIDATE => {}
                t if t < 0x8000 => {
                    if !req.unknown.contains(&t) && req.unknown.len() < MAX_UNKNOWN {
                        req.unknown.push(t);
                    }
                }
                _ => {}
            }
        }
        offset = next;
    }
    Some(req)
}

/// Builds one STUN message.
struct Writer {
    buf: Vec<u8>,
}

impl Writer {
    fn new(msg_type: u16, transaction_id: &[u8; 12]) -> Self {
        let mut buf = Vec::with_capacity(96);
        buf.extend_from_slice(&msg_type.to_be_bytes());
        buf.extend_from_slice(&[0, 0]);
        buf.extend_from_slice(&MAGIC_COOKIE.to_be_bytes());
        buf.extend_from_slice(transaction_id);
        Self { buf }
    }

    fn set_length(&mut self, len: usize) {
        self.buf[2..4].copy_from_slice(&(len as u16).to_be_bytes());
    }

    fn attr(&mut self, typ: u16, value: &[u8]) {
        self.buf.extend_from_slice(&typ.to_be_bytes());
        self.buf.extend_from_slice(&(value.len() as u16).to_be_bytes());
        self.buf.extend_from_slice(value);
        let padded = value.len().div_ceil(4) * 4;
        self.buf.resize(self.buf.len() + padded - value.len(), 0);
        self.set_length(self.buf.len() - HEADER_LEN);
    }

    /// MAPPED-ADDRESS encoding (RESPONSE-ORIGIN, OTHER-ADDRESS), or the
    /// XOR-MAPPED-ADDRESS encoding with `xor`.
    fn address(&mut self, typ: u16, addr: SocketAddr, xor: bool) {
        // Magic cookie followed by the transaction ID.
        let key: [u8; 16] = self.buf[4..HEADER_LEN].try_into().expect("header");
        let mut port = addr.port().to_be_bytes();
        let (family, mut ip) = match addr.ip() {
            IpAddr::V4(v4) => (1u8, v4.octets().to_vec()),
            IpAddr::V6(v6) => (2u8, v6.octets().to_vec()),
        };
        if xor {
            // Port: the cookie's high half. Address: cookie (+ ID for IPv6).
            port[0] ^= key[0];
            port[1] ^= key[1];
            for (b, k) in ip.iter_mut().zip(key) {
                *b ^= k;
            }
        }
        let mut value = vec![0, family];
        value.extend_from_slice(&port);
        value.extend_from_slice(&ip);
        self.attr(typ, &value);
    }

    /// Append FINGERPRINT and return the datagram.
    fn finish(mut self) -> Vec<u8> {
        self.set_length(self.buf.len() - HEADER_LEN + 8);
        let crc = crc32(&self.buf) ^ FINGERPRINT_XOR;
        self.attr(attr::FINGERPRINT, &crc.to_be_bytes());
        self.buf
    }
}

/// Binding success response.
pub fn success_response(
    transaction_id: &[u8; 12],
    mapped: SocketAddr,
    origin: SocketAddr,
    other: Option<SocketAddr>,
) -> Vec<u8> {
    let mut w = Writer::new(BINDING_SUCCESS, transaction_id);
    w.address(attr::XOR_MAPPED_ADDRESS, mapped, true);
    w.address(attr::RESPONSE_ORIGIN, origin, false);
    if let Some(other) = other {
        w.address(attr::OTHER_ADDRESS, other, false);
    }
    w.finish()
}

/// Binding error response; `unknown` becomes UNKNOWN-ATTRIBUTES when set.
pub fn error_response(
    transaction_id: &[u8; 12],
    code: u16,
    reason: &str,
    unknown: &[u16],
) -> Vec<u8> {
    let mut w = Writer::new(BINDING_ERROR, transaction_id);
    let mut value = vec![0, 0, (code / 100) as u8, (code % 100) as u8];
    value.extend_from_slice(reason.as_bytes());
    w.attr(attr::ERROR_CODE, &value);
    if !unknown.is_empty() {
        let list: Vec<u8> = unknown.iter().flat_map(|t| t.to_be_bytes()).collect();
        w.attr(attr::UNKNOWN_ATTRIBUTES, &list);
    }
    w.finish()
}

/// Where one endpoint's responses come from (indices into the endpoint list).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Routes {
    /// OTHER-ADDRESS: set only when the endpoint is part of a complete set
    /// (the same two ports on two public IPs).
    pub other: Option<usize>,
    pub change_ip: Option<usize>,
    pub change_port: Option<usize>,
    pub change_both: Option<usize>,
}

impl Routes {
    /// The endpoint answering a request received on `at`.
    pub fn sender(&self, at: usize, change: Change) -> Option<usize> {
        match (change.ip, change.port) {
            (false, false) => Some(at),
            (true, false) => self.change_ip,
            (false, true) => self.change_port,
            (true, true) => self.change_both,
        }
    }
}

/// Response routes for endpoints with these public addresses (unique).
/// Endpoints only pair with endpoints of the same address family; the first
/// match in configuration order wins.
pub fn plan(publics: &[SocketAddr]) -> Vec<Routes> {
    let index_of = |addr: SocketAddr| publics.iter().position(|a| *a == addr);
    publics
        .iter()
        .enumerate()
        .map(|(i, me)| {
            let peers = || {
                publics
                    .iter()
                    .enumerate()
                    .filter(move |(j, a)| *j != i && a.is_ipv4() == me.is_ipv4())
            };
            let find = |same_ip: bool, same_port: bool| {
                peers()
                    .find(|(_, a)| {
                        (a.ip() == me.ip()) == same_ip && (a.port() == me.port()) == same_port
                    })
                    .map(|(j, _)| j)
            };
            let square = peers()
                .filter(|(_, a)| a.ip() != me.ip() && a.port() != me.port())
                .find_map(|(j, a)| {
                    let change_ip = index_of(SocketAddr::new(a.ip(), me.port()))?;
                    let change_port = index_of(SocketAddr::new(me.ip(), a.port()))?;
                    Some((j, change_ip, change_port))
                });
            match square {
                Some((both, change_ip, change_port)) => Routes {
                    other: Some(both),
                    change_ip: Some(change_ip),
                    change_port: Some(change_port),
                    change_both: Some(both),
                },
                None => Routes {
                    other: None,
                    change_ip: find(false, true),
                    change_port: find(true, false),
                    change_both: find(false, false),
                },
            }
        })
        .collect()
}

/// Answer `req` received on endpoint `at` from `client`: the endpoint to
/// send from and the datagram.
pub fn answer(
    publics: &[SocketAddr],
    routes: &[Routes],
    at: usize,
    req: &BindingRequest,
    client: SocketAddr,
) -> (usize, Vec<u8>) {
    let id = &req.transaction_id;
    if !req.unknown.is_empty() {
        return (at, error_response(id, 420, "Unknown Attribute", &req.unknown));
    }
    let r = &routes[at];
    match r.sender(at, req.change) {
        Some(via) => (
            via,
            success_response(id, client, publics[via], r.other.map(|o| publics[o])),
        ),
        // RFC 5780 section 6: no alternate address/port for this request.
        None => (
            at,
            error_response(id, 420, "Unknown Attribute", &[attr::CHANGE_REQUEST]),
        ),
    }
}

/// Token bucket per source IP (IPv6: per /64) with a bounded table.
struct SourceLimiter {
    buckets: HashMap<IpAddr, (TokenBucket, Instant)>,
    last_prune: Instant,
}

impl SourceLimiter {
    fn new() -> Self {
        Self {
            buckets: HashMap::new(),
            last_prune: Instant::now(),
        }
    }

    fn allow(&mut self, ip: IpAddr) -> bool {
        let key = match ip {
            IpAddr::V6(v6) => IpAddr::V6(Ipv6Addr::from(u128::from(v6) & (u128::MAX << 64))),
            v4 => v4,
        };
        let now = Instant::now();
        if !self.buckets.contains_key(&key) && self.buckets.len() >= MAX_TRACKED {
            // Buckets idle this long are full again and carry no state. Scan
            // at most once per second.
            if now.duration_since(self.last_prune) < Duration::from_secs(1) {
                return false;
            }
            self.last_prune = now;
            let idle = Duration::from_secs(u64::from(RATE_BURST.div_ceil(RATE_PER_S)));
            self.buckets
                .retain(|_, (_, last)| now.duration_since(*last) < idle);
            if self.buckets.len() >= MAX_TRACKED {
                return false;
            }
        }
        let (bucket, last) = self
            .buckets
            .entry(key)
            .or_insert_with(|| (TokenBucket::new(RATE_PER_S, RATE_BURST), now));
        *last = now;
        bucket.try_take()
    }
}

struct Shared {
    sockets: Vec<Arc<UdpSocket>>,
    publics: Vec<SocketAddr>,
    routes: Vec<Routes>,
    limiter: Mutex<SourceLimiter>,
}

impl Shared {
    /// The sending endpoint and the response, or `None` to stay silent.
    fn handle(&self, at: usize, packet: &[u8], from: SocketAddr) -> Option<(usize, Vec<u8>)> {
        let req = parse_binding_request(packet)?;
        let client = SocketAddr::new(from.ip().to_canonical(), from.port());
        // IPv4 clients reaching a dual-stack IPv6 socket: wrong family for
        // this endpoint's public address and partners.
        if client.is_ipv4() != self.publics[at].is_ipv4() {
            return None;
        }
        if !self.limiter.lock().allow(client.ip()) {
            return None;
        }
        Some(answer(&self.publics, &self.routes, at, &req, client))
    }
}

/// The running `[[turn.stun]]` endpoints; dropping stops them.
pub struct StunEndpoints {
    locals: Vec<SocketAddr>,
    publics: Vec<SocketAddr>,
    tasks: Vec<JoinHandle<()>>,
}

impl StunEndpoints {
    /// Bound local addresses, in configuration order.
    pub fn local_addrs(&self) -> &[SocketAddr] {
        &self.locals
    }

    /// Addresses clients see, in configuration order.
    pub fn public_addrs(&self) -> &[SocketAddr] {
        &self.publics
    }

    /// Stop serving and release the ports (a restart binds them again).
    pub async fn close(mut self) {
        for t in std::mem::take(&mut self.tasks) {
            t.abort();
            let _ = t.await;
        }
    }
}

impl Drop for StunEndpoints {
    fn drop(&mut self) {
        for t in &self.tasks {
            t.abort();
        }
    }
}

/// Bind and serve every `[[turn.stun]]` endpoint of `cfg`.
pub async fn start(cfg: &TurnConfig) -> Result<StunEndpoints> {
    let mut sockets = Vec::new();
    let mut locals = Vec::new();
    let mut publics = Vec::new();
    for ep in &cfg.stun {
        let socket = UdpSocket::bind(ep.listen)
            .await
            .with_context(|| format!("binding [[turn.stun]] listen {}", ep.listen))?;
        let local = socket.local_addr()?;
        let public = cfg.stun_public(ep, local.port()).with_context(|| {
            format!("[[turn.stun]] listen {} needs public (or [turn] public_ip)", ep.listen)
        })?;
        sockets.push(Arc::new(socket));
        locals.push(local);
        publics.push(public);
    }
    let routes = plan(&publics);
    for ((local, public), r) in locals.iter().zip(&publics).zip(&routes) {
        tracing::info!(
            listen = %local,
            %public,
            other_address = ?r.other.map(|o| publics[o]),
            change_ip = r.change_ip.is_some(),
            change_port = r.change_port.is_some(),
            "STUN endpoint listening (UDP)"
        );
    }
    let shared = Arc::new(Shared {
        sockets,
        publics: publics.clone(),
        routes,
        limiter: Mutex::new(SourceLimiter::new()),
    });
    let tasks = (0..shared.sockets.len())
        .map(|at| tokio::spawn(serve(Arc::clone(&shared), at)))
        .collect();
    Ok(StunEndpoints {
        locals,
        publics,
        tasks,
    })
}

async fn serve(shared: Arc<Shared>, at: usize) {
    let socket = Arc::clone(&shared.sockets[at]);
    let mut buf = vec![0u8; MAX_PACKET];
    loop {
        let (n, from) = match socket.recv_from(&mut buf).await {
            Ok(r) => r,
            Err(e) => {
                // Windows reports ICMP port unreachable for earlier sends here.
                if e.kind() != std::io::ErrorKind::ConnectionReset {
                    tracing::debug!(error = %e, "STUN receive failed");
                    tokio::time::sleep(Duration::from_millis(50)).await;
                }
                continue;
            }
        };
        let Some((via, response)) = shared.handle(at, &buf[..n], from) else {
            continue;
        };
        if let Err(e) = shared.sockets[via].send_to(&response, from).await {
            tracing::debug!(error = %e, "STUN send failed");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(s: &str) -> Vec<u8> {
        let s: String = s.split_whitespace().collect();
        hex::decode(s).unwrap()
    }

    /// RFC 5769 2.1: SOFTWARE, PRIORITY, ICE-CONTROLLED, USERNAME,
    /// MESSAGE-INTEGRITY, FINGERPRINT.
    const RFC5769_REQUEST: &str = "
        00 01 00 58 21 12 a4 42 b7 e7 a7 01 bc 34 d6 86 fa 87 df ae
        80 22 00 10 53 54 55 4e 20 74 65 73 74 20 63 6c 69 65 6e 74
        00 24 00 04 6e 00 01 ff
        80 29 00 08 93 2f f9 b1 51 26 3b 36
        00 06 00 09 65 76 74 6a 3a 68 36 76 59 20 20 20
        00 08 00 14 9a ea a7 0c bf d8 cb 56 78 1e f2 b5 b2 d3 f2 49 c1 b5 71 a2
        80 28 00 04 e5 7a 3b cf";
    /// RFC 5769 2.2 and 2.3: XOR-MAPPED-ADDRESS of 192.0.2.1:32853 and
    /// [2001:db8:1234:5678:11:2233:4455:6677]:32853.
    const RFC5769_XOR_V4: &str = "00 20 00 08 00 01 a1 47 e1 12 a6 43";
    const RFC5769_XOR_V6: &str = "
        00 20 00 14 00 02 a1 47
        01 13 a9 fa a5 d3 f1 79 bc 25 f4 b5 be d2 b9 d9";
    const RFC5769_TXID: [u8; 12] = [
        0xb7, 0xe7, 0xa7, 0x01, 0xbc, 0x34, 0xd6, 0x86, 0xfa, 0x87, 0xdf, 0xae,
    ];

    /// Attributes of a message (header and padding checked).
    fn attrs(msg: &[u8]) -> Vec<(u16, Vec<u8>)> {
        assert_eq!(usize::from(u16::from_be_bytes([msg[2], msg[3]])), msg.len() - 20);
        assert_eq!(msg[4..8], MAGIC_COOKIE.to_be_bytes());
        let mut out = Vec::new();
        let mut off = 20;
        while off < msg.len() {
            let t = u16::from_be_bytes([msg[off], msg[off + 1]]);
            let l = usize::from(u16::from_be_bytes([msg[off + 2], msg[off + 3]]));
            out.push((t, msg[off + 4..off + 4 + l].to_vec()));
            off += 4 + l.div_ceil(4) * 4;
        }
        assert_eq!(off, msg.len());
        out
    }

    fn get(msg: &[u8], t: u16) -> Option<Vec<u8>> {
        attrs(msg).into_iter().find(|(a, _)| *a == t).map(|(_, v)| v)
    }

    fn plain_addr(v: &[u8]) -> SocketAddr {
        let port = u16::from_be_bytes([v[2], v[3]]);
        let ip: IpAddr = match v[1] {
            1 => <[u8; 4]>::try_from(&v[4..8]).unwrap().into(),
            2 => <[u8; 16]>::try_from(&v[4..20]).unwrap().into(),
            f => panic!("family {f}"),
        };
        SocketAddr::new(ip, port)
    }

    fn xor_addr(v: &[u8], txid: &[u8; 12]) -> SocketAddr {
        let mut key = MAGIC_COOKIE.to_be_bytes().to_vec();
        key.extend_from_slice(txid);
        let mut plain = v.to_vec();
        plain[2] ^= key[0];
        plain[3] ^= key[1];
        for (b, k) in plain[4..].iter_mut().zip(&key) {
            *b ^= k;
        }
        plain_addr(&plain)
    }

    fn request(change: u32, extra: &[(u16, &[u8])], fingerprint: bool) -> Vec<u8> {
        let mut w = Writer::new(BINDING_REQUEST, &[7; 12]);
        if change != 0 {
            w.attr(attr::CHANGE_REQUEST, &change.to_be_bytes());
        }
        for (t, v) in extra {
            w.attr(*t, v);
        }
        if fingerprint { w.finish() } else { w.buf }
    }

    fn sa(s: &str) -> SocketAddr {
        s.parse().unwrap()
    }

    #[test]
    fn crc32_check_value() {
        assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
    }

    #[test]
    fn parses_rfc5769_request() {
        let req = parse_binding_request(&hex(RFC5769_REQUEST)).expect("valid request");
        assert_eq!(req.transaction_id, RFC5769_TXID);
        assert_eq!(req.change, Change::default());
        assert!(req.unknown.is_empty(), "{:?}", req.unknown);
        // A flipped bit anywhere breaks the FINGERPRINT.
        let mut bad = hex(RFC5769_REQUEST);
        bad[30] ^= 1;
        assert_eq!(parse_binding_request(&bad), None);
    }

    #[test]
    fn xor_mapped_address_matches_rfc5769() {
        for (addr, expected) in [
            ("192.0.2.1:32853", RFC5769_XOR_V4),
            ("[2001:db8:1234:5678:11:2233:4455:6677]:32853", RFC5769_XOR_V6),
        ] {
            let mut w = Writer::new(BINDING_SUCCESS, &RFC5769_TXID);
            w.address(attr::XOR_MAPPED_ADDRESS, sa(addr), true);
            assert_eq!(w.buf[20..], hex(expected)[..], "{addr}");
            assert_eq!(xor_addr(&w.buf[24..], &RFC5769_TXID), sa(addr));
        }
    }

    #[test]
    fn success_response_layout() {
        let id = [9u8; 12];
        let msg = success_response(
            &id,
            sa("198.51.100.7:40000"),
            sa("203.0.113.10:3479"),
            Some(sa("203.0.113.20:3480")),
        );
        assert_eq!(msg[..2], BINDING_SUCCESS.to_be_bytes());
        assert_eq!(msg[8..20], id);
        let a = attrs(&msg);
        let types: Vec<u16> = a.iter().map(|(t, _)| *t).collect();
        assert_eq!(
            types,
            [
                attr::XOR_MAPPED_ADDRESS,
                attr::RESPONSE_ORIGIN,
                attr::OTHER_ADDRESS,
                attr::FINGERPRINT
            ]
        );
        assert_eq!(xor_addr(&a[0].1, &id), sa("198.51.100.7:40000"));
        assert_eq!(plain_addr(&a[1].1), sa("203.0.113.10:3479"));
        assert_eq!(plain_addr(&a[2].1), sa("203.0.113.20:3480"));
        let fp_at = msg.len() - 8;
        assert_eq!(a[3].1, (crc32(&msg[..fp_at]) ^ FINGERPRINT_XOR).to_be_bytes());
        // IPv6, without OTHER-ADDRESS.
        let msg = success_response(&id, sa("[2001:db8::5]:1"), sa("[2001:db8::1]:3479"), None);
        let mapped = get(&msg, attr::XOR_MAPPED_ADDRESS).unwrap();
        assert_eq!(xor_addr(&mapped, &id), sa("[2001:db8::5]:1"));
        let origin = get(&msg, attr::RESPONSE_ORIGIN).unwrap();
        assert_eq!(plain_addr(&origin), sa("[2001:db8::1]:3479"));
        assert!(get(&msg, attr::OTHER_ADDRESS).is_none());
    }

    #[test]
    fn error_response_layout() {
        let msg = error_response(&[1; 12], 420, "Unknown Attribute", &[0x0027, 0x0003]);
        assert_eq!(msg[..2], BINDING_ERROR.to_be_bytes());
        let code = get(&msg, attr::ERROR_CODE).unwrap();
        assert_eq!(code[..4], [0, 0, 4, 20]);
        assert_eq!(&code[4..], b"Unknown Attribute");
        assert_eq!(get(&msg, attr::UNKNOWN_ATTRIBUTES).unwrap(), [0x00, 0x27, 0x00, 0x03]);
        assert!(get(&msg, attr::FINGERPRINT).is_some());
    }

    #[test]
    fn request_parsing() {
        let r = parse_binding_request(&request(0, &[], false)).unwrap();
        assert_eq!(r.transaction_id, [7; 12]);
        assert_eq!(r.change, Change::default());
        let r = parse_binding_request(&request(CHANGE_IP | CHANGE_PORT, &[], true)).unwrap();
        assert_eq!(r.change, Change { ip: true, port: true });
        let r = parse_binding_request(&request(CHANGE_PORT | 0x01, &[], true)).unwrap();
        assert_eq!(r.change, Change { ip: false, port: true });
        // Unknown comprehension-required attributes are reported (once),
        // comprehension-optional ones ignored.
        let r = parse_binding_request(&request(
            0,
            &[
                (attr::RESPONSE_PORT, &[0x12, 0x34, 0, 0]),
                (0x8022, b"x"),
                (attr::PADDING, &[0; 8]),
                (attr::RESPONSE_PORT, &[0; 4]),
            ],
            true,
        ))
        .unwrap();
        assert_eq!(r.unknown, [attr::RESPONSE_PORT, attr::PADDING]);
        // After MESSAGE-INTEGRITY only FINGERPRINT counts.
        let r = parse_binding_request(&request(
            0,
            &[(attr::MESSAGE_INTEGRITY, &[0; 20]), (attr::RESPONSE_PORT, &[0; 4])],
            true,
        ))
        .unwrap();
        assert!(r.unknown.is_empty());
    }

    #[test]
    fn invalid_requests_are_dropped() {
        let good = request(CHANGE_PORT, &[], true);
        assert!(parse_binding_request(&good).is_some());
        let mutate = |f: &dyn Fn(&mut Vec<u8>)| {
            let mut p = good.clone();
            f(&mut p);
            parse_binding_request(&p)
        };
        assert_eq!(mutate(&|p| p.truncate(19)), None, "short");
        assert_eq!(mutate(&|p| p[4] ^= 1), None, "magic cookie");
        assert_eq!(mutate(&|p| p[0] |= 0x40), None, "leading bits");
        assert_eq!(mutate(&|p| p.push(0)), None, "trailing byte");
        assert_eq!(mutate(&|p| p[3] += 4), None, "length");
        assert_eq!(mutate(&|p| p[27] ^= 2), None, "fingerprint");
        // Other methods and classes.
        for t in [0x0003u16, 0x0011, 0x0101, 0x0111] {
            assert_eq!(mutate(&|p| p[..2].copy_from_slice(&t.to_be_bytes())), None, "{t:#06x}");
        }
        // Malformed CHANGE-REQUEST, FINGERPRINT not last, attribute overrun.
        let short_change = request(0, &[(attr::CHANGE_REQUEST, &[0; 2])], false);
        assert_eq!(parse_binding_request(&short_change), None);
        let mut p = request(0, &[], true);
        p.extend_from_slice(&[0x80, 0x22, 0, 0]);
        p[3] += 4;
        assert_eq!(parse_binding_request(&p), None);
        let mut p = request(0, &[(0x8022, &[0; 4])], false);
        p[23] = 8;
        assert_eq!(parse_binding_request(&p), None);
    }

    #[test]
    fn plan_full_square() {
        let publics = [
            sa("203.0.113.10:3479"),
            sa("203.0.113.10:3480"),
            sa("198.51.100.20:3479"),
            sa("198.51.100.20:3480"),
        ];
        let routes = plan(&publics);
        let expect = |other, ip, port| Routes {
            other: Some(other),
            change_ip: Some(ip),
            change_port: Some(port),
            change_both: Some(other),
        };
        assert_eq!(routes[0], expect(3, 2, 1));
        assert_eq!(routes[1], expect(2, 3, 0));
        assert_eq!(routes[2], expect(1, 0, 3));
        assert_eq!(routes[3], expect(0, 1, 2));
    }

    #[test]
    fn plan_partial_sets() {
        // One IP, two ports: no OTHER-ADDRESS, change port only.
        let routes = plan(&[sa("203.0.113.10:3479"), sa("203.0.113.10:3480")]);
        assert_eq!(
            routes,
            [
                Routes {
                    change_port: Some(1),
                    ..Routes::default()
                },
                Routes {
                    change_port: Some(0),
                    ..Routes::default()
                }
            ]
        );
        // Three corners of a square: the incomplete corners pair as far as
        // possible but never send OTHER-ADDRESS.
        let routes = plan(&[sa("10.0.0.1:1000"), sa("10.0.0.1:2000"), sa("10.0.0.2:1000")]);
        assert_eq!(
            routes[0],
            Routes {
                other: None,
                change_ip: Some(2),
                change_port: Some(1),
                change_both: None
            }
        );
        assert_eq!(
            routes[1],
            Routes {
                other: None,
                change_ip: None,
                change_port: Some(0),
                change_both: Some(2)
            }
        );
        // A single endpoint, and families that never pair.
        assert_eq!(plan(&[sa("10.0.0.1:1000")]), [Routes::default()]);
        let routes = plan(&[sa("10.0.0.1:1000"), sa("[2001:db8::1]:2000")]);
        assert_eq!(routes, [Routes::default(), Routes::default()]);
        // With a fifth endpoint the first complete square in order wins.
        let routes = plan(&[
            sa("10.0.0.1:1000"),
            sa("10.0.0.3:3000"),
            sa("10.0.0.1:2000"),
            sa("10.0.0.2:1000"),
            sa("10.0.0.2:2000"),
        ]);
        assert_eq!(routes[0].other, Some(4));
        assert_eq!(routes[0].change_ip, Some(3));
        assert_eq!(routes[0].change_port, Some(2));
        assert_eq!(routes[1].other, None);
    }

    #[test]
    fn change_request_routing() {
        let publics = [
            sa("203.0.113.10:3479"),
            sa("203.0.113.10:3480"),
            sa("198.51.100.20:3479"),
            sa("198.51.100.20:3480"),
        ];
        let routes = plan(&publics);
        let client = sa("192.0.2.1:5000");
        for (flags, via) in [
            (0, 0),
            (CHANGE_IP, 2),
            (CHANGE_PORT, 1),
            (CHANGE_IP | CHANGE_PORT, 3),
        ] {
            let req = parse_binding_request(&request(flags, &[], true)).unwrap();
            let (sender, msg) = answer(&publics, &routes, 0, &req, client);
            assert_eq!(sender, via, "flags {flags:#x}");
            assert_eq!(msg[..2], BINDING_SUCCESS.to_be_bytes());
            assert_eq!(msg[8..20], [7; 12]);
            assert_eq!(xor_addr(&get(&msg, attr::XOR_MAPPED_ADDRESS).unwrap(), &[7; 12]), client);
            assert_eq!(plain_addr(&get(&msg, attr::RESPONSE_ORIGIN).unwrap()), publics[via]);
            // OTHER-ADDRESS belongs to the endpoint the request was sent to.
            assert_eq!(plain_addr(&get(&msg, attr::OTHER_ADDRESS).unwrap()), publics[3]);
        }

        // Single IP: change port works, change IP is refused with 420.
        let publics = [sa("203.0.113.10:3479"), sa("203.0.113.10:3480")];
        let routes = plan(&publics);
        let req = parse_binding_request(&request(CHANGE_PORT, &[], true)).unwrap();
        let (sender, msg) = answer(&publics, &routes, 1, &req, client);
        assert_eq!(sender, 0);
        assert_eq!(plain_addr(&get(&msg, attr::RESPONSE_ORIGIN).unwrap()), publics[0]);
        assert!(get(&msg, attr::OTHER_ADDRESS).is_none());
        for flags in [CHANGE_IP, CHANGE_IP | CHANGE_PORT] {
            let req = parse_binding_request(&request(flags, &[], true)).unwrap();
            let (sender, msg) = answer(&publics, &routes, 1, &req, client);
            assert_eq!(sender, 1);
            assert_eq!(msg[..2], BINDING_ERROR.to_be_bytes());
            assert_eq!(get(&msg, attr::ERROR_CODE).unwrap()[2..4], [4, 20]);
            assert_eq!(get(&msg, attr::UNKNOWN_ATTRIBUTES).unwrap(), [0x00, 0x03]);
        }

        // Unsupported attributes win over CHANGE-REQUEST.
        let packet = request(CHANGE_PORT, &[(attr::RESPONSE_PORT, &[0; 4])], true);
        let req = parse_binding_request(&packet).unwrap();
        let (sender, msg) = answer(&publics, &routes, 0, &req, client);
        assert_eq!(sender, 0);
        assert_eq!(get(&msg, attr::UNKNOWN_ATTRIBUTES).unwrap(), [0x00, 0x27]);
    }

    #[test]
    fn source_limiter() {
        let mut l = SourceLimiter::new();
        let ip: IpAddr = "192.0.2.1".parse().unwrap();
        let ok = (0..RATE_BURST * 2).filter(|_| l.allow(ip)).count() as u32;
        assert!((RATE_BURST..RATE_BURST + 3).contains(&ok), "{ok}");
        assert!(l.allow("192.0.2.2".parse().unwrap()));
        // IPv6 sources share a bucket per /64.
        let ok = (0..RATE_BURST * 2)
            .filter(|i| l.allow(format!("2001:db8::{:x}", i + 1).parse().unwrap()))
            .count() as u32;
        assert!((RATE_BURST..RATE_BURST + 3).contains(&ok), "{ok}");
        assert!(l.allow("2001:db8:0:1::1".parse().unwrap()));
    }
}
