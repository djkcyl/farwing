//! A recovered frame is not enough to erase a capture backend's failure history.
use std::time::{Duration, Instant};

pub(super) struct Recovery {
    attempts: u8,
    next: Instant,
    dxgi_since: Option<Instant>,
}
impl Recovery {
    pub fn new(dxgi: bool, now: Instant) -> Self {
        Self {
            attempts: 0,
            next: now + Duration::from_secs(10),
            dxgi_since: dxgi.then_some(now),
        }
    }
    pub fn due(&self, now: Instant) -> bool {
        now >= self.next
    }
    pub fn attempt(&mut self, now: Instant) {
        self.attempts = self.attempts.saturating_add(1);
        self.next = now + self.delay();
    }
    pub fn promoted(&mut self, now: Instant) {
        self.dxgi_since = Some(now);
    }
    pub fn healthy(&mut self, now: Instant) {
        if self
            .dxgi_since
            .is_some_and(|at| now.saturating_duration_since(at) >= Duration::from_secs(30))
        {
            self.attempts = 0;
        }
    }
    pub fn failed(&mut self, now: Instant) -> bool {
        let unstable = self.attempts > 0
            && self
                .dxgi_since
                .is_some_and(|at| now.saturating_duration_since(at) < Duration::from_secs(30));
        if unstable {
            self.attempts = self.attempts.max(3);
        }
        self.dxgi_since = None;
        self.next = now + self.delay();
        unstable
    }

    fn delay(&self) -> Duration {
        // Fullscreen/driver transitions can outlive a fixed retry budget without
        // changing screen identity or dimensions. Back off instead of permanently
        // disabling DXGI; a short-lived recovery must not cause a rapid loop.
        Duration::from_secs(match self.attempts {
            0 | 1 => 10,
            2 => 30,
            _ => 60,
        })
    }
}
