//! Graphics names the shared egui shell uses, whichever platform supplies them,
//! and the UI-only timing diagnostics both presenters record.
use std::time::{Duration, Instant};
use winit::dpi::PhysicalSize;

// The platform layer owns the presenter and its device; the shell only needs
// their names.
#[cfg(windows)]
pub(crate) use crate::platform::graphics::window_hwnd;
#[allow(
    unused_imports,
    reason = "The platform-neutral names of the presenter types; not every caller spells them out."
)]
pub(crate) use crate::platform::graphics::{
    Graphics, Retained, UiPresenter, can_hide, create_device, split_output,
};

/// Opt-in, aggregated UI-only diagnostics. No RTP hot-path counters or HUD.
pub(crate) struct UiTimingAudit {
    since: Instant,
    previous: Option<(Instant, bool)>,
    frames: u32,
    presented: u32,
    following_immediate: u32,
    immediate_gap: Duration,
    max_immediate_gap: Duration,
    max_layout: Duration,
    max_submit: Duration,
}

impl UiTimingAudit {
    pub(crate) fn active(slot: &mut Option<Self>, at: Instant) -> Option<&mut Self> {
        if tracing::enabled!(target: "openuuyc::ui_timing", tracing::Level::DEBUG) {
            Some(slot.get_or_insert_with(|| Self::new(at)))
        } else {
            *slot = None;
            None
        }
    }

    fn new(since: Instant) -> Self {
        Self {
            since,
            previous: None,
            frames: 0,
            presented: 0,
            following_immediate: 0,
            immediate_gap: Duration::ZERO,
            max_immediate_gap: Duration::ZERO,
            max_layout: Duration::ZERO,
            max_submit: Duration::ZERO,
        }
    }

    pub(crate) fn record(
        &mut self,
        at: Instant,
        layout: Duration,
        submit: Duration,
        immediate: bool,
        presented: bool,
    ) {
        self.frames += 1;
        self.presented += u32::from(presented);
        if let Some((previous, true)) = self.previous {
            let gap = at.saturating_duration_since(previous);
            self.following_immediate += 1;
            self.immediate_gap += gap;
            self.max_immediate_gap = self.max_immediate_gap.max(gap);
        }
        self.previous = Some((at, immediate));
        self.max_layout = self.max_layout.max(layout);
        self.max_submit = self.max_submit.max(submit);
        if at.duration_since(self.since) >= Duration::from_secs(5) {
            tracing::debug!(target: "openuuyc::ui_timing",
                frames = self.frames, presented = self.presented,
                seconds = at.duration_since(self.since).as_secs_f64(),
                animation_intervals = self.following_immediate,
                animation_avg_ms = self.immediate_gap.as_secs_f64() * 1000.0 / f64::from(self.following_immediate.max(1)),
                animation_max_ms = self.max_immediate_gap.as_secs_f64() * 1000.0,
                layout_max_ms = self.max_layout.as_secs_f64() * 1000.0,
                submit_max_ms = self.max_submit.as_secs_f64() * 1000.0,
                "UI refresh audit");
            let previous = self.previous;
            *self = Self::new(at);
            self.previous = previous;
        }
    }
}

pub(crate) fn nonzero_size(size: PhysicalSize<u32>) -> PhysicalSize<u32> {
    PhysicalSize::new(size.width.max(1), size.height.max(1))
}
