//! Media receive loops and delivery of complete video frames.
use super::clock::{RemoteNtpEstimator, frame_sender_timing};
use super::feedback::{
    ACTIVE_STREAM_WINDOW, ArrivalCounter, KEYFRAME_PACKET_WINDOW, NackRequester,
    RTP_NACK_PROCESS_INTERVAL, RtcpFeedbackBuffer,
};
use super::ingress::{
    MediaRecoveryFlags, OrderedVideoIngress, PendingMediaPacket, VideoIngressOrigin,
    VideoInterceptorDrainers, parse_rsfec_recovered_packets, recover_rtx_packet,
    remember_fec_source,
};
use super::negotiation::{
    REPAIRED_RTP_STREAM_ID_URI, RTP_STREAM_ID_URI, VIDEO_CAPTURE_INDEX_URI,
    VIDEO_FRAME_SENDING_DELAY_URI, VIDEO_IS_NEW_FRAME_URI, VIDEO_TIMING_URI,
};
use super::tracks::{
    DecodeCompletion, EncodedVideoFrame, MediaKind, PlayoutDelay, VideoFrameSink,
    VideoReceiverFeedback,
};
use super::workers::SessionWorkers;
use crate::diagnostics::performance::PerformanceMonitor;
use crate::media::VideoCodec;
use crate::transport::rsfec::{RsFecConfig, RsFecReceiver, normalize_rtx_source};
use crate::transport::video_receiver::{
    ReceiverResult, VideoCodecKind, VideoHeaderExtensions, VideoStreamReceiver,
};
use bytes::Bytes;
use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex as StdMutex, Weak};
use std::time::{Duration, Instant};
use tokio::sync::{Mutex, mpsc, watch};
use tokio_util::sync::CancellationToken;
use webrtc::peer_connection::RTCPeerConnection;
use webrtc::peer_connection::peer_connection_state::RTCPeerConnectionState;
use webrtc::rtp::packet::Packet as RtpPacket;

pub(super) struct TrackForwardContext {
    /// The track comes through the server's forwarder.
    pub(super) forwarded: bool,
    pub(super) workers: Weak<SessionWorkers>,
    pub(super) stop: CancellationToken,
    pub(super) kind: MediaKind,
    pub(super) codec: String,
    pub(super) codec_fmtp: String,
    pub(super) video_payload_codecs: HashMap<u8, (String, String)>,
    pub(super) rtx_payload_apt: HashMap<u8, u8>,
    pub(super) rsfec_config: Option<RsFecConfig>,
    pub(super) extmap_allow_mixed: bool,
    pub(super) audio: crate::media::audio::AudioPlayback,
    pub(super) audio_generation: Option<u64>,
    pub(super) connection: Arc<RTCPeerConnection>,
    pub(super) video_annexb_sinks: Arc<Mutex<Vec<VideoFrameSink>>>,
    pub(super) performance: PerformanceMonitor,
    pub(super) nack_rtt_micros: Arc<AtomicU64>,
    pub(super) remote_ntp: Arc<StdMutex<RemoteNtpEstimator>>,
    pub(super) receiver_feedback: Option<mpsc::UnboundedReceiver<VideoReceiverFeedback>>,
    pub(super) receiver_feedback_sender: mpsc::UnboundedSender<VideoReceiverFeedback>,
    /// Carries `VideoFeedback` to the host.
    pub(super) stream_control: crate::features::stream_control::StreamControlHandle,
    /// End-to-end forwarding: frames are decrypted before decoding.
    pub(super) decrypt: Option<Arc<StdMutex<crate::media::frame_crypto::Decryptor>>>,
    /// Whether this connection's frames of the track are shown; `source`
    /// names the connection (0: the host's).
    pub(super) picture: Arc<super::tracks::PicturePath>,
    pub(super) source: u64,
}

pub(super) async fn forward_remote_track(
    track: Arc<webrtc::track::track_remote::TrackRemote>,
    mut forwarding_ready: watch::Receiver<bool>,
    video_keyframe_tx: watch::Sender<u64>,
    context: TrackForwardContext,
) {
    let TrackForwardContext {
        forwarded,
        workers,
        stop,
        kind,
        codec,
        codec_fmtp,
        video_payload_codecs,
        rtx_payload_apt,
        rsfec_config,
        extmap_allow_mixed,
        audio,
        audio_generation,
        connection,
        video_annexb_sinks,
        performance,
        nack_rtt_micros,
        remote_ntp,
        receiver_feedback,
        receiver_feedback_sender,
        stream_control,
        decrypt,
        picture,
        source,
    } = context;

    if kind == MediaKind::Video {
        let Some(receiver_feedback) = receiver_feedback else {
            tracing::error!("video receiver feedback channel was not installed");
            return;
        };
        forward_video_track(
            track,
            codec,
            codec_fmtp,
            video_payload_codecs,
            rtx_payload_apt,
            rsfec_config,
            extmap_allow_mixed,
            video_keyframe_tx,
            connection,
            performance,
            nack_rtt_micros,
            remote_ntp,
            receiver_feedback,
            receiver_feedback_sender,
            forwarding_ready,
            video_annexb_sinks,
            workers,
            stop,
            audio.clone(),
            stream_control,
            decrypt,
            forwarded,
            picture,
            source,
        )
        .await;
        return;
    }

    let mut first_packet = true;
    let mut startup_packet_count = 0_usize;

    while !*forwarding_ready.borrow() {
        tokio::select! {
            changed = forwarding_ready.changed() => {
                if changed.is_err() {
                    return;
                }
            }
            received = track.read_rtp() => {
                let Ok((packet, _)) = received else {
                    tracing::debug!(?kind, "remote RTP track ended during startup buffering");
                    return;
                };
                if first_packet {
                    first_packet = false;
                    tracing::debug!(
                        ?kind,
                        sequence_number = packet.header.sequence_number,
                        timestamp = packet.header.timestamp,
                        marker = packet.header.marker,
                        payload_bytes = packet.payload.len(),
                        "first remote RTP packet received"
                    );
                }
                startup_packet_count += 1;
            }
        }
    }

    tracing::debug!(
        ?kind,
        startup_packet_count,
        "discarded pre-player RTP packets"
    );
    let Some(generation) = audio_generation else {
        return;
    };
    while let Ok((packet, _)) = track.read_rtp().await {
        audio.receive(
            generation,
            packet.payload,
            packet.header.timestamp,
            packet.header.sequence_number,
        );
    }
    tracing::debug!(?kind, "remote audio track ended");
}

#[allow(clippy::too_many_arguments)]
pub(super) async fn forward_video_track(
    track: Arc<webrtc::track::track_remote::TrackRemote>,
    codec: String,
    codec_fmtp: String,
    video_payload_codecs: HashMap<u8, (String, String)>,
    rtx_payload_apt: HashMap<u8, u8>,
    rsfec_config: Option<RsFecConfig>,
    extmap_allow_mixed: bool,
    video_keyframe_tx: watch::Sender<u64>,
    connection: Arc<RTCPeerConnection>,
    performance: PerformanceMonitor,
    nack_rtt_micros: Arc<AtomicU64>,
    remote_ntp: Arc<StdMutex<RemoteNtpEstimator>>,
    mut receiver_feedback: mpsc::UnboundedReceiver<VideoReceiverFeedback>,
    receiver_feedback_sender: mpsc::UnboundedSender<VideoReceiverFeedback>,
    mut forwarding_ready: watch::Receiver<bool>,
    video_annexb_sinks: Arc<Mutex<Vec<VideoFrameSink>>>,
    workers: Weak<SessionWorkers>,
    stop: CancellationToken,
    // Carries the session's cinema-mode switch.
    audio: crate::media::audio::AudioPlayback,
    stream_control: crate::features::stream_control::StreamControlHandle,
    decrypt: Option<Arc<StdMutex<crate::media::frame_crypto::Decryptor>>>,
    forwarded: bool,
    picture: Arc<super::tracks::PicturePath>,
    source: u64,
) {
    let mut ingress = match OrderedVideoIngress::open(&track).await {
        Ok(ingress) => ingress,
        Err(error) => {
            tracing::error!(%error, "open ordered video ingress failed");
            return;
        }
    };
    let _interceptor_drainers = {
        let Some(workers) = workers.upgrade() else {
            return;
        };
        VideoInterceptorDrainers::start(
            Arc::clone(&track),
            ingress.rtx_ssrc,
            ingress.fec_ssrc,
            &workers,
            &stop,
        )
    };
    let mut startup_packets = [0_u64; 3];
    while !*forwarding_ready.borrow() {
        tokio::select! {
            changed = forwarding_ready.changed() => {
                if changed.is_err() {
                    return;
                }
            }
            incoming = ingress.recv() => {
                let Ok(incoming) = incoming else {
                    tracing::error!(error = ?incoming.err(), "ordered video ingress ended before viewer start");
                    return;
                };
                let index = match incoming.origin {
                    VideoIngressOrigin::Primary => 0,
                    VideoIngressOrigin::Rtx => 1,
                    VideoIngressOrigin::RsFec => 2,
                };
                startup_packets[index] = startup_packets[index].saturating_add(1);
            }
        }
    }
    tracing::debug!(
        primary = startup_packets[0],
        rtx = startup_packets[1],
        rsfec = startup_packets[2],
        "drained all associated RTP streams before viewer start"
    );
    let mut video_payload_codecs = video_payload_codecs;
    video_payload_codecs
        .entry(track.payload_type())
        .or_insert_with(|| (codec.clone(), codec_fmtp.clone()));
    tracing::info!(track_id = %track.id(),
        new_picture_extension_id = ?track.header_extension_id(VIDEO_IS_NEW_FRAME_URI),
        "picture-update metadata negotiated");
    let receiver = match VideoStreamReceiver::new(
        &codec,
        VideoHeaderExtensions {
            capture_index: track.header_extension_id(VIDEO_CAPTURE_INDEX_URI),
            is_new_picture: track.header_extension_id(VIDEO_IS_NEW_FRAME_URI),
            timing: track.header_extension_id(VIDEO_TIMING_URI),
            sending_delay: track.header_extension_id(VIDEO_FRAME_SENDING_DELAY_URI),
            color_space: track.header_extension_id(crate::media::video_color::COLOR_SPACE_URI),
            frame_meta: track.header_extension_id(crate::media::frame_meta::URI),
        },
    ) {
        Ok(receiver) => receiver,
        Err(error) => {
            tracing::error!(%error, %codec, "create video receiver failed");
            return;
        }
    };
    let mutable_extensions = [
        (
            "http://www.ietf.org/id/draft-holmer-rmcat-transport-wide-cc-extensions-01",
            0,
        ),
        ("urn:farwing:rtp-hdrext:sending-delay", 0),
        (
            "http://www.webrtc.org/experiments/rtp-hdrext/video-timing",
            7,
        ),
    ]
    .into_iter()
    .filter_map(|(uri, preserve)| Some((track.header_extension_id(uri)?, preserve)))
    .collect::<Vec<_>>();
    let mut nack_interval = tokio::time::interval_at(
        tokio::time::Instant::now() + RTP_NACK_PROCESS_INTERVAL,
        RTP_NACK_PROCESS_INTERVAL,
    );
    nack_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut video = VideoReceive {
        fec_receiver: rsfec_config.map(|config| {
            RsFecReceiver::new(track.ssrc(), config.max_k, mutable_extensions.clone())
        }),
        loss_recovery: super::recovery::LossRecovery::new(stream_control, track.ssrc()),
        // `dev video --loss`: losses no repair can undo.
        impairment: super::impair::Injector::new(),
        rid_extension_id: track.header_extension_id(RTP_STREAM_ID_URI),
        repaired_rid_extension_id: track.header_extension_id(REPAIRED_RTP_STREAM_ID_URI),
        receiver,
        nack_requester: NackRequester::new(),
        arrivals: ArrivalCounter::default(),
        pending_media: VecDeque::new(),
        rtcp_feedback: RtcpFeedbackBuffer::default(),
        last_packet_received_at: None,
        last_keyframe_packet_at: None,
        last_keyframe_timestamp: None,
        track,
        connection,
        performance,
        nack_rtt_micros,
        remote_ntp,
        receiver_feedback_sender,
        video_annexb_sinks,
        video_keyframe_tx,
        decrypt,
        picture,
        source,
        audio,
        forwarded,
        video_payload_codecs,
        rtx_payload_apt,
        rsfec_config,
        extmap_allow_mixed,
        mutable_extensions,
    };

    loop {
        if !*forwarding_ready.borrow() {
            video.reset_after_pause();
            while !*forwarding_ready.borrow() {
                tokio::select! {
                    _ = stop.cancelled() => return,
                    changed = forwarding_ready.changed() => if changed.is_err() { return; },
                    incoming = ingress.recv() => if incoming.is_err() { return; },
                    feedback = receiver_feedback.recv() => if feedback.is_none() { return; },
                }
            }
            // Do not replay compressed frames or old decoder completions.
            while receiver_feedback.try_recv().is_ok() {}
        }
        video
            .performance
            .set_ingress_queue_packets(ingress.receiver.len());
        enum Event {
            Media(PendingMediaPacket),
            Fec(RtpPacket, Instant),
            Feedback(VideoReceiverFeedback),
            NackTick,
            ReceiverDeadline,
        }
        let receiver_deadline = video.receiver.next_deadline();
        let event = if let Some(packet) = video.pending_media.pop_front() {
            Event::Media(packet)
        } else {
            tokio::select! {
                    changed = forwarding_ready.changed() => {
                        if changed.is_err() { break; }
                        continue;
                    }
                    incoming = ingress.recv() => {
                        let incoming = match incoming {
                            Ok(incoming) => incoming,
                            Err(error) => {
                            if video.connection.connection_state() == RTCPeerConnectionState::Closed
                                || error.to_string() == "ordered RTP ingress closed"
                            {
                                tracing::debug!(%error, "ordered video ingress closed with the peer connection");
                            } else {
                                tracing::error!(%error, "ordered video ingress failed");
                            }
                            break;
                        }
                    };
                    tracing::trace!(
                        ordinal = incoming.ordinal,
                        origin = ?incoming.origin,
                        "processing ordered video RTP"
                    );
                    // Account for each received datagram once, before recovery.
                    // Locally reconstructed FEC packets are not additional traffic;
                    // RTX/FEC/padding still consume bandwidth even if rejected later.
                    video.performance.record_rtp_packet(incoming.wire_bytes);
                    match incoming.origin {
                        VideoIngressOrigin::Primary => Event::Media(PendingMediaPacket {
                            packet: incoming.packet,
                            flags: MediaRecoveryFlags::default(),
                            received_at: incoming.received_at,
                            rsfec_source: Some(incoming.raw),
                        }),
                        VideoIngressOrigin::Rtx => Event::Media(PendingMediaPacket {
                            packet: incoming.packet,
                            flags: MediaRecoveryFlags {
                                recovered_from_rtx: true,
                                recovered_by_fec: false,
                            },
                            received_at: incoming.received_at,
                            rsfec_source: None,
                        }),
                        VideoIngressOrigin::RsFec => Event::Fec(
                            incoming.packet,
                            incoming.received_at,
                        ),
                    }
                }
                feedback = receiver_feedback.recv() => {
                    let Some(feedback) = feedback else { break; };
                    Event::Feedback(feedback)
                }
                _ = nack_interval.tick() => Event::NackTick,
                _ = async {
                    // A zero or expired deadline still posts a ready task. A Tokio
                    // timer rounds up to milliseconds and would add one tick to
                    // every 0/0 frame even though no wait was requested.
                    if receiver_deadline > Instant::now() {
                        tokio::time::sleep_until(receiver_deadline.into()).await;
                    }
                } => Event::ReceiverDeadline
            }
        };

        match event {
            Event::NackTick => video.on_nack_tick().await,
            Event::ReceiverDeadline => video.on_deadline().await,
            Event::Feedback(VideoReceiverFeedback::DecodeTiming {
                duration,
                finished_at,
            }) => video.receiver.record_decode(duration, finished_at),
            Event::Feedback(VideoReceiverFeedback::DecoderFinished { frame_id, result }) => {
                video.on_decoded(frame_id, result).await
            }
            Event::Fec(packet, received_at) => video.on_fec(packet, received_at),
            Event::Media(media) => video.on_media(media).await,
        }
    }
    tracing::debug!("remote video track ended");
}

/// The receive state of one video track between its RTP ingress and its
/// decoder sinks: repair (NACK, RTX, RS-FEC), assembly, keyframe and
/// recovery requests, and dispatch. `forward_video_track` drives it.
struct VideoReceive {
    track: Arc<webrtc::track::track_remote::TrackRemote>,
    connection: Arc<RTCPeerConnection>,
    performance: PerformanceMonitor,
    nack_rtt_micros: Arc<AtomicU64>,
    remote_ntp: Arc<StdMutex<RemoteNtpEstimator>>,
    receiver_feedback_sender: mpsc::UnboundedSender<VideoReceiverFeedback>,
    video_annexb_sinks: Arc<Mutex<Vec<VideoFrameSink>>>,
    video_keyframe_tx: watch::Sender<u64>,
    decrypt: Option<Arc<StdMutex<crate::media::frame_crypto::Decryptor>>>,
    picture: Arc<super::tracks::PicturePath>,
    source: u64,
    audio: crate::media::audio::AudioPlayback,
    forwarded: bool,
    video_payload_codecs: HashMap<u8, (String, String)>,
    rtx_payload_apt: HashMap<u8, u8>,
    rsfec_config: Option<RsFecConfig>,
    extmap_allow_mixed: bool,
    mutable_extensions: Vec<(u8, usize)>,
    rid_extension_id: Option<u8>,
    repaired_rid_extension_id: Option<u8>,
    receiver: VideoStreamReceiver,
    nack_requester: NackRequester,
    arrivals: ArrivalCounter,
    loss_recovery: super::recovery::LossRecovery,
    impairment: Option<super::impair::Injector>,
    fec_receiver: Option<RsFecReceiver>,
    pending_media: VecDeque<PendingMediaPacket>,
    rtcp_feedback: RtcpFeedbackBuffer,
    last_packet_received_at: Option<Instant>,
    last_keyframe_packet_at: Option<Instant>,
    last_keyframe_timestamp: Option<u32>,
}

impl VideoReceive {
    /// The stream paused: what is buffered or pending is stale.
    fn reset_after_pause(&mut self) {
        self.pending_media.clear();
        self.receiver.reset_after_pause();
        // The unrepaired count is cumulative for the whole stream.
        let unrepaired = self.nack_requester.final_lost_packets();
        self.nack_requester = NackRequester::new();
        self.nack_requester.final_lost_packets = unrepaired;
        self.arrivals = ArrivalCounter::default();
        self.fec_receiver = self.rsfec_config.map(|c| {
            RsFecReceiver::new(self.track.ssrc(), c.max_k, self.mutable_extensions.clone())
        });
        self.rtcp_feedback = RtcpFeedbackBuffer::default();
        self.last_packet_received_at = None;
        self.last_keyframe_packet_at = None;
        self.last_keyframe_timestamp = None;
        self.performance.set_frame_buffer_frames(0);
        self.performance.set_nack_state(0, unrepaired);
    }

    async fn on_nack_tick(&mut self) {
        self.nack_requester.set_rtt(
            Duration::from_micros(self.nack_rtt_micros.load(Ordering::Relaxed))
                + super::impair::extra_rtt(),
        );
        let mut batch = self.nack_requester.process();
        recover_instead(
            &mut batch,
            &mut self.loss_recovery,
            &self.receiver,
            self.nack_requester.rtt,
        );
        batch.request_keyframe |= self.loss_recovery.expired(
            self.receiver.recovery_snapshot(),
            self.nack_requester.rtt,
            self.receiver.playout_delay(),
            Instant::now(),
        );
        self.rtcp_feedback.buffer(batch);
        self.performance.set_nack_state(
            self.nack_requester.outstanding(),
            self.nack_requester.final_lost_packets(),
        );
        self.rtcp_feedback
            .flush(
                &self.connection,
                self.track.ssrc(),
                self.nack_requester.rtt,
                self.picture.needed(self.source),
            )
            .await;
    }

    async fn on_deadline(&mut self) {
        let now = Instant::now();
        let active = self.last_packet_received_at.is_some_and(|received_at| {
            now.saturating_duration_since(received_at) < ACTIVE_STREAM_WINDOW
        });
        let receiving_keyframe = self.last_keyframe_packet_at.is_some_and(|received_at| {
            now.saturating_duration_since(received_at) < KEYFRAME_PACKET_WINDOW
        });
        let result = self.receiver.poll(active, receiving_keyframe);
        self.emit(result).await;
    }

    async fn on_decoded(
        &mut self,
        frame_id: i64,
        result: crate::media::decoder_result::VideoDecodeResult,
    ) {
        let result = self.receiver.decoder_finished(frame_id, result);
        if let Some(sequence_number) = result.continuous_sequence {
            self.nack_requester.clear_up_to(sequence_number);
            self.performance.set_nack_state(
                self.nack_requester.outstanding(),
                self.nack_requester.final_lost_packets(),
            );
        }
        self.emit(result).await;
    }

    fn on_fec(&mut self, packet: RtpPacket, received_at: Instant) {
        self.performance.record_fec_packet_received();
        let Some(fec) = self.fec_receiver.as_mut() else {
            return;
        };
        match fec.receive_repair(&packet.payload) {
            Ok(recovery) => {
                self.performance
                    .record_fec_recovered(recovery.recovered_packets.len());
                self.pending_media.extend(parse_rsfec_recovered_packets(
                    recovery.recovered_packets,
                    received_at,
                ));
            }
            Err(error) => tracing::warn!(%error, "RSFEC repair packet rejected"),
        }
    }

    async fn on_media(&mut self, mut media: PendingMediaPacket) {
        if media.flags.recovered_from_rtx {
            let Some(primary_payload_type) = self
                .rtx_payload_apt
                .get(&media.packet.header.payload_type)
                .copied()
            else {
                self.performance.record_rtx_packet(false);
                return;
            };
            let Some(packet) =
                recover_rtx_packet(media.packet, primary_payload_type, self.track.ssrc())
            else {
                self.performance.record_rtx_packet(false);
                return;
            };
            media.rsfec_source = if self.rsfec_config.is_some_and(|config| config.rtx_as_source) {
                match normalize_rtx_source(
                    &packet,
                    self.rid_extension_id,
                    self.repaired_rid_extension_id,
                    self.extmap_allow_mixed,
                ) {
                    Ok(source) => Some(source),
                    Err(error) => {
                        tracing::debug!(%error, "RTX media retained; its RSFEC source normalization failed");
                        None
                    }
                }
            } else {
                None
            };
            media.packet = packet;
        }

        if media.packet.header.ssrc != self.track.ssrc() {
            tracing::debug!(
                ssrc = media.packet.header.ssrc,
                "recovered RTP belongs to another receive stream"
            );
            return;
        }

        let recovered = media.flags.recovered_from_rtx || media.flags.recovered_by_fec;
        if let Some(injector) = self.impairment.as_mut()
            && injector.drops(
                media.packet.header.sequence_number,
                media.packet.header.timestamp,
                recovered,
                media.received_at,
            )
        {
            return;
        }
        let sequence_number = media.packet.header.sequence_number;
        if !recovered {
            self.performance
                .record_video_rtp_arrival(media.packet.header.timestamp);
            self.performance
                .record_primary_arrival(self.arrivals.expected_by(sequence_number));
        }
        let payload_type = media.packet.header.payload_type;
        // A padded RTP datagram is not necessarily an empty packet:
        // the packet parser has already removed its trailing padding.
        let is_padding = media.packet.payload.is_empty();
        let result;

        if is_padding {
            result = self.receiver.receive_padding(sequence_number);
            self.nack_requester.set_rtt(
                Duration::from_micros(self.nack_rtt_micros.load(Ordering::Relaxed))
                    + super::impair::extra_rtt(),
            );
            let mut nack = self
                .nack_requester
                .on_received(sequence_number, false, false);
            recover_instead(
                &mut nack.batch,
                &mut self.loss_recovery,
                &self.receiver,
                self.nack_requester.rtt,
            );
            self.rtcp_feedback.buffer(nack.batch);
        } else {
            let Some((mime, _)) = self.video_payload_codecs.get(&payload_type) else {
                tracing::debug!(payload_type, "ignoring unrecognized video payload type");
                if let Some(fec_source) = media.rsfec_source.take() {
                    remember_fec_source(
                        &mut self.fec_receiver,
                        &mut self.pending_media,
                        sequence_number,
                        fec_source,
                        media.received_at,
                        &self.performance,
                    );
                }
                return;
            };
            let packet_codec = if mime.eq_ignore_ascii_case("video/H264") {
                VideoCodecKind::H264
            } else if mime.eq_ignore_ascii_case("video/AV1") {
                VideoCodecKind::Av1
            } else if mime.eq_ignore_ascii_case("video/H265")
                || mime.eq_ignore_ascii_case("video/HEVC")
            {
                VideoCodecKind::H265
            } else {
                tracing::debug!(payload_type, %mime, "ignoring unsupported video codec");
                if let Some(fec_source) = media.rsfec_source.take() {
                    remember_fec_source(
                        &mut self.fec_receiver,
                        &mut self.pending_media,
                        sequence_number,
                        fec_source,
                        media.received_at,
                        &self.performance,
                    );
                }
                return;
            };
            let Some(mut parsed) =
                self.receiver
                    .parse_video_packet(&media.packet, media.received_at, packet_codec)
            else {
                tracing::debug!(
                    sequence_number,
                    payload_type,
                    recovered,
                    "video payload depacketizer rejected packet before NACK"
                );
                if let Some(fec_source) = media.rsfec_source.take() {
                    remember_fec_source(
                        &mut self.fec_receiver,
                        &mut self.pending_media,
                        sequence_number,
                        fec_source,
                        media.received_at,
                        &self.performance,
                    );
                }
                if media.flags.recovered_from_rtx {
                    self.performance.record_rtx_packet(false);
                }
                return;
            };

            if !recovered {
                let now = Instant::now();
                self.last_packet_received_at = Some(now);
                if parsed.starts_keyframe()
                    || self.last_keyframe_timestamp == Some(media.packet.header.timestamp)
                {
                    self.last_keyframe_timestamp = Some(media.packet.header.timestamp);
                    self.last_keyframe_packet_at = Some(now);
                    self.rtcp_feedback.keyframe_arrived();
                }
            }
            // The viewer decides its playout buffer: none for the
            // lowest latency, or the cinema-mode buffer.
            let cinema = self.audio.cinema();
            self.nack_requester
                .set_patience(if self.forwarded && cinema {
                    crate::media::audio::FORWARDED_PATIENCE
                } else {
                    Duration::ZERO
                });
            let playout_delay = Some(if cinema {
                PlayoutDelay {
                    min: crate::media::audio::CINEMA_VIDEO_DELAY,
                    max: if self.forwarded {
                        crate::media::audio::FORWARDED_CINEMA_MAX_DELAY
                    } else {
                        crate::media::audio::CINEMA_VIDEO_MAX_DELAY
                    },
                }
            } else {
                PlayoutDelay {
                    min: std::time::Duration::ZERO,
                    max: std::time::Duration::ZERO,
                }
            });

            self.nack_requester.set_rtt(
                Duration::from_micros(self.nack_rtt_micros.load(Ordering::Relaxed))
                    + super::impair::extra_rtt(),
            );
            let nack = self.nack_requester.on_received(
                parsed.sequence_number(),
                parsed.starts_keyframe(),
                recovered,
            );
            if nack.nack_count > 0 {
                tracing::debug!(target: "farwing::nack_audit", seq = parsed.sequence_number(),
                    requests = nack.nack_count, rtx = media.flags.recovered_from_rtx,
                    fec = media.flags.recovered_by_fec, "requested packet arrived");
            }
            parsed.set_receive_timing(playout_delay, nack.nack_count);
            let mut nack = nack;
            recover_instead(
                &mut nack.batch,
                &mut self.loss_recovery,
                &self.receiver,
                self.nack_requester.rtt,
            );
            self.rtcp_feedback.buffer(nack.batch);
            let prepared = self.receiver.prepare_video_packet(&mut parsed);
            // Parameter failure overrides this packet's buffered NACK.
            // Packet-buffer/complete-frame feedback occurs after this flush.
            self.rtcp_feedback.request_keyframe |= !prepared;
            self.rtcp_feedback
                .flush(
                    &self.connection,
                    self.track.ssrc(),
                    self.nack_requester.rtt,
                    self.picture.needed(self.source),
                )
                .await;
            result = if prepared {
                self.receiver.receive_prepared(parsed)
            } else {
                self.receiver.parameter_packet_rejected()
            };
        }

        self.performance.set_nack_state(
            self.nack_requester.outstanding(),
            self.nack_requester.final_lost_packets(),
        );
        if let Some(sequence_number) = result.continuous_sequence {
            self.nack_requester.clear_up_to(sequence_number);
            self.performance.set_nack_state(
                self.nack_requester.outstanding(),
                self.nack_requester.final_lost_packets(),
            );
        }
        if media.flags.recovered_from_rtx {
            self.performance.record_rtx_packet(result.accepted_packet);
        }
        self.emit(result).await;

        if let Some(fec_source) = media.rsfec_source.take() {
            remember_fec_source(
                &mut self.fec_receiver,
                &mut self.pending_media,
                sequence_number,
                fec_source,
                media.received_at,
                &self.performance,
            );
        }
    }

    /// What the frame assembler released or decided: recovery
    /// acknowledgements, NACK resets, keyframe requests and the frames for
    /// the decoder sinks.
    async fn emit(&mut self, result: ReceiverResult) {
        let snapshot = self.receiver.recovery_snapshot();
        let decrypt = self.decrypt.clone();
        let decrypt = decrypt.as_deref();
        if let Some(meta) = result.decoded_meta {
            self.loss_recovery
                .decoded(meta, result.marked_ltr, snapshot);
        }
        if result.clear_nack {
            self.nack_requester.clear_pending();
            self.rtcp_feedback.nack_sequences.clear();
            self.performance.set_nack_state(
                self.nack_requester.outstanding(),
                self.nack_requester.final_lost_packets(),
            );
        }
        // Without a window (it closed) nobody waits for the keyframe, nor for
        // one from a &self.connection whose frames are not shown: asking would cost
        // every forwarded viewer one.
        let needed =
            self.picture.needed(self.source) && !self.video_annexb_sinks.lock().await.is_empty();
        self.rtcp_feedback.request_keyframe |= result.request_keyframe;
        self.performance
            .record_predecode_drops(result.predecode_drops);
        self.performance
            .set_frame_buffer_frames(result.frame_buffer_frames);
        for frame in result.frames {
            // End-to-end forwarding: the frame as the host encoded it. Opened
            // first: only a frame that opens takes the self.picture over (one sealed
            // with a key this &self.connection does not hold yet would leave the
            // self.picture on frames it cannot show).
            let opened = match decrypt {
                Some(decrypt) if self.picture.admissible(self.source, frame.keyframe) => {
                    use crate::media::frame_crypto::Codec;
                    let codec = match frame.codec {
                        VideoCodecKind::H264 => Codec::H264,
                        VideoCodecKind::H265 => Codec::H265,
                        VideoCodecKind::Av1 => Codec::Av1,
                    };
                    let opened = decrypt
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner())
                        .decrypt(codec, &frame.data);
                    match opened {
                        Ok(plain) => Some(plain),
                        Err(error) => {
                            tracing::debug!(%error, "forwarded frame not decrypted; asking for a keyframe");
                            self.rtcp_feedback.request_keyframe = true;
                            None
                        }
                    }
                }
                _ => None,
            };
            // The other &self.connection's frames are shown: this one's are dropped
            // (completed undecoded) until it takes over at a keyframe.
            if (decrypt.is_some() && opened.is_none())
                || !self.picture.admit(self.source, frame.keyframe)
            {
                drop(DecodeCompletion::new(
                    frame.frame_id,
                    self.receiver_feedback_sender.clone(),
                ));
                continue;
            }
            self.performance.set_playout_timing(
                frame.schedule.target_delay,
                frame.schedule.jitter_delay,
                frame.schedule.low_latency,
            );
            tracing::trace!(rtp_timestamp = frame.rtp_timestamp, render_at = ?frame.schedule.render_at,
                "receiver released frame for decoding");
            let capture_at = frame
                .video_timing
                .filter(|timing| timing.flags & 4 != 0)
                .and_then(|_| {
                    self.remote_ntp
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner())
                        .estimate(frame.rtp_timestamp)
                });
            if frame.keyframe {
                tracing::trace!(target: "farwing::transport::rtc::clock", rtp_timestamp = frame.rtp_timestamp,
                    capture_age_ms = ?capture_at.map(|capture| {
                        if frame.last_received_at >= capture {
                            frame.last_received_at.duration_since(capture).as_secs_f64() * 1000.0
                        } else {
                            -capture.duration_since(frame.last_received_at).as_secs_f64() * 1000.0
                        }
                    }), "keyframe clock mapping");
            }
            let sender_timing = frame_sender_timing(
                capture_at,
                frame.video_timing,
                frame.frame_sending_delay_ms,
                frame.last_received_at,
            );
            // Time the complete frame waited in the cinema playout buffer: delay
            // the viewer chose, which the health policy does not count against
            // it. At low latency any wait is real delay.
            let playout_hold = if frame.schedule.low_latency {
                Duration::ZERO
            } else {
                Instant::now().saturating_duration_since(frame.assembled_at)
            };
            self.performance.record_received_frame(
                frame.last_received_at.duration_since(frame.received_at),
                frame.rtp_timestamp,
                frame.assembled_at,
                frame.keyframe,
                frame.frame_sending_delay_ms,
                playout_hold,
            );
            if frame.keyframe {
                self.video_keyframe_tx.send_modify(|value| *value += 1);
            }
            let data = match opened {
                Some(plain) => plain,
                None => frame.data,
            };
            let encoded = EncodedVideoFrame {
                completion: DecodeCompletion::new(
                    frame.frame_id,
                    self.receiver_feedback_sender.clone(),
                ),
                parameter_format: frame.parameter_format,
                color_space: frame.color_space,
                frame_id: frame.frame_id,
                data: Bytes::from(data),
                rtp_timestamp: frame.rtp_timestamp,
                received_at: frame.received_at,
                assembled_at: frame.assembled_at,
                keyframe: frame.keyframe,
                video_capture_index: frame.video_capture_index,
                is_new_picture: frame.is_new_picture,
                frame_meta: frame.frame_meta,
                sender_timing,
                codec: match frame.codec {
                    VideoCodecKind::H264 => VideoCodec::H264,
                    VideoCodecKind::H265 => VideoCodec::H265,
                    VideoCodecKind::Av1 => VideoCodec::Av1,
                },
            };
            let mut sinks = self.video_annexb_sinks.lock().await;
            sinks.retain(|sink| sink.send(encoded.clone()));
            // A failed/cancelled window does not end the media track. If no sink
            // accepts this frame, its completion returns the admission token and
            // the existing receiver recovery remains available for a retry.
        }
        self.rtcp_feedback
            .flush(
                &self.connection,
                self.track.ssrc(),
                self.nack_requester.rtt,
                needed,
            )
            .await;
    }
}

#[allow(clippy::too_many_arguments)]
/// Repair of a loss gave up: ask for a recovery frame instead of a keyframe
/// when this receiver holds a long-term reference the host can use.
fn recover_instead(
    batch: &mut super::feedback::NackBatch,
    recovery: &mut super::recovery::LossRecovery,
    receiver: &VideoStreamReceiver,
    rtt: Duration,
) {
    let early = batch.late && super::recovery::early();
    if ((batch.request_keyframe && batch.loss) || early)
        && recovery.on_loss(
            receiver.recovery_snapshot(),
            rtt,
            receiver.playout_delay(),
            Instant::now(),
        )
    {
        batch.request_keyframe = false;
    }
}
