//! Host side of loss recovery with long-term references
//! (`docs/protocol/rfi-ltr-brief.md` §6.6): feedback routing from the
//! control channel to each video track, and the per-encoder decision of
//! which frames to mark and when to send a recovery frame instead of an IDR.

use crate::features::stream_control::wire::VideoFeedback;
use crate::media::encoding::{FrameControl, LtrOutput};
use crate::media::frame_meta::{FrameMeta, flags};
use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|p| p.into_inner())
}

fn newer(id: u16, than: u16) -> bool {
    let distance = id.wrapping_sub(than);
    distance != 0 && distance < 0x8000
}

/// A controller's request for a recovery frame.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Request {
    pub epoch: u8,
    pub decoded: u16,
    /// Long-term references the controller holds, newest first.
    pub held: Vec<u16>,
    pub request: u32,
}

/// What a controller holds, from its latest feedback.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct Held {
    pub epoch: u8,
    pub decoded: u16,
    pub held: Vec<u16>,
}

#[derive(Default)]
struct Pending {
    held: Option<Held>,
    request: Option<Request>,
    rtt: Option<Duration>,
    /// PLI / FIR from this viewer since last taken.
    keyframe_requests: u32,
    /// Request numbers grow per loss event; older ones were answered.
    last_request: Option<u32>,
}

/// A frame sent on a track, for resolving a browser's acknowledgement.
#[derive(Clone, Copy, Debug)]
struct Sent {
    rtp_timestamp: u32,
    epoch: u8,
    frame_id: u16,
    keyframe: bool,
    /// What it depends on: the previous frame, or a recovery frame's
    /// reference.
    reference: u16,
    marked: bool,
}

/// Frames sent on a track, newest last.
#[derive(Default)]
struct SentLog(std::collections::VecDeque<Sent>);

impl SentLog {
    const LIMIT: usize = 2048;

    fn push(&mut self, sent: Sent) {
        self.0.push_back(sent);
        if self.0.len() > Self::LIMIT {
            self.0.pop_front();
        }
    }

    /// What a viewer that decoded the frame sent at `rtp_timestamp` holds:
    /// the long-term references on that frame's dependency chain (a viewer
    /// that decodes by declared dependencies decoded all of them).
    fn resolve(&self, rtp_timestamp: u32) -> Option<Held> {
        let frames = &self.0;
        let newest = frames.iter().rposition(|s| s.rtp_timestamp == rtp_timestamp)?;
        let (epoch, decoded) = (frames[newest].epoch, frames[newest].frame_id);
        let mut held = Vec::new();
        let mut at = newest;
        loop {
            let frame = frames[at];
            if frame.marked {
                held.push(frame.frame_id);
            }
            if frame.keyframe || held.len() == 4 {
                break;
            }
            let wanted = |s: &Sent| s.epoch == epoch && s.frame_id == frame.reference;
            // Usually the frame just before.
            match at.checked_sub(1).filter(|i| wanted(&frames[*i])) {
                Some(previous) => at = previous,
                None => match frames.iter().take(at).rposition(wanted) {
                    Some(previous) => at = previous,
                    None => break,
                },
            }
        }
        Some(Held { epoch, decoded, held })
    }
}

/// Feedback for one video track of one participant.
#[derive(Default)]
pub(crate) struct TrackFeedback {
    pending: Mutex<Pending>,
    sent: Mutex<SentLog>,
    /// The viewer decodes by declared dependencies (Dependency Descriptor):
    /// only then can a browser's acknowledgement be used.
    generic: std::sync::atomic::AtomicBool,
    /// A browser (no `frame-meta`): it gives up on an unrepairable loss only
    /// after seconds, so the host answers a refused repair itself.
    browser: std::sync::atomic::AtomicBool,
}

impl TrackFeedback {
    /// The Dependency Descriptor was negotiated for this track (and its
    /// browser decodes this codec's recovery frames).
    pub(crate) fn set_generic(&self, generic: bool) {
        self.generic.store(generic, std::sync::atomic::Ordering::Relaxed);
    }

    /// The viewer is a browser (`frame-meta` was not negotiated).
    pub(crate) fn set_browser(&self, browser: bool) {
        self.browser.store(browser, std::sync::atomic::Ordering::Relaxed);
    }

    pub(crate) fn browser(&self) -> bool {
        self.browser.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// A frame went out on this track at `rtp_timestamp`.
    pub(crate) fn sent(&self, rtp_timestamp: u32, meta: FrameMeta, keyframe: bool) {
        if !self.generic.load(std::sync::atomic::Ordering::Relaxed) {
            return;
        }
        lock(&self.sent).push(Sent {
            rtp_timestamp,
            epoch: meta.epoch,
            frame_id: meta.frame_id,
            keyframe,
            reference: if meta.recovery() {
                meta.ref_id
            } else {
                meta.frame_id.wrapping_sub(1)
            },
            marked: meta.marks_ltr(),
        });
    }

    pub(crate) fn receive(&self, feedback: &VideoFeedback) {
        if let Some(rtp_timestamp) = feedback.rtp_timestamp {
            if !self.generic.load(std::sync::atomic::Ordering::Relaxed) {
                return;
            }
            let held = lock(&self.sent).resolve(rtp_timestamp);
            match held {
                Some(held) => {
                    tracing::trace!(target: "farwing::recovery", rtp_timestamp, ?held, "browser acknowledgement");
                    lock(&self.pending).held = Some(held);
                }
                None => tracing::debug!(target: "farwing::recovery", rtp_timestamp,
                    "browser acknowledged an unknown frame"),
            }
            return;
        }
        let (Ok(epoch), Ok(decoded)) = (u8::try_from(feedback.epoch), u16::try_from(feedback.decoded))
        else {
            tracing::debug!(target: "farwing::recovery", ?feedback, "invalid video feedback");
            return;
        };
        let held: Vec<u16> = feedback
            .ltr
            .iter()
            .take(4)
            .filter_map(|id| u16::try_from(*id).ok())
            .collect();
        let mut pending = lock(&self.pending);
        if feedback.recover
            && pending
                .last_request
                .is_none_or(|last| (feedback.request.wrapping_sub(last) as i32) > 0)
        {
            pending.last_request = Some(feedback.request);
            pending.request = Some(Request {
                epoch,
                decoded,
                held: held.clone(),
                request: feedback.request,
            });
        }
        pending.held = Some(Held { epoch, decoded, held });
    }

    /// The latest acknowledgement and an unserved request, if any.
    pub(crate) fn take(&self) -> (Option<Held>, Option<Request>) {
        let mut pending = lock(&self.pending);
        (pending.held.clone(), pending.request.take())
    }

    /// A PLI / FIR from this track's viewer.
    pub(crate) fn note_keyframe_request(&self) {
        lock(&self.pending).keyframe_requests += 1;
    }

    pub(crate) fn take_keyframe_requests(&self) -> u32 {
        std::mem::take(&mut lock(&self.pending).keyframe_requests)
    }

    pub(crate) fn set_rtt(&self, rtt: Duration) {
        lock(&self.pending).rtt = Some(rtt);
    }

    pub(crate) fn rtt(&self) -> Option<Duration> {
        lock(&self.pending).rtt
    }
}

/// Feedback of one participant connection, by media SSRC.
#[derive(Default)]
pub(crate) struct Inbox {
    /// Track index and feedback, by the SSRC this connection sends it on.
    tracks: Mutex<HashMap<u32, (usize, Arc<TrackFeedback>)>>,
    /// The forwarder's tracks the participant receives instead: their SSRC
    /// → its track index (the frames are the same, so is their feedback).
    forwarded: Mutex<HashMap<u32, usize>>,
}

impl Inbox {
    pub(crate) fn register(&self, ssrc: u32, index: usize, track: &Arc<TrackFeedback>) {
        lock(&self.tracks).insert(ssrc, (index, Arc::clone(track)));
    }

    pub(crate) fn unregister(&self, track: &Arc<TrackFeedback>) {
        lock(&self.tracks).retain(|_, (_, t)| !Arc::ptr_eq(t, track));
    }

    /// The participant receives the forwarder's tracks `(SSRC, its track
    /// index)` (none: its own again).
    pub(crate) fn forward(&self, tracks: &[(u32, usize)]) {
        *lock(&self.forwarded) = tracks.iter().copied().collect();
    }

    pub(crate) fn deliver(&self, feedback: &VideoFeedback) {
        let tracks = lock(&self.tracks);
        let track = tracks.get(&feedback.ssrc).map(|(_, t)| t).or_else(|| {
            let index = *lock(&self.forwarded).get(&feedback.ssrc)?;
            tracks.values().find(|(i, _)| *i == index).map(|(_, t)| t)
        });
        match track {
            Some(track) => track.receive(feedback),
            None => tracing::debug!(target: "farwing::recovery", ssrc = feedback.ssrc,
                "video feedback for an unknown track"),
        }
    }
}

/// A viewer that never acknowledges references (a browser) and asks for
/// this many keyframes within `ISOLATION_WINDOW` gets its own encoder for
/// `ISOLATION_TIME`, so its losses stop sending keyframes to everyone.
const ISOLATION_REQUESTS: usize = 3;
const ISOLATION_WINDOW: Duration = Duration::from_secs(10);
const ISOLATION_TIME: Duration = Duration::from_secs(60);
/// Proactive recovery frames after refused repairs, at most this often.
const PROACTIVE_SPACING: Duration = Duration::from_millis(200);
/// Keyframes for a browser after refused repairs, at most this often.
const BROWSER_KEYFRAME_SPACING: Duration = Duration::from_millis(500);

/// What to do about a repair the host refused.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Refused {
    Recover(Request),
    /// A browser that cannot use a recovery frame would wait seconds for
    /// its own keyframe request.
    Keyframe,
    Nothing,
}

static ISOLATED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

/// Per-participant decisions around the shared encoder: isolating a lossy
/// keyframe-only viewer, and recovery frames for repairs the host refused.
#[derive(Default)]
pub(crate) struct Viewer {
    acknowledged: bool,
    keyframe_requests: std::collections::VecDeque<Instant>,
    isolated: Option<(u64, Instant)>,
    proactive: Option<Instant>,
    browser_keyframe: Option<Instant>,
}

impl Viewer {
    /// This viewer's acknowledgement (any) and new keyframe requests;
    /// returns the group isolation key (0: shares).
    pub(crate) fn update(
        &mut self,
        now: Instant,
        held: Option<&Held>,
        keyframe_requests: u32,
        others: bool,
    ) -> u64 {
        self.acknowledged |= held.is_some();
        for _ in 0..keyframe_requests {
            self.keyframe_requests.push_back(now);
        }
        while self
            .keyframe_requests
            .front()
            .is_some_and(|at| now.duration_since(*at) > ISOLATION_WINDOW)
        {
            self.keyframe_requests.pop_front();
        }
        if self.isolated.is_some_and(|(_, until)| now >= until) {
            self.isolated = None;
            tracing::info!(target: "farwing::recovery", "lossy viewer rejoins the shared encoder");
        }
        if self.isolated.is_none()
            && others
            && !self.acknowledged
            && self.keyframe_requests.len() >= ISOLATION_REQUESTS
        {
            let id = ISOLATED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            self.isolated = Some((id, now + ISOLATION_TIME));
            self.keyframe_requests.clear();
            tracing::info!(target: "farwing::recovery",
                "viewer keeps losing keyframes; encoding for it alone for a while");
        }
        self.isolated.map_or(0, |(id, _)| id)
    }

    /// A refused repair: a recovery frame from what the viewer last
    /// acknowledged, or for a browser that acknowledged nothing usable, a
    /// keyframe now.
    pub(crate) fn refused(&mut self, now: Instant, held: Option<&Held>, browser: bool) -> Refused {
        if let Some(request) = self.proactive(now, held) {
            return Refused::Recover(request);
        }
        if browser
            && held.is_none_or(|h| h.held.is_empty())
            && self
                .browser_keyframe
                .is_none_or(|at| now.duration_since(at) >= BROWSER_KEYFRAME_SPACING)
        {
            self.browser_keyframe = Some(now);
            return Refused::Keyframe;
        }
        Refused::Nothing
    }

    /// A refused repair: ask for a recovery frame on this viewer's behalf
    /// from what it last acknowledged, rather than wait for it to notice.
    pub(crate) fn proactive(&mut self, now: Instant, held: Option<&Held>) -> Option<Request> {
        let held = held?;
        if held.held.is_empty()
            || self
                .proactive
                .is_some_and(|at| now.duration_since(at) < PROACTIVE_SPACING)
        {
            return None;
        }
        self.proactive = Some(now);
        Some(Request {
            epoch: held.epoch,
            decoded: held.decoded,
            held: held.held.clone(),
            request: 0,
        })
    }
}

/// Recovery frames refer to references at most this old.
const LTR_MAX_AGE: Duration = Duration::from_secs(5);
/// A keyframe request this soon after a recovery frame counts as a failure.
const FAILURE_WINDOW: Duration = Duration::from_secs(1);
const MAX_FAILURES: u8 = 3;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Mark {
    id: u16,
    at: Instant,
}

#[derive(Clone, Copy, Debug)]
struct Served {
    frame_id: u16,
    at: Instant,
}

/// What one frame should do, and why.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Plan {
    pub control: FrameControl,
    /// This frame answers recovery requests.
    pub serves: bool,
}

/// Long-term reference decisions of one encoder (one epoch at a time).
#[derive(Debug)]
pub(crate) struct Decider {
    slots: u8,
    marks: [Option<Mark>; 2],
    last_mark: Option<Instant>,
    served: Option<Served>,
    failures: u8,
    disabled: bool,
    /// Smoothed time from marking a reference until every viewer holds it
    /// (round trip, decoding and any playout buffer).
    acknowledgement: Option<Duration>,
    /// Recent marks not yet held by every viewer; kept after their slot is
    /// reused, so a slow viewer's acknowledgement is still measured.
    unacknowledged: std::collections::VecDeque<Mark>,
}

impl Decider {
    pub(crate) fn new(slots: u8) -> Self {
        Self {
            slots: slots.min(2),
            marks: [None; 2],
            last_mark: None,
            served: None,
            failures: 0,
            disabled: false,
            acknowledgement: None,
            unacknowledged: std::collections::VecDeque::new(),
        }
    }

    /// A new encoder or epoch: nothing is marked yet.
    pub(crate) fn reset(&mut self, slots: u8) {
        *self = Self::new(slots);
    }

    fn active(&self) -> bool {
        self.slots >= 2 && !self.disabled
    }

    /// `T_mark`: often enough that a fresh reference is held after a round
    /// trip, rarely enough that a mark is acknowledged before the next one
    /// replaces the other slot (viewers with a playout buffer acknowledge
    /// late).
    fn mark_interval(&self, rtt: Duration) -> Duration {
        let acknowledgement = self.acknowledgement.unwrap_or_default().mul_f64(1.5);
        (rtt * 2)
            .max(acknowledgement)
            .clamp(Duration::from_millis(100), Duration::from_secs(1))
    }

    /// `held`: the latest acknowledgement of each viewer of the stream that
    /// sent one recently; `requests`: their unanswered recovery requests.
    pub(crate) fn plan(
        &mut self,
        now: Instant,
        keyframe: bool,
        epoch: u8,
        held: &[Held],
        requests: &[Request],
        rtt: Duration,
    ) -> Plan {
        if keyframe
            && let Some(served) = self.served
            && now.duration_since(served.at) < FAILURE_WINDOW
        {
            self.served = None;
            self.failures += 1;
            if self.failures >= MAX_FAILURES && !self.disabled {
                self.disabled = true;
                tracing::warn!(target: "farwing::recovery", failures = self.failures,
                    "recovery frames keep failing; keyframes only for this encoder");
            }
        }
        let held: Vec<&Held> = held.iter().filter(|h| h.epoch == epoch).collect();
        if !held.is_empty() {
            let mut samples = Vec::new();
            self.unacknowledged.retain(|mark| {
                let acknowledged = held.iter().all(|h| h.held.contains(&mark.id));
                if acknowledged {
                    samples.push(now.saturating_duration_since(mark.at));
                }
                !acknowledged && now.saturating_duration_since(mark.at) < Duration::from_secs(2)
            });
            for sample in samples {
                self.acknowledgement = Some(match self.acknowledgement {
                    Some(average) => average.mul_f64(0.75) + sample.mul_f64(0.25),
                    None => sample,
                });
            }
        }
        if self
            .served
            .is_some_and(|s| held.iter().any(|h| !newer(s.frame_id, h.decoded)))
        {
            // A viewer decoded past the last recovery frame.
            self.failures = 0;
            self.served = None;
        }
        let asked = !requests.is_empty();
        if !self.active() {
            return Plan {
                control: FrameControl {
                    keyframe: keyframe || asked,
                    ..Default::default()
                },
                serves: asked,
            };
        }
        if keyframe {
            return Plan {
                control: FrameControl {
                    keyframe: true,
                    mark_ltr: Some(0),
                    use_ltr: None,
                },
                serves: asked,
            };
        }
        let mark = |slot: u8| self.marks[usize::from(slot)];
        if asked {
            // A reference every requester holds.
            let reference = requests
                .iter()
                .all(|r| r.epoch == epoch)
                .then(|| {
                    (0..2u8)
                        .filter_map(|slot| mark(slot).map(|m| (slot, m)))
                        .filter(|(_, m)| {
                            requests.iter().all(|r| r.held.contains(&m.id))
                                && now.duration_since(m.at) <= LTR_MAX_AGE
                        })
                        .max_by_key(|(_, m)| m.at)
                })
                .flatten();
            return Plan {
                control: match reference {
                    Some((slot, _)) => FrameControl {
                        keyframe: false,
                        mark_ltr: None,
                        use_ltr: Some(slot),
                    },
                    None => FrameControl {
                        keyframe: true,
                        mark_ltr: Some(0),
                        use_ltr: None,
                    },
                },
                serves: true,
            };
        }
        let interval = self.mark_interval(rtt);
        let due = self
            .last_mark
            .is_none_or(|at| now.duration_since(at) >= interval);
        // Only a viewer that acknowledges references can use them (browsers
        // never do); the keyframe's mark starts the exchange.
        if held.is_empty() || !due {
            return Plan::default();
        }
        // Keep the newest reference every viewer holds (or, failing that,
        // any viewer holds); refresh the other slot.
        let newest = |all: bool| {
            (0..2u8)
                .filter(|slot| {
                    mark(*slot).is_some_and(|m| {
                        if all {
                            held.iter().all(|h| h.held.contains(&m.id))
                        } else {
                            held.iter().any(|h| h.held.contains(&m.id))
                        }
                    })
                })
                .max_by_key(|slot| mark(*slot).map(|m| m.at))
        };
        let keep = newest(true).or_else(|| newest(false));
        Plan {
            control: FrameControl {
                mark_ltr: Some(match keep {
                    Some(slot) => 1 - slot,
                    None => 1,
                }),
                ..Default::default()
            },
            serves: false,
        }
    }

    /// Record what the encoder actually produced and describe it in
    /// `meta`. True when the encoder ignored a recovery request and a
    /// keyframe must follow.
    pub(crate) fn encoded(
        &mut self,
        plan: &Plan,
        meta: &mut FrameMeta,
        keyframe: bool,
        output: LtrOutput,
        now: Instant,
    ) -> bool {
        if plan.control.mark_ltr.is_some() || keyframe {
            self.last_mark = Some(now);
        }
        if keyframe {
            self.marks = [None; 2];
        }
        let mut failed = false;
        if let Some(slot) = output.used.filter(|_| !keyframe)
            && let Some(mark) = self.marks.get(usize::from(slot)).copied().flatten()
        {
            meta.flags |= flags::RECOVERY | flags::REF_LTR;
            meta.ref_id = mark.id;
        } else if plan.control.use_ltr.is_some() && !keyframe {
            failed = true;
        }
        if plan.serves && !failed {
            self.served = Some(Served {
                frame_id: meta.frame_id,
                at: now,
            });
        }
        if let Some(slot) = output.marked.filter(|slot| usize::from(*slot) < self.marks.len()) {
            let mark = Mark {
                id: meta.frame_id,
                at: now,
            };
            self.marks[usize::from(slot)] = Some(mark);
            self.unacknowledged.push_back(mark);
            if self.unacknowledged.len() > 32 {
                self.unacknowledged.pop_front();
            }
            *meta = meta.with_ltr_slot(slot);
        }
        failed
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::media::frame_meta::Counter;

    struct Run {
        decider: Decider,
        counter: Counter,
        now: Instant,
    }

    impl Run {
        fn new() -> Self {
            Self {
                decider: Decider::new(2),
                counter: Counter::default(),
                now: Instant::now(),
            }
        }

        /// One frame with an encoder that does what it is asked.
        fn frame(&mut self, keyframe: bool, held: Option<&Held>, request: Option<&Request>) -> (Plan, FrameMeta) {
            self.now += Duration::from_millis(16);
            let epoch = self.counter.next(false).epoch;
            let held: Vec<Held> = held.cloned().into_iter().collect();
            let requests: Vec<Request> = request.cloned().into_iter().collect();
            let plan = self
                .decider
                .plan(self.now, keyframe, epoch, &held, &requests, Duration::from_millis(40));
            let mut meta = self.counter.next(plan.control.keyframe);
            let output = LtrOutput {
                marked: plan.control.mark_ltr,
                used: plan.control.use_ltr,
            };
            assert!(!self.decider.encoded(&plan, &mut meta, plan.control.keyframe, output, self.now));
            (plan, meta)
        }
    }

    #[test]
    fn marks_rotate_after_acknowledgement_and_recover_from_the_newest_held() {
        let mut run = Run::new();
        let (plan, key) = run.frame(true, None, None);
        assert_eq!(plan.control.mark_ltr, Some(0));
        assert!(key.keyframe() && key.marks_ltr() && key.ltr_slot() == 0);
        // Nothing acknowledged (a browser): no further marks.
        for _ in 0..20 {
            assert!(!run.frame(false, None, None).1.marks_ltr());
        }
        // The keyframe's mark acknowledged: the next mark goes to slot 1
        // after T_mark (1.5 × the measured acknowledgement time here).
        let first = Held {
            epoch: key.epoch,
            decoded: key.frame_id,
            held: vec![key.frame_id],
        };
        let mut marked = Vec::new();
        for _ in 0..60 {
            let (_, meta) = run.frame(false, Some(&first), None);
            if meta.marks_ltr() {
                marked.push((meta.ltr_slot(), meta.frame_id));
                break;
            }
        }
        assert_eq!(marked.len(), 1);
        assert_eq!(marked[0].0, 1);
        // The controller holds both: the next mark refreshes slot 0, keeping
        // the newest acknowledged (slot 1).
        let held = Held {
            epoch: key.epoch,
            decoded: marked[0].1,
            held: vec![marked[0].1, key.frame_id],
        };
        let mut next = None;
        for _ in 0..60 {
            let (_, meta) = run.frame(false, Some(&held), None);
            if meta.marks_ltr() {
                next = Some((meta.ltr_slot(), meta.frame_id));
                break;
            }
        }
        assert_eq!(next.map(|n| n.0), Some(0));
        // A loss: the request holds slot 1's reference only.
        let request = Request {
            epoch: key.epoch,
            decoded: marked[0].1 + 3,
            held: vec![marked[0].1],
            request: 1,
        };
        let (plan, meta) = run.frame(false, Some(&held), Some(&request));
        assert_eq!(plan.control.use_ltr, Some(1));
        assert!(meta.recovery() && meta.refs_ltr());
        assert_eq!(meta.ref_id, marked[0].1);
    }

    #[test]
    fn group_viewers_share_references() {
        let mut run = Run::new();
        let (_, key) = run.frame(true, None, None);
        let epoch = key.epoch;
        // Two viewers acknowledged the keyframe; the next mark goes to slot 1.
        let both = |ids: Vec<u16>, other: Vec<u16>| {
            vec![
                Held {
                    epoch,
                    decoded: 0,
                    held: ids,
                },
                Held {
                    epoch,
                    decoded: 0,
                    held: other,
                },
            ]
        };
        let mut slot1 = None;
        for _ in 0..8 {
            run.now += Duration::from_millis(16);
            let held = both(vec![key.frame_id], vec![key.frame_id]);
            let plan = run.decider.plan(run.now, false, epoch, &held, &[], Duration::from_millis(40));
            let mut meta = run.counter.next(false);
            run.decider.encoded(
                &plan,
                &mut meta,
                false,
                LtrOutput {
                    marked: plan.control.mark_ltr,
                    used: None,
                },
                run.now,
            );
            if meta.marks_ltr() {
                slot1 = Some((meta.ltr_slot(), meta.frame_id));
            }
        }
        let (slot, id) = slot1.unwrap();
        assert_eq!(slot, 1);
        // Only one viewer holds slot 1: slot 0 (held by both) is kept.
        run.now += Duration::from_millis(200);
        let held = both(vec![id, key.frame_id], vec![key.frame_id]);
        let plan = run.decider.plan(run.now, false, epoch, &held, &[], Duration::from_millis(40));
        assert_eq!(plan.control.mark_ltr, Some(1));
        // Two requests: only the reference both hold qualifies.
        let requests = [
            Request {
                epoch,
                decoded: id,
                held: vec![id, key.frame_id],
                request: 1,
            },
            Request {
                epoch,
                decoded: key.frame_id,
                held: vec![key.frame_id],
                request: 7,
            },
        ];
        let plan = run.decider.plan(run.now, false, epoch, &held, &requests, Duration::from_millis(40));
        assert_eq!(plan.control.use_ltr, Some(0));
        assert!(plan.serves);
    }

    #[test]
    fn marks_wait_for_slow_acknowledgements() {
        // A viewer with a playout buffer acknowledges each mark 300 ms late:
        // marks are spaced so that one is acknowledged before the other
        // slot is replaced, and the newest acknowledged one stays usable.
        let mut run = Run::new();
        let (_, key) = run.frame(true, None, None);
        let epoch = key.epoch;
        let mut marks: Vec<(u16, Instant)> = vec![(key.frame_id, run.now)];
        let mut acknowledged = vec![key.frame_id];
        let mut intervals = Vec::new();
        for _ in 0..400 {
            run.now += Duration::from_millis(16);
            for (id, at) in &marks {
                if run.now.duration_since(*at) >= Duration::from_millis(300) && !acknowledged.contains(id) {
                    acknowledged.insert(0, *id);
                }
            }
            let held = [Held {
                epoch,
                decoded: 0,
                held: acknowledged.iter().take(2).copied().collect(),
            }];
            let plan = run.decider.plan(run.now, false, epoch, &held, &[], Duration::from_millis(20));
            let mut meta = run.counter.next(false);
            run.decider.encoded(
                &plan,
                &mut meta,
                false,
                LtrOutput {
                    marked: plan.control.mark_ltr,
                    used: None,
                },
                run.now,
            );
            if meta.marks_ltr() {
                if let Some((_, last)) = marks.last() {
                    intervals.push(run.now.duration_since(*last));
                }
                marks.push((meta.frame_id, run.now));
            }
        }
        // After the first acknowledgements, marks are about 450 ms apart.
        let late = &intervals[intervals.len() / 2..];
        assert!(late.iter().all(|i| *i >= Duration::from_millis(400)), "{late:?}");
        // The two newest acknowledged references are both still kept.
        let kept: Vec<u16> = run.decider.marks.iter().flatten().map(|m| m.id).collect();
        assert!(acknowledged.iter().take(1).all(|id| kept.contains(id)), "{kept:?} {acknowledged:?}");
    }

    #[test]
    fn falls_back_to_keyframes() {
        let mut run = Run::new();
        let (_, key) = run.frame(true, None, None);
        // Nothing the controller holds, or another epoch: IDR.
        let request = Request {
            epoch: key.epoch,
            decoded: 5,
            held: vec![999],
            request: 1,
        };
        let (plan, meta) = run.frame(false, None, Some(&request));
        assert!(plan.control.keyframe && meta.keyframe());
        let request = Request {
            epoch: key.epoch.wrapping_add(1),
            held: vec![meta.frame_id],
            request: 2,
            decoded: meta.frame_id,
        };
        assert!(run.frame(false, None, Some(&request)).0.control.keyframe);
        // Without long-term references (AV1, other encoders), requests are
        // keyframes and nothing is marked.
        let mut plain = Decider::new(0);
        let plan = plain.plan(run.now, false, 0, &[], std::slice::from_ref(&request), Duration::ZERO);
        assert!(plan.control.keyframe && plan.control.mark_ltr.is_none());
        assert_eq!(plain.plan(run.now, false, 0, &[], &[], Duration::ZERO), Plan::default());
    }

    #[test]
    fn repeated_failures_disable_recovery_frames() {
        let mut run = Run::new();
        let (_, mut key) = run.frame(true, None, None);
        for request in 1..=3 {
            let ask = Request {
                epoch: key.epoch,
                decoded: key.frame_id,
                held: vec![key.frame_id],
                request,
            };
            let (plan, _) = run.frame(false, None, Some(&ask));
            assert_eq!(plan.control.use_ltr, Some(0), "request {request}");
            // The controller still asks for a keyframe right after.
            key = run.frame(true, None, None).1;
        }
        assert!(run.decider.disabled);
        let ask = Request {
            epoch: key.epoch,
            decoded: key.frame_id,
            held: vec![key.frame_id],
            request: 9,
        };
        assert!(run.frame(false, None, Some(&ask)).0.control.keyframe);
    }

    #[test]
    fn an_encoder_that_ignores_the_request_forces_a_keyframe() {
        let mut decider = Decider::new(2);
        let now = Instant::now();
        let plan = Plan {
            control: FrameControl {
                use_ltr: Some(0),
                ..Default::default()
            },
            serves: true,
        };
        let mut meta = FrameMeta::default();
        decider.marks[0] = Some(Mark { id: 0, at: now });
        assert!(decider.encoded(&plan, &mut meta, false, LtrOutput::default(), now));
        assert!(!meta.recovery());
    }

    #[test]
    fn lossy_keyframe_only_viewers_are_isolated_for_a_while() {
        let now = Instant::now();
        let mut viewer = Viewer::default();
        assert_eq!(viewer.update(now, None, 2, true), 0);
        // Alone, there is nobody to protect.
        let mut alone = Viewer::default();
        assert_eq!(alone.update(now, None, 5, false), 0);
        let id = viewer.update(now + Duration::from_secs(1), None, 1, true);
        assert_ne!(id, 0);
        assert_eq!(viewer.update(now + Duration::from_secs(30), None, 0, true), id);
        assert_eq!(viewer.update(now + Duration::from_secs(62), None, 0, true), 0);
        // A viewer that acknowledges references recovers with recovery
        // frames instead.
        let mut native = Viewer::default();
        let held = Held {
            epoch: 1,
            decoded: 9,
            held: vec![9],
        };
        assert_eq!(native.update(now, Some(&held), 5, true), 0);
        // Requests spread over more than the window do not count.
        let mut slow = Viewer::default();
        for second in 0..6 {
            assert_eq!(slow.update(now + Duration::from_secs(second * 6), None, 1, true), 0);
        }
    }

    #[test]
    fn refused_repairs_ask_for_recovery_frames_sparingly() {
        let now = Instant::now();
        let mut viewer = Viewer::default();
        assert!(viewer.proactive(now, None).is_none());
        let held = Held {
            epoch: 2,
            decoded: 40,
            held: vec![37, 30],
        };
        let request = viewer.proactive(now, Some(&held)).unwrap();
        assert_eq!((request.decoded, request.held.clone()), (40, vec![37, 30]));
        assert!(viewer.proactive(now + Duration::from_millis(100), Some(&held)).is_none());
        assert!(viewer.proactive(now + Duration::from_millis(250), Some(&held)).is_some());
    }

    #[test]
    fn refused_repairs_give_browsers_a_keyframe_at_once() {
        let now = Instant::now();
        let mut viewer = Viewer::default();
        // A native controller recovers on its own.
        assert_eq!(viewer.refused(now, None, false), Refused::Nothing);
        assert_eq!(viewer.refused(now, None, true), Refused::Keyframe);
        assert_eq!(viewer.refused(now + Duration::from_millis(100), None, true), Refused::Nothing);
        assert_eq!(viewer.refused(now + Duration::from_millis(600), None, true), Refused::Keyframe);
        // A browser whose acknowledgements are usable gets a recovery frame.
        let held = Held {
            epoch: 1,
            decoded: 9,
            held: vec![8],
        };
        assert!(matches!(viewer.refused(now + Duration::from_secs(2), Some(&held), true), Refused::Recover(_)));
    }

    #[test]
    fn feedback_on_a_forwarded_track_reaches_the_participants_track() {
        let inbox = Inbox::default();
        let (first, second) = (Arc::new(TrackFeedback::default()), Arc::new(TrackFeedback::default()));
        inbox.register(11, 0, &first);
        inbox.register(22, 1, &second);
        let feedback = |ssrc| VideoFeedback {
            ssrc,
            epoch: 1,
            decoded: 40,
            ltr: vec![32],
            ..Default::default()
        };
        // The forwarder's track 99 stands in for track index 1.
        inbox.deliver(&feedback(99));
        assert!(second.take().0.is_none());
        inbox.forward(&[(99, 1)]);
        inbox.deliver(&feedback(99));
        assert!(first.take().0.is_none());
        assert_eq!(second.take().0.unwrap().held, vec![32]);
    }

    #[test]
    fn feedback_reaches_its_track() {
        let inbox = Inbox::default();
        let track = Arc::new(TrackFeedback::default());
        inbox.register(0x1234_5678, 0, &track);
        inbox.deliver(&VideoFeedback {
            ssrc: 0x1234_5678,
            epoch: 3,
            decoded: 1200,
            ltr: vec![1180],
            recover: true,
            request: 5,
            rtp_timestamp: None,
        });
        inbox.deliver(&VideoFeedback {
            ssrc: 1,
            ..Default::default()
        });
        let (held, request) = track.take();
        assert_eq!(held.unwrap().held, vec![1180]);
        assert_eq!(request.unwrap().request, 5);
        // A request is taken once; the acknowledgement stays.
        let (held, request) = track.take();
        assert!(held.is_some() && request.is_none());
        // The same request number again is not a new loss.
        inbox.deliver(&VideoFeedback {
            ssrc: 0x1234_5678,
            epoch: 3,
            recover: true,
            request: 5,
            ..Default::default()
        });
        assert!(track.take().1.is_none());
        inbox.unregister(&track);
        inbox.deliver(&VideoFeedback {
            ssrc: 0x1234_5678,
            recover: true,
            request: 6,
            ..Default::default()
        });
        assert!(track.take().1.is_none());
    }

    #[test]
    fn browser_acknowledgements_resolve_the_dependency_chain() {
        use crate::media::frame_meta::Counter;
        let track = TrackFeedback::default();
        let mut counter = Counter::default();
        let mut send = |rtp: u32, keyframe: bool, mark: Option<u8>, recovery: Option<u16>| {
            let mut meta = counter.next(keyframe);
            if let Some(slot) = mark {
                meta = meta.with_ltr_slot(slot);
            }
            if let Some(reference) = recovery {
                meta.flags |= flags::RECOVERY | flags::REF_LTR;
                meta.ref_id = reference;
            }
            track.sent(rtp, meta, keyframe);
            meta
        };
        // Without the Dependency Descriptor nothing is recorded or used.
        send(1, true, Some(0), None);
        track.receive(&VideoFeedback {
            ssrc: 1,
            rtp_timestamp: Some(1),
            ..Default::default()
        });
        assert!(track.take().0.is_none());
        track.set_generic(true);
        let key = send(100, true, Some(0), None);
        send(200, false, None, None);
        let marked = send(300, false, Some(1), None);
        send(400, false, None, None);
        // Lost frames: one marked, then more.
        send(500, false, Some(0), None);
        send(600, false, None, None);
        // A recovery frame from the mark at 300, then a delta.
        let recovery = send(700, false, None, Some(marked.frame_id));
        send(800, false, None, None);
        let ack = |rtp| {
            track.receive(&VideoFeedback {
                ssrc: 1,
                rtp_timestamp: Some(rtp),
                ..Default::default()
            });
            track.take().0.unwrap()
        };
        let held = ack(400);
        assert_eq!((held.decoded, held.held.clone()), (marked.frame_id + 1, vec![marked.frame_id, key.frame_id]));
        // After recovery the lost mark (500) is not on the chain.
        let held = ack(800);
        assert_eq!(held.decoded, recovery.frame_id + 1);
        assert_eq!(held.held, vec![marked.frame_id, key.frame_id]);
    }
}
