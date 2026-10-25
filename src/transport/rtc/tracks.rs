//! Track subscriptions, frame ownership and decoder completion feedback.
use super::workers::std_mutex_lock;
use crate::diagnostics::performance::PerformanceMonitor;
use crate::media::VideoCodec;
use bytes::Bytes;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::{Duration, Instant};
use tokio::sync::{Mutex, mpsc, watch};
use tokio_util::sync::CancellationToken;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MediaKind {
    Audio,
    Video,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ForwardedTrack {
    pub kind: MediaKind,
    pub id: String,
    pub codec: String,
    pub payload_type: u8,
    pub ssrc: u32,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RtpForwardConfig {
    /// `None` registers every negotiated video track for screen selection.
    pub video_track_id: Option<String>,
}

pub struct RtpForwarder {
    pub(super) stop: CancellationToken,
    pub(super) announcements: mpsc::UnboundedReceiver<ForwardedTrack>,
    pub(super) forwarding_started: watch::Sender<bool>,
    pub(super) tracks: VideoTrackRegistry,
    pub(super) selected_video: Option<Arc<VideoTrackSource>>,
}

#[derive(Clone, Default)]
pub(crate) struct VideoTrackRegistry {
    pub(super) entries: Arc<StdMutex<HashMap<i32, Arc<VideoTrackSource>>>>,
    pub(super) changed: Arc<tokio::sync::Notify>,
}

pub(crate) struct VideoTrackSource {
    pub metadata: ForwardedTrack,
    pub index: i32,
    pub performance: PerformanceMonitor,
    pub(super) sinks: Arc<Mutex<Vec<VideoFrameSink>>>,
    pub feedback: mpsc::UnboundedSender<VideoReceiverFeedback>,
    pub(super) started: watch::Sender<bool>,
    pub(super) keyframes: watch::Receiver<u64>,
    /// Counts keyframes for [`Self::keyframes`]; a forwarded stream of this
    /// track counts too.
    pub(super) keyframe_tx: watch::Sender<u64>,
    pub(super) nack_rtt_micros: Arc<AtomicU64>,
    /// Whose frames of this track are decoded: the host's or the forwarder's.
    pub(super) picture: Arc<PicturePath>,
}

/// Which connection's frames of a track reach the decoder: the host's
/// (source 0) or forwarder connection n's (source n). Two may carry the
/// track for a while; the one wanted takes over at its first keyframe, so
/// the picture never waits for a switch.
pub(crate) struct PicturePath {
    shown: AtomicU64,
    wanted: AtomicU64,
    changed: watch::Sender<u64>,
    /// The track's statistics: whether a switch is pending (the viewer
    /// covers a picture held by one).
    performance: PerformanceMonitor,
}

impl PicturePath {
    pub(super) fn new(performance: PerformanceMonitor) -> Self {
        Self {
            shown: AtomicU64::new(0),
            wanted: AtomicU64::new(0),
            changed: watch::channel(0).0,
            performance,
        }
    }

    /// Source `source`'s frames should be shown.
    pub(super) fn want(&self, source: u64) {
        self.wanted.store(source, Ordering::Release);
        self.performance
            .set_switch_pending(self.shown.load(Ordering::Acquire) != source);
    }

    /// The source whose frames are shown now.
    pub(super) fn shown(&self) -> u64 {
        self.shown.load(Ordering::Acquire)
    }

    /// Follows which source's frames are shown.
    pub(super) fn watch(&self) -> watch::Receiver<u64> {
        self.changed.subscribe()
    }

    /// Whether a frame from `source` may go on: its picture is shown, or it
    /// is a keyframe of the wanted source (which [`Self::admit`] lets take
    /// over).
    pub(super) fn admissible(&self, source: u64, keyframe: bool) -> bool {
        self.shown.load(Ordering::Acquire) == source
            || (keyframe && self.wanted.load(Ordering::Acquire) == source)
    }

    /// Whether a frame from `source` goes on to the decoder; a keyframe of
    /// the wanted one takes over.
    pub(super) fn admit(&self, source: u64, keyframe: bool) -> bool {
        let shown = self.shown.load(Ordering::Acquire);
        if shown == source {
            return true;
        }
        if !keyframe || self.wanted.load(Ordering::Acquire) != source {
            return false;
        }
        if self
            .shown
            .compare_exchange(shown, source, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
        {
            tracing::info!(source, "the picture comes through another connection now");
            self.performance.set_switch_pending(false);
            self.changed.send_replace(source);
        }
        true
    }

    /// Whether `source`'s frames are shown or should be: only then does it
    /// ask for keyframes.
    pub(super) fn needed(&self, source: u64) -> bool {
        self.shown.load(Ordering::Acquire) == source
            || self.wanted.load(Ordering::Acquire) == source
    }
}

impl VideoTrackRegistry {
    pub(crate) fn get(&self, index: i32) -> Option<Arc<VideoTrackSource>> {
        std_mutex_lock(&self.entries).get(&index).cloned()
    }

    pub(super) fn all(&self) -> Vec<Arc<VideoTrackSource>> {
        std_mutex_lock(&self.entries).values().cloned().collect()
    }
}

impl VideoTrackSource {
    pub(crate) async fn add_sink(&self, sink: VideoFrameSink) {
        self.sinks.lock().await.push(sink);
    }

    pub(crate) fn start(&self) {
        self.started.send_replace(true);
    }
    pub(crate) fn pause(&self) {
        self.started.send_replace(false);
    }
}

impl Drop for RtpForwarder {
    fn drop(&mut self) {
        // Join handles remain with NativePeer until its explicit close, even
        // when the media consumer disappears before the signaling owner.
        self.stop.cancel();
    }
}

#[derive(Clone, Copy, Debug)]
pub(crate) enum VideoReceiverFeedback {
    DecoderFinished {
        frame_id: i64,
        result: crate::media::decoder_result::VideoDecodeResult,
    },
    DecodeTiming {
        duration: Duration,
        finished_at: Instant,
    },
}

#[derive(Clone, Debug)]
pub(crate) struct EncodedVideoFrame {
    pub completion: DecodeCompletion,
    pub frame_id: i64,
    pub data: Bytes,
    pub rtp_timestamp: u32,
    pub received_at: Instant,
    pub assembled_at: Instant,
    pub keyframe: bool,
    pub rotation: u16,
    pub content_type: u8,
    pub video_capture_index: Option<u16>,
    pub is_new_picture: Option<bool>,
    pub frame_meta: Option<crate::media::frame_meta::FrameMeta>,
    pub sender_timing: FrameSenderTiming,
    pub codec: VideoCodec,
    pub parameter_format: Option<crate::media::video_format::VideoFormatSignature>,
    pub color_space: Option<crate::media::video_color::VideoColorSpace>,
}

/// A receive-stream admission follows its frame through queueing and decoder
/// ownership. Dropping the last copy without a Decode result retires it too.
#[derive(Clone, Debug)]
pub(crate) struct DecodeCompletion(pub(super) Arc<DecodeCompletionState>);

#[derive(Debug)]
pub(super) struct DecodeCompletionState {
    pub(super) frame_id: i64,
    pub(super) feedback: mpsc::UnboundedSender<VideoReceiverFeedback>,
    pub(super) completed: AtomicBool,
}

impl DecodeCompletion {
    pub(crate) fn new(
        frame_id: i64,
        feedback: mpsc::UnboundedSender<VideoReceiverFeedback>,
    ) -> Self {
        Self(Arc::new(DecodeCompletionState {
            frame_id,
            feedback,
            completed: AtomicBool::new(false),
        }))
    }

    pub(crate) fn complete(&self, result: crate::media::decoder_result::VideoDecodeResult) {
        self.0.complete(result);
    }
}

impl DecodeCompletionState {
    pub(super) fn complete(&self, result: crate::media::decoder_result::VideoDecodeResult) {
        if !self.completed.swap(true, Ordering::AcqRel) {
            let _ = self.feedback.send(VideoReceiverFeedback::DecoderFinished {
                frame_id: self.frame_id,
                result,
            });
        }
    }
}

impl Drop for DecodeCompletionState {
    fn drop(&mut self) {
        self.complete(crate::media::decoder_result::VideoDecodeResult::Uninitialized);
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct FrameSenderTiming {
    pub capture_at: Option<Instant>,
    pub capture_delay: Option<Duration>,
    pub encode_delay: Option<Duration>,
    pub pacer_delay: Option<Duration>,
    pub sending_delay: Option<Duration>,
    pub transport_delay: Option<Duration>,
}

#[derive(Clone)]
pub(crate) enum VideoFrameSink {
    Unbounded {
        sender: mpsc::UnboundedSender<EncodedVideoFrame>,
        wake: std::thread::Thread,
    },
}

impl VideoFrameSink {
    pub(super) fn send(&self, frame: EncodedVideoFrame) -> bool {
        match self {
            Self::Unbounded { sender, wake } => {
                let sent = sender.send(frame).is_ok();
                if sent {
                    wake.unpark();
                }
                sent
            }
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct PlayoutDelay {
    pub min: Duration,
    pub max: Duration,
}

impl RtpForwarder {
    pub(crate) fn select_registered_video(&mut self, index: i32) -> Option<ForwardedTrack> {
        let source = self.tracks.get(index)?;
        let metadata = source.metadata.clone();
        self.selected_video = Some(source);
        Some(metadata)
    }
    pub(crate) fn selected_metadata(&self) -> Option<ForwardedTrack> {
        self.selected_video.as_ref().map(|v| v.metadata.clone())
    }
    pub async fn next_track(&mut self) -> Option<ForwardedTrack> {
        let track = self.announcements.recv().await?;
        if track.kind == MediaKind::Video && self.selected_video.is_none() {
            self.selected_video = track
                .id
                .strip_prefix("video_")
                .and_then(|id| id.parse().ok())
                .and_then(|index| self.tracks.get(index));
        }
        Some(track)
    }

    pub fn video_keyframe_generation(&self) -> u64 {
        self.selected_video
            .as_ref()
            .map_or(0, |track| *track.keyframes.borrow())
    }

    pub async fn wait_for_video_keyframe_after(&mut self, generation: u64, wait: Duration) -> bool {
        let Some(track) = &self.selected_video else {
            return false;
        };
        let mut video_keyframes = track.keyframes.clone();
        tokio::time::timeout(wait, async {
            loop {
                if *video_keyframes.borrow() > generation {
                    return true;
                }
                if video_keyframes.changed().await.is_err() {
                    return false;
                }
                video_keyframes.borrow_and_update();
            }
        })
        .await
        .unwrap_or(false)
    }

    pub(crate) async fn add_video_sink(&self, sink: VideoFrameSink) {
        self.selected_video
            .as_ref()
            .expect("video selected before attaching sink")
            .add_sink(sink)
            .await;
    }

    pub(crate) fn video_receiver_feedback(&self) -> mpsc::UnboundedSender<VideoReceiverFeedback> {
        self.selected_video
            .as_ref()
            .expect("video selected before attaching decoder")
            .feedback
            .clone()
    }

    pub fn start(&self) {
        self.forwarding_started.send_replace(true);
        if let Some(track) = &self.selected_video {
            track.start();
        }
    }
    pub(crate) fn start_audio(&self) {
        self.forwarding_started.send_replace(true);
    }
}

impl Default for RtpForwardConfig {
    fn default() -> Self {
        Self {
            video_track_id: Some("video_0".to_owned()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::PicturePath;

    #[test]
    fn the_wanted_connection_takes_over_at_its_keyframe() {
        let picture = PicturePath::new(crate::diagnostics::performance::PerformanceMonitor::new(""));
        assert!(picture.admit(0, false));
        // Joining a room: the forwarder's frames wait for a keyframe while
        // the host's go on.
        picture.want(1);
        assert!(!picture.admit(1, false));
        assert!(picture.admit(0, false));
        assert!(picture.needed(1) && picture.needed(0));
        assert!(picture.admit(1, true));
        assert_eq!(picture.shown(), 1);
        assert!(!picture.admit(0, true));
        assert!(!picture.needed(0));
        // Another layer through a second connection.
        picture.want(2);
        assert!(picture.admit(1, false));
        assert!(!picture.admit(2, false));
        assert!(picture.admit(2, true));
        assert!(!picture.admit(1, true) && !picture.needed(1));
        // Leaving: back to the host's at its keyframe.
        picture.want(0);
        assert!(picture.admit(2, false));
        assert!(!picture.admit(0, false));
        assert!(picture.admit(0, true));
        assert_eq!(picture.shown(), 0);
        assert!(!picture.admit(2, true));
    }
}
