//! Diagnostic loss from primary-SSRC reception reports, before RTX/FEC recovery.
//! Keep report counters separate from the repair queue and from UI sampling.
use super::health::Observation;
use std::time::{Duration, Instant};

const FRESH: Duration = Duration::from_secs(3);

#[derive(Default)]
pub(super) struct ReceiveLoss {
    previous: Option<(u32, u32, Instant)>,
    sample: Option<Observation>,
    reset_at: Option<Instant>,
}

impl ReceiveLoss {
    pub(super) fn record(&mut self, highest: u32, lost: u32, at: Instant) {
        if self.reset_at.is_some_and(|reset| at <= reset)
            || self.previous.is_some_and(|(_, _, previous)| at <= previous)
        {
            return;
        }
        let previous = self.previous.replace((highest, lost, at));
        let Some((previous_highest, previous_lost, _)) = previous else {
            // The first sequence number is random, not a packet count.
            return;
        };
        let expected = highest.wrapping_sub(previous_highest);
        if expected > i32::MAX as u32 {
            // A restarted stream is a new baseline, not billions of missing packets.
            self.sample = None;
        } else if expected != 0 {
            // Late originals can lower cumulative loss. RTX has its own SSRC and
            // reconstructed FEC packets never enter these reception counters.
            let missing = lost.saturating_sub(previous_lost).min(expected);
            self.sample = Observation::new(
                f64::from(missing) * 100.0 / f64::from(expected),
                Some(at),
                FRESH,
            );
        }
        // An RR without new sequence progress is not a fresh zero-loss sample.
    }

    pub(super) fn observation(&self, now: Instant) -> Option<Observation> {
        self.sample
            .filter(|sample| now.saturating_duration_since(sample.at) <= FRESH)
    }

    pub(super) fn reset(&mut self, at: Instant) {
        *self = Self {
            reset_at: Some(at),
            ..Self::default()
        };
    }
}
