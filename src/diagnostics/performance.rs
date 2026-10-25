use std::collections::{BTreeMap, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};

use crossbeam_queue::ArrayQueue;

pub(crate) mod health;

#[derive(Clone, Debug)]
pub struct PerformanceSnapshot {
    pub(crate) health: health::Assessment,
    pub connection: String,
    pub decoder: String,
    pub video_codec: String,
    pub video_format: String,
    pub remote_capture: String,
    pub remote_encoder: String,
    pub video_track_index: Option<u64>,
    pub quality: String,
    pub bitrate_mbps: f64,
    pub receive_fps: f64,
    pub decode_fps: f64,
    pub render_fps: f64,
    /// Remote new-picture markers counted at successful presentation.
    pub actual_fps: f64,
    pub current_delay_ms: Option<f64>,
    /// HUD frm: interval processing + interval sending + measured RTT.
    /// This is not clock-correlated E2E or the current local frame sample.
    pub frame_delay_ms: Option<u64>,
    /// The part of `frame_delay_ms` spent in the cinema playout buffer (same
    /// interval average); 0 at low latency.
    pub frame_delay_buffer_ms: u64,
    /// Primary packets the network lost over the last second, repaired or not.
    pub packet_loss_percent: f64,
    /// Packets that stayed missing after RTX/FEC, over the same packets.
    pub unrepaired_loss_percent: f64,
    pub lost_packets: u64,
    pub unrepaired_lost_packets: u64,
    pub rtp_jitter_ms: f64,
    pub target_playout_delay_ms: f64,
    pub jitter_playout_delay_ms: f64,
    pub low_latency_playout: bool,
    pub ingress_queue_packets: u64,
    pub ingress_queue_peak_packets: u64,
    pub outstanding_nacks: u64,
    pub frame_buffer_frames: u64,
    pub frame_buffer_peak_frames: u64,
    pub rtx_packets_received: u64,
    pub rtx_packets_accepted: u64,
    pub fec_packets_received: u64,
    pub fec_packets_recovered: u64,
    pub predecode_dropped_frames: u64,
    pub assembly_delay_ms: f64,
    /// Latest frame's wait in the cinema playout buffer (part of the input
    /// queue delay); 0 at low latency.
    pub playout_hold_ms: f64,
    pub local_frame_delay_ms: f64,
    pub local_frame_delay_average_ms: f64,
    pub local_frame_delay_p95_ms: f64,
    pub local_frame_delay_max_ms: f64,
    pub source_cadence: CadenceMetrics,
    pub receive_cadence: CadenceMetrics,
    pub decode_cadence: CadenceMetrics,
    pub render_cadence: CadenceMetrics,
    pub input_queue_delay_ms: f64,
    pub decode_pipeline_delay_ms: f64,
    pub surface_transfer_delay_ms: f64,
    pub present_wait_delay_ms: f64,
    pub render_queue_delay_ms: f64,
    pub decoder_queue_frames: u64,
    pub decoder_queue_peak_frames: u64,
    pub dropped_present_frames: u64,
    pub presentation_drop_percent: f64,
    pub presentation_queue_frames: u64,
    pub presentation_queue_peak_frames: u64,
    pub small_jank_count: u64,
    pub jank_count: u64,
    pub big_jank_count: u64,
    pub total_received_frames: u64,
    /// Existing received-RTP byte counter, paired with this snapshot's uptime.
    pub total_received_rtp_bytes: u64,
    pub total_decoded_frames: u64,
    pub total_key_frames_decoded: u64,
    pub total_rendered_frames: u64,
    pub total_actual_rendered_frames: u64,
    pub total_marked_rendered_frames: u64,
    pub decoded_resolution: Option<(u32, u32)>,
    pub pipeline_stats: Option<PipelineStatistics>,
    pub stream_switch: Option<StreamSwitchSnapshot>,
    pub uptime: Duration,
}

#[derive(Clone, Debug)]
pub struct StreamSwitchSnapshot {
    pub sequence: i64,
    pub target: String,
    pub stage: &'static str,
    pub age_ms: f64,
    pub request_to_ack_ms: Option<f64>,
    pub request_to_continuity_ms: Option<f64>,
    pub request_to_media_ms: Option<f64>,
    pub request_to_present_ms: Option<f64>,
    pub receive_gap_ms: Option<f64>,
    pub decode_gap_ms: Option<f64>,
    pub presentation_gap_ms: Option<f64>,
    pub from_resolution: Option<(u32, u32)>,
    pub actual_resolution: Option<(u32, u32)>,
    pub error: Option<String>,
}

#[derive(Clone, Debug)]
pub struct PipelineStatistics {
    pub source_fps: Option<f64>,
    pub received_fps: f64,
    pub capture: Option<PhaseStatistics>,
    pub encode: Option<PhaseStatistics>,
    pub pacer: Option<PhaseStatistics>,
    pub sending: Option<PhaseStatistics>,
    pub transport: Option<PhaseStatistics>,
    pub assembly: Option<PhaseStatistics>,
    pub decode: Option<PhaseStatistics>,
    pub e2e: Option<PhaseStatistics>,
}

#[derive(Clone, Debug)]
pub(crate) struct RemoteSenderInfo {
    pub video_track_index: u64,
    pub capture_impl: String,
    pub encoder_impl: String,
}

#[derive(Default)]
struct RemoteSenders {
    active_track: Option<u64>,
    tracks: BTreeMap<u64, RemoteSenderInfo>,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct CadenceMetrics {
    pub average_ms: f64,
    pub p95_ms: f64,
    pub max_ms: f64,
}

#[derive(Clone, Copy)]
pub(crate) struct RenderedFrameTiming {
    pub is_new_picture: Option<bool>,
    pub width: u32,
    pub height: u32,
    pub decoded_at: Instant,
    pub local: Duration,
    pub assembly: Duration,
    pub input_queue: Duration,
    pub decode_pipeline: Duration,
    pub surface_transfer: Duration,
    pub present_wait: Duration,
    pub render_queue: Duration,
    pub sender_capture_at: Option<Instant>,
    pub sender_capture: Option<Duration>,
    pub sender_encode: Option<Duration>,
    pub sender_pacer: Option<Duration>,
    pub sender_total: Option<Duration>,
    pub transport: Option<Duration>,
}

#[derive(Clone)]
pub struct PerformanceMonitor {
    inner: Arc<PerformanceInner>,
    session: Option<Arc<PerformanceInner>>,
}

struct PerformanceInner {
    tracks: RwLock<std::collections::BTreeMap<u64, std::sync::Weak<PerformanceInner>>>,
    started_at: Instant,
    received_bytes: AtomicU64,
    expected_packets: AtomicU64,
    /// All tracks' primary packets expected and arrived, and RTP bytes
    /// received (kept by the session).
    session_packets: (AtomicU64, AtomicU64, AtomicU64),
    arrived_packets: AtomicU64,
    final_lost_packets: AtomicU64,
    rtp_jitter_micros: AtomicU64,
    target_playout_delay_micros: AtomicU64,
    jitter_playout_delay_micros: AtomicU64,
    low_latency_playout: AtomicU64,
    ingress_queue_packets: AtomicU64,
    ingress_queue_peak_packets: AtomicU64,
    outstanding_nacks: AtomicU64,
    frame_buffer_frames: AtomicU64,
    frame_buffer_peak_frames: AtomicU64,
    rtx_packets_received: AtomicU64,
    rtx_packets_accepted: AtomicU64,
    fec_packets_received: AtomicU64,
    fec_packets_recovered: AtomicU64,
    predecode_dropped_frames: AtomicU64,
    received_frames: AtomicU64,
    decoded_frames: AtomicU64,
    key_frames_decoded: AtomicU64,
    decoded_width: AtomicU64,
    decoded_height: AtomicU64,
    rendered_frames: AtomicU64,
    actual_rendered_frames: AtomicU64,
    marked_rendered_frames: AtomicU64,
    presentation_started: AtomicBool,
    current_delay_micros: AtomicU64,
    current_delay_observed_micros: AtomicU64,
    presentation_epoch: AtomicU64,
    /// When the last new picture was shown (µs after `started_at`, 0: none).
    last_picture_micros: AtomicU64,
    /// Since when the picture waits to come through another connection
    /// (µs after `started_at`, 0: it does not).
    switch_pending_micros: AtomicU64,
    measured_media_rtt_micros: AtomicU64,
    measured_media_rtt_observed_micros: AtomicU64,
    assembly_delay_micros: AtomicU64,
    playout_hold_micros: AtomicU64,
    local_frame_delay_micros: AtomicU64,
    input_queue_delay_micros: AtomicU64,
    decode_pipeline_delay_micros: AtomicU64,
    surface_transfer_delay_micros: AtomicU64,
    present_wait_delay_micros: AtomicU64,
    render_queue_delay_micros: AtomicU64,
    decoder_queue_frames: AtomicU64,
    decoder_queue_peak_frames: AtomicU64,
    dropped_present_frames: AtomicU64,
    presentation_queue_frames: AtomicU64,
    presentation_queue_peak_frames: AtomicU64,
    connection: RwLock<String>,
    decoder: RwLock<String>,
    video_codec: RwLock<String>,
    video_format: RwLock<String>,
    remote_senders: RwLock<RemoteSenders>,
    quality: RwLock<String>,
    stream_switch: Mutex<Option<StreamSwitchState>>,
    stream_switch_active: AtomicBool,
    rates: Mutex<RateState>,
    rtp_timing: Mutex<RtpTimingState>,
    sample_events: ArrayQueue<PerformanceSampleEvent>,
    sample_state: Mutex<PerformanceSampleState>,
    snapshot_cache: Mutex<Option<(Instant, Arc<PerformanceSnapshot>)>>,
    health: Mutex<health::Health>,
}

#[derive(Clone, Copy)]
// Keeping the render sample inline avoids a heap allocation on every presented frame.
#[allow(clippy::large_enum_variant)]
enum PerformanceSampleEvent {
    Received {
        rtp_timestamp: u32,
        assembled_at: Instant,
        sending_delay_ms: Option<u16>,
        playout_hold: Duration,
    },
    Decoded {
        decoded_at: Instant,
        processing: Duration,
    },
    Rendered {
        timing: RenderedFrameTiming,
        rendered_at: Instant,
        rendered_before: u64,
    },
}

#[derive(Clone)]
struct PerformanceSampleState {
    latency: LatencyState,
    source_receive_cadence: SourceReceiveCadenceState,
    decode_cadence: CadenceWindow,
    pipeline_latency: PipelineLatencyState,
    frame_period: FrameDelayPeriod,
}

#[derive(Clone, Default)]
struct FrameDelayPeriod {
    started_at: Option<Instant>,
    processing_sum_ms: u128,
    processing_count: u64,
    sending_sum_ms: u128,
    sending_count: u64,
    hold_sum_ms: u128,
    hold_count: u64,
    /// Interval averages: processing, sending, cinema playout hold.
    published: Option<(u64, u64, u64)>,
}

impl FrameDelayPeriod {
    fn advance(&mut self, now: Instant) {
        let start = self.started_at.get_or_insert(now);
        if now.saturating_duration_since(*start) < Duration::from_secs(1) {
            return;
        }
        // Read integer averages and reset interval counters.
        // No samples means zero for that interval, not a held last-frame value.
        self.published = Some((
            (self.processing_sum_ms / u128::from(self.processing_count.max(1))) as u64,
            (self.sending_sum_ms / u128::from(self.sending_count.max(1))) as u64,
            (self.hold_sum_ms / u128::from(self.hold_count.max(1))) as u64,
        ));
        *start = now;
        self.processing_sum_ms = 0;
        self.processing_count = 0;
        self.sending_sum_ms = 0;
        self.sending_count = 0;
        self.hold_sum_ms = 0;
        self.hold_count = 0;
    }
}

impl PerformanceSampleState {
    fn apply(&mut self, event: PerformanceSampleEvent) {
        match event {
            PerformanceSampleEvent::Received {
                rtp_timestamp,
                assembled_at,
                sending_delay_ms,
                playout_hold,
            } => {
                self.frame_period.advance(assembled_at);
                if let Some(delay) = sending_delay_ms {
                    self.frame_period.sending_sum_ms += u128::from(delay);
                    self.frame_period.sending_count += 1;
                }
                self.frame_period.hold_sum_ms += (playout_hold.as_micros() + 500) / 1000;
                self.frame_period.hold_count += 1;
                self.source_receive_cadence.receive.record(assembled_at);
                if let Some(previous) = self.source_receive_cadence.source_timestamp {
                    let ticks = i64::from(rtp_timestamp.wrapping_sub(previous) as i32);
                    if ticks > 0 {
                        let interval = Duration::from_secs_f64(ticks as f64 / 90_000.0);
                        if interval <= Duration::from_millis(100) {
                            self.source_receive_cadence.source.record_interval(interval);
                        }
                    }
                }
                self.source_receive_cadence.source_timestamp = Some(rtp_timestamp);
            }
            PerformanceSampleEvent::Decoded {
                decoded_at,
                processing,
            } => {
                self.frame_period.advance(decoded_at);
                // OnDecodedFrame rounds each microsecond duration to ms before
                // adding it; GetStats then truncates the integer mean.
                self.frame_period.processing_sum_ms += (processing.as_micros() + 500) / 1000;
                self.frame_period.processing_count += 1;
                self.decode_cadence.record(decoded_at);
            }
            PerformanceSampleEvent::Rendered {
                timing,
                rendered_at,
                rendered_before,
            } => {
                let e2e = timing
                    .sender_capture_at
                    .and_then(|capture_at| timing.decoded_at.checked_duration_since(capture_at));
                self.pipeline_latency
                    .samples
                    .push_back(PipelineLatencySample {
                        recorded_at: rendered_at,
                        capture_micros: timing.sender_capture.map(duration_micros),
                        encode_micros: timing.sender_encode.map(duration_micros),
                        pacer_micros: timing.sender_pacer.map(duration_micros),
                        sending_micros: timing.sender_total.map(duration_micros),
                        transport_micros: timing.transport.map(duration_micros),
                        assembly_micros: duration_micros(timing.assembly),
                        decode_micros: duration_micros(timing.decode_pipeline),
                        e2e_micros: e2e.map(duration_micros),
                    });
                while self.pipeline_latency.samples.len() > LATENCY_SAMPLE_WINDOW
                    || self.pipeline_latency.samples.front().is_some_and(|sample| {
                        rendered_at.saturating_duration_since(sample.recorded_at)
                            > PIPELINE_STATS_WINDOW
                    })
                {
                    self.pipeline_latency.samples.pop_front();
                }

                let local_micros = duration_micros(timing.local);
                if let Some(previous) = self.latency.last_rendered_at {
                    let interval = rendered_at.duration_since(previous);
                    let interval_micros = duration_micros(interval);
                    if rendered_before >= 60 {
                        if interval >= JANK_THRESHOLD {
                            self.latency.jank_count = self.latency.jank_count.saturating_add(1);
                            if interval >= BIG_JANK_THRESHOLD {
                                self.latency.big_jank_count =
                                    self.latency.big_jank_count.saturating_add(1);
                            }
                        } else if interval >= SMALL_JANK_THRESHOLD {
                            self.latency.small_jank_count =
                                self.latency.small_jank_count.saturating_add(1);
                        }
                    }
                    self.latency
                        .frame_interval_samples_micros
                        .push_back(interval_micros);
                    self.latency.frame_interval_sum_micros = self
                        .latency
                        .frame_interval_sum_micros
                        .saturating_add(u128::from(interval_micros));
                    if self.latency.frame_interval_samples_micros.len() > LATENCY_SAMPLE_WINDOW
                        && let Some(removed) =
                            self.latency.frame_interval_samples_micros.pop_front()
                    {
                        self.latency.frame_interval_sum_micros = self
                            .latency
                            .frame_interval_sum_micros
                            .saturating_sub(u128::from(removed));
                    }
                }
                self.latency.last_rendered_at = Some(rendered_at);
                self.latency.local_samples_micros.push_back(local_micros);
                self.latency.local_sum_micros = self
                    .latency
                    .local_sum_micros
                    .saturating_add(u128::from(local_micros));
                if self.latency.local_samples_micros.len() > LATENCY_SAMPLE_WINDOW
                    && let Some(removed) = self.latency.local_samples_micros.pop_front()
                {
                    self.latency.local_sum_micros = self
                        .latency
                        .local_sum_micros
                        .saturating_sub(u128::from(removed));
                }
            }
        }
    }
}

#[derive(Clone, Copy)]
enum StreamSwitchStage {
    Requested,
    Acknowledged,
    MediaChanged,
    Presented,
    Failed,
}

struct StreamSwitchState {
    sequence: i64,
    target: String,
    requested_at: Instant,
    acknowledged_at: Option<Instant>,
    continuity_presented_at: Option<Instant>,
    media_changed_at: Option<Instant>,
    presented_at: Option<Instant>,
    finished_at: Option<Instant>,
    previous_received_at: Option<Instant>,
    previous_decoded_at: Option<Instant>,
    previous_rendered_at: Option<Instant>,
    receive_gap: Option<Duration>,
    decode_gap: Option<Duration>,
    presentation_gap: Option<Duration>,
    from_resolution: Option<(u32, u32)>,
    actual_resolution: Option<(u32, u32)>,
    stage: StreamSwitchStage,
    error: Option<String>,
}

#[derive(Clone)]
struct LatencyState {
    local_samples_micros: VecDeque<u64>,
    local_sum_micros: u128,
    last_rendered_at: Option<Instant>,
    small_jank_count: u64,
    jank_count: u64,
    big_jank_count: u64,
    frame_interval_samples_micros: VecDeque<u64>,
    frame_interval_sum_micros: u128,
}

struct RtpTimingState {
    last_arrival: Option<Instant>,
    base_arrival: Option<Instant>,
    base_timestamp: u32,
    previous_transit_ticks: Option<f64>,
    jitter_ticks: f64,
}

#[derive(Clone, Copy)]
struct PipelineLatencySample {
    recorded_at: Instant,
    capture_micros: Option<u64>,
    encode_micros: Option<u64>,
    pacer_micros: Option<u64>,
    sending_micros: Option<u64>,
    transport_micros: Option<u64>,
    assembly_micros: u64,
    decode_micros: u64,
    e2e_micros: Option<u64>,
}

#[derive(Clone, Default)]
struct PipelineLatencyState {
    samples: VecDeque<PipelineLatencySample>,
}

#[derive(Clone, Copy, Debug)]
pub struct PhaseStatistics {
    pub average_ms: f64,
    pub p50_ms: f64,
    pub p90_ms: f64,
    pub p99_ms: f64,
    pub max_ms: f64,
}

impl PipelineLatencyState {
    fn statistics(&self, source_fps: Option<f64>, received_fps: f64) -> Option<PipelineStatistics> {
        if self.samples.is_empty() {
            return None;
        }
        Some(PipelineStatistics {
            source_fps,
            received_fps,
            capture: phase_statistics(&self.samples, |s| s.capture_micros),
            encode: phase_statistics(&self.samples, |s| s.encode_micros),
            pacer: phase_statistics(&self.samples, |s| s.pacer_micros),
            sending: phase_statistics(&self.samples, |s| s.sending_micros),
            transport: phase_statistics(&self.samples, |s| s.transport_micros),
            assembly: phase_statistics(&self.samples, |s| Some(s.assembly_micros)),
            decode: phase_statistics(&self.samples, |s| Some(s.decode_micros)),
            e2e: phase_statistics(&self.samples, |s| s.e2e_micros),
        })
    }
}

fn phase_statistics(
    samples: &VecDeque<PipelineLatencySample>,
    select: impl Fn(&PipelineLatencySample) -> Option<u64>,
) -> Option<PhaseStatistics> {
    let mut values = samples.iter().filter_map(select).collect::<Vec<_>>();
    if values.is_empty() {
        return None;
    }
    let sum = values
        .iter()
        .fold(0_u128, |sum, value| sum.saturating_add(u128::from(*value)));
    values.sort_unstable();
    Some(PhaseStatistics {
        average_ms: sum as f64 / values.len() as f64 / 1_000.0,
        p50_ms: percentile_millis(&values, 50),
        p90_ms: percentile_millis(&values, 90),
        p99_ms: percentile_millis(&values, 99),
        max_ms: values.last().copied().map_or(0.0, micros_to_millis),
    })
}

#[derive(Clone, Default)]
struct SourceReceiveCadenceState {
    source_timestamp: Option<u32>,
    source: CadenceWindow,
    receive: CadenceWindow,
}

#[derive(Clone, Default)]
struct CadenceWindow {
    last_at: Option<Instant>,
    samples_micros: VecDeque<u64>,
    sum_micros: u128,
}

const LATENCY_SAMPLE_WINDOW: usize = 300;
const SMALL_JANK_THRESHOLD: Duration = Duration::from_millis(100);
const JANK_THRESHOLD: Duration = Duration::from_millis(180);
const BIG_JANK_THRESHOLD: Duration = Duration::from_millis(500);
const SNAPSHOT_CACHE_TTL: Duration = Duration::from_millis(250);
const PIPELINE_STATS_WINDOW: Duration = Duration::from_secs(30);
const PERFORMANCE_SAMPLE_QUEUE_CAPACITY: usize = 8_192;

#[derive(Clone, Copy)]
struct RateState {
    sampled_at: Instant,
    received_bytes: u64,
    expected_packets: u64,
    arrived_packets: u64,
    final_lost_packets: u64,
    received_frames: u64,
    decoded_frames: u64,
    rendered_frames: u64,
    actual_rendered_frames: u64,
    bitrate_mbps: f64,
    receive_fps: f64,
    decode_fps: f64,
    render_fps: f64,
    actual_fps: f64,
    packet_loss_percent: f64,
    unrepaired_loss_percent: f64,
}

impl PerformanceMonitor {
    pub fn new(quality: impl Into<String>) -> Self {
        let now = Instant::now();
        Self {
            session: None,
            inner: Arc::new(PerformanceInner {
                tracks: RwLock::new(std::collections::BTreeMap::new()),
                started_at: now,
                received_bytes: AtomicU64::new(0),
                expected_packets: AtomicU64::new(0),
                session_packets: (AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0)),
                arrived_packets: AtomicU64::new(0),
                final_lost_packets: AtomicU64::new(0),
                rtp_jitter_micros: AtomicU64::new(0),
                target_playout_delay_micros: AtomicU64::new(0),
                jitter_playout_delay_micros: AtomicU64::new(0),
                low_latency_playout: AtomicU64::new(0),
                ingress_queue_packets: AtomicU64::new(0),
                ingress_queue_peak_packets: AtomicU64::new(0),
                outstanding_nacks: AtomicU64::new(0),
                frame_buffer_frames: AtomicU64::new(0),
                frame_buffer_peak_frames: AtomicU64::new(0),
                rtx_packets_received: AtomicU64::new(0),
                rtx_packets_accepted: AtomicU64::new(0),
                fec_packets_received: AtomicU64::new(0),
                fec_packets_recovered: AtomicU64::new(0),
                predecode_dropped_frames: AtomicU64::new(0),
                received_frames: AtomicU64::new(0),
                decoded_frames: AtomicU64::new(0),
                key_frames_decoded: AtomicU64::new(0),
                decoded_width: AtomicU64::new(0),
                decoded_height: AtomicU64::new(0),
                rendered_frames: AtomicU64::new(0),
                actual_rendered_frames: AtomicU64::new(0),
                marked_rendered_frames: AtomicU64::new(0),
                presentation_started: AtomicBool::new(false),
                current_delay_micros: AtomicU64::new(u64::MAX),
                current_delay_observed_micros: AtomicU64::new(u64::MAX),
                presentation_epoch: AtomicU64::new(0),
                last_picture_micros: AtomicU64::new(0),
                switch_pending_micros: AtomicU64::new(0),
                measured_media_rtt_micros: AtomicU64::new(u64::MAX),
                measured_media_rtt_observed_micros: AtomicU64::new(u64::MAX),
                assembly_delay_micros: AtomicU64::new(0),
                playout_hold_micros: AtomicU64::new(0),
                local_frame_delay_micros: AtomicU64::new(0),
                input_queue_delay_micros: AtomicU64::new(0),
                decode_pipeline_delay_micros: AtomicU64::new(0),
                surface_transfer_delay_micros: AtomicU64::new(0),
                present_wait_delay_micros: AtomicU64::new(0),
                render_queue_delay_micros: AtomicU64::new(0),
                decoder_queue_frames: AtomicU64::new(0),
                decoder_queue_peak_frames: AtomicU64::new(0),
                dropped_present_frames: AtomicU64::new(0),
                presentation_queue_frames: AtomicU64::new(0),
                presentation_queue_peak_frames: AtomicU64::new(0),
                connection: RwLock::new(crate::i18n::t().session.perf_connecting.to_owned()),
                decoder: RwLock::new(crate::i18n::t().session.perf_waiting_video.to_owned()),
                video_codec: RwLock::new(crate::i18n::t().session.perf_waiting_negotiation.to_owned()),
                video_format: RwLock::new(crate::i18n::t().session.perf_waiting_parameter_sets.to_owned()),
                remote_senders: RwLock::new(RemoteSenders::default()),
                quality: RwLock::new(quality.into()),
                stream_switch: Mutex::new(None),
                stream_switch_active: AtomicBool::new(false),
                rates: Mutex::new(RateState {
                    sampled_at: now,
                    received_bytes: 0,
                    expected_packets: 0,
                    arrived_packets: 0,
                    final_lost_packets: 0,
                    received_frames: 0,
                    decoded_frames: 0,
                    rendered_frames: 0,
                    actual_rendered_frames: 0,
                    bitrate_mbps: 0.0,
                    receive_fps: 0.0,
                    decode_fps: 0.0,
                    render_fps: 0.0,
                    actual_fps: 0.0,
                    packet_loss_percent: 0.0,
                    unrepaired_loss_percent: 0.0,
                }),
                rtp_timing: Mutex::new(RtpTimingState {
                    last_arrival: None,
                    base_arrival: None,
                    base_timestamp: 0,
                    previous_transit_ticks: None,
                    jitter_ticks: 0.0,
                }),
                sample_events: ArrayQueue::new(PERFORMANCE_SAMPLE_QUEUE_CAPACITY),
                sample_state: Mutex::new(PerformanceSampleState {
                    latency: LatencyState {
                        local_samples_micros: VecDeque::with_capacity(LATENCY_SAMPLE_WINDOW),
                        local_sum_micros: 0,
                        last_rendered_at: None,
                        small_jank_count: 0,
                        jank_count: 0,
                        big_jank_count: 0,
                        frame_interval_samples_micros: VecDeque::with_capacity(
                            LATENCY_SAMPLE_WINDOW,
                        ),
                        frame_interval_sum_micros: 0,
                    },
                    source_receive_cadence: SourceReceiveCadenceState::default(),
                    decode_cadence: CadenceWindow::default(),
                    pipeline_latency: PipelineLatencyState::default(),
                    frame_period: FrameDelayPeriod::default(),
                }),
                snapshot_cache: Mutex::new(None),
                health: Mutex::new(health::Health::default()),
            }),
        }
    }

    fn push_sample_event(&self, event: PerformanceSampleEvent) {
        if let Err(event) = self.inner.sample_events.push(event) {
            let _ = self.inner.sample_events.pop();
            let _ = self.inner.sample_events.push(event);
        }
    }

    fn drain_sample_events(&self) {
        let mut state = mutex_lock(&self.inner.sample_state);
        while let Some(event) = self.inner.sample_events.pop() {
            state.apply(event);
        }
    }

    pub(crate) fn record_rtp_packet(&self, bytes: usize) {
        // Decrypted received RTP bytes (primary + actual RTX/FEC), not pure
        // encoded video bytes, estimated bandwidth, or reconstructed FEC bytes.
        self.inner
            .received_bytes
            .fetch_add(bytes as u64, Ordering::Relaxed);
        let session = self.session.as_ref().unwrap_or(&self.inner);
        session.session_packets.2.fetch_add(bytes as u64, Ordering::Relaxed);
    }

    /// One packet arrived on the primary media SSRC (padding included, not
    /// RTX or FEC repair), making `expected` more packets known to be sent.
    pub(crate) fn record_primary_arrival(&self, expected: u64) {
        self.inner
            .expected_packets
            .fetch_add(expected, Ordering::Relaxed);
        self.inner.arrived_packets.fetch_add(1, Ordering::Relaxed);
        let session = self.session.as_ref().unwrap_or(&self.inner);
        session.session_packets.0.fetch_add(expected, Ordering::Relaxed);
        session.session_packets.1.fetch_add(1, Ordering::Relaxed);
    }

    /// Primary video packets known to be sent, those that arrived, and RTP
    /// bytes received, over all tracks of the session.
    pub(crate) fn media_counts(&self) -> (u64, u64, u64) {
        let session = self.session.as_ref().unwrap_or(&self.inner);
        (
            session.session_packets.0.load(Ordering::Relaxed),
            session.session_packets.1.load(Ordering::Relaxed),
            session.session_packets.2.load(Ordering::Relaxed),
        )
    }

    pub(crate) fn set_nack_state(&self, outstanding: usize, final_lost_packets: u64) {
        self.inner
            .outstanding_nacks
            .store(outstanding as u64, Ordering::Relaxed);
        self.inner
            .final_lost_packets
            .store(final_lost_packets, Ordering::Relaxed);
    }

    pub(crate) fn set_ingress_queue_packets(&self, packets: usize) {
        let packets = packets as u64;
        self.inner
            .ingress_queue_packets
            .store(packets, Ordering::Relaxed);
        self.inner
            .ingress_queue_peak_packets
            .fetch_max(packets, Ordering::Relaxed);
    }

    pub(crate) fn set_frame_buffer_frames(&self, frames: usize) {
        let frames = frames as u64;
        self.inner
            .frame_buffer_frames
            .store(frames, Ordering::Relaxed);
        self.inner
            .frame_buffer_peak_frames
            .fetch_max(frames, Ordering::Relaxed);
    }

    pub(crate) fn record_video_rtp_arrival(&self, timestamp: u32) {
        let now = Instant::now();
        let mut timing = mutex_lock(&self.inner.rtp_timing);
        timing.last_arrival = Some(now);
        let Some(base_arrival) = timing.base_arrival else {
            timing.base_arrival = Some(now);
            timing.base_timestamp = timestamp;
            return;
        };
        let arrival_ticks = now.duration_since(base_arrival).as_secs_f64() * 90_000.0;
        let timestamp_ticks = f64::from(timestamp.wrapping_sub(timing.base_timestamp) as i32);
        let transit_ticks = arrival_ticks - timestamp_ticks;
        if let Some(previous) = timing.previous_transit_ticks {
            let delta = (transit_ticks - previous).abs();
            timing.jitter_ticks += (delta - timing.jitter_ticks) / 16.0;
        }
        timing.previous_transit_ticks = Some(transit_ticks);
        let jitter_micros =
            (timing.jitter_ticks * 1_000_000.0 / 90_000.0).clamp(0.0, u64::MAX as f64) as u64;
        self.inner
            .rtp_jitter_micros
            .store(jitter_micros, Ordering::Relaxed);
    }

    pub(crate) fn record_rtx_packet(&self, recovered: bool) {
        self.inner
            .rtx_packets_received
            .fetch_add(1, Ordering::Relaxed);
        if recovered {
            self.inner
                .rtx_packets_accepted
                .fetch_add(1, Ordering::Relaxed);
        }
    }

    pub(crate) fn record_fec_packet_received(&self) {
        self.inner
            .fec_packets_received
            .fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn record_fec_recovered(&self, recovered_packets: usize) {
        self.inner
            .fec_packets_recovered
            .fetch_add(recovered_packets as u64, Ordering::Relaxed);
    }

    pub(crate) fn record_predecode_drops(&self, frames: usize) {
        self.inner
            .predecode_dropped_frames
            .fetch_add(frames as u64, Ordering::Relaxed);
    }

    pub(crate) fn record_received_frame(
        &self,
        assembly_delay: Duration,
        rtp_timestamp: u32,
        assembled_at: Instant,
        keyframe: bool,
        sending_delay_ms: Option<u16>,
        playout_hold: Duration,
    ) {
        self.inner.received_frames.fetch_add(1, Ordering::Relaxed);
        self.inner
            .assembly_delay_micros
            .store(duration_micros(assembly_delay), Ordering::Relaxed);
        self.inner
            .playout_hold_micros
            .store(duration_micros(playout_hold), Ordering::Relaxed);
        self.push_sample_event(PerformanceSampleEvent::Received {
            rtp_timestamp,
            assembled_at,
            sending_delay_ms,
            playout_hold,
        });

        if self.inner.stream_switch_active.load(Ordering::Relaxed) {
            let mut stream_switch = mutex_lock(&self.inner.stream_switch);
            if let Some(switch) = stream_switch.as_mut()
                && !matches!(
                    switch.stage,
                    StreamSwitchStage::Presented | StreamSwitchStage::Failed
                )
            {
                let previous = switch.previous_received_at.replace(assembled_at);
                if keyframe && switch.receive_gap.is_none() {
                    switch.receive_gap =
                        previous.and_then(|previous| assembled_at.checked_duration_since(previous));
                }
            }
        }
    }

    pub(crate) fn record_decoded_frame(
        &self,
        decoded_at: Instant,
        width: u32,
        height: u32,
        keyframe: bool,
        processing: Duration,
    ) {
        self.inner.decoded_frames.fetch_add(1, Ordering::Relaxed);
        if keyframe {
            self.inner
                .key_frames_decoded
                .fetch_add(1, Ordering::Relaxed);
        }
        self.inner
            .decoded_width
            .store(u64::from(width), Ordering::Relaxed);
        self.inner
            .decoded_height
            .store(u64::from(height), Ordering::Relaxed);
        self.push_sample_event(PerformanceSampleEvent::Decoded {
            decoded_at,
            processing,
        });
        if self.inner.stream_switch_active.load(Ordering::Relaxed) {
            let mut stream_switch = mutex_lock(&self.inner.stream_switch);
            if let Some(switch) = stream_switch.as_mut()
                && !matches!(
                    switch.stage,
                    StreamSwitchStage::Presented | StreamSwitchStage::Failed
                )
            {
                let previous = switch.previous_decoded_at.replace(decoded_at);
                if keyframe && switch.decode_gap.is_none() {
                    switch.decode_gap =
                        previous.and_then(|previous| decoded_at.checked_duration_since(previous));
                }
            }
            if keyframe && let Some(switch) = stream_switch.as_mut() {
                let active = !matches!(
                    switch.stage,
                    StreamSwitchStage::Presented | StreamSwitchStage::Failed
                );
                if active {
                    switch.actual_resolution = Some((width, height));
                    switch.media_changed_at.get_or_insert(decoded_at);
                    if switch.acknowledged_at.is_some() {
                        switch.stage = StreamSwitchStage::MediaChanged;
                    }
                    tracing::info!(
                        sequence = switch.sequence,
                        width,
                        height,
                        elapsed_ms =
                            decoded_at.duration_since(switch.requested_at).as_secs_f64() * 1_000.0,
                        "runtime stream switch reached a decodable keyframe"
                    );
                }
            }
        }
    }

    pub(crate) fn record_rendered_frame(&self, timing: RenderedFrameTiming) {
        let now = Instant::now();
        let rendered_before = self.inner.rendered_frames.fetch_add(1, Ordering::Relaxed);
        // Bootstrap a new presentation epoch once, then count only new
        // pictures. A missing marker defaults to true.
        // This counter must not be used to skip decoding or presentation.
        let first = !self
            .inner
            .presentation_started
            .swap(true, Ordering::Relaxed);
        if timing.is_new_picture.unwrap_or(true) || first {
            self.inner
                .actual_rendered_frames
                .fetch_add(1, Ordering::Relaxed);
            self.inner.last_picture_micros.store(
                duration_micros(now.duration_since(self.inner.started_at)).max(1),
                Ordering::Relaxed,
            );
        }
        if timing.is_new_picture.is_some() {
            self.inner
                .marked_rendered_frames
                .fetch_add(1, Ordering::Relaxed);
        }
        self.inner
            .local_frame_delay_micros
            .store(duration_micros(timing.local), Ordering::Relaxed);
        self.inner
            .assembly_delay_micros
            .store(duration_micros(timing.assembly), Ordering::Relaxed);
        self.inner
            .input_queue_delay_micros
            .store(duration_micros(timing.input_queue), Ordering::Relaxed);
        self.inner
            .decode_pipeline_delay_micros
            .store(duration_micros(timing.decode_pipeline), Ordering::Relaxed);
        self.inner
            .surface_transfer_delay_micros
            .store(duration_micros(timing.surface_transfer), Ordering::Relaxed);
        self.inner
            .present_wait_delay_micros
            .store(duration_micros(timing.present_wait), Ordering::Relaxed);
        self.inner
            .render_queue_delay_micros
            .store(duration_micros(timing.render_queue), Ordering::Relaxed);
        self.push_sample_event(PerformanceSampleEvent::Rendered {
            timing,
            rendered_at: now,
            rendered_before,
        });

        if self.inner.stream_switch_active.load(Ordering::Relaxed) {
            let mut stream_switch = mutex_lock(&self.inner.stream_switch);
            if let Some(switch) = stream_switch.as_mut()
                && !matches!(
                    switch.stage,
                    StreamSwitchStage::Presented | StreamSwitchStage::Failed
                )
            {
                let previous_rendered_at = switch.previous_rendered_at.replace(now);
                if switch.acknowledged_at.is_some() && switch.continuity_presented_at.is_none() {
                    switch.continuity_presented_at = Some(now);
                    tracing::info!(
                        sequence = switch.sequence,
                        width = timing.width,
                        height = timing.height,
                        elapsed_ms =
                            now.duration_since(switch.requested_at).as_secs_f64() * 1_000.0,
                        "runtime stream continued presenting after acknowledgement"
                    );
                }
                if switch.acknowledged_at.is_some() && switch.media_changed_at.is_some() {
                    switch.actual_resolution = Some((timing.width, timing.height));
                    switch.presented_at.get_or_insert(now);
                    switch.finished_at.get_or_insert(now);
                    switch.presentation_gap =
                        previous_rendered_at.map(|previous| now.duration_since(previous));
                    switch.stage = StreamSwitchStage::Presented;
                    self.inner
                        .stream_switch_active
                        .store(false, Ordering::Relaxed);
                    tracing::info!(
                        sequence = switch.sequence,
                        width = timing.width,
                        height = timing.height,
                        elapsed_ms =
                            now.duration_since(switch.requested_at).as_secs_f64() * 1_000.0,
                        presentation_gap_ms = switch
                            .presentation_gap
                            .map_or(0.0, |gap| gap.as_secs_f64() * 1_000.0),
                        "runtime stream switch reached presentation"
                    );
                }
            }
        }
    }

    /// The picture waits to come through another connection (`true`), or
    /// no longer does.
    pub(crate) fn set_switch_pending(&self, pending: bool) {
        let now = duration_micros(Instant::now().duration_since(self.inner.started_at)).max(1);
        if pending {
            let _ = self.inner.switch_pending_micros.compare_exchange(
                0,
                now,
                Ordering::Relaxed,
                Ordering::Relaxed,
            );
        } else {
            self.inner.switch_pending_micros.store(0, Ordering::Relaxed);
        }
    }

    /// How long the picture has waited to come through another connection.
    pub(crate) fn switch_pending(&self) -> Option<Duration> {
        let since = self.inner.switch_pending_micros.load(Ordering::Relaxed);
        (since != 0).then(|| {
            Instant::now()
                .duration_since(self.inner.started_at)
                .saturating_sub(Duration::from_micros(since))
        })
    }

    /// Video packets arrived within `window`.
    pub(crate) fn video_arrived_within(&self, window: Duration) -> bool {
        mutex_lock(&self.inner.rtp_timing)
            .last_arrival
            .is_some_and(|at| at.elapsed() < window)
    }

    /// How long ago the last new picture was shown.
    pub(crate) fn picture_age(&self) -> Option<Duration> {
        let at = self.inner.last_picture_micros.load(Ordering::Relaxed);
        (at != 0).then(|| {
            Instant::now()
                .duration_since(self.inner.started_at)
                .saturating_sub(Duration::from_micros(at))
        })
    }

    pub(crate) fn record_dropped_present_frame(&self) {
        self.inner
            .dropped_present_frames
            .fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn set_presentation_queue_frames(&self, frames: usize) {
        let frames = frames as u64;
        self.inner
            .presentation_queue_frames
            .store(frames, Ordering::Relaxed);
        self.inner
            .presentation_queue_peak_frames
            .fetch_max(frames, Ordering::Relaxed);
    }

    pub(crate) fn set_current_delay(&self, delay: Option<(Duration, Instant)>) {
        self.inner.current_delay_micros.store(
            delay.map_or(u64::MAX, |(value, _)| duration_micros(value)),
            Ordering::Relaxed,
        );
        self.inner.current_delay_observed_micros.store(
            delay
                .and_then(|(_, at)| at.checked_duration_since(self.inner.started_at))
                .map_or(u64::MAX, duration_micros),
            Ordering::Relaxed,
        );
    }

    /// The HUD network RTT may fall back to ICE. The frm calculation must
    /// use the selected RTP module's measured RTCP RTT, not that fallback.
    pub(crate) fn set_measured_media_rtt(
        &self,
        delay: Option<Duration>,
        observed_at: Option<Instant>,
    ) {
        self.inner
            .measured_media_rtt_micros
            .store(delay.map_or(u64::MAX, duration_micros), Ordering::Relaxed);
        self.inner.measured_media_rtt_observed_micros.store(
            observed_at
                .and_then(|at| at.checked_duration_since(self.inner.started_at))
                .map_or(u64::MAX, duration_micros),
            Ordering::Relaxed,
        );
    }

    pub(crate) fn set_playout_timing(
        &self,
        target_delay: Duration,
        jitter_delay: Duration,
        low_latency: bool,
    ) {
        self.inner
            .target_playout_delay_micros
            .store(duration_micros(target_delay), Ordering::Relaxed);
        self.inner
            .jitter_playout_delay_micros
            .store(duration_micros(jitter_delay), Ordering::Relaxed);
        self.inner
            .low_latency_playout
            .store(u64::from(low_latency), Ordering::Relaxed);
    }

    pub(crate) fn set_connection(&self, connection: impl Into<String>) {
        *write_lock(&self.inner.connection) = connection.into();
    }

    pub(crate) fn set_decoder(&self, decoder: impl Into<String>) {
        *write_lock(&self.inner.decoder) = decoder.into();
    }

    pub(crate) fn pause_presentation(&self) {
        self.inner
            .presentation_epoch
            .fetch_add(1, Ordering::Relaxed);
        self.inner
            .presentation_started
            .store(false, Ordering::Relaxed);
        self.drain_sample_events();
        mutex_lock(&self.inner.sample_state)
            .latency
            .last_rendered_at = None;
        mutex_lock(&self.inner.snapshot_cache).take();
    }

    pub(crate) fn set_video_codec(&self, codec: impl Into<String>) {
        *write_lock(&self.inner.video_codec) = codec.into();
    }

    pub(crate) fn set_video_format(&self, format: impl Into<String>) {
        *write_lock(&self.inner.video_format) = format.into();
    }

    pub(crate) fn set_active_video_track(&self, index: u64) {
        write_lock(&self.inner.remote_senders).active_track = Some(index);
        mutex_lock(&self.inner.snapshot_cache).take();
    }

    /// Media counters belong to a track; route, quality and sender metadata
    /// belong to the shared connection. The session keeps only weak observers.
    pub(crate) fn for_video_track(&self, index: u64) -> Self {
        let session = self.session.as_ref().unwrap_or(&self.inner);
        let mut tracks = write_lock(&session.tracks);
        if let Some(inner) = tracks.get(&index).and_then(std::sync::Weak::upgrade) {
            return Self {
                inner,
                session: Some(Arc::clone(session)),
            };
        }
        let mut track = Self::new(read_lock(&session.quality).clone());
        track.session = Some(Arc::clone(session));
        track.set_active_video_track(index);
        tracks.insert(index, Arc::downgrade(&track.inner));
        track
    }

    pub(crate) fn video_tracks(&self) -> Vec<Self> {
        if self.session.is_some() {
            return Vec::new();
        }
        read_lock(&self.inner.tracks)
            .values()
            .filter_map(|weak| {
                weak.upgrade().map(|inner| Self {
                    inner,
                    session: Some(Arc::clone(&self.inner)),
                })
            })
            .collect()
    }

    pub(crate) fn set_remote_senders(&self, infos: Vec<RemoteSenderInfo>) {
        {
            let mut remote = write_lock(&self.inner.remote_senders);
            remote
                .tracks
                .extend(infos.into_iter().map(|info| (info.video_track_index, info)));
        }
        mutex_lock(&self.inner.snapshot_cache).take();
    }

    pub(crate) fn set_decoder_queue_frames(&self, frames: usize) {
        let frames = frames as u64;
        self.inner
            .decoder_queue_frames
            .store(frames, Ordering::Relaxed);
        self.inner
            .decoder_queue_peak_frames
            .fetch_max(frames, Ordering::Relaxed);
    }

    pub fn set_quality(&self, quality: impl Into<String>) {
        *write_lock(&self.inner.quality) = quality.into();
    }

    pub(crate) fn begin_stream_switch(&self, sequence: i64, target: impl Into<String>) {
        let target = target.into();
        for track in self.video_tracks() {
            track.begin_stream_switch(sequence, target.clone());
        }
        let decoded_width = self.inner.decoded_width.load(Ordering::Relaxed) as u32;
        let decoded_height = self.inner.decoded_height.load(Ordering::Relaxed) as u32;
        self.drain_sample_events();
        let sample_state = mutex_lock(&self.inner.sample_state);
        let previous_received_at = sample_state.source_receive_cadence.receive.last_at;
        let previous_decoded_at = sample_state.decode_cadence.last_at;
        let previous_rendered_at = sample_state.latency.last_rendered_at;
        drop(sample_state);
        *mutex_lock(&self.inner.stream_switch) = Some(StreamSwitchState {
            sequence,
            target: target.clone(),
            requested_at: Instant::now(),
            acknowledged_at: None,
            continuity_presented_at: None,
            media_changed_at: None,
            presented_at: None,
            finished_at: None,
            previous_received_at,
            previous_decoded_at,
            previous_rendered_at,
            receive_gap: None,
            decode_gap: None,
            presentation_gap: None,
            from_resolution: (decoded_width != 0 && decoded_height != 0)
                .then_some((decoded_width, decoded_height)),
            actual_resolution: None,
            stage: StreamSwitchStage::Requested,
            error: None,
        });
        self.inner
            .stream_switch_active
            .store(true, Ordering::Relaxed);
        tracing::info!(
            sequence,
            %target,
            "runtime stream switch requested"
        );
    }

    pub(crate) fn acknowledge_stream_switch(&self, sequence: i64) {
        for track in self.video_tracks() {
            track.acknowledge_stream_switch(sequence);
        }
        let mut stream_switch = mutex_lock(&self.inner.stream_switch);
        if let Some(switch) = stream_switch.as_mut()
            && switch.sequence == sequence
        {
            switch.acknowledged_at.get_or_insert_with(Instant::now);
            switch.stage = if switch.media_changed_at.is_some() {
                StreamSwitchStage::MediaChanged
            } else {
                StreamSwitchStage::Acknowledged
            };
            tracing::info!(
                sequence,
                elapsed_ms = switch
                    .acknowledged_at
                    .expect("ack timestamp was initialized")
                    .duration_since(switch.requested_at)
                    .as_secs_f64()
                    * 1_000.0,
                "runtime stream switch acknowledged by remote host"
            );
        }
    }

    pub(crate) fn fail_stream_switch(&self, sequence: i64, error: impl Into<String>) {
        let error = error.into();
        for track in self.video_tracks() {
            track.fail_stream_switch(sequence, error.clone());
        }
        let mut stream_switch = mutex_lock(&self.inner.stream_switch);
        if let Some(switch) = stream_switch.as_mut()
            && switch.sequence == sequence
        {
            switch.stage = StreamSwitchStage::Failed;
            switch.error = Some(error);
            switch.finished_at.get_or_insert_with(Instant::now);
            self.inner
                .stream_switch_active
                .store(false, Ordering::Relaxed);
            tracing::warn!(
                sequence,
                error = %switch.error.as_deref().unwrap_or_default(),
                "runtime stream switch failed"
            );
        }
    }

    /// Local UI identity; never sent on the wire or persisted.
    pub(crate) fn history_identity(&self) -> usize {
        Arc::as_ptr(&self.inner) as usize
    }

    pub fn snapshot(&self) -> Arc<PerformanceSnapshot> {
        if self.session.is_none() {
            let selected = read_lock(&self.inner.remote_senders).active_track;
            let tracks = read_lock(&self.inner.tracks);
            let selected = selected
                .and_then(|id| tracks.get(&id))
                .and_then(std::sync::Weak::upgrade);
            drop(tracks);
            if let Some(inner) = selected {
                return Self {
                    inner,
                    session: Some(Arc::clone(&self.inner)),
                }
                .snapshot();
            }
        }
        let now = Instant::now();
        let mut cache = mutex_lock(&self.inner.snapshot_cache);
        if let Some((sampled_at, snapshot)) = cache.as_ref()
            && now.duration_since(*sampled_at) < SNAPSHOT_CACHE_TTL
        {
            return Arc::clone(snapshot);
        }
        let snapshot = Arc::new(self.build_snapshot());
        *cache = Some((now, Arc::clone(&snapshot)));
        snapshot
    }

    fn build_snapshot(&self) -> PerformanceSnapshot {
        let common = self.session.as_ref().unwrap_or(&self.inner);
        self.drain_sample_events();
        self.refresh_rates();
        let dropped_present_frames = self.inner.dropped_present_frames.load(Ordering::Relaxed);
        let rendered_frames = self.inner.rendered_frames.load(Ordering::Relaxed);
        let presentation_total = rendered_frames.saturating_add(dropped_present_frames);
        let presentation_drop_percent = if presentation_total == 0 {
            0.0
        } else {
            dropped_present_frames as f64 * 100.0 / presentation_total as f64
        };
        let sample_state = {
            let mut state = mutex_lock(&self.inner.sample_state);
            state.frame_period.advance(Instant::now());
            state.clone()
        };
        let measured_rtt = self.inner.measured_media_rtt_micros.load(Ordering::Relaxed);
        let frame_delay_ms = sample_state
            .frame_period
            .published
            .filter(|_| measured_rtt != u64::MAX)
            .map(|(processing, sending, _)| processing + sending + measured_rtt / 1000);
        let frame_delay_buffer_ms = sample_state
            .frame_period
            .published
            .map_or(0, |(_, _, hold)| hold);
        let latency = sample_state.latency;
        let latency_count = latency.local_samples_micros.len();
        let local_average_ms = if latency_count == 0 {
            0.0
        } else {
            latency.local_sum_micros as f64 / latency_count as f64 / 1000.0
        };
        let mut sorted_latency = latency
            .local_samples_micros
            .iter()
            .copied()
            .collect::<Vec<_>>();
        sorted_latency.sort_unstable();
        let local_p95_ms = percentile_millis(&sorted_latency, 95);
        let local_max_ms = sorted_latency.last().copied().map_or(0.0, micros_to_millis);
        let small_jank_count = latency.small_jank_count;
        let jank_count = latency.jank_count;
        let big_jank_count = latency.big_jank_count;
        let render_cadence = cadence_metrics(
            &latency.frame_interval_samples_micros,
            latency.frame_interval_sum_micros,
        );
        let cadence = sample_state.source_receive_cadence;
        let decode = sample_state.decode_cadence;
        let source_cadence = cadence.source.metrics();
        let receive_cadence = cadence.receive.metrics();
        let decode_cadence = decode.metrics();
        let rates = *mutex_lock(&self.inner.rates);
        let decoded_width = self.inner.decoded_width.load(Ordering::Relaxed);
        let decoded_height = self.inner.decoded_height.load(Ordering::Relaxed);
        let quality = read_lock(&common.quality).clone();
        let pipeline_stats = sample_state.pipeline_latency.statistics(
            (source_cadence.average_ms > 0.0).then(|| 1_000.0 / source_cadence.average_ms),
            rates.receive_fps,
        );
        let (video_track_index, remote_capture, remote_encoder) = {
            let active_track = read_lock(&self.inner.remote_senders).active_track;
            let remote = read_lock(&common.remote_senders);
            let info = active_track.and_then(|index| remote.tracks.get(&index));
            let capture = info.map_or_else(
                || crate::i18n::t().session.perf_waiting_remote.to_owned(),
                |info| {
                    if info.capture_impl.is_empty() {
                        crate::i18n::t().session.perf_not_provided.to_owned()
                    } else {
                        info.capture_impl.clone()
                    }
                },
            );
            let encoder = info.map_or_else(
                || crate::i18n::t().session.perf_waiting_remote.to_owned(),
                |info| {
                    (crate::i18n::t().session.perf_encoder_track)(
                        if info.encoder_impl.is_empty() {
                            crate::i18n::t().session.perf_not_provided
                        } else {
                            &info.encoder_impl
                        },
                        &info.video_track_index.to_string(),
                    )
                },
            );
            (active_track, capture, encoder)
        };
        let now = Instant::now();
        let received_at = cadence.receive.last_at;
        let rendered_at = latency.last_rendered_at;
        let rtt_at = common.current_delay_observed_micros.load(Ordering::Relaxed);
        let rtt_at =
            (rtt_at != u64::MAX).then(|| common.started_at + Duration::from_micros(rtt_at));
        let packet_at = mutex_lock(&self.inner.rtp_timing).last_arrival;
        let media_rtt_at = self
            .inner
            .measured_media_rtt_observed_micros
            .load(Ordering::Relaxed);
        let media_rtt_fresh = media_rtt_at != u64::MAX
            && self
                .inner
                .started_at
                .elapsed()
                .saturating_sub(Duration::from_micros(media_rtt_at))
                <= Duration::from_secs(5);
        let mut snapshot = PerformanceSnapshot {
            health: health::Assessment::default(),
            connection: read_lock(&common.connection).clone(),
            decoder: read_lock(&self.inner.decoder).clone(),
            video_codec: read_lock(&self.inner.video_codec).clone(),
            video_format: read_lock(&self.inner.video_format).clone(),
            remote_capture,
            remote_encoder,
            video_track_index,
            quality,
            bitrate_mbps: rates.bitrate_mbps,
            receive_fps: rates.receive_fps,
            decode_fps: rates.decode_fps,
            render_fps: rates.render_fps,
            actual_fps: rates.actual_fps,
            frame_delay_ms,
            frame_delay_buffer_ms,
            current_delay_ms: optional_millis(common.current_delay_micros.load(Ordering::Relaxed)),
            packet_loss_percent: rates.packet_loss_percent,
            unrepaired_loss_percent: rates.unrepaired_loss_percent,
            lost_packets: self
                .inner
                .expected_packets
                .load(Ordering::Relaxed)
                .saturating_sub(self.inner.arrived_packets.load(Ordering::Relaxed)),
            unrepaired_lost_packets: self.inner.final_lost_packets.load(Ordering::Relaxed),
            rtp_jitter_ms: micros_to_millis(self.inner.rtp_jitter_micros.load(Ordering::Relaxed)),
            target_playout_delay_ms: micros_to_millis(
                self.inner
                    .target_playout_delay_micros
                    .load(Ordering::Relaxed),
            ),
            jitter_playout_delay_ms: micros_to_millis(
                self.inner
                    .jitter_playout_delay_micros
                    .load(Ordering::Relaxed),
            ),
            low_latency_playout: self.inner.low_latency_playout.load(Ordering::Relaxed) != 0,
            ingress_queue_packets: self.inner.ingress_queue_packets.load(Ordering::Relaxed),
            ingress_queue_peak_packets: self
                .inner
                .ingress_queue_peak_packets
                .load(Ordering::Relaxed),
            outstanding_nacks: self.inner.outstanding_nacks.load(Ordering::Relaxed),
            frame_buffer_frames: self.inner.frame_buffer_frames.load(Ordering::Relaxed),
            frame_buffer_peak_frames: self.inner.frame_buffer_peak_frames.load(Ordering::Relaxed),
            rtx_packets_received: self.inner.rtx_packets_received.load(Ordering::Relaxed),
            rtx_packets_accepted: self.inner.rtx_packets_accepted.load(Ordering::Relaxed),
            fec_packets_received: self.inner.fec_packets_received.load(Ordering::Relaxed),
            fec_packets_recovered: self.inner.fec_packets_recovered.load(Ordering::Relaxed),
            predecode_dropped_frames: self.inner.predecode_dropped_frames.load(Ordering::Relaxed),
            assembly_delay_ms: micros_to_millis(
                self.inner.assembly_delay_micros.load(Ordering::Relaxed),
            ),
            playout_hold_ms: micros_to_millis(
                self.inner.playout_hold_micros.load(Ordering::Relaxed),
            ),
            local_frame_delay_ms: micros_to_millis(
                self.inner.local_frame_delay_micros.load(Ordering::Relaxed),
            ),
            local_frame_delay_average_ms: local_average_ms,
            local_frame_delay_p95_ms: local_p95_ms,
            local_frame_delay_max_ms: local_max_ms,
            source_cadence,
            receive_cadence,
            decode_cadence,
            render_cadence,
            input_queue_delay_ms: micros_to_millis(
                self.inner.input_queue_delay_micros.load(Ordering::Relaxed),
            ),
            decode_pipeline_delay_ms: micros_to_millis(
                self.inner
                    .decode_pipeline_delay_micros
                    .load(Ordering::Relaxed),
            ),
            surface_transfer_delay_ms: micros_to_millis(
                self.inner
                    .surface_transfer_delay_micros
                    .load(Ordering::Relaxed),
            ),
            present_wait_delay_ms: micros_to_millis(
                self.inner.present_wait_delay_micros.load(Ordering::Relaxed),
            ),
            render_queue_delay_ms: micros_to_millis(
                self.inner.render_queue_delay_micros.load(Ordering::Relaxed),
            ),
            decoder_queue_frames: self.inner.decoder_queue_frames.load(Ordering::Relaxed),
            decoder_queue_peak_frames: self.inner.decoder_queue_peak_frames.load(Ordering::Relaxed),
            dropped_present_frames,
            presentation_drop_percent,
            presentation_queue_frames: self.inner.presentation_queue_frames.load(Ordering::Relaxed),
            presentation_queue_peak_frames: self
                .inner
                .presentation_queue_peak_frames
                .load(Ordering::Relaxed),
            small_jank_count,
            jank_count,
            big_jank_count,
            total_received_frames: self.inner.received_frames.load(Ordering::Relaxed),
            total_received_rtp_bytes: self.inner.received_bytes.load(Ordering::Relaxed),
            total_decoded_frames: self.inner.decoded_frames.load(Ordering::Relaxed),
            total_key_frames_decoded: self.inner.key_frames_decoded.load(Ordering::Relaxed),
            total_rendered_frames: self.inner.rendered_frames.load(Ordering::Relaxed),
            total_actual_rendered_frames: self.inner.actual_rendered_frames.load(Ordering::Relaxed),
            total_marked_rendered_frames: self.inner.marked_rendered_frames.load(Ordering::Relaxed),
            decoded_resolution: (decoded_width != 0 && decoded_height != 0)
                .then_some((decoded_width as u32, decoded_height as u32)),
            pipeline_stats,
            stream_switch: stream_switch_snapshot(&self.inner.stream_switch),
            uptime: self.inner.started_at.elapsed(),
        };
        use health::Observation;
        let fresh = Duration::from_secs(1);
        let rate_fresh = Duration::from_millis(1500);
        let stall = health::presentation_stall(
            now,
            rendered_at,
            received_at,
            self.inner.presentation_started.load(Ordering::Relaxed),
        );
        snapshot.health = mutex_lock(&self.inner.health).update(
            now,
            self.inner.presentation_epoch.load(Ordering::Relaxed),
            [
                Observation::new(
                    snapshot.packet_loss_percent,
                    packet_at.map(|at| at.min(rates.sampled_at)),
                    rate_fresh,
                ),
                snapshot
                    .current_delay_ms
                    .and_then(|v| Observation::new(v, rtt_at, Duration::from_secs(5))),
                Observation::new(snapshot.rtp_jitter_ms, packet_at, fresh),
                snapshot
                    .frame_delay_ms
                    .filter(|_| media_rtt_fresh)
                    .and_then(|v| {
                        Observation::new(
                            v as f64,
                            received_at.map(|at| at.min(rates.sampled_at)),
                            rate_fresh,
                        )
                    })
                    // The cinema playout buffer is delay chosen by the viewer.
                    .map(|o| o.allowing(snapshot.frame_delay_buffer_ms as f64)),
                Observation::new(snapshot.local_frame_delay_ms, rendered_at, fresh)
                    .map(|o| o.allowing(snapshot.playout_hold_ms)),
                stall,
            ],
            snapshot.total_rendered_frames,
            snapshot.dropped_present_frames,
        );
        snapshot
    }

    fn refresh_rates(&self) {
        let now = Instant::now();
        let mut rates = mutex_lock(&self.inner.rates);
        let elapsed = now.duration_since(rates.sampled_at);
        // Receive/render RateStatistics use a 1000 ms window. This is
        // only measurement cadence; it never delays or paces media frames.
        if elapsed < Duration::from_secs(1) {
            return;
        }
        let seconds = elapsed.as_secs_f64();
        let received_bytes = self.inner.received_bytes.load(Ordering::Relaxed);
        let expected_packets = self.inner.expected_packets.load(Ordering::Relaxed);
        let arrived_packets = self.inner.arrived_packets.load(Ordering::Relaxed);
        let final_lost_packets = self.inner.final_lost_packets.load(Ordering::Relaxed);
        let received_frames = self.inner.received_frames.load(Ordering::Relaxed);
        let decoded_frames = self.inner.decoded_frames.load(Ordering::Relaxed);
        let rendered_frames = self.inner.rendered_frames.load(Ordering::Relaxed);
        let actual_rendered_frames = self.inner.actual_rendered_frames.load(Ordering::Relaxed);
        rates.bitrate_mbps = received_bytes.saturating_sub(rates.received_bytes) as f64 * 8.0
            / seconds
            / 1_000_000.0;
        rates.receive_fps = received_frames.saturating_sub(rates.received_frames) as f64 / seconds;
        rates.decode_fps = decoded_frames.saturating_sub(rates.decoded_frames) as f64 / seconds;
        rates.render_fps = rendered_frames.saturating_sub(rates.rendered_frames) as f64 / seconds;
        rates.actual_fps =
            actual_rendered_frames.saturating_sub(rates.actual_rendered_frames) as f64 / seconds;
        // Late and repeated packets can arrive beyond those expected.
        let expected_delta = expected_packets.saturating_sub(rates.expected_packets);
        let lost_delta =
            expected_delta.saturating_sub(arrived_packets.saturating_sub(rates.arrived_packets));
        let unrepaired_delta = final_lost_packets.saturating_sub(rates.final_lost_packets);
        let percent = |lost: u64| match expected_delta {
            0 => 0.0,
            expected => lost.min(expected) as f64 * 100.0 / expected as f64,
        };
        rates.packet_loss_percent = percent(lost_delta);
        rates.unrepaired_loss_percent = percent(unrepaired_delta);
        rates.sampled_at = now;
        rates.received_bytes = received_bytes;
        rates.expected_packets = expected_packets;
        rates.arrived_packets = arrived_packets;
        rates.final_lost_packets = final_lost_packets;
        rates.received_frames = received_frames;
        rates.decoded_frames = decoded_frames;
        rates.rendered_frames = rendered_frames;
        rates.actual_rendered_frames = actual_rendered_frames;
    }
}

impl CadenceWindow {
    fn record(&mut self, at: Instant) {
        if let Some(previous) = self.last_at
            && let Some(interval) = at.checked_duration_since(previous)
        {
            self.record_interval(interval);
        }
        self.last_at = Some(at);
    }

    fn record_interval(&mut self, interval: Duration) {
        let interval = duration_micros(interval);
        self.samples_micros.push_back(interval);
        self.sum_micros = self.sum_micros.saturating_add(u128::from(interval));
        if self.samples_micros.len() > LATENCY_SAMPLE_WINDOW
            && let Some(removed) = self.samples_micros.pop_front()
        {
            self.sum_micros = self.sum_micros.saturating_sub(u128::from(removed));
        }
    }

    fn metrics(&self) -> CadenceMetrics {
        cadence_metrics(&self.samples_micros, self.sum_micros)
    }
}

fn cadence_metrics(samples: &VecDeque<u64>, sum_micros: u128) -> CadenceMetrics {
    if samples.is_empty() {
        return CadenceMetrics::default();
    }
    let mut sorted = samples.iter().copied().collect::<Vec<_>>();
    sorted.sort_unstable();
    CadenceMetrics {
        average_ms: sum_micros as f64 / samples.len() as f64 / 1000.0,
        p95_ms: percentile_millis(&sorted, 95),
        max_ms: sorted.last().copied().map_or(0.0, micros_to_millis),
    }
}

fn duration_micros(value: Duration) -> u64 {
    value.as_micros().min(u128::from(u64::MAX)) as u64
}

fn micros_to_millis(value: u64) -> f64 {
    value as f64 / 1000.0
}

fn optional_millis(value: u64) -> Option<f64> {
    (value != u64::MAX).then(|| micros_to_millis(value))
}

fn stream_switch_snapshot(
    state: &Mutex<Option<StreamSwitchState>>,
) -> Option<StreamSwitchSnapshot> {
    let state = mutex_lock(state);
    let state = state.as_ref()?;
    let now = Instant::now();
    let elapsed_ms = |instant: Option<Instant>| {
        instant.map(|instant| instant.duration_since(state.requested_at).as_secs_f64() * 1_000.0)
    };
    Some(StreamSwitchSnapshot {
        sequence: state.sequence,
        target: state.target.clone(),
        stage: match state.stage {
            StreamSwitchStage::Requested => crate::i18n::t().session.switch_requested,
            StreamSwitchStage::Acknowledged => crate::i18n::t().session.switch_acknowledged,
            StreamSwitchStage::MediaChanged => crate::i18n::t().session.switch_keyframe_decoded,
            StreamSwitchStage::Presented => crate::i18n::t().session.switch_keyframe_shown,
            StreamSwitchStage::Failed => crate::i18n::t().session.switch_failed,
        },
        age_ms: state
            .finished_at
            .unwrap_or(now)
            .duration_since(state.requested_at)
            .as_secs_f64()
            * 1_000.0,
        request_to_ack_ms: elapsed_ms(state.acknowledged_at),
        request_to_continuity_ms: elapsed_ms(state.continuity_presented_at),
        request_to_media_ms: elapsed_ms(state.media_changed_at),
        request_to_present_ms: elapsed_ms(state.presented_at),
        receive_gap_ms: state.receive_gap.map(|value| value.as_secs_f64() * 1_000.0),
        decode_gap_ms: state.decode_gap.map(|value| value.as_secs_f64() * 1_000.0),
        presentation_gap_ms: state
            .presentation_gap
            .map(|value| value.as_secs_f64() * 1_000.0),
        from_resolution: state.from_resolution,
        actual_resolution: state.actual_resolution,
        error: state.error.clone(),
    })
}

fn percentile_millis(sorted_micros: &[u64], percentile: usize) -> f64 {
    if sorted_micros.is_empty() {
        return 0.0;
    }
    let index = ((sorted_micros.len() - 1) * percentile).div_ceil(100);
    micros_to_millis(sorted_micros[index.min(sorted_micros.len() - 1)])
}

fn mutex_lock<T>(lock: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    lock.lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn read_lock<T>(lock: &RwLock<T>) -> std::sync::RwLockReadGuard<'_, T> {
    lock.read()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn write_lock<T>(lock: &RwLock<T>) -> std::sync::RwLockWriteGuard<'_, T> {
    lock.write()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}
