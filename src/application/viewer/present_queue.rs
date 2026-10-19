//! Decoded pictures waiting for the render thread.
//!
//! Normally at most one: a newer picture replaces it, for the lowest
//! latency. Catch-up: pictures arriving right after the decoder stalled
//! (repair held them back, and they now decode in a burst) may wait, at most
//! `DEPTH`, and the render thread shows them oldest first on successive
//! refreshes, so motion continues instead of jumping straight to the newest
//! picture. Only where the display refreshes at least 1.5 times as often as
//! the stream (otherwise showing them would only add latency); the window
//! closes after a few frame intervals, and anything still waiting then is
//! dropped, so the extra latency never outlasts the stall that caused it.
//!
//! On in low-latency playback: with repairable loss it shows about three
//! quarters of the pictures that used to be skipped at an unchanged latency
//! p95; every long gap measured was a stall with nothing decoded, the same
//! with and without it. Off in cinema mode, whose playout buffer already
//! paces the pictures (no gain measured there). `FARWING_VIEWER_CATCHUP=1`
//! / `=0` forces it on / off (`docs/selftest-results.md` §19).
//!
//! Also counts how far the content advances between consecutively shown
//! pictures (RTP time), for the session summary.

use super::DecodedVideoFrame;
use std::collections::VecDeque;
use std::time::{Duration, Instant};

/// Waiting pictures hold decoder display surfaces (6–8 per decoder).
const DEPTH: usize = 2;
/// Catch-up stays open this many frame intervals after a stall.
const WINDOW_INTERVALS: u32 = 4;

/// The display must refresh at least this many times per stream frame.
const REFRESH_RATIO: f64 = 1.5;

/// `FARWING_VIEWER_CATCHUP`: `Some(true)` / `Some(false)` forces catch-up
/// on / off; `None` follows the playback mode.
pub(super) fn catch_up_override() -> Option<bool> {
    static ON: std::sync::OnceLock<Option<bool>> = std::sync::OnceLock::new();
    *ON.get_or_init(|| match std::env::var("FARWING_VIEWER_CATCHUP").as_deref() {
        Ok("1") => Some(true),
        Ok("0") => Some(false),
        _ => None,
    })
}

/// Content advance between consecutively shown pictures.
#[derive(Clone, Copy, Debug, Default)]
pub(super) struct ContentSteps {
    last: Option<u32>,
    pub(super) steps: u64,
    pub(super) over_50ms: u64,
    pub(super) over_100ms: u64,
    pub(super) max_ms: f64,
}

impl ContentSteps {
    fn shown(&mut self, rtp_timestamp: u32) {
        if let Some(last) = self.last {
            let step = rtp_timestamp.wrapping_sub(last);
            if step != 0 && step < 0x8000_0000 {
                let ms = f64::from(step) / 90.0;
                self.steps += 1;
                self.over_50ms += u64::from(ms >= 50.0);
                self.over_100ms += u64::from(ms >= 100.0);
                self.max_ms = self.max_ms.max(ms);
            }
        }
        self.last = Some(rtp_timestamp);
    }
}

pub(super) struct PresentQueue {
    frames: VecDeque<DecodedVideoFrame>,
    catch_up: bool,
    last_push: Option<(Instant, u32)>,
    interval: Duration,
    window: Option<Instant>,
    /// Refresh rate of the window's display; catch-up stays off unknown.
    refresh_hz: Option<u32>,
    pub(super) content: ContentSteps,
    /// Pictures decoded so far (diagnostics).
    pub(super) pushed: u64,
}

impl Default for PresentQueue {
    fn default() -> Self {
        Self::new(catch_up_override().unwrap_or(true))
    }
}

impl PresentQueue {
    fn new(catch_up: bool) -> Self {
        Self {
            frames: VecDeque::new(),
            catch_up,
            last_push: None,
            interval: Duration::from_micros(16_667),
            window: None,
            refresh_hz: None,
            content: ContentSteps::default(),
            pushed: 0,
        }
    }

    /// A decoded picture; returns the pictures it replaces (never shown).
    pub(super) fn push(&mut self, frame: DecodedVideoFrame, now: Instant) -> Vec<DecodedVideoFrame> {
        self.pushed += 1;
        if let Some((at, rtp_timestamp)) = self.last_push {
            let step = frame.rtp_timestamp.wrapping_sub(rtp_timestamp);
            let content = Duration::from_micros(u64::from(step) * 1000 / 90);
            // The content follows on (a small step): the picture was held
            // back, not lost, and more held-back pictures follow. A large
            // step (frames lost for good, as before a recovery frame) brings
            // no backlog and says nothing about the frame rate.
            let follows = step > 0 && content <= self.interval * 5 / 2;
            if follows {
                self.interval = (self.interval * 7 + content) / 8;
            }
            let gap = now.saturating_duration_since(at);
            let faster = self
                .refresh_hz
                .is_some_and(|hz| f64::from(hz) * self.interval.as_secs_f64() >= REFRESH_RATIO);
            if self.catch_up
                && faster
                && follows
                && gap >= (self.interval * 3).max(Duration::from_millis(50))
            {
                self.window = Some(now + self.interval * WINDOW_INTERVALS);
            }
        }
        self.last_push = Some((now, frame.rtp_timestamp));
        if self.window.is_some_and(|until| now >= until) {
            self.window = None;
        }
        let mut replaced = Vec::new();
        if self.window.is_some() {
            // Keep the oldest waiting picture (motion continues from it) and
            // the newest.
            if self.frames.len() >= DEPTH {
                replaced.extend(self.frames.pop_back());
            }
        } else {
            replaced.extend(self.frames.drain(..));
        }
        self.frames.push_back(frame);
        replaced
    }

    /// The next picture to show (the oldest waiting).
    pub(super) fn take(&mut self) -> Option<DecodedVideoFrame> {
        self.frames.pop_front()
    }

    /// Put back a picture just taken when it could not be shown yet.
    pub(super) fn restore(&mut self, frame: DecodedVideoFrame) -> Option<DecodedVideoFrame> {
        if self.frames.is_empty() {
            self.frames.push_back(frame);
            None
        } else {
            Some(frame)
        }
    }

    /// Drop everything waiting; returns how many.
    pub(super) fn clear(&mut self) -> usize {
        let count = self.frames.len();
        self.frames.clear();
        count
    }

    pub(super) fn is_empty(&self) -> bool {
        self.frames.is_empty()
    }

    pub(super) fn len(&self) -> usize {
        self.frames.len()
    }

    /// The refresh rate of the display the window is on.
    pub(super) fn set_refresh(&mut self, hz: Option<u32>) {
        self.refresh_hz = hz;
    }

    /// Catch-up on (low-latency playback) or off (cinema mode).
    pub(super) fn set_catch_up(&mut self, on: bool) {
        if self.catch_up != on {
            tracing::debug!(on, "viewer catch-up");
            self.catch_up = on;
            if !on {
                self.window = None;
            }
        }
    }

    /// Whether a catch-up window is open (diagnostics).
    pub(super) fn catching_up(&self, now: Instant) -> bool {
        self.window.is_some_and(|until| now < until)
    }

    /// A picture reached the screen.
    pub(super) fn shown(&mut self, rtp_timestamp: u32) {
        self.content.shown(rtp_timestamp);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(rtp_timestamp: u32) -> DecodedVideoFrame {
        DecodedVideoFrame::test(rtp_timestamp)
    }

    #[test]
    fn without_catch_up_the_newest_replaces() {
        let mut queue = PresentQueue::new(false);
        let now = Instant::now();
        assert!(queue.push(frame(0), now).is_empty());
        // A burst after a stall: each new picture replaces the waiting one.
        assert_eq!(queue.push(frame(1500), now + Duration::from_millis(200)).len(), 1);
        assert_eq!(queue.push(frame(3000), now + Duration::from_millis(202)).len(), 1);
        assert_eq!(queue.len(), 1);
        assert_eq!(queue.take().unwrap().rtp_timestamp, 3000);
    }

    #[test]
    fn after_a_stall_a_burst_waits_briefly_oldest_first() {
        let mut queue = PresentQueue::new(true);
        queue.set_refresh(Some(165));
        let mut now = Instant::now();
        let mut rtp = 0u32;
        // Steady 60 fps: nothing waits.
        for _ in 0..10 {
            assert!(queue.push(frame(rtp), now).len() <= 1);
            rtp += 1500;
            now += Duration::from_micros(16_667);
        }
        assert_eq!(queue.len(), 1);
        queue.take();
        // 120 ms with nothing decoded, then four pictures within 6 ms.
        now += Duration::from_millis(120);
        assert!(queue.push(frame(rtp), now).is_empty());
        assert!(queue.push(frame(rtp + 1500), now + Duration::from_millis(2)).is_empty());
        // Full: the newest waiting one gives way, the oldest stays.
        let replaced = queue.push(frame(rtp + 3000), now + Duration::from_millis(4));
        assert_eq!(replaced[0].rtp_timestamp, rtp + 1500);
        assert_eq!(queue.take().unwrap().rtp_timestamp, rtp);
        assert_eq!(queue.take().unwrap().rtp_timestamp, rtp + 3000);
        // After the window, the newest replaces again.
        assert!(queue.push(frame(rtp + 4500), now + Duration::from_millis(30)).is_empty());
        assert_eq!(queue.push(frame(rtp + 6000), now + Duration::from_millis(75)).len(), 1);
        assert_eq!(queue.len(), 1);
    }

    #[test]
    fn no_catch_up_where_the_display_is_not_faster_than_the_stream() {
        for refresh in [None, Some(60), Some(75)] {
            let mut queue = PresentQueue::new(true);
            queue.set_refresh(refresh);
            let now = Instant::now();
            queue.push(frame(0), now);
            queue.push(frame(1500), now + Duration::from_millis(16));
            queue.take();
            // A stall, then a burst: the newest replaces, as without catch-up.
            assert!(queue.push(frame(3000), now + Duration::from_millis(150)).is_empty());
            assert_eq!(queue.push(frame(4500), now + Duration::from_millis(152)).len(), 1);
        }
    }

    #[test]
    fn no_catch_up_when_the_frames_in_between_were_lost() {
        let mut queue = PresentQueue::new(true);
        queue.set_refresh(Some(165));
        let now = Instant::now();
        queue.push(frame(0), now);
        queue.take();
        // 110 ms later the next picture is 110 ms of content later: nothing
        // waits behind it.
        assert!(queue.push(frame(90 * 110), now + Duration::from_millis(110)).is_empty());
        assert_eq!(queue.push(frame(90 * 110 + 1500), now + Duration::from_millis(112)).len(), 1);
    }

    #[test]
    fn content_steps_count_jumps() {
        let mut queue = PresentQueue::new(false);
        for rtp in [0u32, 1500, 3000, 3000 + 90 * 120, 3000 + 90 * 120 + 1500] {
            queue.shown(rtp);
        }
        let steps = queue.content;
        assert_eq!((steps.steps, steps.over_50ms, steps.over_100ms), (4, 1, 1));
        assert!((steps.max_ms - 120.0).abs() < 0.01);
    }
}
