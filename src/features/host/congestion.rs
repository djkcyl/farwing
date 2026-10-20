//! Sender transport feedback -> GoogCC. Times are taken at actual packet egress.
use goog_cc::{
    GoogCcConfig, GoogCcNetworkController,
    experiments::FieldTrials,
    network_control::{NetworkControllerConfig, NetworkControllerInterface},
    transport::*,
    units::{DataRate, DataSize, TimeDelta, Timestamp},
};
use std::{
    collections::{BTreeMap, VecDeque},
    time::Instant,
};
use webrtc::rtcp::transport_feedbacks::transport_layer_cc::{
    PacketStatusChunk, SymbolTypeTcc, TransportLayerCc,
};

pub(crate) struct Controller {
    core: GoogCcNetworkController,
    origin: Instant,
    maximum: u32,
    bounds: super::parameters::Bounds,
    video_configured: bool,
    sequence: i64,
    history: BTreeMap<i64, Tracked>,
    flight: usize,
    reference: Option<i64>,
    remote_offset: Option<i64>,
    available: bool,
    generation: u64,
    route_at: Timestamp,
    /// Probes sent because the first ones measured nothing, and when the
    /// next may go.
    rescues: i32,
    rescue_at: Timestamp,
    pub target: u32,
    pub pacing: u32,
    pub loss: f64,
    pub rtt: TimeDelta,
    pub cwnd: usize,
    pub cwnd_reduce_ratio: f64,
    pub probes: VecDeque<ProbeClusterConfig>,
    pressure: Pressure,
    burst: super::burst::Burst,
    pub feedback_count: u64,
}
struct Tracked {
    packet: SentPacket,
    reported: Option<bool>,
    burst: Option<u64>,
}

impl Controller {
    pub fn new(bounds: super::parameters::Bounds) -> Self {
        let maximum = bounds.network_maximum();
        let mut field_trials = FieldTrials::default();
        // The robust throughput estimator stays off.
        field_trials.robust_throughput_estimator_settings.enabled = false;
        field_trials.rapid_recovery_experiment = true;
        field_trials.no_bitrate_increase_in_alr = false;
        field_trials.add_pacing_to_congestion_window_pushback = true;
        field_trials.alr_experiment_settings.pacing_factor = 1.6;
        field_trials.alr_experiment_settings.max_paced_queue_time = 6;
        field_trials
            .alr_experiment_settings
            .alr_bandwidth_usage_percent = 85;
        field_trials
            .alr_experiment_settings
            .alr_start_budget_level_percent = 95;
        field_trials
            .alr_experiment_settings
            .alr_stop_budget_level_percent = -60;
        field_trials.alr_experiment_settings.group_id = 3;
        field_trials.probing_configuration.probe_max_allocation = true;
        field_trials.min_alloc_as_lower_bound = false;
        field_trials.ignore_probes_lower_than_network_state_estimate = false;
        field_trials.limit_probes_lower_than_throughput_estimate = false;
        field_trials.safe_reset_on_route_change.enabled = false;
        field_trials.loss_based_control.enabled = false;
        field_trials.loss_based_bwe_v2.enabled = false;
        let at = Timestamp::from_micros(1_000_000);
        let core = GoogCcNetworkController::new(
            NetworkControllerConfig {
                field_trials,
                constraints: constraints(at, maximum, Some(bounds.initial)),
                stream_based_config: StreamsConfig {
                    at_time: at,
                    pacing_factor: Some(1.6),
                    requests_alr_probing: Some(true),
                    max_total_allocated_bitrate: Some(DataRate::from_bits_per_sec(maximum.into())),
                    min_total_allocated_bitrate: Some(DataRate::from_bits_per_sec(
                        bounds.reservation.into(),
                    )),
                    ..Default::default()
                },
            },
            GoogCcConfig {
                feedback_only: true,
            },
        );
        Self {
            core,
            origin: Instant::now(),
            maximum,
            bounds,
            video_configured: bounds.maximum > 0,
            sequence: rand::random::<u16>().into(),
            history: BTreeMap::new(),
            flight: 0,
            reference: None,
            remote_offset: None,
            available: false,
            generation: 0,
            route_at: at,
            rescues: 0,
            rescue_at: at,
            target: bounds.initial,
            pacing: bounds.initial.saturating_mul(16) / 10,
            loss: 0.0,
            rtt: TimeDelta::from_millis(50),
            cwnd: usize::MAX,
            cwnd_reduce_ratio: 0.,
            probes: VecDeque::new(),
            pressure: Pressure::default(),
            burst: super::burst::Burst::default(),
            feedback_count: 0,
        }
    }
    fn now(&self) -> Timestamp {
        Timestamp::from_micros(
            1_000_000
                + self
                    .origin
                    .elapsed()
                    .as_micros()
                    .min(i64::MAX as u128 - 1_000_000) as i64,
        )
    }
    fn apply(&mut self, update: NetworkControlUpdate) {
        if let Some(rate) = update.target_rate {
            let previous = self.target;
            self.target = rate.target_rate.bps_or(0).clamp(0, self.maximum as i64) as u32;
            self.cwnd_reduce_ratio = rate.cwnd_reduce_ratio.clamp(0., 1.);
            self.loss = f64::from(rate.network_estimate.loss_rate_ratio).clamp(0.0, 1.0);
            if rate.network_estimate.round_trip_time.is_finite() {
                self.rtt = rate.network_estimate.round_trip_time;
            }
            if previous >= 600_000 && self.target < previous / 2 {
                tracing::info!(
                    previous_bps = previous,
                    target_bps = self.target,
                    bounded_target_bps = self.core.bounded_target_rate().bps_or(0),
                    loss = self.loss,
                    rtt_ms = self.rtt.ms(),
                    delay_overuse = self.delay_overuse(),
                    link_pressure = self.link_pressure(),
                    cwnd_reduce_ratio = self.cwnd_reduce_ratio,
                    "host bandwidth estimate dropped"
                );
            }
        }
        if let Some(pacer) = update.pacer_config {
            self.pacing = pacer
                .data_rate()
                .bps_or(self.maximum as i64)
                .clamp(1, u32::MAX as i64) as u32;
        }
        if let Some(cwnd) = update.congestion_window {
            self.cwnd = cwnd.bytes_or(i64::MAX).max(0) as usize;
        }
        self.probes.extend(update.probe_cluster_configs);
    }
    pub fn network(&mut self, available: bool) {
        if self.available == available {
            return;
        }
        self.available = available;
        self.generation = self.generation.wrapping_add(1);
        let update = self.core.on_network_availability(NetworkAvailability {
            at_time: self.now(),
            network_available: available,
        });
        self.apply(update);
        if !available {
            self.probes.clear();
        }
    }
    pub fn route_changed(&mut self) {
        self.generation = self.generation.wrapping_add(1);
        self.route_at = self.now();
        self.rescues = 0;
        let update = self.core.on_network_route_change(NetworkRouteChange {
            at_time: self.now(),
            constraints: constraints(self.now(), self.maximum, Some(self.bounds.initial)),
        });
        self.history.clear();
        self.flight = 0;
        self.reference = None;
        self.remote_offset = None;
        self.pressure = Pressure::default();
        self.burst = super::burst::Burst::default();
        self.probes.clear();
        self.apply(update);
    }
    pub fn configure(&mut self, bounds: super::parameters::Bounds, video: bool) {
        // File/audio connections may attach their first video later. Seed that
        // first video once, but never treat a quality/FPS revision as a route
        // change: a starting_rate overwrites both live bandwidth estimators.
        let start = (video && !self.video_configured).then_some(bounds.initial);
        self.video_configured |= video;
        if bounds == self.bounds && start.is_none() {
            return;
        }
        let maximum = bounds.network_maximum();
        self.maximum = maximum;
        self.bounds = bounds;
        let update = self
            .core
            .on_target_rate_constraints(constraints(self.now(), maximum, start));
        self.apply(update);
        let update = self.core.on_streams_config(StreamsConfig {
            at_time: self.now(),
            max_total_allocated_bitrate: Some(DataRate::from_bits_per_sec(maximum.into())),
            min_total_allocated_bitrate: Some(DataRate::from_bits_per_sec(
                bounds.reservation.into(),
            )),
            ..Default::default()
        });
        self.apply(update);
    }
    pub fn estimates(&self) -> (u32, u32, f64, u64) {
        (
            self.target,
            self.core
                .bounded_target_rate()
                .bps_or(0)
                .clamp(0, self.maximum as i64) as u32,
            self.loss,
            self.generation,
        )
    }
    pub fn next_sequence(&mut self) -> i64 {
        let seq = self.sequence;
        self.sequence += 1;
        seq
    }
    pub fn sent(
        &mut self,
        sequence: i64,
        bytes: usize,
        pacing: PacedPacketInfo,
        burst: Option<u64>,
        audio: bool,
    ) {
        let now = self.now();
        if let Some(group) = burst {
            self.burst.sent(group, sequence, bytes, now.us());
        }
        let sent = SentPacket {
            audio,
            send_time: now,
            size: DataSize::from_bytes(bytes as i64),
            pacing_info: pacing,
            sequence_number: sequence,
            data_in_flight: DataSize::from_bytes(self.flight as i64),
            ..Default::default()
        };
        self.flight = self.flight.saturating_add(bytes);
        self.history.insert(
            sequence,
            Tracked {
                packet: sent,
                reported: None,
                burst,
            },
        );
        while self.history.first_key_value().is_some_and(|(_, p)| {
            self.history.len() > 32768 || (now - p.packet.send_time) > TimeDelta::from_seconds(10)
        }) {
            if let Some((_, old)) = self.history.pop_first() {
                if old.reported.is_none() {
                    self.flight = self.flight.saturating_sub(old.packet.size.bytes() as usize);
                }
            }
        }
        let update = self.core.on_sent_packet(sent);
        self.apply(update);
    }
    pub fn tick(&mut self, queued_bytes: usize) {
        self.burst.tick(self.now().us(), self.rtt.us().max(0));
        let update = self.core.on_process_interval(ProcessInterval {
            at_time: self.now(),
            pacer_queue: Some(DataSize::from_bytes(queued_bytes as i64)),
        });
        self.apply(update);
        self.rescue_probe();
    }
    /// The first probes of a path can measure nothing when a few of their
    /// packets are lost: GoogCC then stops probing, and a sender that uses
    /// its whole estimate never probes again (that needs spare capacity), so
    /// the estimate creeps up additively from about the start rate for tens
    /// of seconds. Shortly after such a start, probe again a few times at
    /// twice the estimate, with longer clusters that survive a lost packet.
    fn rescue_probe(&mut self) {
        let now = self.now();
        let age = now - self.route_at;
        if !self.available
            || self.rescues >= RESCUE_PROBES
            || age < TimeDelta::from_millis(1500)
            || age > TimeDelta::from_seconds(10)
            || now < self.rescue_at
            || !self.probes.is_empty()
            || u64::from(self.target) >= 2 * u64::from(self.bounds.initial)
            || u64::from(self.target) * 2 > u64::from(self.maximum)
            || self.delay_overuse()
        {
            return;
        }
        self.rescues += 1;
        self.rescue_at = now + TimeDelta::from_millis(1500);
        let rate = (u64::from(self.target) * 2).min(u64::from(self.maximum)) as i64;
        tracing::debug!(rate_bps = rate, attempt = self.rescues, "host probing again after a start without results");
        self.probes.push_back(ProbeClusterConfig {
            at_time: now,
            target_data_rate: DataRate::from_bits_per_sec(rate),
            target_duration: TimeDelta::from_millis(30),
            min_probe_delta: TimeDelta::from_millis(2),
            target_probe_count: 10,
            id: RESCUE_PROBE_ID + self.rescues,
        });
    }
    pub fn burst_bytes(&self) -> usize {
        self.burst.bytes
    }
    pub fn delay_overuse(&self) -> bool {
        self.core.delay_state() == BandwidthUsage::Overusing
    }
    pub fn probe_is_current(&self, cluster: &ProbeClusterConfig) -> bool {
        self.available
            && cluster.at_time >= self.route_at
            && self.now() - cluster.at_time <= TimeDelta::from_seconds(5)
    }
    pub fn link_pressure(&self) -> bool {
        self.pressure.state == 2
    }
    pub fn feedback(&mut self, feedback: &TransportLayerCc) {
        if feedback.packet_status_count == 0 {
            return;
        }
        let now = self.now();
        let mut statuses = Vec::with_capacity(feedback.packet_status_count as usize);
        for chunk in &feedback.packet_chunks {
            let remaining = feedback.packet_status_count as usize - statuses.len();
            match chunk {
                PacketStatusChunk::RunLengthChunk(c) => statuses.extend(std::iter::repeat_n(
                    c.packet_status_symbol,
                    remaining.min(c.run_length as usize),
                )),
                PacketStatusChunk::StatusVectorChunk(c) => {
                    statuses.extend(c.symbol_list.iter().copied().take(remaining))
                }
            }
            if statuses.len() == feedback.packet_status_count as usize {
                break;
            }
        }
        if statuses.len() != feedback.packet_status_count as usize {
            return;
        }
        let delta_count = statuses
            .iter()
            .filter(|s| {
                matches!(
                    s,
                    SymbolTypeTcc::PacketReceivedSmallDelta
                        | SymbolTypeTcc::PacketReceivedLargeDelta
                )
            })
            .count();
        if delta_count != feedback.recv_deltas.len() {
            return;
        }
        let reference = unwrap(
            feedback.reference_time as i64,
            24,
            self.reference.unwrap_or(feedback.reference_time as i64),
        );
        self.reference = Some(reference);
        let delta_sum: i64 = feedback.recv_deltas.iter().map(|d| d.delta).sum();
        let offset = *self
            .remote_offset
            .get_or_insert(now.us() - reference * 64_000 - delta_sum);
        let mut received_at = reference * 64_000 + offset;
        let base = unwrap(feedback.base_sequence_number.into(), 16, self.sequence - 1);
        let mut deltas = feedback.recv_deltas.iter();
        let mut packets = Vec::new();
        for (index, status) in statuses.into_iter().enumerate() {
            let received = status != SymbolTypeTcc::PacketNotReceived;
            if matches!(
                status,
                SymbolTypeTcc::PacketReceivedSmallDelta | SymbolTypeTcc::PacketReceivedLargeDelta
            ) {
                received_at += deltas.next().expect("validated TWCC delta count").delta;
            }
            let Some(stored) = self.history.get_mut(&(base + index as i64)) else {
                continue;
            };
            if stored.reported == Some(true) || stored.reported == Some(received) {
                continue;
            }
            if stored.reported.is_none() {
                self.flight = self
                    .flight
                    .saturating_sub(stored.packet.size.bytes() as usize);
                if !received {
                    tracing::debug!(target: "farwing::nack_audit", transport_seq = base + index as i64,
                        bytes = stored.packet.size.bytes(),
                        probe = stored.packet.pacing_info.probe_cluster_id != PacedPacketInfo::NOT_APROBE,
                        "viewer reports a packet not received");
                }
            }
            stored.reported = Some(received);
            if let Some(group) = stored.burst {
                self.burst
                    .feedback(group, stored.packet.sequence_number, received, now.us());
            }
            if status == SymbolTypeTcc::PacketReceivedWithoutDelta {
                continue;
            }
            packets.push(PacketResult {
                sent_packet: stored.packet,
                receive_time: if received {
                    Timestamp::from_micros(received_at)
                } else {
                    Timestamp::plus_infinity()
                },
                ..Default::default()
            });
        }
        if packets.is_empty() {
            return;
        }
        self.feedback_count = self.feedback_count.saturating_add(1);
        packets.sort_by_key(|p| p.sent_packet.send_time);
        self.pressure.update(&packets);
        self.core
            .set_random_loss_no_backoff(self.pressure.random_loss);
        let update = self
            .core
            .on_transport_packets_feedback(TransportPacketsFeedback {
                feedback_time: now,
                data_in_flight: DataSize::from_bytes(self.flight as i64),
                packet_feedbacks: packets,
                sendless_arrival_times: Vec::new(),
            });
        self.apply(update);
    }
}
/// At most this many probes after a start without probe results.
const RESCUE_PROBES: i32 = 3;
/// Cluster ids of those probes, apart from GoogCC's own (counted from 1).
const RESCUE_PROBE_ID: i32 = 1 << 30;

fn constraints(at_time: Timestamp, maximum: u32, start: Option<u32>) -> TargetRateConstraints {
    TargetRateConstraints {
        at_time,
        // Field trial PcFactoryDefaultBitrates/min:300 (kbps).
        min_data_rate: Some(DataRate::from_bits_per_sec(
            super::parameters::NETWORK_FLOOR.min(maximum).into(),
        )),
        max_data_rate: Some(DataRate::from_bits_per_sec(maximum.into())),
        starting_rate: start.map(|v| DataRate::from_bits_per_sec(v.into())),
    }
}
fn unwrap(value: i64, bits: u32, reference: i64) -> i64 {
    let modulus = 1i64 << bits;
    let mut extended = (reference & !(modulus - 1)) | value;
    if extended - reference > modulus / 2 {
        extended -= modulus;
    }
    if reference - extended > modulus / 2 {
        extended += modulus;
    }
    extended
}

#[derive(Default)]
struct Pressure {
    end: Option<i64>,
    groups: BTreeMap<i64, BTreeMap<i64, bool>>,
    state: u8,
    candidate: Option<i64>,
    confirmations: u32,
    last_pressure: Option<i64>,
    clear_at: Option<i64>,
    clear_count: u32,
    random_loss: bool,
}
impl Pressure {
    fn update(&mut self, packets: &[PacketResult]) {
        let previous = self.end;
        for packet in packets.iter().filter(|p| !p.sent_packet.audio) {
            let group = packet.sent_packet.send_time.us() / 100_000;
            if self.end.is_some_and(|end| group - end >= 100) {
                *self = Self::default();
            }
            self.end = Some(self.end.map_or(group, |end| end.max(group)));
            if self.end.is_some_and(|end| group < end - 20) {
                continue;
            }
            let received = self
                .groups
                .entry(group)
                .or_default()
                .entry(packet.sent_packet.sequence_number)
                .or_insert(false);
            *received |= packet.is_received();
        }
        let Some(end) = self.end else { return };
        while self
            .groups
            .first_key_value()
            .is_some_and(|(&group, _)| group < end - 20)
        {
            self.groups.pop_first();
        }
        if previous == Some(end) {
            return;
        }
        let now = end * 100_000;
        let valid: Vec<(u32, u32)> = self
            .groups
            .range(end - 20..end)
            .filter(|(_, packets)| packets.len() >= 10)
            .map(|(_, packets)| {
                (
                    packets.len() as u32,
                    packets.values().filter(|&&received| !received).count() as u32,
                )
            })
            .collect();
        let total: u32 = valid.iter().map(|(s, _)| *s).sum();
        let mut correlation = None;
        let mut loss = 0.0;
        if valid.len() >= 14 && total >= 200 {
            let n = valid.len() as f64;
            let sent_mean = total as f64 / n;
            loss = valid
                .iter()
                .map(|(s, l)| *l as f64 / *s as f64)
                .sum::<f64>()
                / n;
            let (mut xx, mut yy, mut xy) = (0.0, 0.0, 0.0);
            for (sent, lost) in &valid {
                let x = *sent as f64 - sent_mean;
                let y = *lost as f64 / *sent as f64 - loss;
                xx += x * x;
                yy += y * y;
                xy += x * y;
            }
            if xx / n / (sent_mean * sent_mean) >= 0.01
                && yy / n >= 0.000001
                && xx > 0.0
                && yy > 0.0
            {
                correlation = Some(xy / (xx * yy).sqrt());
            }
        }
        let low_loss = valid.len() >= 20 && loss < 0.03;
        let over = !low_loss && correlation.is_some_and(|r| r > 0.6);
        match self.state {
            0 if over => {
                self.state = 1;
                self.candidate = Some(now);
                self.confirmations = 1;
            }
            1 if over => {
                self.confirmations += 1;
                if self.confirmations >= 6 || self.candidate.is_some_and(|at| now - at >= 1_000_000)
                {
                    self.state = 2;
                    self.last_pressure = Some(now);
                    self.clear_at = None;
                    self.clear_count = 0;
                }
            }
            1 => {
                self.confirmations = 0;
                if low_loss || self.candidate.is_some_and(|at| now - at >= 1_000_000) {
                    self.state = 0;
                    self.candidate = None;
                }
            }
            2 => {
                if low_loss {
                    let at = *self.clear_at.get_or_insert(now);
                    self.clear_count += 1;
                    if self.clear_count >= 8 || now - at >= 800_000 {
                        self.state = 0;
                    }
                } else {
                    self.clear_at = None;
                    self.clear_count = 0;
                    if correlation.is_some_and(|r| r > 0.5) {
                        self.last_pressure = Some(now);
                    }
                }
                if self.last_pressure.is_some_and(|at| now - at >= 3_000_000) {
                    self.state = 0;
                }
            }
            _ => {}
        }
        if self.state == 2 {
            self.random_loss = false;
        } else if let Some(r) = correlation {
            self.random_loss = loss >= 0.03 && r < 0.6;
        }
    }
}
