//! Load tests of a path the session is not on. Light checks say little about
//! a path a carrier throttles only under load, so before moving the session
//! onto such a path (`choose_path`) the controlling agent asks the peer to
//! send it a few seconds of padding over that path, at a share of the
//! session's bitrate, and counts what arrives. The session stays where it
//! is meanwhile.
//!
//! Both datagrams carry the session's probe tag (`punch::Probe`): only the
//! peer can start a test, and a test only goes to an address that answers
//! this session's connectivity checks.
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

/// `magic, tag (8), id, rate kbps, duration ms (u32 each), size (u16)`.
const REQUEST_MAGIC: [u8; 4] = [0x06, b'F', b'W', b'L'];
const REQUEST_LEN: usize = 4 + 8 + 4 + 4 + 4 + 2;
/// `magic, tag (8), id, sequence (u32 each)`, then padding to the size.
const DATA_MAGIC: [u8; 4] = [0x06, b'F', b'W', b'd'];
const DATA_HEADER: usize = 4 + 8 + 4 + 4;

/// What the controlling agent asks for.
pub(super) const DURATION: Duration = Duration::from_secs(4);
pub(super) const SIZE: u16 = 1100;
/// Half the session's bitrate, within these bounds.
pub(super) const MIN_KBPS: u32 = 1_000;
pub(super) const MAX_KBPS: u32 = 8_000;
/// What a peer accepts.
const LIMIT_KBPS: u32 = 20_000;
const LIMIT_DURATION_MS: u32 = 5_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Request {
    pub id: u32,
    pub rate_kbps: u32,
    pub duration_ms: u32,
    pub size: u16,
}

impl Request {
    pub fn encode(&self, tag: &[u8; 8]) -> Vec<u8> {
        let mut packet = Vec::with_capacity(REQUEST_LEN);
        packet.extend_from_slice(&REQUEST_MAGIC);
        packet.extend_from_slice(tag);
        packet.extend_from_slice(&self.id.to_be_bytes());
        packet.extend_from_slice(&self.rate_kbps.to_be_bytes());
        packet.extend_from_slice(&self.duration_ms.to_be_bytes());
        packet.extend_from_slice(&self.size.to_be_bytes());
        packet
    }

    /// Shape only; the tag is checked by the caller.
    pub fn decode(packet: &[u8]) -> Option<([u8; 8], Self)> {
        if packet.len() != REQUEST_LEN || packet[..4] != REQUEST_MAGIC {
            return None;
        }
        let word = |at: usize| u32::from_be_bytes(packet[at..at + 4].try_into().unwrap());
        Some((
            packet[4..12].try_into().unwrap(),
            Self {
                id: word(12),
                rate_kbps: word(16),
                duration_ms: word(20),
                size: u16::from_be_bytes(packet[24..26].try_into().unwrap()),
            },
        ))
    }

    /// Packets the test sends.
    pub fn total(&self) -> u32 {
        let bytes = u64::from(self.rate_kbps) * 1000 / 8 * u64::from(self.duration_ms) / 1000;
        (bytes / u64::from(self.size.max(1))).clamp(1, 100_000) as u32
    }

    fn within_limits(&self) -> bool {
        self.rate_kbps > 0
            && self.rate_kbps <= LIMIT_KBPS
            && self.duration_ms > 0
            && self.duration_ms <= LIMIT_DURATION_MS
            && (DATA_HEADER as u16..=1200).contains(&self.size)
    }
}

/// `(tag, id)` of a test datagram.
pub(super) fn data_header(packet: &[u8]) -> Option<([u8; 8], u32)> {
    (packet.len() >= DATA_HEADER && packet[..4] == DATA_MAGIC).then(|| {
        (
            packet[4..12].try_into().unwrap(),
            u32::from_be_bytes(packet[12..16].try_into().unwrap()),
        )
    })
}

/// A request's first bytes, before the tag is derived.
pub(super) fn looks_like_request(packet: &[u8]) -> bool {
    packet.len() == REQUEST_LEN && packet[..4] == REQUEST_MAGIC
}

/// `FARWING_DEV_DIRECT_LOSS=<percent>[,<unix seconds>]`: the share of a
/// test over a direct path that is lost on purpose (until that time, when
/// given), as the application loses that share of its media there (a
/// carrier throttling P2P flows); for testing path switching.
pub(super) fn dev_direct_loss() -> f64 {
    static SPEC: std::sync::OnceLock<(f64, Option<std::time::SystemTime>)> =
        std::sync::OnceLock::new();
    let (share, until) = *SPEC.get_or_init(|| {
        let value = std::env::var("FARWING_DEV_DIRECT_LOSS").unwrap_or_default();
        let mut fields = value.split(',').map(str::trim);
        let share = fields
            .next()
            .and_then(|percent| percent.parse::<f64>().ok())
            .map_or(0.0, |percent| (percent / 100.0).clamp(0.0, 1.0));
        let until = fields
            .next()
            .and_then(|seconds| seconds.parse::<u64>().ok())
            .map(|seconds| std::time::UNIX_EPOCH + Duration::from_secs(seconds));
        (share, until)
    });
    if until.is_some_and(|until| std::time::SystemTime::now() >= until) {
        0.0
    } else {
        share
    }
}

/// Send the requested test over `conn` to `target`, paced over its duration;
/// `lose` of it is dropped on purpose (`dev_direct_loss`).
pub(super) async fn send(
    conn: Arc<dyn util::Conn + Send + Sync>,
    target: SocketAddr,
    tag: [u8; 8],
    request: Request,
    lose: f64,
) {
    if !request.within_limits() {
        log::debug!("refused path load test {request:?}");
        return;
    }
    let total = request.total();
    let duration = Duration::from_millis(u64::from(request.duration_ms));
    let mut packet = vec![0u8; usize::from(request.size)];
    packet[..4].copy_from_slice(&DATA_MAGIC);
    packet[4..12].copy_from_slice(&tag);
    packet[12..16].copy_from_slice(&request.id.to_be_bytes());
    log::info!(
        target: "webrtc_ice::agent::path",
        "sending path load test {} to {target}: {} kbps for {} ms",
        request.id,
        request.rate_kbps,
        request.duration_ms
    );
    let started = tokio::time::Instant::now();
    let mut ticks = tokio::time::interval(Duration::from_millis(5));
    let mut sent = 0;
    while sent < total {
        ticks.tick().await;
        let elapsed = started.elapsed().min(duration);
        let due = ((u128::from(total) * elapsed.as_nanos()) / duration.as_nanos().max(1)) as u32;
        while sent < due.max(1).min(total) {
            packet[16..20].copy_from_slice(&sent.to_be_bytes());
            sent += 1;
            if lose > 0.0 && rand::random::<f64>() < lose {
                continue;
            }
            if conn.send_to(&packet, target).await.is_err() {
                return;
            }
        }
    }
}
