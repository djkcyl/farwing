//! Controller side of loss recovery (`docs/protocol/rfi-ltr-brief.md` §6.5):
//! unrecoverable loss asks the host for a recovery frame instead of a
//! keyframe while this receiver holds long-term references; a PLI follows if
//! nothing decodable arrives in time.

use crate::features::stream_control::StreamControlHandle;
use crate::features::stream_control::wire::VideoFeedback;
use crate::media::frame_meta::FrameMeta;
use crate::transport::video_receiver::recovery::Snapshot;
use std::time::{Duration, Instant};

/// One frame interval at 30 fps, added to the fallback wait.
const FRAME: Duration = Duration::from_millis(34);
const FALLBACK_FLOOR: Duration = Duration::from_millis(150);

static LATE_ONLY: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// `dev video --recovery-late`: ask for recovery frames only when repair is
/// hopeless, as for keyframes (A/B of the early judgment).
pub(crate) fn disable_early() {
    LATE_ONLY.store(true, std::sync::atomic::Ordering::Relaxed);
}

pub(super) fn early() -> bool {
    !LATE_ONLY.load(std::sync::atomic::Ordering::Relaxed)
}

pub(super) fn fallback_after(rtt: Duration) -> Duration {
    (rtt * 2).max(FALLBACK_FLOOR) + FRAME
}

pub(super) struct LossRecovery {
    control: StreamControlHandle,
    ssrc: u32,
    request: u32,
    /// A recovery request is outstanding until then.
    deadline: Option<Instant>,
    /// The last recovery did not arrive: keyframes only until one decodes.
    keyframes_only: bool,
    /// This loss already asked a second time.
    retried: bool,
}

impl LossRecovery {
    pub(super) fn new(control: StreamControlHandle, ssrc: u32) -> Self {
        Self {
            control,
            ssrc,
            request: 0,
            deadline: None,
            keyframes_only: false,
            retried: false,
        }
    }

    fn enabled(&self) -> bool {
        crate::protocol::peer::controller_recovery()
            && self
                .control
                .peer_supports(crate::protocol::peer::feature::VIDEO_RECOVERY)
    }

    fn feedback(&self, snapshot: &Snapshot, recover: bool, request: u32) -> VideoFeedback {
        VideoFeedback {
            ssrc: self.ssrc,
            epoch: u32::from(snapshot.epoch),
            decoded: u32::from(snapshot.decoded),
            ltr: snapshot.held.iter().take(4).map(|id| u32::from(*id)).collect(),
            recover,
            request,
            rtp_timestamp: None,
        }
    }

    /// Repair gave up on a loss. True when no keyframe is needed now: a
    /// recovery frame was asked for (now or earlier in this loss).
    /// `playout`: how long the recovery frame will wait in the playout
    /// buffer before it can decode.
    pub(super) fn on_loss(
        &mut self,
        snapshot: Option<Snapshot>,
        rtt: Duration,
        playout: Duration,
        now: Instant,
    ) -> bool {
        if self.keyframes_only || !self.enabled() {
            return false;
        }
        if let Some(deadline) = self.deadline {
            return now < deadline;
        }
        self.retried = false;
        self.ask(snapshot, rtt, playout, now)
    }

    fn ask(&mut self, snapshot: Option<Snapshot>, rtt: Duration, playout: Duration, now: Instant) -> bool {
        let Some(snapshot) = snapshot.filter(|s| !s.held.is_empty()) else {
            return false;
        };
        self.request = self.request.wrapping_add(1).max(1);
        if !self.control.video_feedback(self.feedback(&snapshot, true, self.request)) {
            return false;
        }
        self.deadline = Some(now + fallback_after(rtt) + playout);
        tracing::debug!(
            target: "farwing::recovery",
            request = self.request,
            decoded = snapshot.decoded,
            held = ?snapshot.held,
            retry = self.retried,
            "unrecoverable video loss; requesting a recovery frame"
        );
        true
    }

    /// The fallback wait ran out: true when a keyframe must be requested.
    /// The first time in a loss asks for a recovery frame once more: the
    /// recovery frame itself may have been lost, and a keyframe resets every
    /// reference (and is far more likely to be lost again).
    pub(super) fn expired(
        &mut self,
        snapshot: Option<Snapshot>,
        rtt: Duration,
        playout: Duration,
        now: Instant,
    ) -> bool {
        if !self.deadline.is_some_and(|deadline| now >= deadline) {
            return false;
        }
        self.deadline = None;
        if !self.retried && self.enabled() {
            self.retried = true;
            if self.ask(snapshot, rtt, playout, now) {
                return false;
            }
        }
        self.keyframes_only = true;
        tracing::info!(
            target: "farwing::recovery",
            request = self.request,
            "no recovery frame arrived in time; requesting a keyframe"
        );
        true
    }

    /// A frame decoded; `marked`: it marked a long-term reference.
    pub(super) fn decoded(&mut self, meta: FrameMeta, marked: bool, snapshot: Option<Snapshot>) {
        if meta.keyframe() {
            self.keyframes_only = false;
            self.deadline = None;
            self.retried = false;
        } else if meta.recovery() && self.deadline.take().is_some() {
            self.retried = false;
            tracing::debug!(target: "farwing::recovery", frame = meta.frame_id,
                reference = meta.ref_id, "recovery frame decoded");
        }
        if marked
            && self.enabled()
            && let Some(snapshot) = snapshot
        {
            self.control.video_feedback(self.feedback(&snapshot, false, 0));
        }
    }
}
