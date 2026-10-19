//! Caps/Num/Scroll Lock reconciliation (`LockState`, §7). Lock keys travel as
//! ordinary keys; a `LockState` corrects the host only for a difference that
//! its own forwarded presses do not explain.
use crate::platform::windows::input::keyboard::{LOCKS, lock_index};
use std::time::{Duration, Instant};

/// A lock key injected this recently may not be visible in the key state yet.
pub(super) const SETTLE: Duration = Duration::from_millis(50);
/// A correction waiting for a held lock key is dropped after this long.
pub(super) const EXPIRE: Duration = Duration::from_secs(2);

/// Lock-key presses received in one participant's stream. A `LockState`
/// carries the number of presses its sender had sent before it.
#[derive(Default)]
pub(super) struct Presses(u32);
impl Presses {
    pub fn key(&mut self, usage: u32, down: bool, repeat: bool) {
        if down && !repeat && lock_index(usage).is_some() {
            self.0 = self.0.wrapping_add(1);
        }
    }
    /// Whether a `LockState` with `seq` may correct the host. Presses received
    /// here that the sender had not counted crossed the state: the difference
    /// may be theirs, so it is not corrected. Presses the sender counted that
    /// never arrived (a lost frame) do not make the state stale. Either way
    /// the count realigns to the sender's, so one discontinuity costs at most
    /// one correction.
    pub fn aligned(&mut self, seq: u32) -> bool {
        let ahead = std::mem::replace(&mut self.0, seq).wrapping_sub(seq);
        ahead == 0 || ahead >= 1 << 31
    }
}

#[derive(Debug, PartialEq, Eq)]
pub(super) enum Due {
    Wait,
    Now,
    Expired,
}

/// A wanted lock state on the injecting side.
#[derive(Clone, Copy)]
pub(super) struct Target {
    want: [bool; 3],
    since: Instant,
}
impl Target {
    pub fn new(want: [bool; 3], now: Instant) -> Self {
        Self { want, since: now }
    }
    /// A press after the `LockState` toggles the controller and the host alike.
    pub fn pressed(&mut self, usage: u32) {
        if let Some(index) = lock_index(usage) {
            self.want[index] = !self.want[index];
        }
    }
    /// Compare only after this participant's own lock presses have settled.
    pub fn due(&self, now: Instant, last_lock: Option<Instant>, held: bool) -> Due {
        if now.saturating_duration_since(self.since) >= EXPIRE {
            Due::Expired
        } else if held || last_lock.is_some_and(|at| now.saturating_duration_since(at) < SETTLE) {
            Due::Wait
        } else {
            Due::Now
        }
    }
    /// Lock keys to press so that `have` becomes the wanted state.
    pub fn corrections(&self, have: [bool; 3]) -> impl Iterator<Item = u32> + use<> {
        let want = self.want;
        (0..3)
            .filter(move |&i| want[i] != have[i])
            .map(|i| LOCKS[i])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const CAPS: u32 = LOCKS[0];
    const NUM: u32 = LOCKS[1];
    const SCROLL: u32 = LOCKS[2];

    #[test]
    fn presses_tie_lock_state_to_the_stream() {
        let mut presses = Presses::default();
        assert!(presses.aligned(0));
        presses.key(CAPS, true, false);
        presses.key(CAPS, true, true); // auto-repeat is not a press
        presses.key(CAPS, false, false);
        presses.key(0x0007_0004, true, false);
        assert!(presses.aligned(1));
        presses.key(NUM, true, false);
        // The sender had not counted that press yet: not attributable.
        assert!(!presses.aligned(1));
        // Realigned to the sender's count.
        assert!(presses.aligned(1));
        // The sender counted a press that never arrived: still applicable.
        assert!(presses.aligned(2));
        presses.key(SCROLL, true, false);
        assert!(presses.aligned(3));
        // Counts wrap.
        let mut presses = Presses(u32::MAX);
        presses.key(SCROLL, true, false);
        assert!(presses.aligned(0));
        presses.key(SCROLL, true, false);
        assert!(!presses.aligned(0));
        assert!(presses.aligned(5));
    }

    #[test]
    fn corrections_only_for_differences() {
        let now = Instant::now();
        let target = Target::new([true, true, false], now);
        assert_eq!(target.corrections([true, true, false]).count(), 0);
        assert_eq!(
            target.corrections([false, true, true]).collect::<Vec<_>>(),
            [CAPS, SCROLL]
        );
    }

    #[test]
    fn later_presses_are_not_corrected_twice() {
        let now = Instant::now();
        let mut target = Target::new([false, true, false], now);
        // Caps pressed after the snapshot: both sides now have Caps on.
        target.pressed(CAPS);
        target.pressed(0x0007_0004);
        assert_eq!(target.corrections([true, true, false]).count(), 0);
        target.pressed(CAPS);
        assert_eq!(
            target.corrections([true, true, false]).collect::<Vec<_>>(),
            [CAPS]
        );
    }

    #[test]
    fn waits_for_in_flight_presses() {
        let now = Instant::now();
        let target = Target::new([true, false, false], now);
        assert_eq!(target.due(now, None, false), Due::Now);
        assert_eq!(target.due(now, Some(now), false), Due::Wait);
        assert_eq!(target.due(now + SETTLE, Some(now), false), Due::Now);
        assert_eq!(target.due(now + SETTLE, None, true), Due::Wait);
        assert_eq!(target.due(now + EXPIRE, None, true), Due::Expired);
    }
}
