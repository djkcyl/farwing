//! Hearing the host's microphone (`docs/protocol/camera-design.md` §2): a
//! participant with `listen` asks with `ListenControl`; while the owner
//! allows it, the owner's microphone goes to the participant's `audio_1`
//! track.
use crate::media::microphone::{Encoding, Microphone};
use anyhow::Result;
use std::sync::Arc;
use webrtc::rtp_transceiver::rtp_sender::RTCRtpSender;
use webrtc::track::track_local::track_local_static_rtp::TrackLocalStaticRTP;

/// The `audio_1` track's stream id.
pub(crate) const TRACK: &str = "audio_1";

struct Running {
    microphone: Microphone,
    tasks: Vec<tokio::task::JoinHandle<()>>,
}

/// A participant's listening.
pub(crate) struct Listen {
    track: Arc<TrackLocalStaticRTP>,
    sender: Arc<RTCRtpSender>,
    connection: std::sync::Weak<webrtc::peer_connection::RTCPeerConnection>,
    running: Option<Running>,
    /// Kept with the camera viewers for the owner's notice and stop button.
    using: Option<Arc<super::cameras::Use>>,
}

impl Listen {
    pub(crate) fn new(
        track: Arc<TrackLocalStaticRTP>,
        sender: Arc<RTCRtpSender>,
        connection: std::sync::Weak<webrtc::peer_connection::RTCPeerConnection>,
    ) -> Self {
        Self {
            track,
            sender,
            connection,
            running: None,
            using: None,
        }
    }

    pub(crate) fn active(&self) -> bool {
        self.running.is_some()
    }

    /// Start sending `device` (`None`: the default recording device) to the
    /// participant's track, or stop.
    pub(crate) async fn set(&mut self, start: bool, device: Option<&str>, using: Option<Arc<super::cameras::Use>>) -> Result<()> {
        if !start {
            if let Some(running) = self.running.take() {
                running.microphone.close().await;
                for task in running.tasks {
                    task.abort();
                }
                tracing::info!("participant stopped listening to the microphone");
            }
            self.using = None;
            return Ok(());
        }
        if self.running.is_some() {
            return Ok(());
        }
        let microphone = Microphone::new();
        microphone.set_input_device(device.and_then(|d| d.parse().ok()));
        microphone.configure(Some(Encoding::negotiated(farwing_rtc_wire::OPUS_FMTP, Some(20))?));
        microphone.start()?;
        microphone.capture_now();
        let tasks = vec![
            tokio::spawn({
                let microphone = microphone.clone();
                let track = self.track.clone();
                async move { microphone.send(track).await }
            }),
            tokio::spawn({
                let microphone = microphone.clone();
                let sender = self.sender.clone();
                async move { microphone.feedback(sender).await }
            }),
            tokio::spawn({
                let microphone = microphone.clone();
                let connection = self.connection.clone();
                let sender = self.sender.clone();
                async move { microphone.send_reports(connection, sender).await }
            }),
        ];
        tracing::info!(device = device.unwrap_or("default"), "participant listens to the microphone");
        self.running = Some(Running { microphone, tasks });
        self.using = using;
        Ok(())
    }
}
