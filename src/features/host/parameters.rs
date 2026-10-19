//! Shared streaming budgets for actual output geometry and negotiated format.
//! Quality is a bandwidth ceiling, not a promise of constant traffic or fidelity.
use super::format::{Codec, Format};
use std::{
    collections::VecDeque,
    time::{Duration, Instant},
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Bounds {
    /// Preferred allocation, reduced when the link cannot satisfy all streams.
    pub reservation: u32,
    pub maximum: u32,
    pub initial: u32,
    pub probe: u32,
}
impl Bounds {
    fn video(maximum: u32) -> Self {
        Self {
            reservation: NETWORK_FLOOR.min(maximum),
            maximum,
            initial: (maximum / 5)
                .clamp(NETWORK_FLOOR, STARTUP_CEILING)
                .min(maximum),
            probe: 0,
        }
    }
    pub fn network_maximum(self) -> u32 {
        self.maximum.max(self.probe)
    }
}

pub(crate) const NETWORK_FLOOR: u32 = 300_000;
pub(crate) const STARTUP_CEILING: u32 = 3_000_000;
pub(crate) const MIN_ENCODER_RATE: u32 = 30_000;

// A quality decision threshold, never a network allocation guarantee.
fn downgrade_threshold(maximum: u32) -> u32 {
    (maximum / 5 * 2).clamp(200_000, 15_000_000)
}

// 60 FPS ceilings in Mbps at 720p, 1080p, 1440p and 4K. Each codec has
// independent quality anchors; compression savings are content-dependent.
// 10-bit preserves gradients without a blanket raw-bit-depth surcharge.
fn standard(format: Format, quality: i32, source: (u32, u32), fps: u32) -> Bounds {
    let rates = match (format.codec, quality) {
        (Codec::H264, 1) => [1.5, 1.5, 1.5, 1.5],
        (Codec::H264, 3) => [8.0, 14.0, 24.0, 24.0],
        (Codec::H264, 4) => [12.0, 20.0, 36.0, 60.0],
        (Codec::H264, _) => [5.5, 8.0, 8.0, 8.0],
        (Codec::H265, 1) => [1.3, 1.3, 1.3, 1.3],
        (Codec::H265, 3) => [7.0, 12.0, 22.0, 22.0],
        (Codec::H265, 4) => [10.0, 17.0, 32.0, 48.0],
        (Codec::H265, _) => [4.5, 7.0, 7.0, 7.0],
        (Codec::Av1, 1) => [1.2, 1.2, 1.2, 1.2],
        (Codec::Av1, 3) => [6.0, 11.0, 18.0, 18.0],
        (Codec::Av1, 4) => [8.0, 15.0, 28.0, 42.0],
        (Codec::Av1, _) => [4.0, 6.0, 6.0, 6.0],
    };
    let size = crate::media::geometry::output_size(source.0, source.1, quality);
    let pixels = u64::from(size.0) * u64::from(size.1);
    let anchors = [1280u64 * 720, 1920 * 1080, 2560 * 1440, 3840 * 2160];
    let base = if pixels <= anchors[0] {
        rates[0] * (pixels as f64 / anchors[0] as f64).max(0.25)
    } else {
        (1..anchors.len())
            .find(|&i| pixels <= anchors[i])
            .map_or(rates[3], |i| {
                let fraction =
                    (pixels - anchors[i - 1]) as f64 / (anchors[i] - anchors[i - 1]) as f64;
                rates[i - 1] + (rates[i] - rates[i - 1]) * fraction
            })
    };
    // Like Moonlight's default-rate curve, high FPS grows sublinearly: adjacent
    // frames share more information. No 30/60/90/120 tier discontinuities.
    let ratio = f64::from(fps.clamp(1, 144)) / 60.0;
    let color = if format.chroma == 3 { 2.0 } else { 1.0 };
    let rate = (base * 1_000_000.0 * color * if fps <= 60 { ratio } else { ratio.sqrt() })
        .round()
        .clamp(500_000.0, 500_000_000.0) as u32;
    Bounds::video(rate)
}

pub(crate) fn fixed(
    format: Format,
    quality: i32,
    custom: u32,
    size: (u32, u32),
    fps: u32,
) -> Bounds {
    if quality != 6 {
        return standard(format, quality, size, fps);
    }
    Bounds::video(custom.clamp(1_000_000, 500_000_000))
}

pub(crate) fn automatic(
    format: Format,
    quality: i32,
    source: (u32, u32),
    fps: u32,
    initial: bool,
) -> Bounds {
    let mut result = standard(format, quality, source, fps);
    result.probe = if quality >= 4 {
        result.maximum
    } else {
        let next = standard(format, quality + 1, source, fps);
        if initial {
            next.maximum
        } else {
            (((f64::from(downgrade_threshold(next.maximum)) + f64::from(next.maximum)) * 0.8)
                as u32)
                .min(next.maximum)
        }
    };
    result
}

/// "Auto" picks the quality tier (output size) from the bandwidth estimate
/// at the start of the stream and then keeps it: changing the size rebuilds
/// the encoder and stalls the picture. Later shortfalls are handled by the
/// weak-network strategy (bitrate or frame rate), never by resizing.
pub(crate) struct AutoQuality {
    current: i32,
    maximum: i32,
    startup: i32,
    probes: VecDeque<u32>,
    lower: VecDeque<u32>,
    at: Option<Instant>,
    policy: Option<(Format, (u32, u32), u32)>,
    /// Observations with frames since the policy began.
    observed: u32,
    /// An observation since the policy began reported loss.
    loss_seen: bool,
    settled: bool,
}

/// Observations (about one per second with frames) before the tier settles.
const AUTO_DECISION_SAMPLES: u32 = 8;

/// Observations before the tier may go up: the estimate can reach the next
/// tier within a second through the first probes, before any loss has been
/// measured, and a larger picture on a lossy link has more frames waiting for
/// repair.
const AUTO_UPGRADE_SAMPLES: usize = 3;

#[derive(Clone, Copy)]
pub(crate) struct NetworkSample {
    pub probe: u32,
    pub lower: u32,
    pub loss: f64,
}
impl AutoQuality {
    pub fn new(startup: i32, maximum: i32) -> Self {
        let maximum = maximum.clamp(1, 4);
        let startup = startup.clamp(1, maximum);
        Self {
            current: startup,
            maximum,
            startup,
            probes: VecDeque::with_capacity(15),
            lower: VecDeque::with_capacity(15),
            at: None,
            policy: None,
            observed: 0,
            loss_seen: false,
            settled: false,
        }
    }
    /// The tier is fixed for the rest of the stream.
    pub fn settled(&self) -> bool {
        self.settled
    }
    pub fn quality(&self) -> i32 {
        self.current
    }
    pub fn limit(&mut self, maximum: i32) {
        self.maximum = maximum.clamp(1, 4);
        self.current = self.current.min(self.maximum);
    }
    pub fn observe(
        &mut self,
        now: Instant,
        format: Format,
        source: (u32, u32),
        fps: u32,
        has_frames: bool,
        sample: NetworkSample,
    ) -> Option<i32> {
        // A new source or format is a new decision; a network path change
        // is not (the weak-network strategy covers it).
        let policy = (format, source, fps);
        if self.policy != Some(policy) {
            if self
                .policy
                .is_some_and(|(_, old_source, _)| old_source != source)
            {
                self.settled = false;
                self.observed = 0;
            }
            self.policy = Some(policy);
            self.probes.clear();
            self.lower.clear();
            self.at = None;
            self.loss_seen = false;
        }
        if self.settled {
            return None;
        }
        if self
            .at
            .is_some_and(|at| now.saturating_duration_since(at) < Duration::from_secs(1))
        {
            return None;
        }
        self.at = Some(now);
        if !has_frames {
            return None;
        }
        self.observed += 1;
        if self.observed >= AUTO_DECISION_SAMPLES {
            self.settled = true;
        }
        self.loss_seen |= sample.loss > f64::EPSILON;
        for (window, value) in [
            (&mut self.probes, sample.probe),
            (&mut self.lower, sample.lower),
        ] {
            if window.len() == 15 {
                window.pop_front();
            }
            window.push_back(value);
        }
        if self.startup > 2 && self.probes.len() < 5 {
            return None;
        }
        let mean = |window: &VecDeque<u32>| -> u32 {
            (window.iter().map(|v| u64::from(*v)).sum::<u64>() / window.len() as u64) as u32
        };
        let probe = mean(&self.probes);
        let lower = mean(&self.lower);
        let bounds = standard(format, self.current, source, fps);
        let mut selected = self.current;
        if probe > bounds.maximum
            && !self.loss_seen
            && self.probes.len() >= AUTO_UPGRADE_SAMPLES
            && self.current < self.maximum
        {
            let next = standard(format, self.current + 1, source, fps);
            if f64::from(probe)
                > (f64::from(downgrade_threshold(next.maximum)) + f64::from(next.maximum)) * 0.5
            {
                selected += 1;
            }
        }
        // The lower-limit decision takes precedence over a simultaneous probe.
        if lower != 0 && lower <= downgrade_threshold(bounds.maximum) {
            selected = (self.current - 1).max(2);
        }
        selected = selected.min(self.maximum);
        if selected == self.current {
            None
        } else {
            self.current = selected;
            Some(selected)
        }
    }
}

/// What gives way when the bandwidth estimate falls below the tier's
/// bitrate. The output size never changes mid-stream.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) enum WeakNetwork {
    /// Lower the bitrate first (down to a quarter of the tier's maximum),
    /// then the frame rate.
    #[default]
    Bitrate,
    /// Lower the frame rate first (down to 30 fps) at the same per-frame
    /// quality, then the bitrate.
    FrameRate,
}
impl WeakNetwork {
    pub fn from_wire(value: i32) -> Self {
        if value == 1 {
            Self::FrameRate
        } else {
            Self::Bitrate
        }
    }
}

/// The lowest frame rate the weak-network strategy goes to.
pub(crate) const DEGRADED_FPS: u32 = 30;

/// The bandwidth estimate ramps up at the start of a stream; that is not a
/// weak network.
const DEGRADE_WARMUP: Duration = Duration::from_secs(10);

/// Frame rate under the weak-network strategy, with hysteresis: it drops
/// promptly when bandwidth falls and recovers slowly, in 5-fps steps. It
/// only acts while the encoder is rate-limited: a quiet picture that uses
/// little bandwidth keeps its frame rate.
#[derive(Default)]
pub(crate) struct Degrade {
    fps: Option<u32>,
    changed: Option<Instant>,
    smoothed: Option<f64>,
    started: Option<Instant>,
}
impl Degrade {
    #[cfg(test)]
    fn warmed() -> Self {
        Self {
            started: Instant::now().checked_sub(DEGRADE_WARMUP * 2),
            ..Default::default()
        }
    }
    /// The frame rate to capture and encode at when `available` bits/s can
    /// be sent for a stream whose tier allows `maximum` bits/s at
    /// `requested` fps.
    pub fn fps(
        &mut self,
        now: Instant,
        strategy: WeakNetwork,
        available: u32,
        maximum: u32,
        requested: u32,
        rate_limited: bool,
    ) -> u32 {
        let requested = requested.max(1);
        let started = *self.started.get_or_insert(now);
        let limited = rate_limited && now.saturating_duration_since(started) >= DEGRADE_WARMUP;
        let floor = DEGRADED_FPS.min(requested);
        let smoothed = match self.smoothed {
            Some(previous) => previous * 0.8 + f64::from(available) * 0.2,
            None => f64::from(available),
        };
        self.smoothed = Some(smoothed);
        let maximum = f64::from(maximum.max(1));
        let target = match strategy {
            WeakNetwork::Bitrate => {
                // Keep the full frame rate down to a quarter of the tier's
                // bitrate; below that, spend that quarter on fewer frames.
                let quarter = maximum / 4.0;
                if smoothed >= quarter {
                    f64::from(requested)
                } else {
                    f64::from(requested) * smoothed / quarter
                }
            }
            // Keep each frame's share of the tier bitrate.
            WeakNetwork::FrameRate => smoothed / (maximum / f64::from(requested)),
        };
        // Within half a step of the request counts as the request.
        let target = if !limited || target + 2.5 >= f64::from(requested) {
            requested
        } else {
            ((target as u32).clamp(floor, requested) / 5 * 5).max(floor)
        };
        let current = self.fps.unwrap_or(requested).min(requested);
        let since = self
            .changed
            .map_or(Duration::MAX, |at| now.saturating_duration_since(at));
        let next = if target + 5 <= current && since >= Duration::from_millis(500) {
            target
        } else if target >= current + 10 && since >= Duration::from_secs(3) {
            (current + 10).min(target)
        } else if target == requested && current != requested && since >= Duration::from_secs(3) {
            requested
        } else {
            current
        };
        if next != current {
            self.changed = Some(now);
        }
        self.fps = Some(next);
        next
    }
}

/// Frame-rate cap from the controller's `ReceiverStats`: when its decoder
/// keeps falling behind, frames it would drop are not worth encoding.
#[derive(Default)]
pub(crate) struct ReceiverCap {
    slow: u32,
    healthy: u32,
    cap: u32,
}
impl ReceiverCap {
    /// The cap after one report; 0 = none.
    pub fn report(&mut self, receive_fps: f32, decode_fps: f32, decode_delay_ms: f32) -> u32 {
        if !(receive_fps.is_finite() && decode_fps.is_finite() && decode_delay_ms.is_finite())
            || receive_fps < 5.0
        {
            return self.cap;
        }
        let interval_ms = 1000.0 / receive_fps;
        let slow = decode_fps < receive_fps * 0.85 || decode_delay_ms > interval_ms * 1.5;
        if slow {
            self.slow += 1;
            self.healthy = 0;
            if self.slow >= 3 {
                let cap = ((decode_fps as u32) / 5 * 5).max(DEGRADED_FPS);
                if self.cap == 0 || cap < self.cap {
                    self.cap = cap;
                }
            }
        } else {
            self.slow = 0;
            self.healthy += 1;
            if self.cap != 0 && self.healthy >= 10 && decode_delay_ms < interval_ms * 0.7 {
                self.cap = 0;
            }
        }
        self.cap
    }
}

/// The explicitly enabled congestion-window admission path is separate from
/// the SDK's disabled generic frame dropper.
#[derive(Default)]
pub(crate) struct WindowAdmission {
    counter: u64,
}
impl WindowAdmission {
    pub fn next(&mut self, target: u32, minimum: u32, ratio: f64) -> (u32, bool) {
        if !ratio.is_finite() || ratio <= 0.01 || target <= minimum {
            return (target, false);
        }
        let reduce = (target - minimum).min((f64::from(target) * ratio) as u32);
        if reduce == 0 {
            return (target, false);
        }
        let period = u64::from((target / reduce).max(2));
        let drop = self.counter % period == 0;
        self.counter = self.counter.wrapping_add(1);
        (target - target / period as u32, drop)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bitrate_first_keeps_fps_until_a_quarter_of_the_tier() {
        let now = Instant::now();
        let mut degrade = Degrade::warmed();
        assert_eq!(
            degrade.fps(now, WeakNetwork::Bitrate, 4_000_000, 10_000_000, 60, true),
            60
        );
        let mut degrade = Degrade::warmed();
        assert_eq!(
            degrade.fps(now, WeakNetwork::Bitrate, 1_250_000, 10_000_000, 60, true),
            30
        );
    }

    #[test]
    fn frame_rate_first_scales_fps_with_bandwidth() {
        let now = Instant::now();
        let mut degrade = Degrade::warmed();
        assert_eq!(
            degrade.fps(
                now,
                WeakNetwork::FrameRate,
                5_000_000,
                10_000_000,
                120,
                true
            ),
            60
        );
        let mut degrade = Degrade::warmed();
        assert_eq!(
            degrade.fps(
                now,
                WeakNetwork::FrameRate,
                1_000_000,
                10_000_000,
                120,
                true
            ),
            30
        );
        let mut degrade = Degrade::warmed();
        assert_eq!(
            degrade.fps(
                now,
                WeakNetwork::FrameRate,
                20_000_000,
                10_000_000,
                120,
                true
            ),
            120
        );
    }

    #[test]
    fn recovery_is_gradual() {
        let start = Instant::now();
        let mut degrade = Degrade::warmed();
        assert_eq!(
            degrade.fps(
                start,
                WeakNetwork::FrameRate,
                2_500_000,
                10_000_000,
                120,
                true
            ),
            30
        );
        let next = degrade.fps(
            start + Duration::from_secs(1),
            WeakNetwork::FrameRate,
            10_000_000,
            10_000_000,
            120,
            true,
        );
        assert_eq!(next, 30, "held for 3 s after a change");
        let mut fps = next;
        for second in 4..=30 {
            fps = degrade.fps(
                start + Duration::from_secs(second),
                WeakNetwork::FrameRate,
                10_000_000,
                10_000_000,
                120,
                true,
            );
        }
        assert_eq!(fps, 120);
    }

    #[test]
    fn no_degrade_while_warming_up_or_not_rate_limited() {
        let now = Instant::now();
        let mut degrade = Degrade::default();
        assert_eq!(
            degrade.fps(
                now,
                WeakNetwork::FrameRate,
                1_000_000,
                10_000_000,
                120,
                true
            ),
            120
        );
        let mut degrade = Degrade::warmed();
        assert_eq!(
            degrade.fps(
                now,
                WeakNetwork::FrameRate,
                1_000_000,
                10_000_000,
                120,
                false
            ),
            120
        );
    }

    #[test]
    fn automatic_tier_goes_up_only_after_loss_was_measured() {
        let start = Instant::now();
        let fast = NetworkSample {
            probe: 500_000_000,
            lower: 0,
            loss: 0.0,
        };
        let observe = |auto: &mut AutoQuality, second: u64, sample: NetworkSample| {
            auto.observe(
                start + Duration::from_secs(second),
                Format::AVC,
                (2560, 1600),
                60,
                true,
                sample,
            );
        };
        let mut auto = AutoQuality::new(2, 4);
        observe(&mut auto, 0, fast);
        observe(&mut auto, 1, fast);
        assert_eq!(auto.quality(), 2, "not on the first probes");
        observe(&mut auto, 2, fast);
        assert_eq!(auto.quality(), 3);
        // Loss once seen keeps the tier where it is.
        let mut auto = AutoQuality::new(2, 4);
        observe(&mut auto, 0, fast);
        observe(&mut auto, 1, NetworkSample { loss: 0.02, ..fast });
        for second in 2..8 {
            observe(&mut auto, second, fast);
        }
        assert_eq!(auto.quality(), 2);
    }

    #[test]
    fn automatic_tier_settles() {
        let mut auto = AutoQuality::new(2, 4);
        let start = Instant::now();
        let sample = NetworkSample {
            probe: 500_000_000,
            lower: 0,
            loss: 0.0,
        };
        for second in 0..20 {
            auto.observe(
                start + Duration::from_secs(second),
                Format::AVC,
                (2560, 1600),
                60,
                true,
                sample,
            );
        }
        assert!(auto.settled());
        let settled = auto.quality();
        let starved = NetworkSample {
            probe: 0,
            lower: 100_000,
            loss: 0.2,
        };
        for second in 20..40 {
            auto.observe(
                start + Duration::from_secs(second),
                Format::AVC,
                (2560, 1600),
                60,
                true,
                starved,
            );
        }
        assert_eq!(
            auto.quality(),
            settled,
            "no resize after the startup decision"
        );
    }
}
