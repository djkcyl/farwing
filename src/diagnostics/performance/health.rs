//! Shared viewer health policy. Runs with the cached diagnostic snapshot, never
//! on an input, receive, decode or presentation thread. UI modes only render it.
use std::time::{Duration, Instant};

const CONFIRM: Duration = Duration::from_millis(500);
const HOLD: Duration = Duration::from_secs(3);
const MAX_SAMPLE_GAP: Duration = Duration::from_millis(1500);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Metric {
    Loss,
    Rtt,
    Jitter,
    FrameDelay,
    LocalDelay,
    PresentationStall,
}
impl Metric {
    pub(crate) const ALL: [Self; 6] = [
        Self::Loss,
        Self::Rtt,
        Self::Jitter,
        Self::FrameDelay,
        Self::LocalDelay,
        Self::PresentationStall,
    ];
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Loss => "接收丢包",
            Self::Rtt => "网络往返",
            Self::Jitter => "RTP 抖动",
            Self::FrameDelay => "估算帧延迟",
            Self::LocalDelay => "本地单帧",
            Self::PresentationStall => "呈现停滞",
        }
    }
    pub(crate) fn format(self, value: f64) -> String {
        if self == Self::Loss {
            format!("{value:.2}%")
        } else {
            format!("{value:.1} ms")
        }
    }
    fn limits(self) -> (f64, f64) {
        match self {
            Self::Loss => (0.1, 1.0),
            Self::Rtt => (80.0, 150.0),
            Self::Jitter => (10.0, 30.0),
            Self::FrameDelay => (60.0, 100.0),
            Self::LocalDelay => (20.0, 40.0),
            Self::PresentationStall => (180.0, 500.0),
        }
    }
    pub(crate) fn level(self, value: f64) -> Level {
        let (warning, bad) = self.limits();
        if value > bad {
            Level::Bad
        } else if value > warning {
            Level::Warning
        } else {
            Level::Normal
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Level {
    Normal,
    Warning,
    Bad,
}

#[derive(Clone, Debug)]
pub(crate) struct Alert {
    pub metric: Metric,
    pub value: f64,
    /// A held historical observation; never paint it as a current measurement.
    pub historical: bool,
    pub expires_at: Instant,
}

#[derive(Clone, Debug, Default)]
pub(crate) struct Assessment {
    pub values: [Option<f64>; 6],
    pub alerts: Vec<Alert>,
    pub recent_drop_percent: Option<f64>,
}
impl Assessment {
    pub(crate) fn value(&self, metric: Metric) -> Option<f64> {
        self.values[metric as usize]
    }
    pub(crate) fn level(&self, metric: Metric) -> Option<Level> {
        self.value(metric).map(|v| metric.level(v))
    }
}

#[derive(Clone, Copy)]
pub(super) struct Observation {
    pub value: f64,
    pub at: Instant,
    pub ttl: Duration,
}
impl Observation {
    pub(super) fn new(value: f64, at: Option<Instant>, ttl: Duration) -> Option<Self> {
        at.map(|at| Self { value, at, ttl })
    }
}

#[derive(Default)]
struct Slot {
    observed_at: Option<Instant>,
    pending: Option<(Level, Instant)>,
    held: Option<(Level, f64, Instant)>,
}

#[derive(Default)]
pub(super) struct Health {
    epoch: u64,
    updated_at: Option<Instant>,
    slots: [Slot; 6],
    counters: Option<(Instant, u64, u64)>,
    recent_drop: Option<(Instant, f64)>,
}
impl Health {
    pub(super) fn update(
        &mut self,
        now: Instant,
        epoch: u64,
        observations: [Option<Observation>; 6],
        rendered: u64,
        dropped: u64,
    ) -> Assessment {
        if self.epoch != epoch
            || self
                .updated_at
                .is_some_and(|at| now.saturating_duration_since(at) > MAX_SAMPLE_GAP)
        {
            *self = Self {
                epoch,
                ..Self::default()
            };
        }
        self.updated_at = Some(now);
        match self.counters {
            Some((at, r, d)) if rendered >= r && dropped >= d => {
                let elapsed = now.saturating_duration_since(at);
                if elapsed >= Duration::from_secs(1) {
                    let total = rendered - r + dropped - d;
                    self.recent_drop = (elapsed <= MAX_SAMPLE_GAP && total > 0)
                        .then(|| (now, (dropped - d) as f64 * 100.0 / total as f64));
                    self.counters = Some((now, rendered, dropped));
                }
            }
            _ => {
                self.counters = Some((now, rendered, dropped));
                self.recent_drop = None;
            }
        }
        let mut result = Assessment {
            recent_drop_percent: self
                .recent_drop
                .filter(|(at, _)| now.saturating_duration_since(*at) <= MAX_SAMPLE_GAP)
                .map(|(_, value)| value),
            ..Assessment::default()
        };
        for metric in Metric::ALL {
            let index = metric as usize;
            let slot = &mut self.slots[index];
            let observation = observations[index].filter(|o| {
                o.value.is_finite()
                    && o.value >= 0.0
                    && o.at <= now
                    && now.duration_since(o.at) <= o.ttl
            });
            result.values[index] = observation.map(|o| o.value);
            if let Some(o) = observation {
                if slot.observed_at.is_none_or(|at| o.at > at) {
                    if slot
                        .observed_at
                        .is_some_and(|at| o.at.duration_since(at) > o.ttl.max(MAX_SAMPLE_GAP))
                    {
                        slot.pending = None;
                    }
                    slot.observed_at = Some(o.at);
                    let level = metric.level(o.value);
                    if level == Level::Normal {
                        slot.pending = None;
                    } else {
                        let since = match slot.pending {
                            Some((_, since)) => since,
                            _ => o.at,
                        };
                        slot.pending = Some((level, since));
                        if o.at.duration_since(since) >= CONFIRM {
                            slot.held = Some((level, o.value, o.at));
                        }
                    }
                }
            } else {
                slot.pending = None;
            }
            if let Some((level, value, at)) = slot.held {
                if now.saturating_duration_since(at) < HOLD {
                    result.alerts.push(Alert {
                        metric,
                        value,
                        historical: observation.is_none_or(|o| metric.level(o.value) != level),
                        expires_at: at + HOLD,
                    });
                } else {
                    slot.held = None;
                }
            }
        }
        // RTT already contributes to frm. Avoid reporting the same network delay twice.
        if result
            .alerts
            .iter()
            .any(|a| a.metric == Metric::Rtt && !a.historical)
        {
            let rtt = result.value(Metric::Rtt).unwrap_or_default();
            result
                .alerts
                .retain(|a| a.metric != Metric::FrameDelay || a.value > rtt + 20.0);
        }
        // A progressing stream with no presentation supersedes its last-frame latency.
        if result
            .alerts
            .iter()
            .any(|a| a.metric == Metric::PresentationStall && !a.historical)
        {
            result.alerts.retain(|a| a.metric != Metric::LocalDelay);
        }
        result
    }
}
