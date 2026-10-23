//! A viewer's downlink, from its transport-wide feedback on the sequence
//! numbers this leg writes (`docs/protocol/deployment-design.md` §5.4).
//! The forwarder neither probes nor paces, so it only learns the downlink's
//! capacity from congestion: GoogCC's delay-based detector overusing, or
//! more than [`CONGESTED_LOSS`] of the last second's packets lost while the
//! one-way delay stands [`QUEUEING`] above its recent minimum (a full
//! queue; losses without queueing are a lossy link, which a lower rate does
//! not help). Then the leg's limit is [`BETA`] of the downlink's capacity:
//! the rate packets arrived at over the last [`ARRIVAL_WINDOW`] (a full
//! link delivers back to back at its rate, while a mostly still picture
//! averages far less), at least what reached the viewer over the last
//! second; it relaxes by [`RELAX`] a second without
//! congestion (also while the picture is still and the stream small, so
//! the next motion meets a limit near the downlink's capacity).
use goog_cc::{
    GoogCcConfig, GoogCcNetworkController,
    experiments::FieldTrials,
    network_control::{NetworkControllerConfig, NetworkControllerInterface},
    transport::*,
    units::{DataRate, DataSize, TimeDelta, Timestamp},
};
use std::collections::{BTreeMap, VecDeque};
use std::time::{Duration, Instant};
use webrtc::rtcp::transport_feedbacks::transport_layer_cc::{
    PacketStatusChunk, SymbolTypeTcc, TransportLayerCc,
};

const MINIMUM: f64 = 300_000.0;
const MAXIMUM: i64 = 1_000_000_000;
/// Share of the delivered rate a congested leg is limited to.
const BETA: f64 = 0.85;
const CONGESTED_LOSS: f64 = 0.10;
/// Queueing that, with losses, means the downlink is full.
const QUEUEING: Duration = Duration::from_millis(60);
/// The one-way delay's minimum is taken over this long.
const DELAY_WINDOW: Duration = Duration::from_secs(10);
/// Sent packets of which none arrived for this long: the downlink is dark.
const DARK_AFTER: Duration = Duration::from_secs(1);
/// Arrivals over this long give the capacity of a full downlink...
const ARRIVAL_WINDOW: Duration = Duration::from_millis(300);
/// ...from at least this many packets over at least this long.
const ARRIVAL_PACKETS: usize = 10;
const ARRIVAL_SPAN: Duration = Duration::from_millis(30);
/// Losses above this share make a downlink impaired.
const IMPAIRED_LOSS: f64 = 0.02;
/// The limit is lowered at most this often.
const DECREASE_INTERVAL: Duration = Duration::from_millis(500);
/// A limit relaxes this much a second without congestion...
const RELAX: f64 = 0.10;
/// ...starting this long after the last congestion.
const RELAX_AFTER: Duration = Duration::from_secs(1);
/// IP, UDP and SRTP overhead per packet.
const OVERHEAD: usize = 48;
/// What the leg delivers over this long gives its rate and loss.
const WINDOW: Duration = Duration::from_secs(1);

pub(super) struct Downlink {
    core: GoogCcNetworkController,
    origin: Instant,
    /// Unwrapped transport-wide sequence → the packet and whether reported.
    history: BTreeMap<i64, (SentPacket, bool)>,
    last_sequence: Option<i64>,
    reference: Option<i64>,
    remote_offset: Option<i64>,
    flight: usize,
    limit: Option<f64>,
    /// The first report: a second of them comes before any verdict (the
    /// first keyframe's burst says little about the downlink).
    first_report: Option<Instant>,
    congested_at: Option<Instant>,
    relaxed_at: Instant,
    /// Reported packets: when reported, delivered bytes (`None`: lost).
    reported: VecDeque<(Instant, Option<usize>)>,
    /// Received packets: arrival (µs, the viewer's clock mapped to ours)
    /// and bytes, over the last [`ARRIVAL_WINDOW`].
    arrivals: VecDeque<(i64, usize)>,
    /// Mean one-way delay (µs, with the clocks' offset) of each report.
    delays: VecDeque<(Instant, i64)>,
    queueing: Duration,
    last_sent: Option<Instant>,
    last_delivered: Option<Instant>,
}

impl Downlink {
    pub fn new() -> Self {
        let mut field_trials = FieldTrials::default();
        field_trials.robust_throughput_estimator_settings.enabled = false;
        field_trials.loss_based_control.enabled = false;
        field_trials.loss_based_bwe_v2.enabled = false;
        let origin = Instant::now();
        let at = Timestamp::from_micros(1_000_000);
        let mut core = GoogCcNetworkController::new(
            NetworkControllerConfig {
                field_trials,
                constraints: TargetRateConstraints {
                    at_time: at,
                    min_data_rate: Some(DataRate::from_bits_per_sec(MINIMUM as i64)),
                    max_data_rate: Some(DataRate::from_bits_per_sec(MAXIMUM)),
                    starting_rate: Some(DataRate::from_bits_per_sec(MAXIMUM)),
                },
                stream_based_config: StreamsConfig {
                    at_time: at,
                    ..Default::default()
                },
            },
            GoogCcConfig {
                feedback_only: true,
            },
        );
        let _ = core.on_network_availability(NetworkAvailability {
            at_time: at,
            network_available: true,
        });
        let _ = core.on_process_interval(ProcessInterval {
            at_time: at,
            pacer_queue: None,
        });
        Self {
            core,
            origin,
            history: BTreeMap::new(),
            last_sequence: None,
            reference: None,
            remote_offset: None,
            flight: 0,
            limit: None,
            first_report: None,
            arrivals: VecDeque::new(),
            delays: VecDeque::new(),
            queueing: Duration::ZERO,
            last_sent: None,
            last_delivered: None,
            congested_at: None,
            relaxed_at: origin,
            reported: VecDeque::new(),
        }
    }

    fn now(&self) -> Timestamp {
        Timestamp::from_micros(1_000_000 + self.origin.elapsed().as_micros() as i64)
    }

    /// A packet of `bytes` (RTP) written with transport-wide `sequence`.
    pub fn sent(&mut self, sequence: u16, bytes: usize) {
        let now = self.now();
        let sequence = unwrap(
            sequence.into(),
            16,
            self.last_sequence.unwrap_or(sequence.into()),
        );
        self.last_sequence = Some(sequence);
        let size = bytes + OVERHEAD;
        let packet = SentPacket {
            send_time: now,
            size: DataSize::from_bytes(size as i64),
            sequence_number: sequence,
            data_in_flight: DataSize::from_bytes(self.flight as i64),
            ..Default::default()
        };
        self.flight = self.flight.saturating_add(size);
        self.history.insert(sequence, (packet, false));
        while self.history.first_key_value().is_some_and(|(_, (p, _))| {
            self.history.len() > 32768 || now - p.send_time > TimeDelta::from_seconds(10)
        }) {
            if let Some((_, (old, reported))) = self.history.pop_first()
                && !reported
            {
                self.flight = self.flight.saturating_sub(old.size.bytes() as usize);
            }
        }
        let _ = self.core.on_sent_packet(packet);
        self.last_sent = Some(Instant::now());
    }

    pub fn feedback(&mut self, feedback: &TransportLayerCc) {
        let Some(last) = self.last_sequence else {
            return;
        };
        let count = feedback.packet_status_count as usize;
        if count == 0 {
            return;
        }
        let mut statuses = Vec::with_capacity(count);
        for chunk in &feedback.packet_chunks {
            let remaining = count - statuses.len();
            match chunk {
                PacketStatusChunk::RunLengthChunk(c) => statuses.extend(std::iter::repeat_n(
                    c.packet_status_symbol,
                    remaining.min(c.run_length as usize),
                )),
                PacketStatusChunk::StatusVectorChunk(c) => {
                    statuses.extend(c.symbol_list.iter().copied().take(remaining))
                }
            }
            if statuses.len() == count {
                break;
            }
        }
        let with_delta = |s: &SymbolTypeTcc| {
            matches!(
                s,
                SymbolTypeTcc::PacketReceivedSmallDelta | SymbolTypeTcc::PacketReceivedLargeDelta
            )
        };
        if statuses.len() != count
            || statuses.iter().filter(|s| with_delta(s)).count() != feedback.recv_deltas.len()
        {
            return;
        }
        let now = self.now();
        let at = Instant::now();
        let reference = unwrap(
            feedback.reference_time.into(),
            24,
            self.reference.unwrap_or(feedback.reference_time.into()),
        );
        self.reference = Some(reference);
        let delta_sum: i64 = feedback.recv_deltas.iter().map(|d| d.delta).sum();
        let offset = *self
            .remote_offset
            .get_or_insert(now.us() - reference * 64_000 - delta_sum);
        let mut received_at = reference * 64_000 + offset;
        let base = unwrap(feedback.base_sequence_number.into(), 16, last);
        let mut deltas = feedback.recv_deltas.iter();
        let mut packets = Vec::new();
        let (mut delay_sum, mut delay_count) = (0i64, 0i64);
        for (index, status) in statuses.into_iter().enumerate() {
            let received = status != SymbolTypeTcc::PacketNotReceived;
            if with_delta(&status) {
                received_at += deltas.next().map_or(0, |d| d.delta);
            }
            let Some((packet, reported)) = self.history.get_mut(&(base + index as i64)) else {
                continue;
            };
            if *reported {
                continue;
            }
            *reported = true;
            self.flight = self.flight.saturating_sub(packet.size.bytes() as usize);
            self.reported
                .push_back((at, received.then_some(packet.size.bytes() as usize)));
            if status == SymbolTypeTcc::PacketReceivedWithoutDelta {
                continue;
            }
            if received {
                delay_sum += received_at - packet.send_time.us();
                delay_count += 1;
                self.arrivals
                    .push_back((received_at, packet.size.bytes() as usize));
            }
            packets.push(PacketResult {
                sent_packet: *packet,
                receive_time: if received {
                    Timestamp::from_micros(received_at)
                } else {
                    Timestamp::plus_infinity()
                },
                ..Default::default()
            });
        }
        trim(&mut self.reported, at);
        if let Some(&(newest, _)) = self.arrivals.back() {
            let window = ARRIVAL_WINDOW.as_micros() as i64;
            while self
                .arrivals
                .front()
                .is_some_and(|(t, _)| newest - *t > window)
            {
                self.arrivals.pop_front();
            }
        }
        if delay_count > 0 {
            self.last_delivered = Some(at);
            self.delays.push_back((at, delay_sum / delay_count));
            while self
                .delays
                .front()
                .is_some_and(|(t, _)| at.duration_since(*t) > DELAY_WINDOW)
            {
                self.delays.pop_front();
            }
            let minimum = self.delays.iter().map(|(_, d)| *d).min().unwrap_or(0);
            let latest = delay_sum / delay_count;
            self.queueing = Duration::from_micros((latest - minimum).max(0) as u64);
        }
        if !packets.is_empty() {
            packets.sort_by_key(|p| p.sent_packet.send_time);
            let _ = self
                .core
                .on_transport_packets_feedback(TransportPacketsFeedback {
                    feedback_time: now,
                    data_in_flight: DataSize::from_bytes(self.flight as i64),
                    packet_feedbacks: packets,
                    sendless_arrival_times: Vec::new(),
                });
        }
        self.assess(at);
    }

    /// Congestion lowers the limit to [`BETA`] of what got through.
    fn assess(&mut self, at: Instant) {
        if at.duration_since(*self.first_report.get_or_insert(at)) < WINDOW {
            return;
        }
        let (delivered, _) = self
            .reported
            .iter()
            .fold((0usize, 0usize), |(d, l), (_, b)| {
                (d + b.unwrap_or(0), l + usize::from(b.is_none()))
            });
        let loss = self.loss();
        let overusing = self.core.delay_state() == BandwidthUsage::Overusing;
        if !overusing && (loss <= CONGESTED_LOSS || self.queueing < QUEUEING) {
            return;
        }
        if self
            .congested_at
            .is_some_and(|c| at.duration_since(c) < DECREASE_INTERVAL)
        {
            return;
        }
        let delivered = delivered as f64 * 8.0 / WINDOW.as_secs_f64();
        let limit = BETA * self.arrival_rate().unwrap_or(0.0).max(delivered).max(MINIMUM);
        if self.limit.is_none() {
            tracing::info!(
                limit_bps = limit as u64,
                overusing,
                loss,
                "forwarded viewer's downlink congested"
            );
        }
        tracing::trace!(
            limit_bps = limit as u64,
            overusing,
            loss,
            "downlink limit lowered"
        );
        self.limit = Some(self.limit.map_or(limit, |l| l.min(limit)));
        self.congested_at = Some(at);
        self.relaxed_at = at;
    }

    pub fn tick(&mut self) {
        let _ = self.core.on_process_interval(ProcessInterval {
            at_time: self.now(),
            pacer_queue: None,
        });
        let now = Instant::now();
        trim(&mut self.reported, now);
        let Some(limit) = self.limit else { return };
        let calm = self
            .congested_at
            .is_none_or(|c| now.duration_since(c) >= RELAX_AFTER);
        let seconds = now.duration_since(self.relaxed_at).as_secs_f64();
        self.relaxed_at = now;
        let limit = if calm {
            limit * (1.0 + RELAX * seconds)
        } else {
            limit
        };
        self.limit = if limit >= MAXIMUM as f64 {
            tracing::info!("forwarded viewer's downlink no longer limits");
            None
        } else {
            Some(limit)
        };
    }

    /// The rate packets arrived at over the last [`ARRIVAL_WINDOW`].
    fn arrival_rate(&self) -> Option<f64> {
        let (&(first, _), &(last, _)) = (self.arrivals.front()?, self.arrivals.back()?);
        let span = last - first;
        if self.arrivals.len() < ARRIVAL_PACKETS || span < ARRIVAL_SPAN.as_micros() as i64 {
            return None;
        }
        // The first packet's bytes arrived before the span began.
        let bytes: usize = self.arrivals.iter().skip(1).map(|(_, b)| b).sum();
        Some(bytes as f64 * 8.0 / (span as f64 / 1e6))
    }

    /// Share of the last second's reported packets that were lost.
    pub fn loss(&self) -> f64 {
        let lost = self.reported.iter().filter(|(_, b)| b.is_none()).count();
        let total = self.reported.len();
        if total >= 20 {
            lost as f64 / total as f64
        } else {
            0.0
        }
    }

    /// Packets went out but none arrived for a while: the viewer's downlink
    /// is dark (or the viewer is gone).
    pub fn dark(&self) -> bool {
        let now = Instant::now();
        self.last_sent
            .is_some_and(|sent| now.duration_since(sent) < DARK_AFTER)
            && self
                .last_delivered
                .is_none_or(|delivered| now.duration_since(delivered) >= DARK_AFTER)
            && self.first_report.is_some()
    }

    /// The downlink loses, is limited or dark: its viewer's keyframe
    /// requests are rationed.
    pub fn impaired(&self) -> bool {
        self.limit.is_some() || self.loss() > IMPAIRED_LOSS || self.dark()
    }

    /// The most this leg carries while it is congested.
    pub fn limit(&self) -> Option<u64> {
        self.limit.map(|l| l as u64)
    }
}

/// Drops entries older than [`WINDOW`].
fn trim<T>(entries: &mut VecDeque<(Instant, T)>, now: Instant) {
    while entries
        .front()
        .is_some_and(|(t, _)| now.duration_since(*t) > WINDOW)
    {
        entries.pop_front();
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
