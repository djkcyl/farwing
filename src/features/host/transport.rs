//! Publisher-side pacing and negotiated RTX. No viewer-side retransmission logic
//! is reused: a repair has its own SSRC, payload type and SRTP sequence number.
use super::protection::{Budget, Kind};
use super::{Lease, lock};

/// Whether the owner (a participant, or the forwarder uplink) still sends.
type Requested = Arc<dyn Fn() -> bool + Send + Sync>;
use async_trait::async_trait;
use bytes::Bytes;
use goog_cc::{
    transport::{PacedPacketInfo, ProbeClusterConfig},
    units::DataRate,
};
use std::{
    collections::{BTreeMap, HashMap, HashSet, VecDeque},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};
use tokio_util::sync::CancellationToken;
use webrtc::rtcp::transport_feedbacks::transport_layer_cc::TransportLayerCc;
use webrtc::{
    interceptor::{
        Attributes, Error, Interceptor, InterceptorBuilder, RTCPReader, RTCPWriter, RTPReader,
        RTPWriter, stream_info::StreamInfo,
    },
    rtcp::transport_feedbacks::transport_layer_nack::TransportLayerNack,
    rtp::packet::Packet,
    util::marshal::MarshalSize,
};
type Result<T> = std::result::Result<T, Error>;
type Writer = Arc<dyn RTPWriter + Send + Sync>;
const FORMAT_EPOCH_ATTRIBUTE: usize = 0x5555_484f_5354_4550;
fn format_attributes(epoch: u64) -> Attributes {
    let mut attributes = Attributes::new();
    attributes.insert(FORMAT_EPOCH_ATTRIBUTE, epoch as usize);
    attributes
}

#[derive(Clone)]
struct Bound {
    info: StreamInfo,
    writer: Writer,
}
struct Stored {
    epoch: u64,
    packet: Packet,
    serial: u64,
    repair_at: Option<Instant>,
    keyframe: bool,
    repairs: u32,
    sent_at: Instant,
    captured: Option<Instant>,
}
struct FrameMeta {
    timestamp: u32,
    keyframe: bool,
    captured: Option<Instant>,
    first_delay: Option<u16>,
}
struct History {
    format: Option<super::format::Format>,
    packets: HashMap<u16, Stored>,
    order: VecDeque<(u16, u64)>,
    bytes: usize,
    next_serial: u64,
    primary: Option<u32>,
    rtx: Option<Bound>,
    rtx_sequence: u16,
    frame: Option<FrameMeta>,
    fec: Option<Bound>,
}
struct Pacer {
    at: Instant,
    debt: f64,
    draining: bool,
    group: u64,
    rate: f64,
}
struct SendRate {
    samples: VecDeque<(Instant, usize)>,
    bytes: usize,
    started: Option<Instant>,
    updated: Instant,
    limit_base: f64,
}
impl SendRate {
    fn expire(&mut self, now: Instant) {
        while self
            .samples
            .front()
            .is_some_and(|(at, _)| now.duration_since(*at) >= Duration::from_secs(1))
        {
            if let Some((_, bytes)) = self.samples.pop_front() {
                self.bytes -= bytes;
            }
        }
    }
    fn record(&mut self, bytes: usize) {
        let now = Instant::now();
        self.expire(now);
        if self.samples.is_empty() {
            self.started = Some(now);
        }
        self.samples.push_back((now, bytes));
        self.bytes += bytes;
    }
    fn limit(&mut self, target: u32) -> u64 {
        let now = Instant::now();
        self.expire(now);
        let span = self.started.map_or(1.0, |at| {
            now.duration_since(at).as_secs_f64().clamp(0.001, 1.0)
        });
        let actual = self.bytes as f64 * 8.0 / span;
        let target = f64::from(target);
        // Converge toward the measured pacer output, not the user limit.
        self.limit_base = if actual > target {
            (self.limit_base.max(target)
                + now.duration_since(self.updated).as_secs_f64() * (actual - target))
                .min(actual)
        } else {
            target
        };
        self.updated = now;
        (self.limit_base * 0.7).max(0.0) as u64
    }
}
/// How evenly each frame's packets are spread: they leave within
/// `alpha` x the frame interval. Loss reports widen the spread (shallow
/// buffers drop line-rate bursts); a quiet link narrows it again.
struct Drain {
    alpha: f64,
    lossy: Option<Instant>,
}
const DRAIN_MIN: f64 = 0.3;
const DRAIN_MAX: f64 = 0.9;
const DRAIN_INITIAL: f64 = 0.4;

struct Network {
    /// `FARWING_HOST_LINK`: emulated link to this viewer (measurements).
    link: Option<Arc<super::link::Link>>,
    /// The selected path is direct (not relayed).
    direct: AtomicBool,
    pacer: tokio::sync::Mutex<Pacer>,
    drain: Mutex<Drain>,
    /// Cinema mode: always spread frames over most of the interval.
    smooth: AtomicBool,
    rtt_us: AtomicU64,
    closed: AtomicBool,
    cancel: CancellationToken,
    requested: Requested,
    feedback_wake: tokio::sync::Notify,
    controller: Mutex<super::congestion::Controller>,
    queued: AtomicU64,
    network: AtomicBool,
    egress: tokio::sync::Mutex<()>,
    waiting: Mutex<BTreeMap<(u8, u64), (Instant, u64)>>,
    next_ticket: AtomicU64,
    network_overhead: AtomicU64,
    /// The largest datagram the selected route takes whole (IP header
    /// included).
    path_mtu: AtomicU64,
    srtp_overhead: AtomicU64,
    counts: [AtomicU64; 4],
    streams: Mutex<Vec<std::sync::Weak<Shared>>>,
    bindings: Mutex<HashMap<u32, std::sync::Weak<Shared>>>,
    allocations: Mutex<BTreeMap<usize, super::parameters::Bounds>>,
}
struct Shared {
    connection: Arc<Network>,
    index: usize,
    history: Mutex<History>,
    repair_rate: AtomicU64,
    send_rate: Mutex<SendRate>,
    repairs: Mutex<(VecDeque<(u32, u16)>, HashSet<(u32, u16)>)>,
    wake: tokio::sync::Notify,
    tcc_id: AtomicU64,
    timing_ids: Mutex<[u8; 4]>,
    queued_at: Mutex<Option<Instant>>,
    /// The kind of the selected route (`LAN`, `P2P`, `relay`), once chosen.
    route: Mutex<Option<&'static str>>,
    /// Probing starts once the first frame has crossed the link at the start
    /// rate: on a slow link with a shallow buffer, probes sent alongside it
    /// overflow the buffer and lose parts of the first keyframe.
    probe_after: Mutex<Option<Instant>>,
    budget: Mutex<Budget>,
    /// A NACKed media packet was not retransmitted (too old, rate limit or
    /// budget): the receiver will not repair it.
    repair_refused: std::sync::atomic::AtomicBool,
    /// The viewer decides decodability from the Dependency Descriptor: a
    /// refused repair is answered with an empty RTX packet for that sequence
    /// number, so libwebrtc stops waiting for it (its H.264/H.265 frame
    /// assembly holds every later frame while a packet is missing).
    generic_dependencies: std::sync::atomic::AtomicBool,
    fec: Mutex<Option<super::fec::Sender>>,
    fec_blocks: Mutex<VecDeque<(Bound, super::fec::Block, Packet, Instant, u64)>>,
    fec_wake: tokio::sync::Notify,
    fec_sequence: AtomicU64,
    fec_origin: u32,
    started: Instant,
    fps: AtomicU64,
    rtx_types: Mutex<HashMap<u8, u8>>,
    format_epoch: AtomicU64,
    repair_header: AtomicU64,
}
impl Network {
    /// `FARWING_DEV_DIRECT_LOSS=<percent>[,<unix seconds>]`: lose that share
    /// of media while the session is on a direct path (until that time, when
    /// given), as a carrier throttling P2P flows would (connectivity checks
    /// pass; the ICE agent's path load tests lose as much); for testing path
    /// switching.
    fn direct_loss(&self) -> bool {
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
        share > 0.0
            && until.is_none_or(|until| std::time::SystemTime::now() < until)
            && self.direct.load(Ordering::Acquire)
            && rand::random::<f64>() < share
    }
}

impl std::ops::Deref for Shared {
    type Target = Network;
    fn deref(&self) -> &Self::Target {
        &self.connection
    }
}

#[derive(Clone)]
pub(crate) struct Transport(Arc<Shared>);
impl Transport {
    pub(crate) fn new(
        lease: Lease,
        cancel: CancellationToken,
        bounds: super::parameters::Bounds,
    ) -> Self {
        Self::for_owner(Arc::new(move || lease.requested()), cancel, bounds)
    }
    pub(crate) fn for_owner(
        requested: Requested,
        cancel: CancellationToken,
        bounds: super::parameters::Bounds,
    ) -> Self {
        let rate = bounds.initial;
        let connection = Arc::new(Network {
            link: super::link::Link::from_environment(),
            direct: AtomicBool::new(false),
            pacer: tokio::sync::Mutex::new(Pacer {
                at: Instant::now(),
                debt: 0.0,
                draining: false,
                group: 1,
                rate: f64::from(rate) * 1.6 / 8.0,
            }),
            drain: Mutex::new(Drain {
                alpha: DRAIN_INITIAL,
                lossy: None,
            }),
            smooth: AtomicBool::new(false),
            rtt_us: AtomicU64::new(0),
            closed: AtomicBool::new(false),
            cancel,
            requested,
            feedback_wake: tokio::sync::Notify::new(),
            controller: Mutex::new(super::congestion::Controller::new(bounds)),
            queued: AtomicU64::new(0),
            network: AtomicBool::new(false),
            egress: tokio::sync::Mutex::new(()),
            waiting: Mutex::new(BTreeMap::new()),
            next_ticket: AtomicU64::new(0),
            network_overhead: AtomicU64::new(28),
            path_mtu: AtomicU64::new(1280),
            srtp_overhead: AtomicU64::new(16),
            counts: std::array::from_fn(|_| AtomicU64::new(0)),
            streams: Mutex::default(),
            bindings: Mutex::default(),
            allocations: Mutex::new(BTreeMap::from([(0, bounds)])),
        });
        Self::with_connection(connection, 0)
    }
    fn with_connection(connection: Arc<Network>, index: usize) -> Self {
        let rate = lock(&connection.allocations)
            .get(&index)
            .map_or(0, |b| b.initial);
        let transport = Self(Arc::new(Shared {
            connection: connection.clone(),
            index,
            history: Mutex::new(History {
                format: None,
                packets: HashMap::new(),
                order: VecDeque::new(),
                bytes: 0,
                next_serial: 0,
                primary: None,
                rtx: None,
                rtx_sequence: rand::random(),
                frame: None,
                fec: None,
            }),
            repair_rate: AtomicU64::new(u64::from(rate) * 7 / 10),
            send_rate: Mutex::new(SendRate {
                samples: VecDeque::new(),
                bytes: 0,
                started: None,
                updated: Instant::now(),
                limit_base: 0.0,
            }),
            repairs: Mutex::new((VecDeque::new(), HashSet::new())),
            wake: tokio::sync::Notify::new(),
            tcc_id: AtomicU64::new(0),
            timing_ids: Mutex::new([0; 4]),
            queued_at: Mutex::new(None),
            route: Mutex::new(None),
            probe_after: Mutex::new(None),
            budget: Mutex::new(Budget::default()),
            repair_refused: std::sync::atomic::AtomicBool::new(false),
            generic_dependencies: std::sync::atomic::AtomicBool::new(false),
            fec: Mutex::new(None),
            fec_blocks: Mutex::new(VecDeque::new()),
            fec_wake: tokio::sync::Notify::new(),
            fec_sequence: AtomicU64::new(rand::random::<u16>().into()),
            fec_origin: rand::random(),
            started: Instant::now(),
            fps: AtomicU64::new(60),
            rtx_types: Mutex::new(HashMap::new()),
            format_epoch: AtomicU64::new(0),
            repair_header: AtomicU64::new(32),
        }));
        lock(&connection.streams).push(Arc::downgrade(&transport.0));
        transport
    }
    pub(crate) fn fork(&self, index: usize) -> Self {
        Self::with_connection(self.0.connection.clone(), index)
    }
    pub(crate) fn register_ssrc(&self, ssrc: u32) {
        lock(&self.0.bindings).insert(ssrc, Arc::downgrade(&self.0));
    }
    fn streams(&self) -> Vec<Self> {
        lock(&self.0.streams)
            .iter()
            .filter_map(std::sync::Weak::upgrade)
            .map(Self)
            .collect()
    }
    fn for_stream(&self, info: &StreamInfo) -> Option<Self> {
        let primary = info
            .associated_stream
            .as_ref()
            .map_or(info.ssrc, |stream| stream.ssrc);
        lock(&self.0.bindings)
            .get(&primary)
            .and_then(std::sync::Weak::upgrade)
            .map(Self)
    }

    pub(crate) fn audio_budget(&self, bitrate: u32) {
        let mut allocations = lock(&self.0.allocations);
        if allocations.get(&usize::MAX).map_or(0, |b| b.maximum) == bitrate {
            return;
        }
        if bitrate == 0 {
            allocations.remove(&usize::MAX);
        } else {
            allocations.insert(
                usize::MAX,
                super::parameters::Bounds {
                    reservation: bitrate,
                    maximum: bitrate,
                    initial: bitrate,
                    probe: bitrate,
                },
            );
        }
        let bounds = super::allocation::total(&allocations);
        drop(allocations);
        if bounds.maximum > 0 {
            lock(&self.0.controller).configure(bounds, false);
        }
    }
    pub(crate) fn configure(&self, bounds: super::parameters::Bounds) {
        let bounds = {
            let mut streams = lock(&self.0.allocations);
            streams.insert(self.0.index, bounds);
            super::allocation::total(&streams)
        };
        lock(&self.0.controller).configure(bounds, true);
    }
    fn allocation(&self, target: u32, view: super::allocation::View) -> u32 {
        super::allocation::share(&lock(&self.0.allocations), self.0.index, target, view)
    }
    pub(crate) fn pause(&self) {
        let (bounds, video) = {
            let mut streams = lock(&self.0.allocations);
            if streams.remove(&self.0.index).is_none() {
                return;
            }
            // The audio's budget is kept under `usize::MAX`.
            (super::allocation::total(&streams), streams.keys().any(|k| *k != usize::MAX))
        };
        let mut controller = lock(&self.0.controller);
        if !video {
            controller.pause_video();
        }
        if bounds.maximum > 0 {
            controller.configure(bounds, false);
        }
        drop(controller);
        self.invalidate();
    }
    pub(crate) fn invalidate(&self) {
        self.0.format_epoch.fetch_add(1, Ordering::AcqRel);
        {
            let mut h = lock(&self.0.history);
            h.format = None;
            h.frame = None;
            h.packets.clear();
            h.order.clear();
            h.bytes = 0;
        }
        {
            let mut repairs = lock(&self.0.repairs);
            repairs.0.clear();
            repairs.1.clear();
        }
        lock(&self.0.fec_blocks).clear();
        if let Some(fec) = lock(&self.0.fec).as_mut() {
            fec.clear();
        }
        self.0.feedback_wake.notify_waiters();
    }
    pub(crate) fn media_rate(&self) -> u32 {
        let target = lock(&self.0.controller).target;
        let total = self.allocation(target, super::allocation::View::Media);
        lock(&self.0.budget).media_rate(total)
    }
    /// The latest RTCP round-trip time, once measured.
    pub(crate) fn round_trip(&self) -> Option<Duration> {
        match self.0.rtt_us.load(Ordering::Relaxed) {
            0 => None,
            us => Some(Duration::from_micros(us)),
        }
    }
    /// The congestion controller's current target, for the next session.
    pub(crate) fn estimate(&self) -> u32 {
        lock(&self.0.controller).estimates().0
    }
    pub(crate) fn automatic_rates(&self) -> super::parameters::NetworkSample {
        let (target, lower, loss, _generation) = lock(&self.0.controller).estimates();
        super::parameters::NetworkSample {
            probe: self.allocation(target, super::allocation::View::Probe),
            lower: self.allocation(lower, super::allocation::View::Probe),
            loss,
        }
    }
    pub(crate) fn cwnd_ratio(&self) -> f64 {
        lock(&self.0.controller).cwnd_reduce_ratio
    }
    /// The selected route: IP and UDP/TCP header bytes, and the largest
    /// datagram it takes whole.
    pub(crate) fn route(&self, network: usize, mtu: u64) {
        self.0
            .network_overhead
            .store(network as u64, Ordering::Relaxed);
        self.0.path_mtu.store(mtu, Ordering::Relaxed);
    }
    pub(crate) fn srtp_overhead(&self, bytes: usize) {
        self.0.srtp_overhead.store(bytes as u64, Ordering::Relaxed);
    }
    pub(crate) fn negotiated_codecs(
        &self,
        codecs: &[webrtc::rtp_transceiver::rtp_codec::RTCRtpCodecParameters],
    ) {
        let types = codecs
            .iter()
            .filter(|c| c.capability.mime_type.eq_ignore_ascii_case("video/rtx"))
            .filter_map(|c| {
                c.capability
                    .sdp_fmtp_line
                    .split(';')
                    .find_map(|s| {
                        s.trim()
                            .strip_prefix("apt=")
                            .and_then(|v| v.parse::<u8>().ok())
                    })
                    .map(|apt| (apt, c.payload_type))
            })
            .collect();
        *lock(&self.0.rtx_types) = types;
    }
    pub(crate) fn selected_route(&self, route: &'static str) {
        *lock(&self.0.route) = Some(route);
        self.0.direct.store(route != "relay", Ordering::Release);
    }
    /// The kind of the selected route, once chosen.
    pub(crate) fn route_class(&self) -> Option<&'static str> {
        *lock(&self.0.route)
    }
    pub(crate) fn remote_report(&self, loss: u8, rtt: Duration) {
        if !self.active() || !lock(&self.0.allocations).contains_key(&self.0.index) {
            return;
        }
        self.rtt(rtt);
        let mut drain = lock(&self.0.drain);
        let now = Instant::now();
        // `loss` is the RTCP fraction lost in 1/256 units; 5/256 is about 2 %.
        if loss >= 5 {
            drain.alpha = (drain.alpha * 1.3 + 0.05).min(DRAIN_MAX);
            drain.lossy = Some(now);
        } else if drain
            .lossy
            .is_none_or(|at| now.saturating_duration_since(at) >= Duration::from_secs(5))
        {
            drain.alpha = (drain.alpha * 0.9).max(DRAIN_MIN);
        }
    }
    /// Cinema mode spreads each frame over most of its interval.
    pub(crate) fn smooth(&self, smooth: bool) {
        self.0.smooth.store(smooth, Ordering::Relaxed);
    }
    fn overhead(&self) -> usize {
        self.network_overhead() + self.0.srtp_overhead.load(Ordering::Relaxed) as usize
    }
    fn network_overhead(&self) -> usize {
        self.0.network_overhead.load(Ordering::Relaxed) as usize
    }
    pub(crate) fn packet_limit(&self) -> usize {
        1460.min((self.0.path_mtu.load(Ordering::Relaxed) as usize).saturating_sub(self.overhead()))
    }
    pub(crate) fn wire_size(&self, rtp: usize) -> usize {
        rtp + self.overhead()
    }
    pub(crate) fn payload_limit(&self, header: usize, repair_header: usize) -> usize {
        self.0
            .repair_header
            .store(repair_header as u64, Ordering::Relaxed);
        // RSFEC protects the entire original RTP packet plus its length word.
        // Reserve its largest (109-source) mask, repair RTP header, MID and TCC
        // whenever the repair stream exists, not only while FEC is on: media
        // and their retransmissions (two bytes longer) then stay well below
        // a 1500-byte MTU, for links with a smaller one (PPPoE, tunnels) and
        // receivers with a smaller read buffer, where a lost full-size
        // packet could never be repaired.
        let repair = if lock(&self.0.history).fec.is_some() {
            2 + 27 + repair_header
        } else {
            0
        };
        self.packet_limit().saturating_sub(header + repair.max(2))
    }
    /// Video FEC goes out now: its stream is set up, it is not switched off
    /// (`FARWING_FEC=0`), the media leaves bandwidth unused for it, and the
    /// link shows loss (`fec_policy::MIN_LOSS`). Otherwise no repair packets
    /// are sent.
    fn fec_active(&self) -> bool {
        static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        *ON.get_or_init(|| std::env::var("FARWING_FEC").map_or(true, |v| v != "0"))
            && lock(&self.0.history).fec.is_some()
            && lock(&self.0.budget).fec_room()
            && lock(&self.0.controller).loss >= crate::transport::fec_policy::MIN_LOSS
    }
    pub(crate) fn media_ready(&self) -> bool {
        lock(&self.0.history).primary.is_some()
    }
    pub(crate) fn network(&self, available: bool) {
        self.0.network.store(available, Ordering::Release);
        lock(&self.0.controller).network(available);
        for stream in self.streams() {
            lock(&stream.0.budget).network(available);
        }
        self.0.feedback_wake.notify_waiters();
    }
    pub(crate) fn route_changed(&self) {
        lock(&self.0.controller).route_changed();
        for stream in self.streams() {
            lock(&stream.0.budget).reset(true);
            lock(&stream.0.fec_blocks).clear();
            if let Some(fec) = lock(&stream.0.fec).as_mut() {
                fec.clear();
            }
        }
        self.0.feedback_wake.notify_waiters();
    }
    pub(crate) fn feedback(&self, feedback: &TransportLayerCc) {
        lock(&self.0.controller).feedback(feedback);
        self.0.feedback_wake.notify_waiters();
    }
    pub(crate) async fn control(&self) {
        let mut interval = tokio::time::interval(Duration::from_millis(25));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let mut report_at = Instant::now();
        let mut reported_bytes = BTreeMap::<usize, [u64; 3]>::new();
        while self.active() {
            tokio::select! { _=self.0.cancel.cancelled()=>break, _=interval.tick()=>{} }
            let mut controller = lock(&self.0.controller);
            controller.tick(self.0.queued.load(Ordering::Relaxed) as usize);
            for stream in self.streams() {
                // Probe headroom is only an observation for automatic quality.
                // Repair budgets must use the same ordinary share as media.
                let allocation =
                    stream.allocation(controller.target, super::allocation::View::Media);
                stream.0.repair_rate.store(
                    lock(&stream.0.send_rate).limit(allocation),
                    Ordering::Release,
                );
                lock(&stream.0.budget).update(
                    allocation,
                    controller.loss,
                    controller.delay_overuse(),
                    controller.link_pressure(),
                    Duration::from_secs_f64(
                        self.0.queued.load(Ordering::Relaxed) as f64 * 8.0
                            / f64::from(controller.pacing.max(1)),
                    ),
                );
            }
            if report_at.elapsed() >= Duration::from_secs(5) {
                let span = report_at.elapsed().as_secs_f64();
                for stream in self.streams() {
                    let allocation =
                        stream.allocation(controller.target, super::allocation::View::Media);
                    let budget = lock(&stream.0.budget);
                    let totals = budget.sent_totals();
                    let rates = reported_bytes
                        .insert(stream.0.index, totals)
                        .filter(|previous| totals.iter().zip(previous).all(|(a, b)| a >= b))
                        .map(|previous| {
                            std::array::from_fn::<_, 3, _>(|i| {
                                ((totals[i] - previous[i]) as f64 * 8.0 / span).round() as u64
                            })
                        });
                    tracing::info!(
                        stream = stream.0.index,
                        allocation_bps = allocation,
                        encoder_allocation_bps = budget.media_rate(allocation),
                        sample_ms = (span * 1000.0).round() as u64,
                        media_wire_bps = rates.map(|r| r[0]),
                        rtx_wire_bps = rates.map(|r| r[1]),
                        fec_wire_bps = rates.map(|r| r[2]),
                        "host measured media egress"
                    );
                }
                tracing::info!(
                    feedbacks = controller.feedback_count,
                    target_bps = controller.target,
                    pacing_bps = controller.pacing,
                    loss = controller.loss,
                    delay_overuse = controller.delay_overuse(),
                    link_pressure = controller.link_pressure(),
                    rtt_ms = controller.rtt.ms(),
                    cwnd_reduce_ratio = controller.cwnd_reduce_ratio,
                    bounded_target_bps = controller.estimates().1,
                    burst_bytes = controller.burst_bytes(),
                    queued_bytes = self.0.queued.load(Ordering::Relaxed),
                    media_packets = self.0.counts[0].load(Ordering::Relaxed),
                    rtx_packets = self.0.counts[1].load(Ordering::Relaxed),
                    fec_packets = self.0.counts[2].load(Ordering::Relaxed),
                    probe_packets = self.0.counts[3].load(Ordering::Relaxed),
                    "host send controller"
                );
                report_at = Instant::now();
            }
        }
    }
    pub(crate) fn frame(
        &self,
        timestamp: u32,
        keyframe: bool,
        captured: Option<Instant>,
        format: super::format::Format,
    ) {
        let mut history = lock(&self.0.history);
        let changed = history.format != Some(format);
        if changed {
            history.format = Some(format);
            self.0.format_epoch.fetch_add(1, Ordering::AcqRel);
            history.packets.clear();
            history.order.clear();
            history.bytes = 0;
        }
        history.frame = Some(FrameMeta {
            timestamp,
            keyframe,
            captured,
            first_delay: None,
        });
        drop(history);
        if changed {
            let mut repairs = lock(&self.0.repairs);
            repairs.0.clear();
            repairs.1.clear();
            drop(repairs);
            lock(&self.0.fec_blocks).clear();
            if let Some(fec) = lock(&self.0.fec).as_mut() {
                fec.clear();
            }
            self.0.feedback_wake.notify_waiters();
        }
    }
    pub(crate) fn fps(&self, fps: u32) {
        self.0.fps.store(fps.into(), Ordering::Relaxed);
    }
    pub(crate) fn queued_frame(&self, bytes: usize) -> QueuedFrame {
        *lock(&self.0.queued_at) = Some(Instant::now());
        self.0.queued.fetch_add(bytes as u64, Ordering::Relaxed);
        QueuedFrame {
            transport: self.clone(),
            bytes: bytes as u64,
            remaining: bytes as u64,
        }
    }
    pub(crate) fn rtt(&self, rtt: Duration) {
        self.0.rtt_us.store(
            rtt.as_micros().min(u64::MAX as u128) as u64,
            Ordering::Relaxed,
        );
    }
    fn active(&self) -> bool {
        !self.0.closed.load(Ordering::Acquire)
            && !self.0.cancel.is_cancelled()
            && (self.0.requested)()
    }
    async fn write(
        &self,
        parent: &Writer,
        packet: &Packet,
        attributes: &Attributes,
    ) -> Result<usize> {
        let (priority, extra) = {
            let history = lock(&self.0.history);
            if history.primary == Some(packet.header.ssrc) {
                (2, 0)
            } else if history
                .rtx
                .as_ref()
                .is_some_and(|b| b.info.ssrc == packet.header.ssrc)
            {
                (1, packet.marshal_size() + self.overhead())
            } else {
                (2, packet.marshal_size() + self.overhead())
            }
        };
        let ticket = self.ticket(priority, extra);
        let bytes = (packet.marshal_size() + self.overhead()) as f64;
        loop {
            let notified = self.0.feedback_wake.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if !self.active() {
                return Err(Error::ErrIoEOF);
            }
            if !self.0.network.load(Ordering::Acquire) {
                tokio::select! { _=self.0.cancel.cancelled()=>return Err(Error::ErrIoEOF), _=notified=>{} }
                continue;
            }
            if !ticket.first() {
                tokio::select! { _=self.0.cancel.cancelled()=>return Err(Error::ErrIoEOF),_=notified=>{} }
                continue;
            }
            let egress = tokio::select! { _=self.0.cancel.cancelled()=>return Err(Error::ErrIoEOF),guard=self.0.egress.lock()=>guard };
            if !ticket.first() {
                drop(egress);
                continue;
            }
            let (base_rate, burst) = {
                let c = lock(&self.0.controller);
                (
                    f64::from(c.pacing.max(300_000)) / 8.0,
                    c.burst_bytes() as f64,
                )
            };
            let mut pacer = self.0.pacer.lock().await;
            let now = Instant::now();
            pacer.debt =
                (pacer.debt - now.duration_since(pacer.at).as_secs_f64() * pacer.rate).max(0.0);
            pacer.at = now;
            if pacer.debt == 0.0 {
                pacer.draining = false;
                pacer.group = pacer.group.wrapping_add(1);
            }
            // Drain each frame within alpha x its interval (4–30 ms), using
            // the remaining time, with a 1 ms floor.
            let queued = self.0.queued.load(Ordering::Relaxed) as f64;
            let oldest = lock(&self.0.waiting).values().map(|v| v.0).min();
            let oldest = oldest.into_iter().chain(*lock(&self.0.queued_at)).min();
            let age = oldest.map_or(0., |at| now.saturating_duration_since(at).as_secs_f64());
            let alpha = if self.0.smooth.load(Ordering::Relaxed) {
                DRAIN_MAX
            } else {
                lock(&self.0.drain).alpha
            };
            let interval = 1.0 / self.0.fps.load(Ordering::Relaxed).max(1) as f64;
            let window = (alpha * interval).clamp(0.004, 0.030);
            let required = queued / (window - age).max(0.001);
            let rate = base_rate
                .max(required)
                .max(if pacer.debt > 0.0 { pacer.rate } else { 0.0 });
            pacer.rate = rate;
            // Send a burst, then repay the full media debt. Idle
            // time cannot accumulate credit for a later oversized burst.
            if !pacer.draining && pacer.debt < burst.min(rate * 0.040) {
                pacer.debt += bytes;
                let group = pacer.group;
                drop(pacer);
                return self
                    .emit_locked(
                        parent,
                        packet,
                        attributes,
                        PacedPacketInfo::default(),
                        Some(group),
                    )
                    .await;
            }
            pacer.draining = true;
            let delay = Duration::from_secs_f64(pacer.debt / rate);
            drop(pacer);
            drop(egress);
            tokio::select! {_ = self.0.cancel.cancelled()=>return Err(Error::ErrIoEOF),_=tokio::time::sleep(delay)=>{},_=notified=>{}}
        }
    }
    async fn emit(
        &self,
        parent: &Writer,
        packet: &Packet,
        attributes: &Attributes,
        pacing: PacedPacketInfo,
        burst: Option<u64>,
    ) -> Result<usize> {
        let ticket = self.ticket(7, packet.marshal_size() + self.overhead());
        loop {
            let notified = self.0.feedback_wake.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if !self.active() {
                return Err(Error::ErrIoEOF);
            }
            if !ticket.first() || !self.0.network.load(Ordering::Acquire) {
                tokio::select! {_=self.0.cancel.cancelled()=>return Err(Error::ErrIoEOF),_=notified=>{}}
                continue;
            }
            let _egress = tokio::select! {_=self.0.cancel.cancelled()=>return Err(Error::ErrIoEOF),guard=self.0.egress.lock()=>guard};
            if ticket.first() {
                return self
                    .emit_locked(parent, packet, attributes, pacing, burst)
                    .await;
            }
        }
    }
    fn ticket(&self, priority: u8, extra: usize) -> Ticket {
        let key = (priority, self.0.next_ticket.fetch_add(1, Ordering::Relaxed));
        lock(&self.0.waiting).insert(key, (Instant::now(), extra as u64));
        self.0.queued.fetch_add(extra as u64, Ordering::Relaxed);
        self.0.feedback_wake.notify_waiters();
        Ticket {
            transport: self.clone(),
            key,
        }
    }
    async fn emit_locked(
        &self,
        parent: &Writer,
        packet: &Packet,
        attributes: &Attributes,
        pacing: PacedPacketInfo,
        burst: Option<u64>,
    ) -> Result<usize> {
        if !self.active() {
            return Err(Error::ErrIoEOF);
        }
        if attributes
            .get(&FORMAT_EPOCH_ATTRIBUTE)
            .is_some_and(|epoch| *epoch as u64 != self.0.format_epoch.load(Ordering::Acquire))
        {
            return Ok(0);
        }
        let id = self.0.tcc_id.load(Ordering::Relaxed) as u8;
        let mut packet = packet.clone();
        let [absolute, timing, sending_delay, offset] = *lock(&self.0.timing_ids);
        let now = Instant::now();
        let (capture, delay) = {
            let mut h = lock(&self.0.history);
            if h.primary == Some(packet.header.ssrc) {
                if let Some(f) = h
                    .frame
                    .as_mut()
                    .filter(|f| f.timestamp == packet.header.timestamp)
                {
                    let delay = f.captured.map(|at| {
                        *f.first_delay.get_or_insert_with(|| {
                            ((now.saturating_duration_since(at).as_micros() + 500) / 1000)
                                .min(u16::MAX.into()) as u16
                        })
                    });
                    (f.captured, delay)
                } else {
                    (None, None)
                }
            } else if h
                .rtx
                .as_ref()
                .is_some_and(|r| r.info.ssrc == packet.header.ssrc)
                && packet.payload.len() >= 2
            {
                let seq = u16::from_be_bytes([packet.payload[0], packet.payload[1]]);
                (h.packets.get(&seq).and_then(|p| p.captured), None)
            } else {
                (None, None)
            }
        };
        if absolute != 0 && packet.header.get_extension(absolute).is_some() {
            let ntp = webrtc::rtp::extension::abs_send_time_extension::unix2ntp(
                std::time::SystemTime::now(),
            );
            let value = ((ntp >> 14) as u32 & 0xFFFFFF).to_be_bytes();
            packet
                .header
                .set_extension(absolute, Bytes::copy_from_slice(&value[1..]))?;
        }
        if let Some(capture) = capture {
            let delta = ((now.saturating_duration_since(capture).as_micros() + 500) / 1000)
                .min(u16::MAX.into()) as u16;
            if let Some(mut value) = packet
                .header
                .get_extension(timing)
                .filter(|v| v.len() == 13)
                .map(|v| v.to_vec())
            {
                value[7..9].copy_from_slice(&delta.to_be_bytes());
                packet.header.set_extension(timing, value.into())?;
            }
            if offset != 0 && packet.header.get_extension(offset).is_some() {
                let ticks = (now.saturating_duration_since(capture).as_micros() * 9 / 100)
                    .min(0x7FFFFF) as u32;
                packet
                    .header
                    .set_extension(offset, Bytes::copy_from_slice(&ticks.to_be_bytes()[1..]))?;
            }
        }
        if let Some(delay) = delay
            .filter(|_| sending_delay != 0 && packet.header.get_extension(sending_delay).is_some())
        {
            packet
                .header
                .set_extension(sending_delay, Bytes::copy_from_slice(&delay.to_be_bytes()))?;
        }
        if id > 14 && packet.header.extension_profile != 0x1000 {
            packet.header.extension = true;
            packet.header.extension_profile = 0x1000;
            let length: usize = packet
                .header
                .extensions
                .iter()
                .map(|e| e.payload.len() + 2)
                .sum();
            packet.header.extensions_padding = (4 - length % 4) % 4;
        }
        let sequence = if id != 0 {
            let seq = lock(&self.0.controller).next_sequence();
            packet
                .header
                .set_extension(id, Bytes::copy_from_slice(&(seq as u16).to_be_bytes()))?;
            Some(seq)
        } else {
            None
        };
        let size = if self.0.direct_loss() {
            packet.marshal_size()
        } else {
            match self.0.link.as_ref() {
                Some(link) => link.send(parent, &packet, attributes, self.overhead()),
                None => tokio::select! {
                    _=self.0.cancel.cancelled()=>return Err(Error::ErrIoEOF),
                    result=parent.write(&packet, attributes)=>result?,
                },
            }
        };
        if size > 0 {
            if size + self.overhead() > 1500 {
                tracing::debug!(target: "farwing::packet_size", ssrc = packet.header.ssrc,
                    rtp = size, wire = size + self.overhead(), "host packet exceeds a 1500-byte MTU");
            }
            let wire = size + self.network_overhead();
            lock(&self.0.send_rate).record(wire);
            if let Some(sequence) = sequence {
                lock(&self.0.controller).sent(sequence, wire, pacing, burst, false);
            }
            let (primary, fec) = {
                let history = lock(&self.0.history);
                (
                    history.primary == Some(packet.header.ssrc),
                    history
                        .fec
                        .as_ref()
                        .is_some_and(|f| f.info.ssrc == packet.header.ssrc),
                )
            };
            let kind = if pacing.probe_cluster_id != PacedPacketInfo::NOT_APROBE {
                Kind::Probe
            } else if primary {
                Kind::Media
            } else if fec {
                Kind::Fec
            } else {
                Kind::Rtx
            };
            if kind == Kind::Rtx {
                tracing::debug!(target: "farwing::nack_audit",
                    seq = packet.payload.get(..2).map(|osn| u16::from_be_bytes([osn[0], osn[1]])),
                    rtx_seq = packet.header.sequence_number, transport_seq = sequence, bytes = size,
                    "host retransmitted a packet");
            }
            self.0.counts[match kind {
                Kind::Media => 0,
                Kind::Rtx => 1,
                Kind::Fec => 2,
                Kind::Probe => 3,
            }]
            .fetch_add(1, Ordering::Relaxed);
            lock(&self.0.budget).record(kind, wire);
            if primary {
                self.remember(&packet);
                self.fec_source(&packet);
            }
        }
        Ok(size)
    }
    fn fec_source(&self, packet: &Packet) {
        if !self.fec_active() {
            if let Some(f) = lock(&self.0.fec).as_mut() {
                f.clear();
            }
            return;
        }
        let (bound, key) = {
            let h = lock(&self.0.history);
            (
                h.fec.clone(),
                h.frame
                    .as_ref()
                    .is_some_and(|f| f.timestamp == packet.header.timestamp && f.keyframe),
            )
        };
        let Some(bound) = bound else { return };
        let (loss, rtt) = {
            let c = lock(&self.0.controller);
            (c.loss, c.rtt.ms().max(0) as u64)
        };
        let result = {
            let mut f = lock(&self.0.fec);
            let Some(f) = f.as_mut() else { return };
            f.push(packet, key, loss, rtt)
        };
        match result {
            Ok(Some(block)) => {
                let mut queue = lock(&self.0.fec_blocks);
                if queue.len() < 8 {
                    queue.push_back((
                        bound,
                        block,
                        packet.clone(),
                        Instant::now(),
                        self.0.format_epoch.load(Ordering::Acquire),
                    ));
                    self.0.fec_wake.notify_one();
                }
            }
            Ok(None) => {}
            Err(error) => tracing::warn!(%error,"host FEC source rejected"),
        }
    }
    pub(crate) async fn fec_worker(&self) {
        while self.active() {
            let wake = self.0.fec_wake.notified();
            tokio::pin!(wake);
            wake.as_mut().enable();
            let item = lock(&self.0.fec_blocks).pop_front();
            let Some((bound, block, source, _, epoch)) = item else {
                tokio::select! {_=self.0.cancel.cancelled()=>return,_=wake=>{}};
                continue;
            };
            let (count, id) = lock(&self.0.budget).reserve_fec(
                block.desired,
                self.wire_size(
                    block.shard_size + 27 + self.0.repair_header.load(Ordering::Relaxed) as usize,
                ),
            );
            if count == 0 {
                continue;
            }
            let mut reservation = Reserved {
                transport: self.clone(),
                id,
                actual: 0,
            };
            let keyframe = block.keyframe;
            let payloads = match block.payloads(count) {
                Ok(p) => p,
                Err(error) => {
                    tracing::warn!(%error,"host FEC encode failed");
                    continue;
                }
            };
            tracing::debug!(
                repairs = payloads.len(),
                keyframe,
                "host FEC block generated"
            );
            for payload in payloads {
                if !self.active() {
                    break;
                }
                let mut packet = Packet {
                    header: webrtc::rtp::header::Header {
                        version: 2,
                        extension: source.header.extension_profile == 0x1000,
                        extension_profile: if source.header.extension_profile == 0x1000 {
                            0x1000
                        } else {
                            0
                        },
                        ssrc: bound.info.ssrc,
                        payload_type: bound.info.payload_type,
                        sequence_number: self.0.fec_sequence.fetch_add(1, Ordering::Relaxed) as u16,
                        timestamp: self
                            .0
                            .fec_origin
                            .wrapping_add((self.0.started.elapsed().as_micros() * 9 / 100) as u32),
                        ..Default::default()
                    },
                    payload: payload.into(),
                };
                if let Some(mid) = bound
                    .info
                    .rtp_header_extensions
                    .iter()
                    .find(|e| e.uri == "urn:ietf:params:rtp-hdrext:sdes:mid")
                {
                    if let Some(value) = source.header.get_extension(mid.id as u8) {
                        let _ = packet.header.set_extension(mid.id as u8, value);
                    }
                }
                let attributes = format_attributes(epoch);
                match self.write(&bound.writer, &packet, &attributes).await {
                    Ok(0) => break,
                    Ok(size) => reservation.actual += size + self.network_overhead(),
                    Err(error) => {
                        if self.active() {
                            tracing::debug!(%error,"host FEC send failed");
                        }
                        break;
                    }
                }
            }
        }
    }
    fn probe_packet(&self) -> Option<(Bound, Packet, u64)> {
        let mut history = lock(&self.0.history);
        let bound = history.rtx.clone()?;
        let mut packet = history.order.iter().rev().find_map(|(seq, serial)| {
            history
                .packets
                .get(seq)
                .filter(|p| p.serial == *serial && p.packet.marshal_size() >= 200)
                .map(|p| p.packet.clone())
        })?;
        let mut payload = Vec::with_capacity(packet.payload.len() + 2);
        payload.extend_from_slice(&packet.header.sequence_number.to_be_bytes());
        payload.extend_from_slice(&packet.payload);
        packet.payload = payload.into();
        packet.header.ssrc = bound.info.ssrc;
        packet.header.payload_type = *lock(&self.0.rtx_types).get(&packet.header.payload_type)?;
        packet.header.sequence_number = history.rtx_sequence;
        history.rtx_sequence = history.rtx_sequence.wrapping_add(1);
        Some((bound, packet, self.0.format_epoch.load(Ordering::Acquire)))
    }
    pub(crate) async fn probes(&self) {
        // `FARWING_PROBE_PACING=0`: the previous sender (runtime timers, a
        // cluster dropped 10 ms behind, one step per wakeup), for comparison.
        let legacy = std::env::var("FARWING_PROBE_PACING").is_ok_and(|value| value == "0");
        // A probe cluster this far behind its byte clock is abandoned.
        let late_limit_us: u64 = if legacy { 10_000 } else { 30_000 };
        let mut active = None::<(ProbeClusterConfig, Instant, usize, i64)>;
        // The runtime's timers wake 5–15 ms late at the default Windows timer
        // resolution, far coarser than a cluster's 2 ms steps: while a cluster
        // is out, wait on a high-resolution waitable timer instead.
        let wake = Arc::new(tokio::sync::Notify::new());
        let mut precise = (!legacy)
            .then(|| {
                crate::platform::windows::input::deadline::Timer::new(wake.clone())
                    .map_err(|error| tracing::debug!(%error, "probe pacing falls back to runtime timers"))
                    .ok()
            })
            .flatten();
        while self.active() {
            // Follow the cluster's byte clock. Sleeping another 2ms on every
            // iteration accumulates timer rounding and write time, lowering
            // the measured probe rate (especially with small media packets).
            let deadline = active
                .as_ref()
                .and_then(|(cluster, started, sent, _)| {
                    let rate = cluster.target_data_rate.bps_or(0);
                    (rate > 0).then(|| {
                        *started + Duration::from_micros(*sent as u64 * 8_000_000 / rate as u64)
                    })
                })
                .unwrap_or_else(|| Instant::now() + Duration::from_millis(2));
            let armed = match precise.as_mut() {
                Some(timer) if active.is_some() => timer.arm(deadline).is_ok(),
                Some(timer) => {
                    timer.disarm();
                    false
                }
                None => false,
            };
            if armed {
                tokio::select! {_=self.0.cancel.cancelled()=>break,_=wake.notified()=>{}}
            } else {
                tokio::select! {_=self.0.cancel.cancelled()=>break,_=tokio::time::sleep_until(tokio::time::Instant::from_std(deadline))=>{}}
            }
            let owner = self.streams().into_iter().find(|stream| {
                stream.0.tcc_id.load(Ordering::Relaxed) != 0
                    && lock(&self.0.allocations).contains_key(&stream.0.index)
                    && !lock(&stream.0.history).packets.is_empty()
            });
            let Some(owner) = owner else {
                active = None;
                continue;
            };
            if active.is_none()
                && lock(&self.0.probe_after).is_none_or(|at| Instant::now() < at)
            {
                continue;
            }
            if active.is_none() {
                let mut c = lock(&self.0.controller);
                while let Some(cluster) = c.probes.pop_front() {
                    if c.probe_is_current(&cluster) {
                        active = Some((cluster, Instant::now(), 0, 0));
                        break;
                    }
                }
            }
            let Some((cluster, started, sent, probes)) = active.as_mut() else {
                continue;
            };
            if !lock(&self.0.controller).probe_is_current(cluster) {
                active = None;
                continue;
            }
            let rate = cluster.target_data_rate.bps_or(0).max(0) as u64;
            if rate == 0 {
                active = None;
                continue;
            }
            let minimum =
                ((rate as u128 * cluster.target_duration.us().max(0) as u128) / 8_000_000) as usize;
            let schedule_us = *sent as u64 * 8_000_000 / rate;
            let elapsed_us = started.elapsed().as_micros() as u64;
            // A late wakeup sends what is due by now, up to two steps, rather
            // than dropping the cluster: a cluster of one or two packets gives
            // no probe result. Give up only far behind.
            if *probes > 0 && elapsed_us > schedule_us + late_limit_us {
                active = None;
                continue;
            }
            if elapsed_us < schedule_us {
                continue;
            }
            let delta = cluster.min_probe_delta.us().max(2_000) as u64;
            let step = ((rate as u128 * delta as u128) / 8_000_000).clamp(200, 128_000) as usize;
            let due = ((rate as u128 * u128::from(elapsed_us + delta)) / 8_000_000) as usize;
            let burst = if legacy {
                step
            } else {
                due.saturating_sub(*sent).clamp(step, step * 2)
            };
            let mut bytes = 0;
            while bytes < burst && self.active() {
                let Some((bound, packet, epoch)) = owner.probe_packet() else {
                    break;
                };
                let info = PacedPacketInfo {
                    send_bitrate: DataRate::from_bits_per_sec(rate as i64),
                    probe_cluster_id: cluster.id,
                    probe_cluster_min_probes: cluster.target_probe_count as i64,
                    probe_cluster_min_bytes: minimum as i64,
                    probe_cluster_bytes_sent: *sent as i64,
                };
                match owner
                    .emit(
                        &bound.writer,
                        &packet,
                        &format_attributes(epoch),
                        info,
                        None,
                    )
                    .await
                {
                    Ok(0) => break,
                    Ok(size) => {
                        bytes += size + self.network_overhead();
                        *sent += size + self.network_overhead();
                        if !legacy {
                            *probes += 1;
                        }
                    }
                    Err(_) => break,
                }
            }
            if legacy {
                *probes += 1;
            }
            // An unavailable payload/retired writer cannot advance the byte
            // clock. Retire this attempt instead of spinning on a past deadline.
            // Done once the cluster's packets and bytes are out (what the
            // probe estimator needs).
            if bytes == 0 || (*probes >= cluster.target_probe_count as i64 && *sent >= minimum) {
                active = None;
            }
        }
    }
    fn remember(&self, packet: &Packet) {
        if !self.active() {
            return;
        }
        let mut history = lock(&self.0.history);
        history.next_serial = history.next_serial.wrapping_add(1);
        let serial = history.next_serial;
        let seq = packet.header.sequence_number;
        if let Some(old) = history.packets.remove(&seq) {
            history.bytes -= old.packet.marshal_size();
        }
        history.bytes += packet.marshal_size();
        let keyframe = history
            .frame
            .as_ref()
            .is_some_and(|f| f.timestamp == packet.header.timestamp && f.keyframe);
        let captured = history
            .frame
            .as_ref()
            .filter(|f| f.timestamp == packet.header.timestamp)
            .and_then(|f| f.captured);
        history.packets.insert(
            seq,
            Stored {
                epoch: self.0.format_epoch.load(Ordering::Acquire),
                packet: packet.clone(),
                serial,
                repair_at: None,
                keyframe,
                repairs: 0,
                sent_at: Instant::now(),
                captured,
            },
        );
        history.order.push_back((seq, serial));
        self.prune_history(&mut history);
    }
    fn prune_history(&self, history: &mut History) {
        let minimum = Duration::from_micros(
            self.0
                .rtt_us
                .load(Ordering::Relaxed)
                .saturating_mul(3)
                .max(1_000_000),
        );
        while let Some(&(seq, serial)) = history.order.front() {
            let Some(packet) = history.packets.get(&seq).filter(|p| p.serial == serial) else {
                history.order.pop_front();
                continue;
            };
            let age = packet.repair_at.unwrap_or(packet.sent_at).elapsed();
            let hard = history.packets.len() >= 38400 || history.bytes > 64 * 1024 * 1024;
            if !hard
                && history
                    .primary
                    .is_some_and(|ssrc| lock(&self.0.repairs).1.contains(&(ssrc, seq)))
            {
                break;
            }
            let expired = age >= minimum
                && (history.packets.len() >= 24000 || age >= minimum.saturating_mul(3));
            if !hard && !expired {
                break;
            }
            history.order.pop_front();
            if let Some(packet) = history.packets.remove(&seq) {
                history.bytes -= packet.packet.marshal_size();
            }
        }
    }
    pub(crate) fn nack(&self, nack: &TransportLayerNack) {
        if !self.active() || lock(&self.0.history).primary != Some(nack.media_ssrc) {
            return;
        }
        lock(&self.0.budget).nack();
        let mut queue = lock(&self.0.repairs);
        for pair in &nack.nacks {
            for seq in pair.into_iter() {
                if queue.1.len() >= 38400 {
                    break;
                }
                if queue.1.insert((nack.media_ssrc, seq)) {
                    queue.0.push_back((nack.media_ssrc, seq));
                }
            }
        }
        self.0.wake.notify_one();
    }
    /// Whether a repair was refused since the last call.
    pub(crate) fn take_repair_refused(&self) -> bool {
        self.0.repair_refused.swap(false, Ordering::AcqRel)
    }
    fn refuse_repair(&self) {
        self.0.repair_refused.store(true, Ordering::Release);
    }
    pub(crate) fn set_generic_dependencies(&self, generic: bool) {
        self.0
            .generic_dependencies
            .store(generic, Ordering::Relaxed);
    }
    pub(crate) async fn repairs(&self) {
        let mut window = VecDeque::<(Instant, usize)>::new();
        let mut window_bytes = 0;
        loop {
            let wake = self.0.wake.notified();
            tokio::pin!(wake);
            wake.as_mut().enable();
            if !self.active() {
                return;
            }
            let pending = lock(&self.0.repairs).0.pop_front();
            let Some((ssrc, seq)) = pending else {
                tokio::select! {_=self.0.cancel.cancelled()=>return,_=&mut wake=>{}}
                continue;
            };
            let result = self.repair(ssrc, seq, &mut window, &mut window_bytes).await;
            lock(&self.0.repairs).1.remove(&(ssrc, seq));
            if let Err(error) = result {
                if self.active() {
                    tracing::debug!(%error,"host RTX write failed");
                }
            }
        }
    }
    /// A refused repair for a viewer that decodes by declared dependencies:
    /// an RTX packet with only the original sequence number, which libwebrtc
    /// takes as padding for `seq` (no picture data).
    async fn give_up(&self, seq: u16) -> Result<()> {
        if !self.0.generic_dependencies.load(Ordering::Relaxed) {
            return Ok(());
        }
        let next = {
            let mut history = lock(&self.0.history);
            let Some(bound) = history.rtx.clone() else {
                return Ok(());
            };
            // Any recent packet supplies the header (payload type, timestamp
            // and the transport extensions); the payload is only the OSN.
            let Some(template) = history
                .order
                .back()
                .and_then(|(newest, _)| history.packets.get(newest))
                .map(|stored| (stored.packet.clone(), stored.epoch))
            else {
                return Ok(());
            };
            let Some(payload_type) = lock(&self.0.rtx_types)
                .get(&template.0.header.payload_type)
                .copied()
            else {
                return Ok(());
            };
            let mut packet = template.0;
            packet.payload = Bytes::copy_from_slice(&seq.to_be_bytes());
            packet.header.ssrc = bound.info.ssrc;
            packet.header.payload_type = payload_type;
            packet.header.marker = false;
            packet.header.padding = false;
            packet.header.sequence_number = history.rtx_sequence;
            history.rtx_sequence = history.rtx_sequence.wrapping_add(1);
            (bound, packet, template.1)
        };
        tracing::debug!(target: "farwing::recovery", seq, "repair refused; telling the viewer to stop waiting");
        self.write(&next.0.writer, &next.1, &format_attributes(next.2))
            .await
            .map(|_| ())
    }
    async fn repair(
        &self,
        ssrc: u32,
        seq: u16,
        window: &mut VecDeque<(Instant, usize)>,
        window_bytes: &mut usize,
    ) -> Result<()> {
        if !self.active() {
            return Ok(());
        }
        // Refused: `None` (the history lock is released before writing).
        let next = 'decide: {
            let mut history = lock(&self.0.history);
            self.prune_history(&mut history);
            if history.primary != Some(ssrc) {
                return Ok(());
            }
            let Some(bound) = history.rtx.clone() else {
                return Ok(());
            };
            let Some(stored) = history.packets.get_mut(&seq) else {
                self.refuse_repair();
                break 'decide None;
            };
            let rtt = self.0.rtt_us.load(Ordering::Relaxed);
            if stored
                .repair_at
                .is_some_and(|at| rtt > 0 && at.elapsed() < Duration::from_micros(rtt))
            {
                return Ok(());
            }
            while window
                .front()
                .is_some_and(|(at, _)| at.elapsed() >= Duration::from_secs(1))
            {
                if let Some((_, bytes)) = window.pop_front() {
                    *window_bytes -= bytes;
                }
            }
            let bytes = stored.packet.marshal_size() + 2 + self.overhead();
            let Some(payload_type) = lock(&self.0.rtx_types)
                .get(&stored.packet.header.payload_type)
                .copied()
            else {
                return Ok(());
            };
            // RetransmissionRateLimit: ratio .7, 1000ms window.
            // A rate or budget refusal is temporary: a later NACK may still
            // be answered, so the viewer is not told to stop waiting.
            if !stored.keyframe
                && stored.repairs > 1
                && (*window_bytes + bytes) as u64 > self.0.repair_rate.load(Ordering::Acquire) / 8
            {
                self.refuse_repair();
                return Ok(());
            }
            let Some(reservation) = lock(&self.0.budget).reserve_rtx(bytes, stored.repairs) else {
                self.refuse_repair();
                return Ok(());
            };
            let serial = stored.serial;
            let epoch = stored.epoch;
            let mut packet = stored.packet.clone();
            let mut payload = Vec::with_capacity(packet.payload.len() + 2);
            payload.extend_from_slice(&seq.to_be_bytes());
            payload.extend_from_slice(&packet.payload);
            packet.payload = Bytes::from(payload);
            packet.header.ssrc = bound.info.ssrc;
            packet.header.payload_type = payload_type;
            packet.header.sequence_number = history.rtx_sequence;
            history.rtx_sequence = history.rtx_sequence.wrapping_add(1);
            Some((bound, packet, reservation, serial, epoch))
        };
        let Some(next) = next else {
            return self.give_up(seq).await;
        };
        let mut reservation = Reserved {
            transport: self.clone(),
            id: next.2,
            actual: 0,
        };
        let bytes = self
            .write(&next.0.writer, &next.1, &format_attributes(next.4))
            .await?;
        if bytes == 0 {
            return Ok(());
        }
        reservation.actual = bytes + self.network_overhead();
        if let Some(stored) = lock(&self.0.history)
            .packets
            .get_mut(&seq)
            .filter(|p| p.serial == next.3)
        {
            stored.repair_at = Some(Instant::now());
            stored.repairs = stored.repairs.saturating_add(1);
        }
        window.push_back((Instant::now(), reservation.actual));
        *window_bytes += reservation.actual;
        Ok(())
    }
}
struct Ticket {
    transport: Transport,
    key: (u8, u64),
}
impl Ticket {
    fn first(&self) -> bool {
        lock(&self.transport.0.waiting)
            .first_key_value()
            .is_some_and(|(key, _)| *key == self.key)
    }
}
impl Drop for Ticket {
    fn drop(&mut self) {
        if let Some((_, extra)) = lock(&self.transport.0.waiting).remove(&self.key) {
            self.transport.0.queued.fetch_sub(extra, Ordering::Relaxed);
        }
        self.transport.0.feedback_wake.notify_waiters();
    }
}
pub(crate) struct QueuedFrame {
    transport: Transport,
    bytes: u64,
    remaining: u64,
}
impl QueuedFrame {
    pub(crate) fn sent(&mut self, bytes: usize) {
        let bytes = (bytes as u64).min(self.remaining);
        self.remaining -= bytes;
        self.transport.0.queued.fetch_sub(bytes, Ordering::Relaxed);
    }
}
impl Drop for QueuedFrame {
    fn drop(&mut self) {
        *lock(&self.transport.0.queued_at) = None;
        let mut probe_after = lock(&self.transport.0.probe_after);
        if probe_after.is_none() && self.remaining < self.bytes {
            let rate = u64::from(lock(&self.transport.0.controller).target.max(100_000));
            *probe_after = Some(
                Instant::now() + Duration::from_micros(self.bytes * 8_000_000 / rate),
            );
        }
        drop(probe_after);
        self.transport
            .0
            .queued
            .fetch_sub(self.remaining, Ordering::Relaxed);
    }
}
struct Reserved {
    transport: Transport,
    id: u64,
    actual: usize,
}
impl Drop for Reserved {
    fn drop(&mut self) {
        lock(&self.transport.0.budget).finish(self.id, self.actual);
    }
}
impl InterceptorBuilder for Transport {
    fn build(&self, _: &str) -> Result<Arc<dyn Interceptor + Send + Sync>> {
        Ok(Arc::new(self.clone()))
    }
}
struct MediaWriter {
    parent: Writer,
    transport: Transport,
}
struct FeedbackReader {
    parent: Arc<dyn RTCPReader + Send + Sync>,
    transport: Transport,
}
/// `FARWING_HOST_LINK`: the host's RTCP through the emulated link.
struct LinkControlWriter {
    parent: Arc<dyn RTCPWriter + Send + Sync>,
    link: Arc<super::link::Link>,
}
#[async_trait]
impl RTCPWriter for LinkControlWriter {
    async fn write(
        &self,
        packets: &[Box<dyn webrtc::rtcp::packet::Packet + Send + Sync>],
        attributes: &Attributes,
    ) -> Result<usize> {
        Ok(self.link.send_control(&self.parent, packets, attributes))
    }
}
#[async_trait]
impl RTCPReader for FeedbackReader {
    async fn read(
        &self,
        buf: &mut [u8],
        attributes: &Attributes,
    ) -> Result<(
        Vec<Box<dyn webrtc::rtcp::packet::Packet + Send + Sync>>,
        Attributes,
    )> {
        let (packets, attributes) = self.parent.read(buf, attributes).await?;
        for packet in &packets {
            if let Some(feedback) = packet.as_any().downcast_ref::<TransportLayerCc>() {
                self.transport.feedback(feedback);
            }
        }
        Ok((packets, attributes))
    }
}
#[async_trait]
impl RTPWriter for MediaWriter {
    async fn write(&self, packet: &Packet, attributes: &Attributes) -> Result<usize> {
        let size = self
            .transport
            .write(&self.parent, packet, attributes)
            .await?;
        Ok(size)
    }
}
#[async_trait]
impl Interceptor for Transport {
    async fn bind_rtcp_reader(
        &self,
        reader: Arc<dyn RTCPReader + Send + Sync>,
    ) -> Arc<dyn RTCPReader + Send + Sync> {
        Arc::new(FeedbackReader {
            parent: reader,
            transport: self.clone(),
        })
    }
    async fn bind_rtcp_writer(
        &self,
        writer: Arc<dyn RTCPWriter + Send + Sync>,
    ) -> Arc<dyn RTCPWriter + Send + Sync> {
        match &self.0.link {
            Some(link) => Arc::new(LinkControlWriter {
                parent: writer,
                link: link.clone(),
            }),
            None => writer,
        }
    }
    async fn bind_local_stream(&self, info: &StreamInfo, writer: Writer) -> Writer {
        if info.mime_type.eq_ignore_ascii_case("audio/opus") {
            let tcc = info.rtp_header_extensions.iter().find(|e|e.uri == "http://www.ietf.org/id/draft-holmer-rmcat-transport-wide-cc-extensions-01")
                .map_or(0,|e|if (1..=255).contains(&e.id) { e.id as u8 } else { 0 });
            return Arc::new(AudioWriter {
                parent: writer,
                transport: self.clone(),
                tcc,
            });
        }
        if let Some(stream) = self.for_stream(info) {
            return stream.bind_stream(info, writer).await;
        }
        writer
    }
    async fn unbind_local_stream(&self, info: &StreamInfo) {
        if let Some(stream) = self.for_stream(info) {
            stream.unbind_stream(info);
        }
    }
    async fn bind_remote_stream(
        &self,
        _: &StreamInfo,
        reader: Arc<dyn RTPReader + Send + Sync>,
    ) -> Arc<dyn RTPReader + Send + Sync> {
        reader
    }
    async fn unbind_remote_stream(&self, _: &StreamInfo) {}
    async fn close(&self) -> Result<()> {
        self.0.closed.store(true, Ordering::Release);
        self.0.cancel.cancel();
        for stream in self.streams() {
            stream.clear_stream();
        }
        Ok(())
    }
}
impl Transport {
    async fn bind_stream(&self, info: &StreamInfo, writer: Writer) -> Writer {
        for ext in &info.rtp_header_extensions {
            let index = match ext.uri.as_str() {
                "http://www.webrtc.org/experiments/rtp-hdrext/abs-send-time" => 0,
                "http://www.webrtc.org/experiments/rtp-hdrext/video-timing" => 1,
                "urn:farwing:rtp-hdrext:sending-delay" => 2,
                "urn:ietf:params:rtp-hdrext:toffset" => 3,
                _ => continue,
            };
            if (1..=255).contains(&ext.id) {
                lock(&self.0.timing_ids)[index] = ext.id as u8;
            }
        }
        if let Some(extension) = info.rtp_header_extensions.iter().find(|e| {
            e.uri == "http://www.ietf.org/id/draft-holmer-rmcat-transport-wide-cc-extensions-01"
        }) {
            if (1..=255).contains(&extension.id) {
                self.0.tcc_id.store(extension.id as u64, Ordering::Relaxed);
            }
        }
        if info
            .mime_type
            .eq_ignore_ascii_case(crate::transport::rsfec::MIME_TYPE)
            && info.associated_stream.is_some()
        {
            lock(&self.0.history).fec = Some(Bound {
                info: info.clone(),
                writer: writer.clone(),
            });
            *lock(&self.0.fec) = Some(super::fec::Sender::new(info));
            return writer;
        }
        if info.mime_type.eq_ignore_ascii_case("video/rtx") {
            if info.associated_stream.is_some() {
                lock(&self.0.history).rtx = Some(Bound {
                    info: info.clone(),
                    writer: writer.clone(),
                });
            }
            return writer;
        }
        if super::format::Codec::from_mime(&info.mime_type).is_some() {
            lock(&self.0.history).primary = Some(info.ssrc);
            return Arc::new(MediaWriter {
                parent: writer,
                transport: self.clone(),
            });
        }
        writer
    }
    fn unbind_stream(&self, info: &StreamInfo) {
        let mut history = lock(&self.0.history);
        if history.primary == Some(info.ssrc) {
            history.primary = None;
            history.packets.clear();
            history.order.clear();
            history.bytes = 0;
        }
        if history
            .rtx
            .as_ref()
            .is_some_and(|bound| bound.info.ssrc == info.ssrc)
        {
            history.rtx = None;
        }
        if history
            .fec
            .as_ref()
            .is_some_and(|bound| bound.info.ssrc == info.ssrc)
        {
            history.fec = None;
            drop(history);
            *lock(&self.0.fec) = None;
            lock(&self.0.fec_blocks).clear();
        }
    }
    fn clear_stream(&self) {
        lock(&self.0.budget).reset(false);
        lock(&self.0.fec_blocks).clear();
        *lock(&self.0.fec) = None;
        let mut history = lock(&self.0.history);
        history.rtx = None;
        history.fec = None;
        history.primary = None;
        history.packets.clear();
        history.order.clear();
        history.bytes = 0;
    }
}

// Audio shares the transport estimate/TWCC sequence space, but bypasses the video
// frame pacer and RTX/FEC history. Its fixed allocation is reserved above.
struct AudioWriter {
    parent: Writer,
    transport: Transport,
    tcc: u8,
}
#[async_trait]
impl RTPWriter for AudioWriter {
    async fn write(&self, packet: &Packet, attributes: &Attributes) -> Result<usize> {
        let shared = &self.transport.0;
        if shared.cancel.is_cancelled()
            || !(shared.requested)()
            || !shared.network.load(Ordering::Acquire)
        {
            return Ok(0);
        }
        let _egress = tokio::select! { _=shared.cancel.cancelled()=>return Err(Error::ErrIoEOF), guard=shared.egress.lock()=>guard };
        let mut packet = packet.clone();
        let seq = if self.tcc != 0 {
            let seq = lock(&shared.controller).next_sequence();
            packet.header.set_extension(
                self.tcc,
                Bytes::copy_from_slice(&(seq as u16).to_be_bytes()),
            )?;
            Some(seq)
        } else {
            None
        };
        // Audio shares the transport-wide sequence: it takes the same
        // emulated link, or it would overtake delayed video and look like loss.
        let size = if shared.direct_loss() {
            packet.marshal_size()
        } else {
            match shared.link.as_ref() {
                Some(link) => link.send(&self.parent, &packet, attributes, self.transport.overhead()),
                None => {
                    tokio::select! { _=shared.cancel.cancelled()=>return Err(Error::ErrIoEOF), result=self.parent.write(&packet,attributes)=>result? }
                }
            }
        };
        if size > 0 {
            if let Some(seq) = seq {
                lock(&shared.controller).sent(
                    seq,
                    self.transport.wire_size(size),
                    PacedPacketInfo::default(),
                    None,
                    true,
                );
            }
        }
        Ok(size)
    }
}
