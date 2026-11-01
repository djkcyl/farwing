//! A room: one host's published tracks (the uplink) and its viewers' legs.

use super::leg::{Leg, Rtcp};
use super::sdp::{self, Kind};
use farwing_protocol::api::ForwardLayer;
use parking_lot::{Mutex, RwLock};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
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
/// A viewer gets the low layer when its downlink carries less than this
/// share of the main stream (or the room's limit); one that carries at
/// least this share brings the stream down to it instead...
const LOW_SHARE: f64 = 0.75;
/// ...for this long...
const SHORT_FOR: Duration = Duration::from_millis(1500);
/// ...or this long when it carries less than this share...
const FAR_SHORT: f64 = 0.5;
const FAR_SHORT_FOR: Duration = Duration::from_secs(1);
/// ...unless the main stream is this small anyway (the low layer is about
/// 0.7 Mbps).
const LOW_WORTH: u64 = 2_500_000;
/// A viewer that came this recently and never had the low layer moves to
/// it at once when short (its first verdict already covers half a second).
const JUST_CAME: Duration = Duration::from_secs(5);
/// On the low layer, the main stream is tried again after this long without
/// congestion...
const TRY_MAIN_AFTER: Duration = Duration::from_secs(20);
/// A try first sends copies of recent packets at the main stream's rate
/// this long; congestion meanwhile ends it...
const PROBE_FOR: Duration = Duration::from_millis(1500);
/// ...rising to that rate over this long...
const PROBE_RAMP: Duration = Duration::from_millis(1000);
/// ...(the copies cycle through this many of the newest packets)...
const PROBE_SPAN: u16 = 256;
/// A viewer's layer is kept this long while it has no connection here.
const LAYER_KEPT: Duration = Duration::from_secs(10);
/// ...and the next try waits twice as long, as when the viewer is back on
/// the low layer within this; at most this long.
const TRY_FAILED_WITHIN: Duration = Duration::from_secs(30);
const TRY_MAIN_MOST: Duration = Duration::from_secs(320);
/// Viewers with impaired downlinks (losing, limited or dark) on the main
/// layer ask for keyframes (for everyone) together at most this often: a
/// few viewers' losses do not cost the others a keyframe each, and they
/// still catch up soon (the slow ones are on the low layer)...
const IMPAIRED_KEYFRAME_INTERVAL: Duration = Duration::from_secs(3);
/// ...and this often while the stream follows limited viewers, or when no
/// viewer's downlink is sound.
const FOLLOWED_KEYFRAME_INTERVAL: Duration = Duration::from_secs(1);
/// A connection this young is still waiting for its first picture.
const JOINING: Duration = Duration::from_secs(5);
/// A viewer's new connection takes the picture over within this.
const SWITCH_OVER: Duration = Duration::from_secs(2);
/// One impaired viewer's own keyframe requests go on at most this often (a
/// viewer on the main stream its downlink cannot carry, which it chose,
/// does not cost the others a keyframe every few seconds).
const IMPAIRED_LEG_KEYFRAME_INTERVAL: Duration = Duration::from_secs(10);
/// Lost uplink packets are requested again after this, up to `NACK_TRIES`
/// times within `NACK_AGE`.
const NACK_RETRY: Duration = Duration::from_millis(40);
const NACK_FIRST: Duration = Duration::from_millis(5);
const NACK_TRIES: u8 = 8;
const NACK_AGE: Duration = Duration::from_secs(1);

pub(super) struct Cache {
    slots: Vec<Option<Packet>>,
}

impl Cache {
    pub(super) fn new() -> Self {
        Self {
            slots: vec![None; CACHE],
        }
    }

    pub(super) fn get(&self, seq: u16) -> Option<&Packet> {
        self.slots[seq as usize % CACHE]
            .as_ref()
            .filter(|p| p.header.sequence_number == seq)
    }

    /// `false` if the packet was already there.
    pub(super) fn insert(&mut self, packet: Packet) -> bool {
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
pub(super) struct Missing {
    highest: Option<u16>,
    lost: HashMap<u16, Lost>,
}

impl Missing {
    pub(super) fn received(&mut self, seq: u16) {
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

    pub(super) fn due(&mut self, now: Instant) -> Vec<u16> {
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
    rate: Mutex<Rate>,
}

/// A track's rate over the last whole second.
#[derive(Default)]
struct Rate {
    since: Option<Instant>,
    bytes: u64,
    bps: u64,
}

impl Rate {
    fn add(&mut self, now: Instant, bytes: usize) {
        let since = *self.since.get_or_insert(now);
        let elapsed = now.duration_since(since);
        if elapsed >= Duration::from_secs(1) {
            self.bps = self.bytes * 8 * 1000 / elapsed.as_millis().max(1) as u64;
            self.bytes = 0;
            self.since = Some(now);
        }
        self.bytes += bytes as u64;
    }

    fn bps(&self) -> u64 {
        let stale = self
            .since
            .is_none_or(|since| since.elapsed() >= Duration::from_secs(2));
        if stale { 0 } else { self.bps }
    }
}

/// A viewer's layer: the main stream, or the low one when its downlink
/// cannot carry the main one (`docs/protocol/deployment-design.md` §5.5).
struct Layer {
    low: bool,
    /// The last switch.
    since: Instant,
    /// Since when the downlink has fallen short of the main stream.
    short_since: Option<Instant>,
    /// On the low layer: how long the downlink must stay calm before the
    /// main stream is tried again.
    backoff: Duration,
    /// The last time the main stream was tried again.
    tried_at: Option<Instant>,
    /// When the viewer last had a connection here: its layer outlives a
    /// moment without one (between leaving one and opening the next).
    seen: Instant,
    /// The viewer's own choice: the forwarder moves it only on `Auto`.
    pin: ForwardLayer,
    /// What its downlink carried when it moved to the low layer (or failed a
    /// try of the main stream): its connection there is calm on the small
    /// stream. Kept when the stream comes down to it (its next connection
    /// starts from it), forgotten once a try carries the main stream.
    capacity: Option<u64>,
    /// Before the main stream is tried again: copies of recent packets at
    /// its rate show whether the downlink carries it.
    probe: Option<Probe>,
}

struct Probe {
    started: Instant,
    /// The leg probed (the viewer's newest).
    leg: String,
    bps: u64,
    sent_at: Instant,
    /// Bytes still to send.
    owed: f64,
    /// Which recent packet to copy next.
    cursor: u16,
}

/// What a viewer's layer did.
#[derive(PartialEq)]
enum LayerStep {
    Kept,
    Switched,
    /// A probe of the main stream's rate began.
    Probing,
    /// The downlink did not carry the probe: the main stream waits longer.
    ProbeFailed,
}

impl Layer {
    fn new(now: Instant) -> Self {
        Self {
            low: false,
            since: now,
            short_since: None,
            backoff: TRY_MAIN_AFTER,
            tried_at: None,
            probe: None,
            seen: now,
            pin: ForwardLayer::Auto,
            capacity: None,
        }
    }

    /// Follows the viewer's downlink (`limit` while congested, `calm`
    /// since the last congestion; its newest leg `leg`) against the main
    /// stream's rate (`main`, under the room's `limit`).
    #[allow(clippy::too_many_arguments)]
    fn update(
        &mut self,
        now: Instant,
        leg: &str,
        limit: Option<u64>,
        dark: bool,
        calm: Duration,
        main: u64,
        room: Option<u64>,
    ) -> LayerStep {
        // The viewer chose its stream itself.
        match self.pin {
            ForwardLayer::Low => {
                self.probe = None;
                if self.low {
                    return LayerStep::Kept;
                }
                self.low = true;
                self.since = now;
                self.short_since = None;
                self.capacity = limit;
                return LayerStep::Switched;
            }
            ForwardLayer::Main => {
                self.probe = None;
                self.short_since = None;
                if !self.low {
                    return LayerStep::Kept;
                }
                self.low = false;
                self.since = now;
                self.tried_at = Some(now);
                self.capacity = None;
                return LayerStep::Switched;
            }
            ForwardLayer::Auto => {}
        }
        if self.low {
            if let Some(probe) = &self.probe {
                let probing = now.duration_since(probe.started);
                if calm < probing || dark || probe.leg != leg {
                    self.probe = None;
                    self.since = now;
                    self.backoff = (self.backoff * 2).min(TRY_MAIN_MOST);
                    self.capacity = limit.or(self.capacity);
                    return LayerStep::ProbeFailed;
                }
                if probing < PROBE_FOR {
                    return LayerStep::Kept;
                }
                self.probe = None;
                self.low = false;
                self.since = now;
                self.tried_at = Some(now);
                self.capacity = None;
                return LayerStep::Switched;
            }
            // The stream came down to what this viewer carries (most viewers
            // are as slow): back to it.
            if let (Some(room), Some(capacity)) = (room, self.capacity)
                && capacity as f64 >= LOW_SHARE * room.min(main) as f64
            {
                self.low = false;
                self.since = now;
                self.tried_at = Some(now);
                return LayerStep::Switched;
            }
            if calm >= self.backoff && now.duration_since(self.since) >= self.backoff && main > 0 {
                // The main stream joins the low one for a moment when it
                // takes over: probe for both.
                self.probe = Some(Probe {
                    started: now,
                    leg: leg.to_owned(),
                    bps: main,
                    sent_at: now,
                    owed: 0.0,
                    cursor: 0,
                });
                return LayerStep::Probing;
            }
            return LayerStep::Kept;
        }
        let reference = room.map_or(main, |room| room.min(main)) as f64;
        let share = limit.map_or(1.0, |limit| limit as f64 / reference.max(1.0));
        let short = !dark && main >= LOW_WORTH && share < LOW_SHARE;
        if !short {
            self.short_since = None;
            return LayerStep::Kept;
        }
        // Far short (a viewer that just came on a slow line): waiting does
        // not help.
        let just_came = self.tried_at.is_none() && now.duration_since(self.since) < JUST_CAME;
        let wait = if just_came {
            Duration::ZERO
        } else if share < FAR_SHORT {
            FAR_SHORT_FOR
        } else {
            SHORT_FOR
        };
        if now.duration_since(*self.short_since.get_or_insert(now)) < wait {
            return LayerStep::Kept;
        }
        // Back to the low layer soon after trying the main one: wait longer
        // before the next try.
        self.backoff = if self
            .tried_at
            .is_some_and(|at| now.duration_since(at) < TRY_FAILED_WITHIN)
        {
            (self.backoff * 2).min(TRY_MAIN_MOST)
        } else {
            TRY_MAIN_AFTER
        };
        self.low = true;
        self.since = now;
        self.short_since = None;
        self.capacity = limit;
        LayerStep::Switched
    }
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
                        rate: Mutex::default(),
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
    /// Keyframes the viewer asked for, and those passed on (before the
    /// per-track merge).
    #[serde(default)]
    pub keyframes_asked: u64,
    #[serde(default)]
    pub keyframes_passed: u64,
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
    /// Subscriptions so far (development impairments pick viewers by it).
    /// Development downlink emulation, by viewer session.
    pub emulators: Mutex<HashMap<String, Option<Arc<Mutex<super::emulate::Emulator>>>>>,
    /// The forwarded stream's limit, and when the host was last told.
    limit: Mutex<Option<u64>>,
    /// The server's egress budget per viewer (`docs/protocol/egress-policy.md`
    /// §4.2); 0: none.
    pub budget: AtomicU64,
    /// Each viewer's layer, by session.
    layers: Mutex<HashMap<String, Layer>>,
    limit_at: Mutex<Instant>,
    /// The last keyframe a viewer with an impaired downlink asked for.
    impaired_keyframe_at: Mutex<Option<Instant>>,
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
            emulators: Mutex::default(),
            limit: Mutex::default(),
            budget: AtomicU64::new(0),
            layers: Mutex::default(),
            limit_at: Mutex::new(Instant::now()),
            impaired_keyframe_at: Mutex::default(),
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
        track
            .rate
            .lock()
            .add(
                Instant::now(),
                webrtc::util::MarshalSize::marshal_size(&packet.header) + packet.payload.len(),
            );
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
        self.ask_keyframe(ssrc, true).await;
    }

    /// Keyframes of the video tracks (`ssrcs`) a viewer that just connected
    /// receives: not merged into one asked for before, which did not reach
    /// it.
    pub async fn first_keyframe(&self, ssrcs: &[u32]) {
        for ssrc in ssrcs {
            self.ask_keyframe(*ssrc, false).await;
        }
    }

    async fn ask_keyframe(&self, ssrc: u32, merge: bool) {
        let Some(uplink) = self.current() else { return };
        let Some(track) = uplink.track(ssrc) else { return };
        {
            let mut at = track.keyframe_at.lock();
            if merge && at.is_some_and(|at| at.elapsed() < KEYFRAME_INTERVAL) {
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

    /// Viewer `session` chose `layer` (`Auto`: the forwarder decides).
    pub fn choose_layer(&self, session: &str, layer: ForwardLayer) {
        let mut layers = self.layers.lock();
        let entry = layers
            .entry(session.to_owned())
            .or_insert_with(|| Layer::new(Instant::now()));
        if entry.pin != layer {
            tracing::info!(room = %self.id, %session, ?layer, "viewer chose its stream");
        }
        entry.pin = layer;
    }

    /// What viewer `session`'s downlink is known to carry: its connections'
    /// limits, else what it carried when it moved to the low layer. A new
    /// connection of the viewer starts from it.
    pub fn known_capacity(&self, session: &str) -> Option<u64> {
        let legs = self
            .legs()
            .iter()
            .filter(|leg| super::session_of(&leg.id) == session)
            .filter_map(|leg| leg.downlink.lock().limit())
            .min();
        legs.or_else(|| self.layers.lock().get(session).and_then(|l| l.capacity))
    }

    /// Whether `leg`'s keyframe request goes on (to the per-track merge).
    fn keyframe_allowed(&self, leg: &Leg) -> bool {
        leg.stats.keyframes_asked.fetch_add(1, Ordering::Relaxed);
        let allowed = self.keyframe_rationed(leg);
        tracing::debug!(leg = %leg.id, allowed, "viewer asks for a keyframe");
        if allowed {
            leg.stats.keyframes_passed.fetch_add(1, Ordering::Relaxed);
        }
        allowed
    }

    fn keyframe_rationed(&self, leg: &Leg) -> bool {
        {
            let mut downlink = leg.downlink.lock();
            // Back from an outage: everything since is lost, the picture
            // waits for this keyframe.
            if !downlink.impaired() || downlink.take_return() {
                return true;
            }
        }
        // A new connection waits for its first picture; a low-layer
        // viewer's keyframes go only to the others as slow as itself.
        let session = super::session_of(&leg.id);
        if leg.created.elapsed() < JOINING
            || self.layers.lock().get(session).is_some_and(|l| l.low)
        {
            return true;
        }
        let followed = self.limit.lock().is_some();
        let sound = self
            .legs()
            .iter()
            .any(|other| !other.downlink.lock().impaired());
        let interval = if followed || !sound {
            FOLLOWED_KEYFRAME_INTERVAL
        } else {
            IMPAIRED_KEYFRAME_INTERVAL
        };
        let now = Instant::now();
        let mut own = leg.rationed_keyframe_at.lock();
        if own.is_some_and(|at| now.duration_since(at) < IMPAIRED_LEG_KEYFRAME_INTERVAL) {
            return false;
        }
        let mut at = self.impaired_keyframe_at.lock();
        if at.is_some_and(|at| now.duration_since(at) < interval) {
            return false;
        }
        *at = Some(now);
        *own = Some(now);
        true
    }

    /// Probe traffic: copies of the newest packets of each probed viewer's
    /// video, sent as retransmissions (the viewer discards them), at the
    /// probe's rate.
    fn probe(&self, uplink: &Uplink, now: Instant) {
        let mut layers = self.layers.lock();
        for layer in layers.values_mut() {
            let Some(probe) = layer.probe.as_mut() else { continue };
            let Some(leg) = self.legs.read().get(&probe.leg).cloned() else { continue };
            // Rising to the full rate over most of the probe: a downlink that
            // cannot carry it shows so before the low stream suffers much.
            let ramp = (now.duration_since(probe.started).as_secs_f64() / PROBE_RAMP.as_secs_f64())
                .min(1.0);
            probe.owed +=
                probe.bps as f64 * ramp / 8.0 * now.duration_since(probe.sent_at).as_secs_f64();
            probe.sent_at = now;
            let Some(track) = leg.video.first().and_then(|ssrc| uplink.track(*ssrc)) else {
                continue;
            };
            let Some(highest) = track.missing.lock().highest else { continue };
            let copies: Vec<Packet> = {
                let cache = track.cache.lock();
                let mut copies = Vec::new();
                let mut misses = 0;
                while probe.owed > 0.0 && misses < 64 {
                    probe.cursor = (probe.cursor + 1) % PROBE_SPAN;
                    let Some(original) = cache.get(highest.wrapping_sub(probe.cursor)) else {
                        misses += 1;
                        continue;
                    };
                    let Some(rtx_pt) = uplink.wire.codecs.iter().find_map(|(pt, c)| {
                        (c.apt() == Some(original.header.payload_type)).then_some(*pt)
                    }) else {
                        break;
                    };
                    probe.owed -= (original.payload.len() + 60) as f64;
                    copies.push(leg.probe_copy(original, track.info.rtx, rtx_pt));
                }
                copies
            };
            for packet in copies {
                leg.push(packet);
            }
        }
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
        self.probe(&uplink, now);
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
    /// least half the main stream's viewers' downlinks carry (any rate:
    /// none). Viewers slower than that get the low layer, named in an RTCP
    /// APP packet sent with it (`farwing_rtc_wire::layers`); they do not
    /// lower everyone's picture.
    async fn send_limit(&self, uplink: &Uplink) {
        let now = Instant::now();
        // Each viewer by its newest connection.
        let mut viewers: HashMap<String, (u32, Arc<Leg>)> = HashMap::new();
        for leg in self.legs() {
            leg.downlink.lock().tick();
            let (session, connection) = match leg.id.split_once('#') {
                Some((session, connection)) => (session, connection.parse().unwrap_or(0)),
                None => (leg.id.as_str(), 0),
            };
            if viewers.get(session).is_none_or(|(newest, _)| *newest < connection) {
                viewers.insert(session.to_owned(), (connection, leg.clone()));
            }
        }
        let main = uplink
            .tracks
            .iter()
            .filter(|t| t.info.kind == Kind::Video)
            .map(|t| t.rate.lock().bps())
            .max()
            .unwrap_or(0);
        let (limit, low) = {
            let mut layers = self.layers.lock();
            for (session, layer) in layers.iter_mut() {
                if viewers.contains_key(session) {
                    layer.seen = now;
                }
            }
            layers.retain(|_, layer| now.duration_since(layer.seen) < LAYER_KEPT);
            // What each viewer carries (`None`: any rate): its newest
            // connection's limit; on the low layer, at most what it carried
            // when it moved there (that connection, calm on the small
            // stream, says little more). Dark
            // downlinks (nothing arrives) are not counted: a lower rate does
            // not reach them either.
            let capacities: Vec<(bool, Option<u64>)> = viewers
                .iter()
                .filter_map(|(session, (_, leg))| {
                    let downlink = leg.downlink.lock();
                    if downlink.dark() {
                        return None;
                    }
                    let low = layers.get(session).filter(|l| l.low);
                    let capacity = match (downlink.limit(), low.and_then(|l| l.capacity)) {
                        (Some(limit), Some(known)) => Some(limit.min(known)),
                        (limit, known) => known.or(limit),
                    };
                    Some((low.is_some(), capacity))
                })
                .collect();
            let limit = room_limit(&capacities, main);
            for (session, (_, leg)) in &viewers {
                let (limit_bps, dark, calm) = {
                    let downlink = leg.downlink.lock();
                    (downlink.limit(), downlink.dark(), downlink.calm_for())
                };
                let layer = layers
                    .entry(session.clone())
                    .or_insert_with(|| Layer::new(now));
                let step = layer.update(now, &leg.id, limit_bps, dark, calm, main, limit);
                if step != LayerStep::Kept {
                    tracing::info!(room = %self.id, %session, low = layer.low,
                        probing = step == LayerStep::Probing,
                        probe_failed = step == LayerStep::ProbeFailed,
                        main_bps = main, downlink_bps = ?limit_bps,
                        backoff_s = layer.backoff.as_secs(), "viewer's layer changed");
                }
            }
            let low: Vec<String> = layers
                .iter()
                .filter(|(_, l)| l.low)
                .map(|(session, _)| session.clone())
                .collect();
            // A viewer moving to the low layer: its older (main) connection
            // stops, so that it does not crowd out the new one on a downlink
            // that cannot carry it (one that can plays on until the viewer
            // switches over and leaves it).
            for leg in self.legs() {
                let session = super::session_of(&leg.id);
                let superseded = low.iter().any(|s| s == session)
                    && viewers.get(session).is_some_and(|(_, newest)| {
                        !Arc::ptr_eq(newest, &leg)
                            && (leg.downlink.lock().limit().is_some()
                                || newest.created.elapsed() >= SWITCH_OVER)
                    });
                leg.superseded.store(superseded, Ordering::Relaxed);
            }
            (limit, low)
        };
        // The server's budget per viewer binds the stream too.
        let limit = match self.budget.load(Ordering::Relaxed) {
            0 => limit,
            budget => Some(limit.map_or(budget, |l| l.min(budget))),
        };
        let previous = std::mem::replace(&mut *self.limit.lock(), limit);
        if previous.is_some() != limit.is_some() {
            tracing::info!(room = %self.id, limit_bps = ?limit, "forwarded stream limit");
        }
        let remb = ReceiverEstimatedMaximumBitrate {
            sender_ssrc: 0,
            // Any rate: one far above every stream's.
            bitrate: limit.map_or(1e12, |l| l as f32),
            ssrcs: vec![0],
        };
        let report = farwing_rtc_wire::layers::report(&low);
        if let Err(error) = uplink
            .pc
            .write_rtcp(&[Box::new(remb), Box::new(report)])
            .await
        {
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
                keyframes_asked: leg.stats.keyframes_asked.load(Ordering::Relaxed),
                keyframes_passed: leg.stats.keyframes_passed.load(Ordering::Relaxed),
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

/// The forwarded stream's limit for viewers that carry `capacities` (on the
/// low layer or not; `None`: any rate) while it runs at `main`: what at
/// least half of them carry, and no more than a viewer on the main layer
/// only a little short of it carries (the stream comes down by at most a
/// quarter rather than leave it stalling); `None` when that is any rate.
fn room_limit(capacities: &[(bool, Option<u64>)], main: u64) -> Option<u64> {
    let majority = majority_limit(capacities.iter().map(|(_, c)| *c));
    let near = capacities
        .iter()
        .filter(|(low, _)| !low)
        .filter_map(|(_, c)| *c)
        .filter(|c| *c as f64 >= LOW_SHARE * main as f64)
        .min();
    match (majority, near) {
        (Some(majority), Some(near)) => Some(majority.min(near)),
        (majority, near) => majority.or(near),
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
    use super::{majority_limit, room_limit};

    #[test]
    fn a_viewer_a_little_short_brings_the_stream_down_a_slower_one_does_not() {
        let main = 8_000_000;
        // 6.8 of 8 Mbps: the stream follows it.
        assert_eq!(room_limit(&[(false, None), (false, None), (false, Some(6_800_000))], main), Some(6_800_000));
        // 4 of 8 Mbps: left to the low layer.
        assert_eq!(room_limit(&[(false, None), (false, None), (false, Some(4_000_000))], main), None);
        // Most viewers slow, some of them already on the low layer: followed.
        assert_eq!(
            room_limit(&[(false, None), (true, Some(3_400_000)), (false, Some(3_400_000))], main),
            Some(3_400_000)
        );
    }

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

    #[test]
    fn a_slow_viewer_gets_the_low_layer_and_probes_before_trying_the_main_one() {
        use super::{FAR_SHORT_FOR, Layer, LayerStep, PROBE_FOR, SHORT_FOR, TRY_MAIN_AFTER};
        use std::time::{Duration, Instant};
        let start = Instant::now();
        let at = |s: f64| start + Duration::from_secs_f64(s);
        let main = 8_000_000;
        let mut layer = Layer::new(start);
        // Not a viewer that just came.
        layer.since = start.checked_sub(super::JUST_CAME).unwrap();
        // A small main stream, or a downlink that carries most of it: kept.
        assert!(layer.update(at(0.0), "a", Some(1_000_000), false, Duration::ZERO, 2_000_000, None) == LayerStep::Kept);
        assert!(layer.update(at(1.0), "a", Some(7_000_000), false, Duration::ZERO, main, None) == LayerStep::Kept);
        // Short of the main stream for SHORT_FOR: the low layer.
        assert!(layer.update(at(2.0), "a", Some(5_000_000), false, Duration::ZERO, main, None) == LayerStep::Kept);
        let halfway = 2.0 + SHORT_FOR.as_secs_f64() / 2.0;
        assert!(layer.update(at(halfway), "a", Some(5_000_000), false, Duration::ZERO, main, None) == LayerStep::Kept);
        let down = 2.0 + SHORT_FOR.as_secs_f64();
        assert!(layer.update(at(down), "a", Some(5_000_000), false, Duration::ZERO, main, None) == LayerStep::Switched);
        assert!(layer.low);
        // Calm for TRY_MAIN_AFTER: a probe, which congestion ends.
        let try_at = down + TRY_MAIN_AFTER.as_secs_f64();
        let calm = TRY_MAIN_AFTER;
        assert!(layer.update(at(try_at), "b", None, false, calm, main, None) == LayerStep::Probing);
        assert!(layer.update(at(try_at + 0.3), "b", Some(2_000_000), false, Duration::from_millis(100), main, None) == LayerStep::ProbeFailed);
        assert!(layer.low && layer.backoff == TRY_MAIN_AFTER * 2);
        // The next probe after twice as long; carried through, the main
        // stream.
        let again = try_at + 0.3 + 2.0 * TRY_MAIN_AFTER.as_secs_f64();
        let calm = 2 * TRY_MAIN_AFTER;
        assert!(layer.update(at(again), "b", None, false, calm, main, None) == LayerStep::Probing);
        let done = again + PROBE_FOR.as_secs_f64();
        assert!(layer.update(at(done), "b", None, false, calm + PROBE_FOR, main, None) == LayerStep::Switched);
        assert!(!layer.low);
        // Back down soon after (far short: sooner): the next try waits
        // longer still.
        assert!(layer.update(at(done + 1.0), "c", Some(2_000_000), false, Duration::ZERO, main, None) == LayerStep::Kept);
        let down = done + 1.0 + FAR_SHORT_FOR.as_secs_f64();
        assert!(layer.update(at(down), "c", Some(2_000_000), false, Duration::ZERO, main, None) == LayerStep::Switched);
        assert_eq!(layer.backoff, TRY_MAIN_AFTER * 4);
    }

    #[test]
    fn a_viewer_that_just_came_short_gets_the_low_layer_at_once() {
        use super::{JUST_CAME, Layer, LayerStep};
        use std::time::{Duration, Instant};
        let start = Instant::now();
        let main = 8_000_000;
        let mut layer = Layer::new(start);
        let first = start + Duration::from_millis(1200);
        assert!(layer.update(first, "a", Some(2_200_000), false, Duration::ZERO, main, None) == LayerStep::Switched);
        // Later on, far short waits a moment (a dip may pass).
        let mut layer = Layer::new(start);
        let later = start + JUST_CAME;
        assert!(layer.update(later, "a", Some(2_200_000), false, Duration::ZERO, main, None) == LayerStep::Kept);
    }

    #[test]
    fn a_slow_majority_is_followed_not_moved_to_the_low_layer() {
        use super::{Layer, LayerStep, SHORT_FOR};
        use std::time::{Duration, Instant};
        let start = Instant::now();
        let mut layer = Layer::new(start);
        // The room's limit follows these viewers: the main stream is theirs.
        for s in [0.0, SHORT_FOR.as_secs_f64() + 1.0] {
            let now = start + Duration::from_secs_f64(s);
            assert!(layer.update(now, "a", Some(2_500_000), false, Duration::ZERO, 8_000_000, Some(2_600_000)) == LayerStep::Kept);
        }
        assert!(!layer.low);
    }

    #[test]
    fn a_viewer_that_chose_its_stream_keeps_it() {
        use super::{Layer, LayerStep};
        use farwing_protocol::api::ForwardLayer;
        use std::time::{Duration, Instant};
        let start = Instant::now();
        let at = |s: f64| start + Duration::from_secs_f64(s);
        let main = 8_000_000;
        // 流畅: the low layer at once, never a try of the main stream.
        let mut layer = Layer::new(start);
        layer.pin = ForwardLayer::Low;
        assert!(layer.update(at(0.5), "a", None, false, Duration::ZERO, main, None) == LayerStep::Switched);
        assert!(layer.update(at(400.0), "a", None, false, Duration::from_secs(400), main, None) == LayerStep::Kept);
        assert!(layer.low && layer.probe.is_none());
        // 高清: the main stream however short the downlink.
        let mut layer = Layer::new(start);
        layer.pin = ForwardLayer::Main;
        for s in [1.2, 3.0, 9.0] {
            assert!(layer.update(at(s), "a", Some(2_000_000), false, Duration::ZERO, main, None) == LayerStep::Kept);
        }
        assert!(!layer.low);
        // Back to automatic: the forwarder decides again.
        layer.pin = ForwardLayer::Auto;
        layer.since = start.checked_sub(super::JUST_CAME).unwrap();
        assert!(layer.update(at(10.0), "a", Some(2_000_000), false, Duration::ZERO, main, None) == LayerStep::Kept);
        assert!(layer.update(at(11.1), "a", Some(2_000_000), false, Duration::ZERO, main, None) == LayerStep::Switched);
    }

    #[test]
    fn a_low_viewer_returns_once_the_stream_comes_down_to_it() {
        use super::{Layer, LayerStep};
        use std::time::{Duration, Instant};
        let start = Instant::now();
        let main = 8_000_000;
        let mut layer = Layer::new(start);
        let at = |s: f64| start + Duration::from_secs_f64(s);
        assert!(layer.update(at(1.2), "a", Some(3_400_000), false, Duration::ZERO, main, None) == LayerStep::Switched);
        assert!(layer.low);
        // Still the only slow one: it stays.
        assert!(layer.update(at(2.0), "b", None, false, Duration::ZERO, main, None) == LayerStep::Kept);
        // Most viewers are as slow: the stream follows them, and this one
        // is back on it.
        let room = Some(3_400_000);
        assert!(layer.update(at(3.0), "b", None, false, Duration::ZERO, 3_400_000, room) == LayerStep::Switched);
        assert!(!layer.low);
    }
}
