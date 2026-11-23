use crate::diagnostics::performance::PerformanceMonitor;
use crate::media::decoder::RenderSurface;
use crate::media::video_color::RenderColor;
use crate::transport::rtc::FrameSenderTiming;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};
use std::time::{Duration, Instant};

#[derive(Debug)]
pub(super) struct DecodedVideoFrame {
    pub(super) is_new_picture: Option<bool>,
    pub(super) width: u32,
    pub(super) height: u32,
    pub(super) surface: RenderSurface,
    pub(super) color: RenderColor,
    pub(super) received_at: Instant,
    pub(super) decoded_at: Instant,
    pub(super) assembly_delay: Duration,
    pub(super) input_queue_delay: Duration,
    pub(super) decode_pipeline_delay: Duration,
    pub(super) sender_timing: FrameSenderTiming,
    pub(super) rtp_timestamp: u32,
}

pub(super) const DECODER_INFLIGHT_LIMIT: usize = 100;

use super::PresentQueue;

// Presentation owns the pending decoded pictures (normally one; see
// `present_queue`). Replacing a picture releases its display lease; the
// decoder retains its own references.
pub(super) type FrameQueue = Arc<Mutex<PresentQueue>>;

/// The presentation clock must not throttle decoding of reference pictures.
pub(super) fn take_next_frame(
    queue: &mut PresentQueue,
    performance: &PerformanceMonitor,
) -> Option<DecodedVideoFrame> {
    let frame = queue.take();
    performance.set_presentation_queue_frames(queue.len());
    frame
}

#[derive(Clone, Default)]
pub(super) struct FrameWake {
    pub(super) visible: Arc<AtomicBool>,
    pub(super) render_thread: Arc<Mutex<Option<std::thread::Thread>>>,
    #[cfg(target_os = "linux")]
    repaint: Option<egui::Context>,
}

impl FrameWake {
    #[cfg(target_os = "linux")]
    pub(super) fn for_egui(context: egui::Context) -> Self {
        Self {
            visible: Arc::new(AtomicBool::new(true)),
            repaint: Some(context),
            ..Self::default()
        }
    }

    pub(super) fn install_render_thread(&self, thread: std::thread::Thread) {
        *mutex_lock(&self.render_thread) = Some(thread);
        self.visible.store(true, Ordering::Release);
    }

    pub(super) fn notify(&self) {
        #[cfg(target_os = "linux")]
        if let Some(context) = &self.repaint {
            context.request_repaint();
        }
        if let Some(thread) = mutex_lock(&self.render_thread).as_ref() {
            thread.unpark();
        }
    }
}

pub(super) fn mutex_lock<T>(lock: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    lock.lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}
