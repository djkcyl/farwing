//! Measured quality of candidate pairs, for the controlling agent's choice
//! between the selected path and its alternative (direct ↔ relay).
//!
//! Each pair keeps a once-a-second series of its check counters, its smoothed
//! RTT and, while selected, the application's media counters (packets
//! expected and arrived on the video stream). A path costs its RTT plus a
//! penalty for the loss that FEC and retransmission no longer hide:
//!
//! ```text
//! cost = RTT + 25 ms × (loss % − 1)      (loss up to 1 % costs nothing)
//! ```
//!
//! so a few percent of loss weighs like a few dozen milliseconds of extra
//! delay, and heavy loss outweighs any detour. The selected path's loss is
//! measured on its media; the alternative carries none and is probed with
//! connectivity checks four times a second. The session moves to a relay
//! that is cheaper by max(30 ms, 25 %), and back to a direct path that is no
//! more expensive than the relay (direct paths spare the server), once the
//! verdict has held for three seconds in a row.
//!
//! Probes understate what a path loses under load (a carrier that throttles
//! a heavy P2P flow drops few small checks), so a path left for its loss is
//! not retried for 60 s, doubling with each repeat up to 10 min. A retry is
//! a trial: the path it came from stays available and is returned to as soon
//! as it is cheaper again.
//!
//! Free of crate dependencies: Farwing's tests include this file.
use std::collections::VecDeque;
use std::time::Duration;

const SECOND: u64 = 1_000_000_000;
const SAMPLE_INTERVAL: u64 = SECOND;
/// Loss and RTT are judged over this window...
const WINDOW: u64 = 10 * SECOND;
/// ...once it spans at least this much.
const MIN_SPAN: u64 = 5 * SECOND;
/// Media loss needs this many packets in the window.
const MIN_MEDIA_PACKETS: u64 = 300;
/// Check loss needs this many checks due in the window.
const MIN_CHECKS: u64 = 8;
/// A path losing this share of its checks over [`ALARM_WINDOW`] is failing
/// whatever its media say (or when it carries none).
const ALARM_LOSS: f64 = 0.4;
const ALARM_WINDOW: u64 = 20 * SECOND;
const ALARM_MIN_CHECKS: u64 = 6;
/// The smoothed RTT starts here before any response.
const UNKNOWN_RTT_MS: u32 = 3000;
/// Loss FEC and retransmission hide.
const FREE_LOSS: f64 = 0.01;
/// What each further percent of loss costs, in milliseconds of round trip.
const LOSS_MS_PER_PERCENT: f64 = 25.0;
/// The alternative must be cheaper by this much...
const MARGIN_MS: f64 = 30.0;
/// ...and by this share of the current cost.
const MARGIN_SHARE: f64 = 0.25;
/// Consecutive one-second verdicts before switching.
pub(crate) const HOLD: u32 = 3;
/// How often the alternative path is probed.
pub(crate) const PROBE_INTERVAL: Duration = Duration::from_millis(250);
/// A retried path is on trial this long.
pub(crate) const TRIAL: Duration = Duration::from_secs(30);
const RETRY_BASE: u64 = 60 * SECOND;
const RETRY_MAX: u64 = 600 * SECOND;
/// A path left again this long after its last departure starts over at
/// [`RETRY_BASE`].
const STRIKE_RESET: u64 = 600 * SECOND;

#[derive(Clone, Copy, Debug)]
struct Sample {
    at: u64,
    sent: u64,
    answered: u64,
    rtt_ms: u32,
    /// Media packets (expected, arrived) and bytes so far, while this path
    /// carried them.
    media: Option<(u64, u64, u64)>,
}

/// A path's recent RTT and loss.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Measure {
    pub rtt_ms: u32,
    /// Share lost; `None` when the window holds too little to judge.
    pub loss: Option<f64>,
    /// The loss was measured on the session's media.
    pub media: bool,
}

impl Measure {
    fn penalty(&self) -> f64 {
        ((self.loss.unwrap_or(0.0) - FREE_LOSS).max(0.0) * 100.0) * LOSS_MS_PER_PERCENT
    }

    pub(crate) fn cost(&self) -> f64 {
        f64::from(self.rtt_ms) + self.penalty()
    }

    /// The path loses enough of its media to cost something.
    pub(crate) fn lossy_under_load(&self) -> bool {
        self.media && self.penalty() > 0.0
    }

    fn describe(&self) -> String {
        match self.loss {
            Some(loss) => format!(
                "RTT {} ms, {:.1}% {} lost",
                self.rtt_ms,
                loss * 100.0,
                if self.media { "media" } else { "checks" }
            ),
            None => format!("RTT {} ms", self.rtt_ms),
        }
    }
}

/// Why `alternative` should carry the session instead of `current`, if it
/// should: clearly cheaper, or no more expensive when it is `preferred` (a
/// direct path against a relay). The alternative's loss must be known.
pub(crate) fn better(current: &Measure, alternative: &Measure, preferred: bool) -> Option<String> {
    alternative.loss?;
    let current_cost = current.cost();
    let alternative_cost = alternative.cost();
    let margin = if preferred {
        0.0
    } else {
        (current_cost * MARGIN_SHARE).max(MARGIN_MS)
    };
    (alternative_cost + margin <= current_cost).then(|| {
        format!(
            "cost {current_cost:.0} ms ({}) against {alternative_cost:.0} ms ({})",
            current.describe(),
            alternative.describe()
        )
    })
}

#[derive(Default, Debug)]
pub(crate) struct PathQuality {
    samples: VecDeque<Sample>,
    retry_after: u64,
    strikes: u32,
    left_at: u64,
}

impl PathQuality {
    /// Record the pair's cumulative counters (at most once a second);
    /// `media` only while the pair carries the session.
    pub(crate) fn sample(
        &mut self,
        now: u64,
        sent: u64,
        answered: u64,
        rtt_ms: u32,
        media: Option<(u64, u64, u64)>,
    ) {
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
            media,
        });
        let keep = WINDOW.max(ALARM_WINDOW) + SAMPLE_INTERVAL;
        while self
            .samples
            .front()
            .is_some_and(|first| now.saturating_sub(first.at) > keep)
        {
            self.samples.pop_front();
        }
    }

    /// Check loss over the samples within `window`; `in_flight` checks may
    /// still be answered.
    fn check_loss(&self, now: u64, window: u64, in_flight: u64, min_checks: u64) -> Option<f64> {
        let first = self
            .samples
            .iter()
            .find(|s| now.saturating_sub(s.at) <= window)?;
        let last = self.samples.back()?;
        let sent = last.sent.saturating_sub(first.sent);
        let answered = last.answered.saturating_sub(first.answered);
        let due = sent.saturating_sub(in_flight);
        (due >= min_checks).then(|| 1.0 - (answered as f64 / due as f64).min(1.0))
    }

    /// The latest unbroken stretch within the window that the path carried
    /// media: its span, and media counters at both ends.
    fn carried(&self, now: u64) -> Option<(u64, (u64, u64, u64), (u64, u64, u64))> {
        let carried = self
            .samples
            .iter()
            .rev()
            .take_while(|s| now.saturating_sub(s.at) <= WINDOW && s.media.is_some())
            .collect::<Vec<_>>();
        let (last, first) = (carried.first()?, carried.last()?);
        Some((last.at.saturating_sub(first.at), first.media?, last.media?))
    }

    /// Media loss over the latest stretch the path carried media.
    fn media_loss(&self, now: u64) -> Option<f64> {
        let (span, first, last) = self.carried(now)?;
        let expected = last.0.saturating_sub(first.0);
        let arrived = last.1.saturating_sub(first.1);
        (span >= MIN_SPAN && expected >= MIN_MEDIA_PACKETS)
            .then(|| 1.0 - (arrived as f64 / expected as f64).min(1.0))
    }

    /// The media bitrate the path carried lately.
    pub(crate) fn media_kbps(&self, now: u64) -> Option<u32> {
        let (span, first, last) = self.carried(now)?;
        (span >= SECOND).then(|| {
            (last.2.saturating_sub(first.2) * 8 * SECOND / span / 1000).min(u64::from(u32::MAX))
                as u32
        })
    }

    /// RTT and loss over the window: loss from media when the path carried
    /// enough, else from checks. `None` until the window says something.
    pub(crate) fn measure(&self, now: u64, in_flight: u64) -> Option<Measure> {
        let recent = self
            .samples
            .iter()
            .filter(|s| now.saturating_sub(s.at) <= WINDOW)
            .collect::<Vec<_>>();
        if now.saturating_sub(recent.first()?.at) < MIN_SPAN {
            return None;
        }
        // The lowest RTT over the window: brief spikes do not count.
        let rtt_ms = recent
            .iter()
            .map(|s| s.rtt_ms)
            .filter(|rtt| *rtt < UNKNOWN_RTT_MS)
            .min()?;
        let (loss, media) = match self.media_loss(now) {
            Some(loss) => (Some(loss), true),
            None => (self.check_loss(now, WINDOW, in_flight, MIN_CHECKS), false),
        };
        // Failing checks override clean (or no) media.
        let alarm = self
            .check_loss(now, ALARM_WINDOW, in_flight, ALARM_MIN_CHECKS)
            .filter(|loss| *loss >= ALARM_LOSS);
        Some(match alarm {
            Some(alarm) if loss.is_none_or(|loss| loss < alarm) => Measure {
                rtt_ms,
                loss: Some(alarm),
                media: false,
            },
            _ => Measure {
                rtt_ms,
                loss,
                media,
            },
        })
    }

    /// The session left this path for losing too much of its media: do not
    /// retry it for 60 s, doubling with each repeat up to 10 min.
    pub(crate) fn left(&mut self, now: u64) -> Duration {
        if self.left_at != 0 && now.saturating_sub(self.left_at) > STRIKE_RESET {
            self.strikes = 0;
        }
        let span = (RETRY_BASE << self.strikes.min(4)).min(RETRY_MAX);
        self.retry_after = now + span;
        self.left_at = now;
        self.strikes += 1;
        Duration::from_nanos(span)
    }

    /// Not to be retried yet.
    pub(crate) fn blocked(&self, now: u64) -> bool {
        now < self.retry_after
    }

    /// The session left this path before (switching to it is a retry).
    pub(crate) fn was_left(&self, now: u64) -> bool {
        self.left_at != 0 && now.saturating_sub(self.left_at) <= STRIKE_RESET
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `seconds` of samples at one per second, from `start`.
    fn feed(
        quality: &mut PathQuality,
        start: u64,
        seconds: u64,
        checks: (u64, u64),
        rtt: u32,
        media: Option<(u64, u64)>,
    ) -> u64 {
        for t in 0..=seconds {
            quality.sample(
                (start + t) * SECOND,
                checks.0 * t,
                checks.1 * t,
                rtt,
                media.map(|(expected, arrived)| (expected * t, arrived * t, 1000 * t)),
            );
        }
        (start + seconds) * SECOND
    }

    fn measure(rtt_ms: u32, loss: f64, media: bool) -> Measure {
        Measure {
            rtt_ms,
            loss: Some(loss),
            media,
        }
    }

    #[test]
    fn media_loss_is_measured_on_the_carried_stretch() {
        let mut quality = PathQuality::default();
        // 1000 packets a second, 150 lost: 15 %.
        let now = feed(&mut quality, 0, 10, (1, 1), 30, Some((1000, 850)));
        let measured = quality.measure(now, 0).unwrap();
        assert!(measured.media);
        assert!((measured.loss.unwrap() - 0.15).abs() < 1e-9);
        assert_eq!(measured.rtt_ms, 30);
        // Once the path stops carrying media, its loss comes from checks.
        assert_eq!(quality.media_kbps(now), Some(8));
        quality.sample(now + SECOND, 11, 11, 30, None);
        let measured = quality.measure(now + SECOND, 0).unwrap();
        assert!(!measured.media);
        assert_eq!(measured.loss, Some(0.0));
    }

    #[test]
    fn too_little_says_nothing() {
        let mut quality = PathQuality::default();
        let now = feed(&mut quality, 0, 3, (4, 4), 30, None);
        assert!(quality.measure(now, 0).is_none(), "window too short");
        let mut sparse = PathQuality::default();
        // A selected path checked every few seconds, no media: RTT only.
        let now = feed(&mut sparse, 0, 10, (0, 0), 40, Some((10, 10)));
        assert_eq!(
            sparse.measure(now, 0),
            Some(Measure {
                rtt_ms: 40,
                loss: None,
                media: false
            })
        );
    }

    #[test]
    fn failing_checks_override_media() {
        let mut quality = PathQuality::default();
        let now = feed(&mut quality, 0, 20, (1, 0), 30, Some((1000, 1000)));
        // RTT 30 stays the last known; checks all lost.
        let measured = quality.measure(now, 0).unwrap();
        assert_eq!(measured.loss, Some(1.0));
        assert!(!measured.media);
    }

    #[test]
    fn throttled_direct_path_loses_to_a_clean_relay() {
        let to_relay = |direct: Measure, relay: Measure| better(&direct, &relay, false);
        // Carrier QoS on P2P: 15 % media loss at 30 ms vs a 60 ms relay.
        assert!(to_relay(measure(30, 0.15, true), measure(60, 0.0, false)).is_some());
        // 4 % at 30 ms vs a 60 ms relay: switch.
        assert!(to_relay(measure(30, 0.04, true), measure(60, 0.0, false)).is_some());
        // 3 % at 30 ms vs a 120 ms relay: stay, the detour costs more.
        assert!(to_relay(measure(30, 0.03, true), measure(120, 0.0, false)).is_none());
        // Up to 1 % is free.
        assert!(to_relay(measure(30, 0.01, true), measure(40, 0.0, false)).is_none());
        // A relay losing as much is no better: the lower delay wins.
        assert!(to_relay(measure(30, 0.15, true), measure(50, 0.20, false)).is_none());
        // A long detour clean against a short relay.
        assert!(to_relay(measure(250, 0.0, true), measure(80, 0.0, false)).is_some());
        // 60 against 40: within the margin.
        assert!(to_relay(measure(60, 0.0, true), measure(40, 0.0, false)).is_none());
    }

    #[test]
    fn a_direct_path_no_worse_than_the_relay_takes_over() {
        let to_direct = |relay: Measure, direct: Measure| better(&relay, &direct, true);
        assert!(to_direct(measure(60, 0.0, true), measure(20, 0.0, false)).is_some());
        assert!(to_direct(measure(30, 0.0, true), measure(30, 0.0, false)).is_some());
        // Slower, or losing more: stay on the relay.
        assert!(to_direct(measure(30, 0.0, true), measure(45, 0.0, false)).is_none());
        assert!(to_direct(measure(60, 0.0, true), measure(20, 0.05, false)).is_none());
        // A relay that loses under load gives way to a slower clean path.
        assert!(to_direct(measure(40, 0.06, true), measure(90, 0.0, false)).is_some());
    }

    #[test]
    fn unknown_loss() {
        let unknown = Measure {
            rtt_ms: 10,
            loss: None,
            media: false,
        };
        // On the alternative: no verdict.
        assert!(better(&measure(300, 0.3, true), &unknown, false).is_none());
        assert!(better(&measure(300, 0.3, true), &unknown, true).is_none());
        // On the current path: counts as none.
        let current = Measure {
            rtt_ms: 30,
            ..unknown
        };
        assert!(better(&current, &measure(60, 0.0, false), false).is_none());
    }

    #[test]
    fn retries_back_off() {
        let mut quality = PathQuality::default();
        assert!(!quality.was_left(0));
        assert_eq!(quality.left(SECOND), Duration::from_secs(60));
        assert!(quality.blocked(30 * SECOND));
        assert!(!quality.blocked(62 * SECOND));
        assert!(quality.was_left(62 * SECOND));
        assert_eq!(quality.left(100 * SECOND), Duration::from_secs(120));
        assert_eq!(quality.left(300 * SECOND), Duration::from_secs(240));
        // Ten quiet minutes start over.
        let later = 300 * SECOND + STRIKE_RESET + SECOND;
        assert!(!quality.was_left(later));
        assert_eq!(quality.left(later), Duration::from_secs(60));
        let mut capped = PathQuality::default();
        for t in 0..8 {
            capped.left(t * SECOND);
        }
        assert_eq!(capped.left(9 * SECOND), Duration::from_secs(600));
    }
}
