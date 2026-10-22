//! WebRTC peers standing in for a Farwing host and its viewers: the same
//! codec and extension tables, loopback candidates only, and direct access to
//! every sender stream (media, RTX and RS-FEC SSRCs).

use async_trait::async_trait;
use parking_lot::Mutex;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{mpsc, watch};
use webrtc::api::APIBuilder;
use webrtc::api::media_engine::{MIME_TYPE_H264, MIME_TYPE_OPUS, MediaEngine};
use webrtc::api::setting_engine::SettingEngine;
use webrtc::ice::network_type::NetworkType;
use webrtc::interceptor::registry::Registry;
use webrtc::interceptor::stream_info::StreamInfo;
use webrtc::interceptor::{
    Attributes, Interceptor, InterceptorBuilder, RTCPReader, RTCPWriter, RTPReader, RTPWriter,
};
use webrtc::peer_connection::RTCPeerConnection;
use webrtc::peer_connection::configuration::RTCConfiguration;
use webrtc::peer_connection::peer_connection_state::RTCPeerConnectionState;
use webrtc::peer_connection::sdp::session_description::RTCSessionDescription;
use webrtc::rtcp::packet::Packet as RtcpPacket;
use webrtc::rtp::packet::Packet;
use webrtc::rtp_transceiver::RTCRtpTransceiverInit;
use webrtc::rtp_transceiver::rtp_codec::{RTCRtpCodecCapability, RTPCodecType};
use webrtc::rtp_transceiver::rtp_transceiver_direction::RTCRtpTransceiverDirection;
use webrtc::track::track_local::track_local_static_rtp::TrackLocalStaticRTP;

pub type Rtcp = Vec<Box<dyn RtcpPacket + Send + Sync>>;

#[derive(Default)]
pub struct Capture {
    streams: Mutex<HashMap<u32, Arc<dyn RTPWriter + Send + Sync>>>,
}

struct CaptureBuilder(Arc<Capture>);

impl InterceptorBuilder for CaptureBuilder {
    fn build(
        &self,
        _: &str,
    ) -> Result<Arc<dyn Interceptor + Send + Sync>, webrtc::interceptor::Error> {
        Ok(self.0.clone())
    }
}

#[async_trait]
impl Interceptor for Capture {
    async fn bind_rtcp_reader(
        &self,
        reader: Arc<dyn RTCPReader + Send + Sync>,
    ) -> Arc<dyn RTCPReader + Send + Sync> {
        reader
    }
    async fn bind_rtcp_writer(
        &self,
        writer: Arc<dyn RTCPWriter + Send + Sync>,
    ) -> Arc<dyn RTCPWriter + Send + Sync> {
        writer
    }
    async fn bind_local_stream(
        &self,
        info: &StreamInfo,
        writer: Arc<dyn RTPWriter + Send + Sync>,
    ) -> Arc<dyn RTPWriter + Send + Sync> {
        self.streams.lock().insert(info.ssrc, writer.clone());
        writer
    }
    async fn unbind_local_stream(&self, _: &StreamInfo) {}
    async fn bind_remote_stream(
        &self,
        _: &StreamInfo,
        reader: Arc<dyn RTPReader + Send + Sync>,
    ) -> Arc<dyn RTPReader + Send + Sync> {
        reader
    }
    async fn unbind_remote_stream(&self, _: &StreamInfo) {}
    async fn close(&self) -> Result<(), webrtc::interceptor::Error> {
        Ok(())
    }
}

/// What a viewer's tracks deliver: `("media" | "rtx" | "fec", packet)`.
pub type Received = mpsc::UnboundedReceiver<(&'static str, Packet)>;

pub struct Peer {
    pub pc: Arc<RTCPeerConnection>,
    capture: Arc<Capture>,
    connected: watch::Receiver<RTCPeerConnectionState>,
}

/// A loopback peer with Farwing's tables; `dependency` registers the
/// dependency descriptor like a host does.
pub async fn peer(dependency: bool) -> Peer {
    let mut media = MediaEngine::default();
    farwing_rtc_wire::register_codecs(&mut media).unwrap();
    let extra: &[&str] = if dependency {
        &[farwing_rtc_wire::DEPENDENCY_DESCRIPTOR_URI]
    } else {
        &[]
    };
    farwing_rtc_wire::register_header_extensions(&mut media, extra).unwrap();
    let capture = Arc::new(Capture::default());
    let mut registry = Registry::new();
    registry.add(Box::new(CaptureBuilder(capture.clone())));
    let mut settings = SettingEngine::default();
    settings.set_include_loopback_candidate(true);
    settings.set_ip_filter(Box::new(|ip| ip.is_loopback()));
    settings.set_network_types(vec![NetworkType::Udp4]);
    settings.set_receive_mtu(2048);
    settings.enable_sender_rtx(true);
    settings.enable_sender_rsfec(true);
    let api = APIBuilder::new()
        .with_media_engine(media)
        .with_interceptor_registry(registry)
        .with_setting_engine(settings)
        .build();
    let pc = Arc::new(
        api.new_peer_connection(RTCConfiguration::default())
            .await
            .unwrap(),
    );
    let (state_tx, connected) = watch::channel(RTCPeerConnectionState::New);
    pc.on_peer_connection_state_change(Box::new(move |state| {
        let _ = state_tx.send(state);
        Box::pin(async {})
    }));
    Peer {
        pc,
        capture,
        connected,
    }
}

/// A host publishing `audio_0` (Opus) and `video_0` (H.264) as stream `host`.
pub async fn host() -> Peer {
    let peer = peer(true).await;
    for (mime, clock, channels, fmtp, id) in [
        (MIME_TYPE_OPUS, 48_000, 2, farwing_rtc_wire::OPUS_FMTP, "audio_0"),
        (
            MIME_TYPE_H264,
            90_000,
            0,
            "level-asymmetry-allowed=1;packetization-mode=1;profile-level-id=42e01f",
            "video_0",
        ),
    ] {
        let track = Arc::new(TrackLocalStaticRTP::new(
            RTCRtpCodecCapability {
                mime_type: mime.to_owned(),
                clock_rate: clock,
                channels,
                sdp_fmtp_line: fmtp.to_owned(),
                rtcp_feedback: Vec::new(),
            },
            id.to_owned(),
            "host".to_owned(),
        ));
        peer.pc
            .add_transceiver_from_track(
                track,
                Some(RTCRtpTransceiverInit {
                    direction: RTCRtpTransceiverDirection::Sendonly,
                    send_encodings: Vec::new(),
                }),
            )
            .await
            .unwrap();
    }
    peer
}

/// A viewer receiving `audio` audio and `video` video sections.
pub async fn viewer(audio: usize, video: usize) -> (Peer, Received, mpsc::UnboundedReceiver<Rtcp>) {
    let peer = peer(false).await;
    for (kind, count) in [(RTPCodecType::Audio, audio), (RTPCodecType::Video, video)] {
        for _ in 0..count {
            peer.pc
                .add_transceiver_from_kind(
                    kind,
                    Some(RTCRtpTransceiverInit {
                        direction: RTCRtpTransceiverDirection::Recvonly,
                        send_encodings: Vec::new(),
                    }),
                )
                .await
                .unwrap();
        }
    }
    let (tx, rx) = mpsc::unbounded_channel();
    let (rtcp_tx, rtcp_rx) = mpsc::unbounded_channel();
    peer.pc.on_track(Box::new(move |track, receiver, _| {
        let tx = tx.clone();
        let rtcp_tx = rtcp_tx.clone();
        Box::pin(async move {
            let media = (Arc::clone(&track), tx.clone());
            tokio::spawn(async move {
                while let Ok((packet, _)) = media.0.read_rtp().await {
                    let _ = media.1.send(("media", packet));
                }
            });
            if track.kind() == RTPCodecType::Video {
                let rtx = (Arc::clone(&track), tx.clone());
                tokio::spawn(async move {
                    while let Ok((packet, _)) = rtx.0.read_rtx().await {
                        let _ = rtx.1.send(("rtx", packet));
                    }
                });
                let fec = (Arc::clone(&track), tx);
                tokio::spawn(async move {
                    while let Ok((packet, _)) = fec.0.read_fec().await {
                        let _ = fec.1.send(("fec", packet));
                    }
                });
            }
            tokio::spawn(async move {
                while let Ok((packets, _)) = receiver.read_rtcp().await {
                    let _ = rtcp_tx.send(packets);
                }
            });
        })
    }));
    (peer, rx, rtcp_rx)
}

impl Peer {
    /// A complete offer (candidates gathered).
    pub async fn offer(&self) -> String {
        let offer = self.pc.create_offer(None).await.unwrap();
        let mut gathered = self.pc.gathering_complete_promise().await;
        self.pc.set_local_description(offer).await.unwrap();
        tokio::time::timeout(Duration::from_secs(5), gathered.recv())
            .await
            .expect("gathering");
        self.pc.local_description().await.unwrap().sdp
    }

    pub async fn answer(&self, sdp: String) {
        self.pc
            .set_remote_description(RTCSessionDescription::answer(sdp).unwrap())
            .await
            .unwrap();
    }

    pub async fn connected(&mut self) {
        tokio::time::timeout(
            Duration::from_secs(15),
            self.connected
                .wait_for(|s| *s == RTCPeerConnectionState::Connected),
        )
        .await
        .expect("connected in time")
        .expect("state channel");
    }

    pub fn state(&self) -> RTCPeerConnectionState {
        *self.connected.borrow()
    }

    /// Writes `packet` on the sender stream of its SSRC, unchanged.
    pub async fn write(&self, packet: &Packet) {
        let writer = self
            .capture
            .streams
            .lock()
            .get(&packet.header.ssrc)
            .cloned()
            .unwrap_or_else(|| panic!("no sender stream {}", packet.header.ssrc));
        writer.write(packet, &Attributes::new()).await.unwrap();
    }

    /// RTCP the first sender (the given kind's) receives.
    pub async fn sender_rtcp(&self, kind: RTPCodecType) -> mpsc::UnboundedReceiver<Rtcp> {
        let (tx, rx) = mpsc::unbounded_channel();
        for sender in self.pc.get_senders().await {
            let tx = tx.clone();
            if sender.track().await.is_some_and(|t| t.kind() == kind) {
                tokio::spawn(async move {
                    while let Ok((packets, _)) = sender.read_rtcp().await {
                        let _ = tx.send(packets);
                    }
                });
            }
        }
        rx
    }
}
