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
            Self::Loss => crate::i18n::t().session.health_loss,
            Self::Rtt => crate::i18n::t().session.health_round_trip,
            Self::Jitter => crate::i18n::t().session.health_jitter,
            Self::FrameDelay => crate::i18n::t().session.health_frame_delay,
            Self::LocalDelay => crate::i18n::t().session.health_local_frame,
            Self::PresentationStall => crate::i18n::t().session.health_presentation_stall,
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
            Self::Loss => (1.0, 5.0),
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
    pub level: Level,
    /// A held historical observation; never paint it as a current measurement.
    pub historical: bool,
    pub expires_at: Instant,
}

#[derive(Clone, Debug, Default)]
pub(crate) struct Assessment {
    pub values: [Option<f64>; 6],
    /// Delay the viewer chose to add (the cinema playout buffer), part of
    /// each value but not judged by the limits.
    pub allowances: [f64; 6],
    pub alerts: Vec<Alert>,
    pub recent_drop_percent: Option<f64>,
}
impl Assessment {
    pub(crate) fn value(&self, metric: Metric) -> Option<f64> {
        self.values[metric as usize]
    }
    pub(crate) fn allowance(&self, metric: Metric) -> f64 {
        self.allowances[metric as usize]
    }
    pub(crate) fn level(&self, metric: Metric) -> Option<Level> {
        self.value(metric)
            .map(|v| metric.level((v - self.allowance(metric)).max(0.0)))
    }
}

#[derive(Clone, Copy)]
pub(super) struct Observation {
    pub value: f64,
    pub at: Instant,
    pub ttl: Duration,
    /// Part of `value` added on purpose (see `Assessment::allowances`).
    pub allowance: f64,
}
impl Observation {
    pub(super) fn new(value: f64, at: Option<Instant>, ttl: Duration) -> Option<Self> {
        at.map(|at| Self {
            value,
            at,
            ttl,
            allowance: 0.0,
        })
    }
    pub(super) fn allowing(mut self, allowance: f64) -> Self {
        self.allowance = if allowance.is_finite() {
            allowance.max(0.0)
        } else {
            0.0
        };
        self
    }
    fn level(&self, metric: Metric) -> Level {
        metric.level((self.value - self.allowance).max(0.0))
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
            result.allowances[index] = observation.map_or(0.0, |o| o.allowance);
            if let Some(o) = observation {
                if slot.observed_at.is_none_or(|at| o.at > at) {
                    if slot
                        .observed_at
                        .is_some_and(|at| o.at.duration_since(at) > o.ttl.max(MAX_SAMPLE_GAP))
                    {
                        slot.pending = None;
                    }
                    slot.observed_at = Some(o.at);
                    let level = o.level(metric);
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
                        level,
                        historical: observation.is_none_or(|o| o.level(metric) != level),
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
            let allowance = result.allowance(Metric::FrameDelay);
            result
                .alerts
                .retain(|a| a.metric != Metric::FrameDelay || a.value - allowance > rtt + 20.0);
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

#[cfg(test)]
mod tests {
    use super::*;

    fn frame_delay(health: &mut Health, observation: Observation) -> Assessment {
        let mut observations = [None; 6];
        observations[Metric::FrameDelay as usize] = Some(observation);
        health.update(observation.at, 1, observations, 0, 0)
    }

    #[test]
    fn the_cinema_buffer_is_not_reported_as_delay() {
        let start = Instant::now();
        let ttl = Duration::from_secs(2);
        let (mut cinema, mut slow_cinema, mut low_latency) =
            (Health::default(), Health::default(), Health::default());
        let mut last = Vec::new();
        // Longer than CONFIRM, so a bad level becomes an alert.
        for step in 0..=6 {
            let at = start + Duration::from_millis(step * 100);
            let observe = |value| Observation::new(value, Some(at), ttl).unwrap();
            last = vec![
                frame_delay(&mut cinema, observe(125.0).allowing(115.0)),
                frame_delay(&mut slow_cinema, observe(225.0).allowing(115.0)),
                frame_delay(&mut low_latency, observe(125.0)),
            ];
        }
        let alerted = |assessment: &Assessment| {
            assessment
                .alerts
                .iter()
                .find(|alert| alert.metric == Metric::FrameDelay)
                .map(|alert| alert.level)
        };
        // The value shown still includes the buffer.
        assert_eq!(last[0].value(Metric::FrameDelay), Some(125.0));
        assert_eq!(last[0].level(Metric::FrameDelay), Some(Level::Normal));
        assert_eq!(alerted(&last[0]), None);
        // Delay beyond the buffer is still reported.
        assert_eq!(last[1].level(Metric::FrameDelay), Some(Level::Bad));
        assert_eq!(alerted(&last[1]), Some(Level::Bad));
        assert_eq!(last[2].level(Metric::FrameDelay), Some(Level::Bad));
        assert_eq!(alerted(&last[2]), Some(Level::Bad));
    }
}
