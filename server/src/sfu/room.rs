//! A room: one host's published tracks (the uplink) and its viewers' legs.

use super::leg::{Leg, Rtcp};
use super::sdp::{self, Kind};
use parking_lot::{Mutex, RwLock};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};
use tokio::task::JoinHandle;
use webrtc::peer_connection::RTCPeerConnection;
use webrtc::rtcp::payload_feedbacks::full_intra_request::FullIntraRequest;
use webrtc::rtcp::payload_feedbacks::picture_loss_indication::PictureLossIndication;
use webrtc::rtcp::payload_feedbacks::receiver_estimated_maximum_bitrate::ReceiverEstimatedMaximumBitrate;
use webrtc::rtcp::receiver_report::ReceiverReport;
use webrtc::rtcp::sender_report::SenderReport;
use webrtc::rtcp::transport_feedbacks::transport_layer_cc::TransportLayerCc;
use webrtc::rtcp::transport_feedbacks::transport_layer_nack::{
    TransportLayerNack, nack_pairs_from_sequence_numbers,
};
use webrtc::rtp::packet::Packet;

/// Packets kept per stream for viewers' retransmission requests (about one
/// second of a high-rate stream).
const CACHE: usize = 2048;
/// Keyframe requests passed to the host, per stream.
const KEYFRAME_INTERVAL: Duration = Duration::from_millis(300);
/// The host hears the forwarded stream's limit this often.
const LIMIT_INTERVAL: Duration = Duration::from_millis(250);
/// Viewers whose downlinks cannot carry the stream ask for keyframes (for
/// everyone) together at most this often: their losses do not cost the
/// others a keyframe each, and they still get a chance to catch up.
const SLOW_KEYFRAME_INTERVAL: Duration = Duration::from_secs(10);
/// Other congested viewers ask together at most this often: a keyframe on a
/// congested downlink is lost again and asked for again.
const CONGESTED_KEYFRAME_INTERVAL: Duration = Duration::from_secs(2);
/// Lost uplink packets are requested again after this, up to `NACK_TRIES`
/// times within `NACK_AGE`.
const NACK_RETRY: Duration = Duration::from_millis(40);
const NACK_FIRST: Duration = Duration::from_millis(5);
const NACK_TRIES: u8 = 8;
const NACK_AGE: Duration = Duration::from_secs(1);

struct Cache {
    slots: Vec<Option<Packet>>,
}

impl Cache {
    fn new() -> Self {
        Self {
            slots: vec![None; CACHE],
        }
    }

    fn get(&self, seq: u16) -> Option<&Packet> {
        self.slots[seq as usize % CACHE]
            .as_ref()
            .filter(|p| p.header.sequence_number == seq)
    }

    /// `false` if the packet was already there.
    fn insert(&mut self, packet: Packet) -> bool {
        let seq = packet.header.sequence_number;
        if self.get(seq).is_some() {
            return false;
        }
        self.slots[seq as usize % CACHE] = Some(packet);
        true
    }
}

struct Lost {
    since: Instant,
    last: Option<Instant>,
    tries: u8,
}

/// Sequence gaps of an uplink stream not yet filled.
#[derive(Default)]
struct Missing {
    highest: Option<u16>,
    lost: HashMap<u16, Lost>,
}

impl Missing {
    fn received(&mut self, seq: u16) {
        let Some(highest) = self.highest else {
            self.highest = Some(seq);
            return;
        };
        let ahead = seq.wrapping_sub(highest) as i16;
        if ahead > 0 {
            if ahead < 512 {
                let now = Instant::now();
                for gap in 1..ahead as u16 {
                    self.lost.insert(
                        highest.wrapping_add(gap),
                        Lost {
                            since: now,
                            last: None,
                            tries: 0,
                        },
                    );
                }
            } else {
                self.lost.clear();
            }
            self.highest = Some(seq);
        }
        self.lost.remove(&seq);
    }

    fn due(&mut self, now: Instant) -> Vec<u16> {
        self.lost
            .retain(|_, l| l.tries < NACK_TRIES && now.duration_since(l.since) < NACK_AGE);
        let mut due: Vec<u16> = self
            .lost
            .iter_mut()
            .filter(|(_, l)| match l.last {
                None => now.duration_since(l.since) >= NACK_FIRST,
                Some(last) => now.duration_since(last) >= NACK_RETRY,
            })
            .map(|(seq, l)| {
                l.last = Some(now);
                l.tries += 1;
                *seq
            })
            .collect();
        due.sort_unstable_by_key(|s| s.wrapping_sub(self.highest.unwrap_or(0)));
        due
    }
}

pub(super) struct UpTrack {
    pub info: sdp::Track,
    cache: Mutex<Cache>,
    missing: Mutex<Missing>,
    keyframe_at: Mutex<Option<Instant>>,
}

pub(super) struct Uplink {
    pub pc: Arc<RTCPeerConnection>,
    pub tracks: Vec<Arc<UpTrack>>,
    /// Negotiated payload types and extensions (the answer).
    pub wire: sdp::Wire,
    pub tasks: Mutex<Vec<JoinHandle<()>>>,
}

impl Uplink {
    pub fn new(pc: Arc<RTCPeerConnection>, tracks: Vec<sdp::Track>, wire: sdp::Wire) -> Self {
        Self {
            pc,
            tracks: tracks
                .into_iter()
                .map(|info| {
                    Arc::new(UpTrack {
                        info,
                        cache: Mutex::new(Cache::new()),
                        missing: Mutex::default(),
                        keyframe_at: Mutex::default(),
                    })
                })
                .collect(),
            wire,
            tasks: Mutex::default(),
        }
    }

    fn track(&self, ssrc: u32) -> Option<&Arc<UpTrack>> {
        self.tracks.iter().find(|t| t.info.ssrc == ssrc)
    }

    async fn close(&self) {
        for task in self.tasks.lock().drain(..) {
            task.abort();
        }
        let _ = self.pc.close().await;
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct LegStats {
    pub id: String,
    pub packets: u64,
    pub bytes: u64,
    pub repairs: u64,
    pub dropped: u64,
    /// Loss the viewer reports, percent.
    pub loss: f64,
    pub rtt_ms: Option<u64>,
    /// While the viewer's downlink is congested: what it takes.
    #[serde(default)]
    pub limit_bps: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RoomStats {
    pub id: String,
    pub tracks: usize,
    pub packets: u64,
    pub bytes: u64,
    pub uplink_repairs: u64,
    pub keyframe_requests: u64,
    pub legs: Vec<LegStats>,
    /// The congested viewers' limit passed on to the host.
    #[serde(default)]
    pub limit_bps: Option<u64>,
}

pub(super) struct Room {
    pub id: String,
    pub uplink: RwLock<Option<Arc<Uplink>>>,
    pub legs: RwLock<HashMap<String, Arc<Leg>>>,
    packets: AtomicU64,
    bytes: AtomicU64,
    nacked: AtomicU64,
    keyframes: AtomicU64,
    /// The forwarded stream's limit, and when the host was last told.
    limit: Mutex<Option<u64>>,
    limit_at: Mutex<Instant>,
    /// Viewers whose downlinks cannot carry the forwarded stream, and the
    /// last keyframe one of them asked for.
    slow: Mutex<HashSet<String>>,
    slow_keyframe_at: Mutex<Option<Instant>>,
    /// The last keyframe another congested viewer asked for.
    congested_keyframe_at: Mutex<Option<Instant>>,
    pub ticker: Mutex<Option<JoinHandle<()>>>,
}

impl Room {
    pub fn new(id: String) -> Arc<Self> {
        Arc::new(Self {
            id,
            uplink: RwLock::default(),
            legs: RwLock::default(),
            packets: AtomicU64::new(0),
            bytes: AtomicU64::new(0),
            nacked: AtomicU64::new(0),
            keyframes: AtomicU64::new(0),
            limit: Mutex::default(),
            limit_at: Mutex::new(Instant::now()),
            slow: Mutex::default(),
            slow_keyframe_at: Mutex::default(),
            congested_keyframe_at: Mutex::default(),
            ticker: Mutex::default(),
        })
    }

    pub fn current(&self) -> Option<Arc<Uplink>> {
        self.uplink.read().clone()
    }

    fn legs(&self) -> Vec<Arc<Leg>> {
        self.legs.read().values().cloned().collect()
    }

    fn fan_out(&self, packet: Packet) {
        self.packets.fetch_add(1, Ordering::Relaxed);
        self.bytes
            .fetch_add(packet.payload.len() as u64, Ordering::Relaxed);
        for leg in self.legs() {
            leg.push(packet.clone());
        }
    }

    /// A media packet of `track` from the host (or one recovered from RTX).
    pub fn media(&self, track: &UpTrack, packet: Packet) {
        if track.info.kind == Kind::Video {
            track.missing.lock().received(packet.header.sequence_number);
            if !track.cache.lock().insert(packet.clone()) {
                return;
            }
        }
        self.fan_out(packet);
    }

    /// An RTX packet from the host: the original packet, forwarded as media.
    pub fn rtx(&self, uplink: &Uplink, track: &UpTrack, packet: Packet) {
        // Bandwidth probes are padding-only RTX.
        if packet.payload.len() <= 2 {
            return;
        }
        let Some(apt) = uplink
            .wire
            .codecs
            .get(&packet.header.payload_type)
            .and_then(sdp::Codec::apt)
        else {
            return;
        };
        let mut original = packet;
        let seq = u16::from_be_bytes([original.payload[0], original.payload[1]]);
        original.payload = original.payload.slice(2..);
        original.header.ssrc = track.info.ssrc;
        original.header.sequence_number = seq;
        original.header.payload_type = apt;
        original.header.padding = false;
        self.media(track, original);
    }

    /// An RS-FEC repair packet: viewers recover with it themselves.
    pub fn fec(&self, packet: Packet) {
        self.fan_out(packet);
    }

    /// A host sender report: every viewer gets it for its clock mapping.
    pub async fn sender_report(&self, report: &SenderReport) {
        for leg in self.legs() {
            leg.report_sent(report.ntp_time);
            let _ = leg.pc.write_rtcp(&[Box::new(report.clone())]).await;
        }
    }

    /// Asks the host for a keyframe of `ssrc` (merged across viewers).
    pub async fn keyframe(&self, ssrc: u32) {
        let Some(uplink) = self.current() else { return };
        let Some(track) = uplink.track(ssrc) else { return };
        {
            let mut at = track.keyframe_at.lock();
            if at.is_some_and(|at| at.elapsed() < KEYFRAME_INTERVAL) {
                return;
            }
            *at = Some(Instant::now());
        }
        self.keyframes.fetch_add(1, Ordering::Relaxed);
        let _ = uplink
            .pc
            .write_rtcp(&[Box::new(PictureLossIndication {
                sender_ssrc: 0,
                media_ssrc: ssrc,
            })])
            .await;
    }

    /// A viewer's RTCP: retransmission and keyframe requests, reports.
    pub async fn leg_rtcp(&self, leg: &Leg, packets: Rtcp) {
        for packet in packets {
            tracing::trace!(leg = %leg.id, header = ?packet.header(), "viewer RTCP");
            let any = packet.as_any();
            if let Some(nack) = any.downcast_ref::<TransportLayerNack>() {
                self.repair(leg, nack);
            } else if let Some(pli) = any.downcast_ref::<PictureLossIndication>() {
                if self.keyframe_allowed(leg) {
                    self.keyframe(pli.media_ssrc).await;
                }
            } else if let Some(fir) = any.downcast_ref::<FullIntraRequest>() {
                if self.keyframe_allowed(leg) {
                    for entry in &fir.fir {
                        self.keyframe(entry.ssrc).await;
                    }
                }
            } else if let Some(rr) = any.downcast_ref::<ReceiverReport>() {
                for block in &rr.reports {
                    leg.report_received(block.fraction_lost, block.last_sender_report, block.delay);
                }
            } else if let Some(feedback) = any.downcast_ref::<TransportLayerCc>() {
                leg.downlink.lock().feedback(feedback);
            }
        }
    }

    /// Whether `leg`'s keyframe request goes on (to the per-track merge).
    fn keyframe_allowed(&self, leg: &Leg) -> bool {
        let (at, interval) = if self.slow.lock().contains(&leg.id) {
            (&self.slow_keyframe_at, SLOW_KEYFRAME_INTERVAL)
        } else if leg.downlink.lock().limit().is_some() {
            (&self.congested_keyframe_at, CONGESTED_KEYFRAME_INTERVAL)
        } else {
            return true;
        };
        let now = Instant::now();
        let mut at = at.lock();
        if at.is_some_and(|at| now.duration_since(at) < interval) {
            return false;
        }
        *at = Some(now);
        true
    }

    fn repair(&self, leg: &Leg, nack: &TransportLayerNack) {
        let Some(uplink) = self.current() else { return };
        let Some(track) = uplink.track(nack.media_ssrc) else {
            return;
        };
        if track.info.rtx == 0 {
            return;
        }
        let repairs: Vec<Packet> = {
            let cache = track.cache.lock();
            nack.nacks
                .iter()
                .flat_map(|pair| pair.packet_list())
                .filter_map(|seq| {
                    let original = cache.get(seq)?;
                    let rtx_pt = uplink.wire.codecs.iter().find_map(|(pt, c)| {
                        (c.apt() == Some(original.header.payload_type)).then_some(*pt)
                    })?;
                    leg.repair(original, track.info.rtx, rtx_pt)
                })
                .collect()
        };
        for packet in repairs {
            leg.push(packet);
        }
    }

    /// Requests lost uplink packets once for every viewer; tells the host
    /// the forwarded stream's limit.
    pub async fn tick(&self) {
        let Some(uplink) = self.current() else { return };
        let now = Instant::now();
        if now.duration_since(*self.limit_at.lock()) >= LIMIT_INTERVAL {
            *self.limit_at.lock() = now;
            self.send_limit(&uplink).await;
        }
        for track in &uplink.tracks {
            if track.info.kind != Kind::Video || track.info.rtx == 0 {
                continue;
            }
            let due = track.missing.lock().due(now);
            if due.is_empty() {
                continue;
            }
            self.nacked.fetch_add(due.len() as u64, Ordering::Relaxed);
            let _ = uplink
                .pc
                .write_rtcp(&[Box::new(TransportLayerNack {
                    sender_ssrc: 0,
                    media_ssrc: track.info.ssrc,
                    nacks: nack_pairs_from_sequence_numbers(&due),
                })])
                .await;
        }
    }

    /// The forwarded stream's limit, to the host as a REMB on SSRC 0 (the
    /// host reads the uplink's transport-wide feedback there): the rate at
    /// least half the viewers' downlinks carry. Viewers slower than that
    /// stall; they do not lower everyone's picture. None while that is any
    /// rate, and the host's limit lapses.
    async fn send_limit(&self, uplink: &Uplink) {
        let legs: Vec<(String, Option<u64>)> = self
            .legs()
            .iter()
            .map(|leg| {
                let mut downlink = leg.downlink.lock();
                downlink.tick();
                (leg.id.clone(), downlink.limit())
            })
            .collect();
        let limit = majority_limit(legs.iter().map(|(_, l)| *l));
        // The minority below the forwarded rate.
        *self.slow.lock() = legs
            .into_iter()
            .filter(|(_, l)| l.is_some_and(|l| limit.is_none_or(|limit| l * 100 < limit * 85)))
            .map(|(id, _)| id)
            .collect();
        let previous = std::mem::replace(&mut *self.limit.lock(), limit);
        if previous.is_some() != limit.is_some() {
            tracing::info!(room = %self.id, limit_bps = ?limit, "forwarded stream limit");
        }
        let Some(limit) = limit else { return };
        let remb = ReceiverEstimatedMaximumBitrate {
            sender_ssrc: 0,
            bitrate: limit as f32,
            ssrcs: vec![0],
        };
        if let Err(error) = uplink.pc.write_rtcp(&[Box::new(remb)]).await {
            tracing::debug!(%error, room = %self.id, "limit not sent to the host");
        }
    }

    pub async fn replace_uplink(&self, uplink: Option<Arc<Uplink>>) {
        let old = std::mem::replace(&mut *self.uplink.write(), uplink);
        if let Some(old) = old {
            old.close().await;
        }
    }

    pub async fn remove_leg(&self, id: &str) {
        let leg = self.legs.write().remove(id);
        if let Some(leg) = leg {
            leg.close().await;
        }
    }

    pub async fn close(&self) {
        if let Some(ticker) = self.ticker.lock().take() {
            ticker.abort();
        }
        let legs: Vec<Arc<Leg>> = self.legs.write().drain().map(|(_, l)| l).collect();
        for leg in legs {
            leg.close().await;
        }
        self.replace_uplink(None).await;
    }

    pub fn stats(&self) -> RoomStats {
        let mut legs: Vec<LegStats> = self
            .legs()
            .iter()
            .map(|leg| LegStats {
                id: leg.id.clone(),
                packets: leg.stats.packets.load(Ordering::Relaxed),
                bytes: leg.stats.bytes.load(Ordering::Relaxed),
                repairs: leg.stats.repairs.load(Ordering::Relaxed),
                dropped: leg.stats.dropped.load(Ordering::Relaxed),
                loss: f64::from(leg.stats.loss.load(Ordering::Relaxed)) * 100.0 / 256.0,
                rtt_ms: Some(leg.stats.rtt_ms.load(Ordering::Relaxed)).filter(|r| *r > 0),
                limit_bps: leg.downlink.lock().limit(),
            })
            .collect();
        legs.sort_by(|a, b| a.id.cmp(&b.id));
        RoomStats {
            id: self.id.clone(),
            tracks: self.current().map_or(0, |u| u.tracks.len()),
            packets: self.packets.load(Ordering::Relaxed),
            bytes: self.bytes.load(Ordering::Relaxed),
            uplink_repairs: self.nacked.load(Ordering::Relaxed),
            keyframe_requests: self.keyframes.load(Ordering::Relaxed),
            limit_bps: *self.limit.lock(),
            legs,
        }
    }
}

/// The rate at least half the viewers carry: a congested viewer's limit,
/// any rate for the others; `None` when that is any rate.
fn majority_limit(limits: impl Iterator<Item = Option<u64>>) -> Option<u64> {
    let mut rates: Vec<u64> = limits.map(|l| l.unwrap_or(u64::MAX)).collect();
    if rates.is_empty() {
        return None;
    }
    rates.sort_unstable_by(|a, b| b.cmp(a));
    let rate = rates[rates.len().div_ceil(2) - 1];
    (rate != u64::MAX).then_some(rate)
}

#[cfg(test)]
mod tests {
    use super::majority_limit;

    #[test]
    fn one_slow_viewer_does_not_limit_the_others() {
        assert_eq!(majority_limit([None, Some(2_000_000)].into_iter()), None);
        assert_eq!(majority_limit([None, None, Some(1_000_000)].into_iter()), None);
        assert_eq!(
            majority_limit([None, Some(3_000_000), Some(5_000_000)].into_iter()),
            Some(5_000_000)
        );
        assert_eq!(
            majority_limit([Some(4_000_000), Some(2_000_000), Some(8_000_000), None].into_iter()),
            Some(8_000_000)
        );
        assert_eq!(majority_limit(std::iter::empty()), None);
    }
}
