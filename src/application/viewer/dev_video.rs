//! `dev video`: the native decode path without a window or presenter, for
//! measuring loss recovery (`docs/protocol/rfi-ltr-brief.md` §6.10).

use super::FrameWake;
use super::decode_pipeline::{DecoderConfig, decoder_manager};
pub(crate) use super::decode_pipeline::DecodedObservation;
use crate::diagnostics::performance::PerformanceMonitor;
use crate::media::VideoCodec;
use crate::transport::rtc::{EncodedVideoFrame, VideoFrameSink, VideoReceiverFeedback};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use tokio::sync::mpsc;

/// A decoder thread whose pictures are released as soon as they decode
/// (no presentation queue: `FrameWake` stays invisible).
pub(crate) struct HeadlessDecoder {
    sender: Option<mpsc::UnboundedSender<EncodedVideoFrame>>,
    wake: std::thread::Thread,
    shutdown: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
    fatal_error: Arc<Mutex<Option<String>>>,
}

pub(crate) struct HeadlessDecoderConfig {
    pub codec: VideoCodec,
    pub decoder: crate::media::selection::DecoderPreference,
    pub size: (u32, u32),
    pub frame_rate: u32,
    pub receiver_feedback: mpsc::UnboundedSender<VideoReceiverFeedback>,
    pub performance: PerformanceMonitor,
    pub observer: std::sync::mpsc::Sender<DecodedObservation>,
}

impl HeadlessDecoder {
    pub(crate) fn start(config: HeadlessDecoderConfig) -> Self {
        let (sender, source) = mpsc::unbounded_channel();
        let shutdown = Arc::new(AtomicBool::new(false));
        let fatal_error = Arc::new(Mutex::new(None));
        let (decode_idle, _) = tokio::sync::watch::channel(0);
        let decoder = DecoderConfig {
            codec: config.codec,
            width: config.size.0,
            height: config.size.1,
            frame_rate: config.frame_rate,
            decoder: config.decoder,
            software_decode: Arc::new(AtomicBool::new(!config.decoder.mode.hardware())),
            decode_enabled: Arc::new(AtomicBool::new(true)),
            decode_idle,
            pause_epoch: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            surface_writer: None,
            observer: Some(config.observer),
        };
        let thread_shutdown = Arc::clone(&shutdown);
        let thread_error = Arc::clone(&fatal_error);
        let thread = std::thread::spawn(move || {
            decoder_manager(
                decoder,
                source,
                Arc::new(Mutex::new(super::PresentQueue::default())),
                FrameWake::default(),
                config.performance,
                thread_shutdown,
                thread_error,
                config.receiver_feedback,
                None,
            );
        });
        Self {
            sender: Some(sender),
            wake: thread.thread().clone(),
            shutdown,
            thread: Some(thread),
            fatal_error,
        }
    }

    pub(crate) fn sink(&self) -> VideoFrameSink {
        VideoFrameSink::Unbounded {
            sender: self.sender.clone().expect("sender lives until drop"),
            wake: self.wake.clone(),
        }
    }

    /// The decoder gave up (a fatal error), if it did.
    pub(crate) fn error(&self) -> Option<String> {
        self.fatal_error.lock().unwrap_or_else(|p| p.into_inner()).clone()
    }
}

impl Drop for HeadlessDecoder {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::Release);
        self.sender.take();
        self.wake.unpark();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}
