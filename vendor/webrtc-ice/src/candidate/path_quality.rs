//! Measured quality of one candidate pair: connectivity-check loss and RTT
//! over a recent window. The controlling agent leaves a direct path that is
//! still writable but losing checks (or far slower than the relay) for a
//! healthy relay backup, and avoids it for a while so the two do not flap.
use std::collections::VecDeque;
use std::time::Duration;

const SECOND: u64 = 1_000_000_000;
const SAMPLE_INTERVAL: u64 = SECOND;
/// Loss is measured over this window.
const LOSS_WINDOW: u64 = 20 * SECOND;
/// RTT must stay high this long (its minimum is used, so spikes do not count).
const RTT_WINDOW: u64 = 10 * SECOND;
/// Fewer checks than this say nothing about loss.
const MIN_CHECKS: u64 = 6;
const LOSS_LIMIT: f64 = 0.4;
/// The smoothed RTT starts here before any response.
const UNKNOWN_RTT_MS: u32 = 3000;
const AVOID_BASE: u64 = 60 * SECOND;
const AVOID_MAX: u64 = 600 * SECOND;
/// A pair that stays out of trouble this long after its last avoidance
/// starts over at the base avoidance.
const STRIKE_RESET: u64 = 600 * SECOND;

#[derive(Clone, Copy, Debug)]
struct Sample {
    at: u64,
    sent: u64,
    answered: u64,
    rtt_ms: u32,
}

#[derive(Default, Debug)]
pub(crate) struct PathQuality {
    samples: VecDeque<Sample>,
    avoid_until: u64,
    strikes: u32,
}

impl PathQuality {
    /// Record the pair's cumulative check counters (at most once a second).
    pub(crate) fn sample(&mut self, now: u64, sent: u64, answered: u64, rtt_ms: u32) {
        if self
            .samples
            .back()
            .is_some_and(|last| now.saturating_sub(last.at) < SAMPLE_INTERVAL)
        {
            return;
        }
        self.samples.push_back(Sample {
            at: now,
            sent,
            answered,
            rtt_ms,
        });
        while self
            .samples
            .front()
            .is_some_and(|first| now.saturating_sub(first.at) > LOSS_WINDOW + SAMPLE_INTERVAL)
        {
            self.samples.pop_front();
        }
    }

    /// Check loss over the window; `outstanding` checks may still be answered.
    fn loss(&self, now: u64, outstanding: u64) -> Option<f64> {
        let first = self
            .samples
            .iter()
            .find(|s| now.saturating_sub(s.at) <= LOSS_WINDOW)?;
        let last = self.samples.back()?;
        let sent = last.sent.saturating_sub(first.sent);
        let answered = last.answered.saturating_sub(first.answered);
        let due = sent.saturating_sub(outstanding.min(1));
        (due >= MIN_CHECKS).then(|| 1.0 - (answered as f64 / due as f64).min(1.0))
    }

    /// The lowest RTT seen over a full [`RTT_WINDOW`].
    fn sustained_rtt(&self, now: u64) -> Option<u32> {
        let recent = self
            .samples
            .iter()
            .filter(|s| now.saturating_sub(s.at) <= RTT_WINDOW)
            .collect::<Vec<_>>();
        let oldest = recent.first()?;
        if now.saturating_sub(oldest.at) < RTT_WINDOW * 8 / 10 {
            return None;
        }
        recent.iter().map(|s| s.rtt_ms).min()
    }

    /// Why this (selected, direct) path is worse than a healthy relay with
    /// `relay_rtt_ms`, if it is.
    pub(crate) fn degraded(&self, now: u64, outstanding: u64, relay_rtt_ms: u32) -> Option<String> {
        if let Some(loss) = self.loss(now, outstanding).filter(|loss| *loss >= LOSS_LIMIT) {
            return Some(format!("{:.0}% of connectivity checks lost", loss * 100.0));
        }
        if relay_rtt_ms >= UNKNOWN_RTT_MS {
            return None;
        }
        let limit = relay_rtt_ms.saturating_mul(3).max(relay_rtt_ms + 150);
        self.sustained_rtt(now)
            .filter(|rtt| *rtt < UNKNOWN_RTT_MS && *rtt >= limit)
            .map(|rtt| format!("RTT {rtt} ms against {relay_rtt_ms} ms on the relay"))
    }

    /// Avoid this path: 60 s, doubling with each repeat up to 10 min.
    pub(crate) fn avoid(&mut self, now: u64) -> Duration {
        if self.avoid_until != 0 && now.saturating_sub(self.avoid_until) > STRIKE_RESET {
            self.strikes = 0;
        }
        let span = (AVOID_BASE << self.strikes.min(4)).min(AVOID_MAX);
        self.avoid_until = now + span;
        self.strikes += 1;
        self.samples.clear();
        Duration::from_nanos(span)
    }

    pub(crate) fn avoided(&self, now: u64) -> bool {
        now < self.avoid_until
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(quality: &mut PathQuality, seconds: u64, sent_per_s: u64, answered_per_s: u64, rtt: u32) -> u64 {
        let (mut sent, mut answered) = (0, 0);
        for t in 0..=seconds {
            quality.sample(t * SECOND, sent, answered, rtt);
            sent += sent_per_s;
            answered += answered_per_s;
        }
        seconds * SECOND
    }

    #[test]
    fn heavy_check_loss_is_degraded() {
        let mut quality = PathQuality::default();
        let now = run(&mut quality, 20, 2, 1, 30);
        assert!(quality.degraded(now, 1, 40).is_some());
    }

    #[test]
    fn healthy_or_sparse_paths_are_not() {
        let mut quality = PathQuality::default();
        let now = run(&mut quality, 20, 1, 1, 30);
        assert!(quality.degraded(now, 1, 40).is_none());
        // Five checks are too few to judge, whatever their fate.
        let mut sparse = PathQuality::default();
        sparse.sample(0, 0, 0, 30);
        sparse.sample(10 * SECOND, 5, 0, 30);
        assert!(sparse.degraded(10 * SECOND, 0, 40).is_none());
    }

    #[test]
    fn sustained_slow_path_is_degraded_but_a_spike_is_not() {
        let mut slow = PathQuality::default();
        let now = run(&mut slow, 12, 1, 1, 400);
        assert!(slow.degraded(now, 0, 40).is_some());
        let mut spike = PathQuality::default();
        for t in 0..=12 {
            spike.sample(t * SECOND, t, t, if t == 11 { 900 } else { 50 });
        }
        assert!(spike.degraded(12 * SECOND, 0, 40).is_none());
        // Without a relay RTT there is nothing to compare with.
        assert!(slow.degraded(now, 0, UNKNOWN_RTT_MS).is_none());
    }

    #[test]
    fn avoidance_doubles_and_resets() {
        let mut quality = PathQuality::default();
        assert_eq!(quality.avoid(0), Duration::from_secs(60));
        assert!(quality.avoided(30 * SECOND));
        assert!(!quality.avoided(61 * SECOND));
        assert_eq!(quality.avoid(100 * SECOND), Duration::from_secs(120));
        let later = 100 * SECOND + 120 * SECOND + STRIKE_RESET + SECOND;
        assert_eq!(quality.avoid(later), Duration::from_secs(60));
        let mut capped = PathQuality::default();
        for _ in 0..8 {
            capped.avoid(0);
        }
        assert_eq!(capped.avoid(0), Duration::from_secs(600));
    }
}
