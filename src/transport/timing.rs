//! Receive-stream timing. The model belongs to the receiver, not a decoder.

use std::collections::{BTreeMap, VecDeque};
use std::time::{Duration, Instant};

use crate::transport::rtc::PlayoutDelay;

const RENDER_DELAY_US: i64 = 10_000;
const MAX_TIMING_ERROR_US: i64 = 10_000_000;
const MIN_DECODE_PACING_US: i64 = 8_000;
const MAX_DECODE_QUEUE: usize = 8;

#[derive(Clone, Copy, Debug)]
pub(crate) struct PlayoutSchedule {
    /// None is VCMTiming's special zero render timestamp, not "arrival time".
    pub render_at: Option<Instant>,
    pub target_delay: Duration,
    pub jitter_delay: Duration,
    pub low_latency: bool,
}

#[derive(Clone, Copy)]
pub(crate) struct DecodeSchedule {
    pub deadline: Instant,
    render_us: Option<i64>,
}

/// All timestamps inside the timing model use one receive-stream clock.
/// A positive epoch keeps the absolute-zero sentinel distinct from startup.
pub(crate) struct VideoPlayoutTiming {
    origin: Instant,
    extrapolator: TimestampExtrapolator,
    inter_frame_delay: InterFrameDelay,
    jitter: JitterEstimator,
    decode_timer: DecodeTimer,
    min_us: i64,
    max_us: i64,
    jitter_us: i64,
    current_us: i64,
    last_decode_us: i64,
}

impl VideoPlayoutTiming {
    pub(crate) fn new(now: Instant) -> Self {
        Self {
            origin: now,
            extrapolator: TimestampExtrapolator::new(1_000_000),
            inter_frame_delay: InterFrameDelay::default(),
            jitter: JitterEstimator::new(),
            decode_timer: DecodeTimer::default(),
            min_us: 0,
            max_us: 10_000_000,
            jitter_us: 0,
            current_us: 0,
            last_decode_us: 0,
        }
    }

    fn micros(&self, time: Instant) -> i64 {
        1_000_000 + signed_micros(time, self.origin)
    }

    fn instant(&self, micros: i64) -> Instant {
        let delta = micros - 1_000_000;
        if delta >= 0 {
            self.origin + Duration::from_micros(delta as u64)
        } else {
            self.origin - Duration::from_micros(delta.unsigned_abs())
        }
    }

    pub(crate) fn set_playout_delay(&mut self, delay: PlayoutDelay) {
        if self.min_us != duration_micros(delay.min) || self.max_us != duration_micros(delay.max) {
            tracing::debug!(
                min_ms = delay.min.as_millis(),
                max_ms = delay.max.as_millis(),
                "encoded-frame playout delay changed"
            );
        }
        self.min_us = duration_micros(delay.min);
        self.max_us = duration_micros(delay.max);
    }

    /// How long frames wait in the playout buffer (zero at low latency).
    pub(crate) fn playout_delay(&self) -> Duration {
        if self.min_us == 0 && self.max_us <= 500_000 {
            Duration::ZERO
        } else {
            Duration::from_micros(self.current_us.max(self.min_us).min(self.max_us).max(0) as u64)
        }
    }

    pub(crate) fn incoming_timestamp(&mut self, timestamp: u32, last_received_at: Instant) {
        // RTP frame stores the last packet's receive time rounded to milliseconds.
        let received_us = round_ms(self.micros(last_received_at)) * 1000;
        self.extrapolator.update(received_us, timestamp);
    }

    fn render_time(&self, timestamp: u32, now_us: i64) -> Option<i64> {
        if self.min_us == 0 && self.max_us <= 500_000 {
            None
        } else {
            let estimate = self.extrapolator.extrapolate(timestamp).unwrap_or(now_us);
            Some(estimate + self.current_us.max(self.min_us).min(self.max_us))
        }
    }

    fn waiting_time(&self, render_us: Option<i64>, now_us: i64, queued: usize) -> i64 {
        if render_us.is_none() && self.min_us == 0 && self.max_us > 0 {
            if queued > MAX_DECODE_QUEUE {
                0
            } else {
                (self.last_decode_us + MIN_DECODE_PACING_US - now_us).max(0)
            }
        } else {
            // This configuration keeps exclude-decode-time=true. Decode P95 is
            // still measured, but is NOT subtracted in this configured path.
            render_us.unwrap_or(0) - now_us - RENDER_DELAY_US
        }
    }

    pub(crate) fn decode_schedule(
        &self,
        timestamp: u32,
        last_decodable_timestamp: u32,
        queued: usize,
        now: Instant,
        remaining_timeout: Duration,
    ) -> Option<DecodeSchedule> {
        let now_us = self.micros(now);
        let render_us = self.render_time(timestamp, now_us);
        let wait = self.waiting_time(render_us, now_us, queued);
        if timestamp != last_decodable_timestamp && wait <= -5_000 {
            return None;
        }
        let upper = duration_micros(remaining_timeout)
            .saturating_sub(1000)
            .max(0);
        Some(DecodeSchedule {
            deadline: now + Duration::from_micros(wait.min(upper).max(0) as u64),
            render_us,
        })
    }

    pub(crate) fn keyframe_schedule(&self, timestamp: u32, now: Instant) -> DecodeSchedule {
        DecodeSchedule {
            deadline: now,
            render_us: self.render_time(timestamp, self.micros(now)),
        }
    }

    pub(crate) fn frame_released(
        &mut self,
        timestamp: u32,
        last_received_at: Instant,
        frame_size: usize,
        nack_delayed: bool,
        mut schedule: DecodeSchedule,
        now: Instant,
    ) -> PlayoutSchedule {
        let now_us = self.micros(now);
        if schedule.render_us.is_some_and(|render| {
            render < 0 || (render - now_us).unsigned_abs() > MAX_TIMING_ERROR_US as u64
        }) || self.target_us() > MAX_TIMING_ERROR_US
        {
            tracing::warn!(
                timestamp,
                "resetting jitter/timing after invalid render timing"
            );
            self.jitter = JitterEstimator::new();
            self.extrapolator.reset(now_us);
            self.decode_timer = DecodeTimer::default();
            self.min_us = 0;
            self.jitter_us = 0;
            self.current_us = 0;
            // Reset retains max playout, last scheduled decode and
            // InterFrameDelay. A decoder format change does not call this reset.
            schedule.render_us = self.render_time(timestamp, now_us);
        }
        if !nack_delayed {
            let arrival_us = round_ms(self.micros(last_received_at)) * 1000;
            if let Some(delay_us) = self.inter_frame_delay.update(timestamp, arrival_us) {
                self.jitter.update(delay_us, frame_size, now_us);
            }
            self.jitter_us = self.jitter.estimate();
            if self.current_us == 0 {
                self.current_us = self.jitter_us;
            }
            let late_us = now_us - schedule.render_us.unwrap_or(0) + RENDER_DELAY_US;
            if round_ms(late_us) >= 0 {
                self.current_us = (self.current_us + late_us).min(self.target_us());
            }
        }
        // RttMult is disabled in this configuration. NACK frames
        // skip jitter/current-delay updates; they are not fresh network samples.
        self.last_decode_us = now_us;
        PlayoutSchedule {
            render_at: schedule.render_us.map(|time| self.instant(time)),
            target_delay: Duration::from_micros(self.target_us().max(0) as u64),
            jitter_delay: Duration::from_micros(self.jitter_us.max(0) as u64),
            low_latency: self.min_us == 0 && self.max_us <= 500_000,
        }
    }

    fn target_us(&self) -> i64 {
        self.jitter_us + RENDER_DELAY_US
    }

    pub(crate) fn record_decode(&mut self, duration: Duration, finished_at: Instant) {
        self.decode_timer.record(
            round_ms(duration_micros(duration)),
            round_ms(self.micros(finished_at)),
        );
    }

    pub(crate) fn decode_estimate(&self) -> Duration {
        Duration::from_millis(self.decode_timer.samples.percentile(0.95).max(0) as u64)
    }
}

fn duration_micros(duration: Duration) -> i64 {
    duration.as_micros().min(i64::MAX as u128) as i64
}

fn signed_micros(time: Instant, base: Instant) -> i64 {
    if time >= base {
        duration_micros(time.duration_since(base))
    } else {
        -duration_micros(base.duration_since(time))
    }
}

fn round_ms(micros: i64) -> i64 {
    (micros + if micros >= 0 { 500 } else { -500 }) / 1000
}

/// The uint32 unwrapper retains a nonnegative unwrapped value and
/// resolves the half-period tie by comparing the raw timestamps.
#[derive(Default)]
struct TimestampUnwrapper {
    previous: Option<i64>,
}

impl TimestampUnwrapper {
    fn peek(&self, raw: u32) -> i64 {
        let Some(previous) = self.previous else {
            return i64::from(raw);
        };
        let previous_raw = previous as u32;
        let forward = i64::from(raw.wrapping_sub(previous_raw));
        if (forward == 0x8000_0000 && raw > previous_raw)
            || (forward != 0x8000_0000 && (forward as u32 as i32) > 0)
        {
            return previous + forward;
        }
        if raw == previous_raw {
            return previous;
        }
        if previous + forward <= i64::from(u32::MAX) {
            previous + forward
        } else {
            previous + forward - (1_i64 << 32)
        }
    }

    fn unwrap(&mut self, raw: u32) -> i64 {
        let value = self.peek(raw);
        self.previous = Some(value);
        value
    }
}

struct TimestampExtrapolator {
    start_us: i64,
    previous_arrival_us: i64,
    first_timestamp: Option<i64>,
    last_accepted: Option<i64>,
    unwrapper: TimestampUnwrapper,
    theta: [f64; 2],
    covariance: [[f64; 2]; 2],
    count: u32,
    positive: f64,
    negative: f64,
}

impl TimestampExtrapolator {
    fn new(now_us: i64) -> Self {
        let mut value = Self {
            start_us: now_us,
            previous_arrival_us: now_us,
            first_timestamp: None,
            last_accepted: None,
            unwrapper: TimestampUnwrapper::default(),
            theta: [90.0, 0.0],
            covariance: [[1.0, 0.0], [0.0, 1.0e10]],
            count: 0,
            positive: 0.0,
            negative: 0.0,
        };
        value.reset(now_us);
        value
    }

    fn reset(&mut self, now_us: i64) {
        self.start_us = now_us;
        self.previous_arrival_us = now_us;
        self.first_timestamp = None;
        self.unwrapper = TimestampUnwrapper::default();
        self.theta = [90.0, 0.0];
        self.covariance = [[1.0, 0.0], [0.0, 1.0e10]];
        self.count = 0;
        self.positive = 0.0;
        self.negative = 0.0;
        // Deliberately leave last_accepted unchanged.
    }

    fn update(&mut self, now_us: i64, timestamp: u32) {
        if now_us - self.previous_arrival_us > 10_000_000 {
            self.reset(now_us);
        }
        self.previous_arrival_us = now_us;
        let elapsed_ms = round_ms(now_us - self.start_us) as f64;
        let timestamp = self.unwrapper.unwrap(timestamp);
        let first = *self.first_timestamp.get_or_insert_with(|| {
            self.theta[1] = -self.theta[0] * elapsed_ms;
            timestamp
        });
        let residual = timestamp as f64 - first as f64 - self.theta[0] * elapsed_ms - self.theta[1];
        let error = residual.clamp(-7_000.0, 7_000.0);
        self.positive = (self.positive + error - 6_600.0).max(0.0);
        self.negative = (self.negative + error + 6_600.0).min(0.0);
        if self.positive > 60_000.0 || self.negative < -60_000.0 {
            self.positive = 0.0;
            self.negative = 0.0;
            if self.count >= 2 {
                self.covariance[1][1] = 1.0e10;
            }
        }
        if self.last_accepted.is_some_and(|last| timestamp < last) {
            return;
        }
        let p = self.covariance;
        let mut gain = [
            p[0][0] * elapsed_ms + p[0][1],
            p[1][0] * elapsed_ms + p[1][1],
        ];
        let denominator = 1.0 + elapsed_ms * gain[0] + gain[1];
        gain[0] /= denominator;
        gain[1] /= denominator;
        self.theta[0] += gain[0] * residual;
        self.theta[1] += gain[1] * residual;
        for (row, output) in self.covariance.iter_mut().enumerate() {
            for (column, value) in output.iter_mut().enumerate() {
                *value = p[row][column]
                    - (gain[row] * elapsed_ms * p[0][column] + gain[row] * p[1][column]);
            }
        }
        self.last_accepted = Some(timestamp);
        self.count = (self.count + 1).min(2);
    }

    fn extrapolate(&self, timestamp: u32) -> Option<i64> {
        let first = self.first_timestamp?;
        let timestamp = self.unwrapper.peek(timestamp);
        if self.count < 2 {
            let last = self.last_accepted?;
            return Some(
                self.previous_arrival_us + ((timestamp - last) as f64 / 90.0 * 1000.0) as i64,
            );
        }
        if self.theta[0] < 0.001 {
            return Some(self.start_us);
        }
        Some(
            self.start_us
                + 1000
                    * (((timestamp - first) as f64 - self.theta[1]) / self.theta[0] + 0.5) as i64,
        )
    }
}

#[derive(Default)]
struct InterFrameDelay {
    unwrapper: TimestampUnwrapper,
    previous: Option<(i64, i64)>,
}

impl InterFrameDelay {
    fn update(&mut self, timestamp: u32, arrived_us: i64) -> Option<i64> {
        let unwrapped = self.unwrapper.unwrap(timestamp);
        let Some((last, last_arrival)) = self.previous else {
            self.previous = Some((unwrapped, arrived_us));
            return Some(0);
        };
        let raw_delta = timestamp.wrapping_sub(last as u32);
        let ahead = if raw_delta == 0x8000_0000 {
            timestamp > last as u32
        } else {
            raw_delta != 0 && (raw_delta as i32) > 0
        };
        if unwrapped < last || !ahead {
            return None;
        }
        self.previous = Some((unwrapped, arrived_us));
        Some(arrived_us - last_arrival - (unwrapped - last) * 1000 / 90)
    }
}

/// Duplicate-preserving order statistics: no per-frame allocation/sort of samples.
#[derive(Default)]
struct Quantile {
    counts: BTreeMap<i64, usize>,
    len: usize,
}

impl Quantile {
    fn insert(&mut self, value: i64) {
        *self.counts.entry(value).or_default() += 1;
        self.len += 1;
    }

    fn remove(&mut self, value: i64) {
        if let Some(count) = self.counts.get_mut(&value) {
            *count -= 1;
            self.len -= 1;
            if *count == 0 {
                self.counts.remove(&value);
            }
        }
    }

    fn percentile(&self, quantile: f32) -> i64 {
        if self.len == 0 {
            return 0;
        }
        let mut rank = ((self.len - 1) as f32 * quantile) as usize;
        for (&value, &count) in &self.counts {
            if rank < count {
                return value;
            }
            rank -= count;
        }
        unreachable!("quantile counts equal sample count")
    }
}

#[derive(Default)]
struct DecodeTimer {
    ignored: u8,
    samples: Quantile,
    history: VecDeque<(i64, i64)>,
}

impl DecodeTimer {
    fn record(&mut self, duration_ms: i64, finished_ms: i64) {
        if self.ignored < 5 {
            self.ignored += 1;
            return;
        }
        self.samples.insert(duration_ms);
        self.history.push_back((duration_ms, finished_ms));
        while self
            .history
            .front()
            .is_some_and(|(_, then)| finished_ms - then > 10_000)
        {
            if let Some((duration, _)) = self.history.pop_front() {
                self.samples.remove(duration);
            }
        }
    }
}

struct JitterEstimator {
    mean_size: f64,
    size_variance: f64,
    max_size: f64,
    size_sum: f64,
    size_startup: u8,
    size_samples: Quantile,
    size_history: VecDeque<i64>,
    previous_size: Option<i64>,
    theta: [f64; 2],
    covariance: [[f64; 2]; 2],
    noise_mean: f64,
    noise_variance: f64,
    noise_count: u32,
    previous_noise_us: Option<i64>,
    intervals: VecDeque<i64>,
    interval_sum: i64,
    startup: u8,
    filtered_us: i64,
    previous_estimate_us: i64,
}

impl JitterEstimator {
    fn new() -> Self {
        Self {
            mean_size: 500.0,
            size_variance: 100.0,
            max_size: 500.0,
            size_sum: 0.0,
            size_startup: 0,
            size_samples: Quantile::default(),
            size_history: VecDeque::with_capacity(300),
            previous_size: None,
            theta: [1.0 / 64_000.0, 0.0],
            covariance: [[1.0e-4, 0.0], [0.0, 100.0]],
            noise_mean: 0.0,
            noise_variance: 4.0,
            noise_count: 1,
            previous_noise_us: None,
            intervals: VecDeque::with_capacity(30),
            interval_sum: 0,
            startup: 0,
            filtered_us: 0,
            previous_estimate_us: 0,
        }
    }

    fn update(&mut self, delay_us: i64, size: usize, now_us: i64) {
        if size == 0 {
            return;
        }
        let size = size as i64;
        if self.size_startup < 5 {
            self.size_sum += size as f64;
            self.size_startup += 1;
        } else if self.size_startup == 5 {
            self.mean_size = self.size_sum / 5.0;
            self.size_startup = 6;
        }
        let sample = size as f64;
        let candidate = self.mean_size * 0.97 + (1.0 - 0.97) * sample;
        if sample < self.mean_size + 2.0 * self.size_variance.sqrt() {
            self.mean_size = candidate;
        }
        self.size_variance =
            (self.size_variance * 0.97 + (sample - candidate).powi(2) * (1.0 - 0.97)).max(1.0);
        self.max_size = (self.max_size * 0.9999).max(sample);
        self.size_samples.insert(size);
        self.size_history.push_back(size);
        if self.size_history.len() > 300
            && let Some(oldest) = self.size_history.pop_front()
        {
            self.size_samples.remove(oldest);
        }
        let Some(previous_size) = self.previous_size.replace(size) else {
            return;
        };
        let size_delta = (size - previous_size) as f64;
        let bound = ((3.5 * self.noise_variance.sqrt() + 0.5) * 1000.0) as i64;
        let delay_ms = round_ms(delay_us.clamp(-bound, bound)) as f64;
        let residual = delay_ms - (self.theta[0] * size_delta + self.theta[1]);
        let max_size = self.size_samples.percentile(0.95) as f64;
        if residual.abs() >= 15.0 * self.noise_variance.sqrt()
            && sample <= self.mean_size + 3.0 * self.size_variance.sqrt()
        {
            let outlier = if residual >= 0.0 { 15.0 } else { -15.0 };
            self.update_noise(outlier * self.noise_variance.sqrt(), now_us);
        } else {
            self.update_noise(residual, now_us);
            if size_delta > -0.25 * max_size {
                self.update_kalman(delay_ms, size_delta, max_size);
            }
        }
        if self.startup < 30 {
            self.startup += 1;
        } else {
            self.filtered_us = self.calculate();
        }
    }

    fn fps_milli(&self) -> i64 {
        if self.intervals.is_empty() {
            return 0;
        }
        let interval = self.interval_sum / self.intervals.len() as i64;
        if interval <= 0 {
            0
        } else {
            (1_000_000_000 / interval).min(200_000)
        }
    }

    fn update_noise(&mut self, residual: f64, now_us: i64) {
        if let Some(previous) = self.previous_noise_us.replace(now_us) {
            let interval = now_us - previous;
            self.intervals.push_back(interval);
            self.interval_sum += interval;
            if self.intervals.len() > 30
                && let Some(oldest) = self.intervals.pop_front()
            {
                self.interval_sum -= oldest;
            }
        }
        let mut alpha = f64::from(self.noise_count - 1) / f64::from(self.noise_count);
        self.noise_count = (self.noise_count + 1).min(400);
        let fps_milli = self.fps_milli();
        if fps_milli > 0 {
            let mut exponent = 30_000.0 / fps_milli as f64;
            if self.noise_count < 30 {
                exponent = (f64::from(30 - self.noise_count)
                    + exponent * f64::from(self.noise_count))
                    / 30.0;
            }
            alpha = alpha.powf(exponent);
        }
        let variance =
            (1.0 - alpha) * (residual - self.noise_mean).powi(2) + alpha * self.noise_variance;
        self.noise_mean = (1.0 - alpha) * residual + alpha * self.noise_mean;
        self.noise_variance = variance.max(1.0);
    }

    fn update_kalman(&mut self, delay_ms: f64, size_delta: f64, max_size: f64) {
        if max_size < 1.0 || self.noise_variance <= 0.0 {
            return;
        }
        self.covariance[0][0] += 2.5e-10;
        self.covariance[1][1] += 1.0e-10;
        let p = self.covariance;
        let predicted = self.theta[0] * size_delta + self.theta[1];
        let measurement = (self.noise_variance.sqrt()
            * (300.0 * (-size_delta.abs() / max_size).exp() + 1.0))
            .max(1.0);
        let ph = [
            p[0][0] * size_delta + p[0][1],
            p[1][0] * size_delta + p[1][1],
        ];
        let denominator = ph[0] * size_delta + ph[1] + measurement;
        if denominator.abs() < 1.0e-9 {
            return;
        }
        let gain = [ph[0] / denominator, ph[1] / denominator];
        self.theta[0] = (self.theta[0] + gain[0] * (delay_ms - predicted)).max(1.0e-6);
        self.theta[1] += gain[1] * (delay_ms - predicted);
        self.covariance = [
            [
                (1.0 - gain[0] * size_delta) * p[0][0] - gain[0] * p[1][0],
                (1.0 - gain[0] * size_delta) * p[0][1] - gain[0] * p[1][1],
            ],
            [
                (1.0 - gain[1]) * p[1][0] - gain[1] * size_delta * p[0][0],
                (1.0 - gain[1]) * p[1][1] - gain[1] * size_delta * p[0][1],
            ],
        ];
    }

    fn calculate(&mut self) -> i64 {
        let spread =
            (self.size_samples.percentile(0.95) - self.size_samples.percentile(0.5)) as f64;
        let estimate =
            (self.theta[0] * spread + (2.33 * self.noise_variance.sqrt() - 30.0).max(0.0)) * 1000.0;
        let value = estimate as i64;
        if value >= 0 {
            self.previous_estimate_us = value.min(MAX_TIMING_ERROR_US);
        }
        self.previous_estimate_us
    }

    fn estimate(&mut self) -> i64 {
        let estimate = self.calculate().max(self.filtered_us);
        match self.fps_milli() {
            0 => estimate,
            1..5_000 => 0,
            fps @ 5_000..10_000 => {
                (estimate as f64 * ((fps - 5000) as f64 * 0.001 * 0.2)).round() as i64
            }
            _ => estimate,
        }
        .max(0)
    }
}
