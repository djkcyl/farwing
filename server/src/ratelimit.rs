//! In-memory rate limiting.

use parking_lot::Mutex;
use std::collections::{HashMap, VecDeque};
use std::time::{Duration, Instant};

/// Sliding-window event counter keyed by string.
#[derive(Default)]
pub struct WindowLimiter {
    map: Mutex<HashMap<String, VecDeque<Instant>>>,
}

impl WindowLimiter {
    /// `Err(retry_after_ms)` when `key` already has `max` events within `window`.
    pub fn check(&self, key: &str, max: u32, window: Duration) -> Result<(), u64> {
        if max == 0 {
            return Ok(());
        }
        let now = Instant::now();
        let mut map = self.map.lock();
        let Some(q) = map.get_mut(key) else {
            return Ok(());
        };
        while q.front().is_some_and(|t| now.duration_since(*t) >= window) {
            q.pop_front();
        }
        if q.len() as u32 >= max {
            let oldest = *q.front().expect("non-empty");
            let wait = window.saturating_sub(now.duration_since(oldest));
            return Err(wait.as_millis().max(1) as u64);
        }
        Ok(())
    }

    /// Atomically check and record one event. Record attempts before the
    /// expensive or secret-revealing work, so concurrent requests cannot all
    /// pass the check; use [`Self::refund`] for outcomes that reveal nothing.
    pub fn take(&self, key: &str, max: u32, window: Duration) -> Result<(), u64> {
        let now = Instant::now();
        let mut map = self.map.lock();
        let q = map.entry(key.to_owned()).or_default();
        while q.front().is_some_and(|t| now.duration_since(*t) >= window) {
            q.pop_front();
        }
        if max > 0 && q.len() as u32 >= max {
            let oldest = *q.front().expect("non-empty");
            let wait = window.saturating_sub(now.duration_since(oldest));
            return Err(wait.as_millis().max(1) as u64);
        }
        q.push_back(now);
        Ok(())
    }

    /// Return the most recent event recorded by [`Self::take`].
    pub fn refund(&self, key: &str) {
        let mut map = self.map.lock();
        if let Some(q) = map.get_mut(key) {
            q.pop_back();
            if q.is_empty() {
                map.remove(key);
            }
        }
    }

    pub fn clear(&self, key: &str) {
        self.map.lock().remove(key);
    }

    /// Drop entries older than `max_window`.
    pub fn prune(&self, max_window: Duration) {
        let now = Instant::now();
        self.map.lock().retain(|_, q| {
            while q
                .front()
                .is_some_and(|t| now.duration_since(*t) >= max_window)
            {
                q.pop_front();
            }
            !q.is_empty()
        });
    }
}

/// Token bucket used per realtime connection.
pub struct TokenBucket {
    capacity: f64,
    tokens: f64,
    rate: f64,
    last: Instant,
}

impl TokenBucket {
    pub fn new(rate_per_s: u32, burst: u32) -> Self {
        let capacity = burst.max(rate_per_s).max(1) as f64;
        Self {
            capacity,
            tokens: capacity,
            rate: rate_per_s.max(1) as f64,
            last: Instant::now(),
        }
    }

    pub fn try_take(&mut self) -> bool {
        let now = Instant::now();
        let elapsed = now.duration_since(self.last).as_secs_f64();
        self.last = now;
        self.tokens = (self.tokens + elapsed * self.rate).min(self.capacity);
        if self.tokens >= 1.0 {
            self.tokens -= 1.0;
            true
        } else {
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn window() {
        let l = WindowLimiter::default();
        for _ in 0..3 {
            assert!(l.take("k", 3, Duration::from_secs(60)).is_ok());
        }
        assert!(l.take("k", 3, Duration::from_secs(60)).is_err());
        assert!(l.take("other", 3, Duration::from_secs(60)).is_ok());
        l.refund("k");
        assert!(l.take("k", 3, Duration::from_secs(60)).is_ok());
        assert!(l.take("k", 3, Duration::from_secs(60)).is_err());
    }

    #[test]
    fn bucket() {
        let mut b = TokenBucket::new(10, 20);
        let ok = (0..30).filter(|_| b.try_take()).count();
        assert_eq!(ok, 20);
    }
}
