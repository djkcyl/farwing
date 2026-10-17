//! Media receive loops and delivery of complete video frames.
use super::clock::{RemoteNtpEstimator, frame_sender_timing};
use super::feedback::{
    ACTIVE_STREAM_WINDOW, KEYFRAME_PACKET_WINDOW, NackRequester, RTP_NACK_PROCESS_INTERVAL,
    RtcpFeedbackBuffer, send_full_intra_request, send_picture_loss_indication,
};
use super::ingress::{
    MediaRecoveryFlags, OrderedVideoIngress, PendingMediaPacket, RedPacket, VideoIngressOrigin,
    VideoInterceptorDrainers, parse_flexfec_recovered_packets, parse_rsfec_recovered_packets,
    parse_ulpfec_recovered_packets, recover_rtx_packet, remember_fec_source, unwrap_red_packet,
};
use super::negotiation::{
    PLAYOUT_DELAY_URI, REPAIRED_RTP_STREAM_ID_URI, RTP_STREAM_ID_URI, VIDEO_CAPTURE_INDEX_URI,
    VIDEO_CONTENT_TYPE_URI, VIDEO_FRAME_SENDING_DELAY_URI, VIDEO_IS_NEW_FRAME_URI,
    VIDEO_TIMING_URI,
};
use super::tracks::{
    DecodeCompletion, EncodedVideoFrame, MediaKind, PlayoutDelay, VideoFrameSink,
    VideoReceiverFeedback,
};
use super::workers::SessionWorkers;
use crate::diagnostics::performance::PerformanceMonitor;
use crate::media::VideoCodec;
use crate::transport::flexfec::FlexFecReceiver;
use crate::transport::official_receiver::{
    OfficialVideoReceiver, ReceiverResult, VideoCodecKind, VideoHeaderExtensions,
};
use crate::transport::rsfec::{RsFecConfig, RsFecReceiver, normalize_rtx_source};
use crate::transport::ulpfec::UlpfecReceiver;
use bytes::Bytes;
use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex as StdMutex, Weak};
use std::time::{Duration, Instant};
use tokio::sync::{Mutex, mpsc, watch};
use tokio_util::sync::CancellationToken;
use webrtc::peer_connection::RTCPeerConnection;
use webrtc::peer_connection::peer_connection_state::RTCPeerConnectionState;
use webrtc::rtp::packet::Packet as RtpPacket;
use webrtc::util::marshal::Marshal;

pub(super) fn parse_playout_delay(payload: &[u8]) -> Option<PlayoutDelay> {
    let [first, second, third] = payload else {
        return None;
    };
    let min_units = (u16::from(*first) << 4) | (u16::from(*second) >> 4);
    let max_units = (u16::from(*second & 0x0f) << 8) | u16::from(*third);
    if min_units > max_units {
        return None;
    }
    Some(PlayoutDelay {
        min: Duration::from_millis(u64::from(min_units) * 10),
        max: Duration::from_millis(u64::from(max_units) * 10),
    })
}

pub(super) struct TrackForwardContext {
    pub(super) workers: Weak<SessionWorkers>,
    pub(super) stop: CancellationToken,
    pub(super) kind: MediaKind,
    pub(super) codec: String,
    pub(super) codec_fmtp: String,
    pub(super) video_payload_codecs: HashMap<u8, (String, String)>,
    pub(super) rtx_payload_apt: HashMap<u8, u8>,
    pub(super) red_payload_types: HashSet<u8>,
    pub(super) ulpfec_payload_types: HashSet<u8>,
    pub(super) flexfec_payload_types: HashSet<u8>,
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
}

pub(super) async fn forward_remote_track(
    track: Arc<webrtc::track::track_remote::TrackRemote>,
    mut forwarding_ready: watch::Receiver<bool>,
    video_keyframe_tx: watch::Sender<u64>,
    context: TrackForwardContext,
) {
    let TrackForwardContext {
        workers,
        stop,
        kind,
        codec,
        codec_fmtp,
        video_payload_codecs,
        rtx_payload_apt,
        red_payload_types,
        ulpfec_payload_types,
        flexfec_payload_types,
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
    } = context;

    if kind == MediaKind::Video {
        let Some(receiver_feedback) = receiver_feedback else {
            tracing::error!("video receiver feedback channel was not installed");
            return;
        };
        let playout_delay_extension_id = track.header_extension_id(PLAYOUT_DELAY_URI);
        forward_official_video_track(
            track,
            codec,
            codec_fmtp,
            video_payload_codecs,
            rtx_payload_apt,
            red_payload_types,
            ulpfec_payload_types,
            flexfec_payload_types,
            rsfec_config,
            extmap_allow_mixed,
            video_keyframe_tx,
            connection,
            performance,
            nack_rtt_micros,
            remote_ntp,
            receiver_feedback,
            receiver_feedback_sender,
            playout_delay_extension_id,
            forwarding_ready,
            video_annexb_sinks,
            workers,
            stop,
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
pub(super) async fn forward_official_video_track(
    track: Arc<webrtc::track::track_remote::TrackRemote>,
    codec: String,
    codec_fmtp: String,
    video_payload_codecs: HashMap<u8, (String, String)>,
    rtx_payload_apt: HashMap<u8, u8>,
    red_payload_types: HashSet<u8>,
    ulpfec_payload_types: HashSet<u8>,
    flexfec_payload_types: HashSet<u8>,
    rsfec_config: Option<RsFecConfig>,
    extmap_allow_mixed: bool,
    video_keyframe_tx: watch::Sender<u64>,
    connection: Arc<RTCPeerConnection>,
    performance: PerformanceMonitor,
    nack_rtt_micros: Arc<AtomicU64>,
    remote_ntp: Arc<StdMutex<RemoteNtpEstimator>>,
    mut receiver_feedback: mpsc::UnboundedReceiver<VideoReceiverFeedback>,
    receiver_feedback_sender: mpsc::UnboundedSender<VideoReceiverFeedback>,
    playout_delay_extension_id: Option<u8>,
    mut forwarding_ready: watch::Receiver<bool>,
    video_annexb_sinks: Arc<Mutex<Vec<VideoFrameSink>>>,
    workers: Weak<SessionWorkers>,
    stop: CancellationToken,
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
        "UU picture-update metadata negotiated");
    let mut receiver = match OfficialVideoReceiver::new(
        &codec,
        VideoHeaderExtensions {
            orientation: track.header_extension_id("urn:3gpp:video-orientation"),
            content_type: track.header_extension_id(VIDEO_CONTENT_TYPE_URI),
            capture_index: track.header_extension_id(VIDEO_CAPTURE_INDEX_URI),
            is_new_picture: track.header_extension_id(VIDEO_IS_NEW_FRAME_URI),
            timing: track.header_extension_id(VIDEO_TIMING_URI),
            sending_delay: track.header_extension_id(VIDEO_FRAME_SENDING_DELAY_URI),
            color_space: track.header_extension_id(crate::media::video_color::COLOR_SPACE_URI),
        },
        &codec_fmtp,
    ) {
        Ok(receiver) => receiver,
        Err(error) => {
            tracing::error!(%error, %codec, "create official-compatible video receiver failed");
            return;
        }
    };
    let mut nack_requester = NackRequester::new();
    let mutable_extensions = [
        ("urn:ietf:params:rtp-hdrext:toffset", 0),
        (
            "http://www.webrtc.org/experiments/rtp-hdrext/abs-send-time",
            0,
        ),
        (
            "http://www.ietf.org/id/draft-holmer-rmcat-transport-wide-cc-extensions-01",
            0,
        ),
        (
            "http://www.webrtc.org/experiments/rtp-hdrext/video-frame-sending-delay",
            0,
        ),
        (
            "http://www.webrtc.org/experiments/rtp-hdrext/video-timing",
            7,
        ),
    ]
    .into_iter()
    .filter_map(|(uri, preserve)| Some((track.header_extension_id(uri)?, preserve)))
    .collect::<Vec<_>>();
    let mut fec_receiver = rsfec_config
        .map(|config| RsFecReceiver::new(track.ssrc(), config.max_k, mutable_extensions.clone()));
    let mut flexfec_receiver = FlexFecReceiver::new(track.ssrc(), mutable_extensions.clone());
    let mut ulpfec_receiver = UlpfecReceiver::new(track.ssrc(), mutable_extensions.clone());
    let mut pending_media = VecDeque::<PendingMediaPacket>::new();
    let mut configured_payload_type = track.payload_type();
    let mut rtcp_feedback = RtcpFeedbackBuffer::default();
    let rid_extension_id = track.header_extension_id(RTP_STREAM_ID_URI);
    let repaired_rid_extension_id = track.header_extension_id(REPAIRED_RTP_STREAM_ID_URI);
    let mut nack_interval = tokio::time::interval_at(
        tokio::time::Instant::now() + RTP_NACK_PROCESS_INTERVAL,
        RTP_NACK_PROCESS_INTERVAL,
    );
    nack_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut last_packet_received_at = None;
    let mut last_keyframe_packet_at = None;
    let mut last_keyframe_timestamp = None;

    loop {
        if !*forwarding_ready.borrow() {
            pending_media.clear();
            receiver.reset_after_pause();
            nack_requester = NackRequester::new();
            fec_receiver = rsfec_config
                .map(|c| RsFecReceiver::new(track.ssrc(), c.max_k, mutable_extensions.clone()));
            flexfec_receiver = FlexFecReceiver::new(track.ssrc(), mutable_extensions.clone());
            ulpfec_receiver = UlpfecReceiver::new(track.ssrc(), mutable_extensions.clone());
            rtcp_feedback = RtcpFeedbackBuffer::default();
            last_packet_received_at = None;
            last_keyframe_packet_at = None;
            last_keyframe_timestamp = None;
            performance.set_frame_buffer_frames(0);
            performance.set_nack_state(0, 0);
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
        performance.set_ingress_queue_packets(ingress.receiver.len());
        enum Event {
            Media(PendingMediaPacket),
            Fec(RtpPacket, Instant),
            Feedback(VideoReceiverFeedback),
            NackTick,
            ReceiverDeadline,
        }
        let receiver_deadline = receiver.next_deadline();
        let event = if let Some(packet) = pending_media.pop_front() {
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
                            if connection.connection_state() == RTCPeerConnectionState::Closed
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
                    performance.record_rtp_packet(incoming.wire_bytes);
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
                    // UU posts a ready task for a zero/expired deadline. A Tokio
                    // timer rounds up to milliseconds and would add one tick to
                    // every 0/0 frame even though no wait was requested.
                    if receiver_deadline > Instant::now() {
                        tokio::time::sleep_until(receiver_deadline.into()).await;
                    }
                } => Event::ReceiverDeadline
            }
        };

        match event {
            Event::NackTick => {
                nack_requester.set_rtt(Duration::from_micros(
                    nack_rtt_micros.load(Ordering::Relaxed),
                ));
                let batch = nack_requester.process();
                rtcp_feedback.buffer(batch);
                performance.set_nack_state(
                    nack_requester.outstanding(),
                    nack_requester.final_lost_packets(),
                );
                rtcp_feedback.flush(&connection, track.ssrc()).await;
            }
            Event::ReceiverDeadline => {
                let now = Instant::now();
                let active = last_packet_received_at.is_some_and(|received_at| {
                    now.saturating_duration_since(received_at) < ACTIVE_STREAM_WINDOW
                });
                let receiving_keyframe = last_keyframe_packet_at.is_some_and(|received_at| {
                    now.saturating_duration_since(received_at) < KEYFRAME_PACKET_WINDOW
                });
                let result = receiver.poll(active, receiving_keyframe);
                emit_official_receiver_result(
                    result,
                    &receiver_feedback_sender,
                    &mut nack_requester,
                    &mut rtcp_feedback,
                    &video_annexb_sinks,
                    &video_keyframe_tx,
                    &performance,
                    &remote_ntp,
                    &connection,
                    track.ssrc(),
                )
                .await;
            }
            Event::Feedback(VideoReceiverFeedback::DecodeTiming {
                duration,
                finished_at,
            }) => {
                receiver.record_decode(duration, finished_at);
            }
            Event::Feedback(VideoReceiverFeedback::DecoderFinished { frame_id, result }) => {
                let result = receiver.decoder_finished(frame_id, result);
                if let Some(sequence_number) = result.continuous_sequence {
                    nack_requester.clear_up_to(sequence_number);
                    performance.set_nack_state(
                        nack_requester.outstanding(),
                        nack_requester.final_lost_packets(),
                    );
                }
                emit_official_receiver_result(
                    result,
                    &receiver_feedback_sender,
                    &mut nack_requester,
                    &mut rtcp_feedback,
                    &video_annexb_sinks,
                    &video_keyframe_tx,
                    &performance,
                    &remote_ntp,
                    &connection,
                    track.ssrc(),
                )
                .await;
            }
            Event::Fec(packet, received_at) => {
                performance.record_fec_packet_received();
                if flexfec_payload_types.contains(&packet.header.payload_type) {
                    match flexfec_receiver.receive_repair(
                        packet.header.ssrc,
                        packet.header.sequence_number,
                        &packet.payload,
                    ) {
                        Ok(recovery) => {
                            performance.record_fec_recovered(recovery.len());
                            pending_media
                                .extend(parse_flexfec_recovered_packets(recovery, received_at));
                        }
                        Err(error) => tracing::debug!(%error, "FlexFEC repair packet rejected"),
                    }
                } else {
                    let Some(fec_receiver) = fec_receiver.as_mut() else {
                        continue;
                    };
                    match fec_receiver.receive_repair(&packet.payload) {
                        Ok(recovery) => {
                            performance.record_fec_recovered(recovery.recovered_packets.len());
                            pending_media.extend(parse_rsfec_recovered_packets(
                                recovery.recovered_packets,
                                received_at,
                            ));
                        }
                        Err(error) => tracing::warn!(%error, "RSFEC repair packet rejected"),
                    }
                }
            }
            Event::Media(mut media) => {
                if media.flags.recovered_from_rtx {
                    let Some(primary_payload_type) = rtx_payload_apt
                        .get(&media.packet.header.payload_type)
                        .copied()
                    else {
                        performance.record_rtx_packet(false);
                        continue;
                    };
                    let Some(packet) =
                        recover_rtx_packet(media.packet, primary_payload_type, track.ssrc())
                    else {
                        performance.record_rtx_packet(false);
                        continue;
                    };
                    media.rsfec_source = if rsfec_config.is_some_and(|config| config.rtx_as_source)
                    {
                        match normalize_rtx_source(
                            &packet,
                            rid_extension_id,
                            repaired_rid_extension_id,
                            extmap_allow_mixed,
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

                if media.packet.header.ssrc != track.ssrc() {
                    tracing::debug!(
                        ssrc = media.packet.header.ssrc,
                        "recovered RTP belongs to another receive stream"
                    );
                    continue;
                }

                if red_payload_types.contains(&media.packet.header.payload_type) {
                    match unwrap_red_packet(media.packet, &ulpfec_payload_types) {
                        Ok(RedPacket::Media(packet)) => {
                            media.rsfec_source = packet.marshal().ok();
                            media.packet = packet;
                        }
                        Ok(RedPacket::Ulpfec {
                            sequence_number,
                            payload,
                        }) => {
                            performance.record_fec_packet_received();
                            if media.flags.recovered_from_rtx {
                                // UU/WebRTC does not feed an RTX-recovered RED/FEC
                                // packet back into the ULPFEC erasure decoder.
                                continue;
                            }
                            match ulpfec_receiver.receive_repair(sequence_number, &payload) {
                                Ok(recovery) => {
                                    performance.record_fec_recovered(recovery.len());
                                    pending_media.extend(parse_ulpfec_recovered_packets(
                                        recovery,
                                        media.received_at,
                                    ));
                                }
                                Err(error) => {
                                    tracing::debug!(%error, "ULPFEC repair packet rejected")
                                }
                            }
                            continue;
                        }
                        Err(error) => {
                            tracing::debug!(%error, "RED packet rejected");
                            continue;
                        }
                    }
                }

                let recovered = media.flags.recovered_from_rtx || media.flags.recovered_by_fec;
                if !recovered {
                    performance.record_video_rtp_arrival(media.packet.header.timestamp);
                }
                let sequence_number = media.packet.header.sequence_number;
                let payload_type = media.packet.header.payload_type;
                // A padded RTP datagram is not necessarily an empty packet:
                // the packet parser has already removed its trailing padding.
                let is_padding = media.packet.payload.is_empty();
                if !recovered && !is_padding {
                    performance.record_primary_media_packet();
                }
                let result;

                if is_padding {
                    result = receiver.receive_padding(sequence_number);
                    nack_requester.set_rtt(Duration::from_micros(
                        nack_rtt_micros.load(Ordering::Relaxed),
                    ));
                    let nack = nack_requester.on_received(sequence_number, false, false);
                    rtcp_feedback.buffer(nack.batch);
                } else {
                    let Some((mime, fmtp)) = video_payload_codecs.get(&payload_type) else {
                        tracing::debug!(payload_type, "ignoring unrecognized video payload type");
                        if let Some(source) = media.rsfec_source.take() {
                            remember_fec_source(
                                &mut fec_receiver,
                                &mut flexfec_receiver,
                                &mut ulpfec_receiver,
                                &mut pending_media,
                                sequence_number,
                                source,
                                media.flags.recovered_from_rtx,
                                media.received_at,
                                &performance,
                            );
                        }
                        continue;
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
                        if let Some(source) = media.rsfec_source.take() {
                            remember_fec_source(
                                &mut fec_receiver,
                                &mut flexfec_receiver,
                                &mut ulpfec_receiver,
                                &mut pending_media,
                                sequence_number,
                                source,
                                media.flags.recovered_from_rtx,
                                media.received_at,
                                &performance,
                            );
                        }
                        continue;
                    };
                    if configured_payload_type != payload_type {
                        receiver.configure_codec(packet_codec, fmtp);
                        configured_payload_type = payload_type;
                    }
                    let Some(mut parsed) =
                        receiver.parse_video_packet(&media.packet, media.received_at, packet_codec)
                    else {
                        tracing::debug!(
                            sequence_number,
                            payload_type,
                            recovered,
                            "video payload depacketizer rejected packet before NACK"
                        );
                        if let Some(source) = media.rsfec_source.take() {
                            remember_fec_source(
                                &mut fec_receiver,
                                &mut flexfec_receiver,
                                &mut ulpfec_receiver,
                                &mut pending_media,
                                sequence_number,
                                source,
                                media.flags.recovered_from_rtx,
                                media.received_at,
                                &performance,
                            );
                        }
                        if media.flags.recovered_from_rtx {
                            performance.record_rtx_packet(false);
                        }
                        continue;
                    };

                    if !recovered {
                        let now = Instant::now();
                        last_packet_received_at = Some(now);
                        if parsed.starts_keyframe()
                            || last_keyframe_timestamp == Some(media.packet.header.timestamp)
                        {
                            last_keyframe_timestamp = Some(media.packet.header.timestamp);
                            last_keyframe_packet_at = Some(now);
                        }
                    }
                    let playout_delay = playout_delay_extension_id
                        .and_then(|id| media.packet.header.get_extension(id))
                        .and_then(|payload| parse_playout_delay(&payload));

                    nack_requester.set_rtt(Duration::from_micros(
                        nack_rtt_micros.load(Ordering::Relaxed),
                    ));
                    let nack = nack_requester.on_received(
                        parsed.sequence_number(),
                        parsed.starts_keyframe(),
                        recovered,
                    );
                    parsed.set_receive_timing(playout_delay, nack.nack_count);
                    rtcp_feedback.buffer(nack.batch);
                    let prepared = receiver.prepare_video_packet(&mut parsed);
                    // Parameter failure overrides this packet's buffered NACK.
                    // Packet-buffer/complete-frame feedback occurs after this flush.
                    rtcp_feedback.request_keyframe |= !prepared;
                    rtcp_feedback.flush(&connection, track.ssrc()).await;
                    result = if prepared {
                        receiver.receive_prepared(parsed)
                    } else {
                        receiver.parameter_packet_rejected()
                    };
                }

                performance.set_nack_state(
                    nack_requester.outstanding(),
                    nack_requester.final_lost_packets(),
                );
                if let Some(sequence_number) = result.continuous_sequence {
                    nack_requester.clear_up_to(sequence_number);
                    performance.set_nack_state(
                        nack_requester.outstanding(),
                        nack_requester.final_lost_packets(),
                    );
                }
                if media.flags.recovered_from_rtx {
                    performance.record_rtx_packet(result.accepted_packet);
                }
                emit_official_receiver_result(
                    result,
                    &receiver_feedback_sender,
                    &mut nack_requester,
                    &mut rtcp_feedback,
                    &video_annexb_sinks,
                    &video_keyframe_tx,
                    &performance,
                    &remote_ntp,
                    &connection,
                    track.ssrc(),
                )
                .await;

                if let Some(source) = media.rsfec_source.take() {
                    remember_fec_source(
                        &mut fec_receiver,
                        &mut flexfec_receiver,
                        &mut ulpfec_receiver,
                        &mut pending_media,
                        sequence_number,
                        source,
                        media.flags.recovered_from_rtx,
                        media.received_at,
                        &performance,
                    );
                }
            }
        }
    }
    tracing::debug!("official-compatible remote video track ended");
}

#[allow(clippy::too_many_arguments)]
pub(super) async fn emit_official_receiver_result(
    result: ReceiverResult,
    receiver_feedback_sender: &mpsc::UnboundedSender<VideoReceiverFeedback>,
    nack_requester: &mut NackRequester,
    rtcp_feedback: &mut RtcpFeedbackBuffer,
    video_sinks: &Mutex<Vec<VideoFrameSink>>,
    video_keyframe_tx: &watch::Sender<u64>,
    performance: &PerformanceMonitor,
    remote_ntp: &StdMutex<RemoteNtpEstimator>,
    connection: &RTCPeerConnection,
    media_ssrc: u32,
) {
    if result.clear_nack {
        nack_requester.clear_pending();
        rtcp_feedback.nack_sequences.clear();
        performance.set_nack_state(
            nack_requester.outstanding(),
            nack_requester.final_lost_packets(),
        );
    }
    if result.request_keyframe {
        let _ = send_picture_loss_indication(connection, media_ssrc).await;
        // 部分 host（macOS）只对 FIR 生成关键帧，因此同时发送。
        let _ = send_full_intra_request(connection, media_ssrc).await;
    }
    performance.record_predecode_drops(result.predecode_drops);
    performance.set_frame_buffer_frames(result.frame_buffer_frames);
    for frame in result.frames {
        performance.set_playout_timing(
            frame.schedule.target_delay,
            frame.schedule.jitter_delay,
            frame.schedule.low_latency,
        );
        tracing::trace!(rtp_timestamp = frame.rtp_timestamp, render_at = ?frame.schedule.render_at,
            "UU receiver released frame for decoding");
        let capture_at = frame
            .video_timing
            .filter(|timing| timing.flags & 4 != 0)
            .and_then(|_| {
                remote_ntp
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .estimate(frame.rtp_timestamp)
            });
        if frame.keyframe {
            tracing::trace!(target: "openuuyc::transport::rtc::clock", rtp_timestamp = frame.rtp_timestamp,
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
        performance.record_received_frame(
            frame.last_received_at.duration_since(frame.received_at),
            frame.rtp_timestamp,
            frame.assembled_at,
            frame.keyframe,
            frame.frame_sending_delay_ms,
        );
        if frame.keyframe {
            video_keyframe_tx.send_modify(|value| *value += 1);
        }
        let encoded = EncodedVideoFrame {
            completion: DecodeCompletion::new(frame.frame_id, receiver_feedback_sender.clone()),
            parameter_format: frame.parameter_format,
            color_space: frame.color_space,
            frame_id: frame.frame_id,
            data: Bytes::from(frame.data),
            rtp_timestamp: frame.rtp_timestamp,
            received_at: frame.received_at,
            assembled_at: frame.assembled_at,
            keyframe: frame.keyframe,
            rotation: frame.rotation,
            content_type: frame.content_type,
            video_capture_index: frame.video_capture_index,
            is_new_picture: frame.is_new_picture,
            sender_timing,
            codec: match frame.codec {
                VideoCodecKind::H264 => VideoCodec::H264,
                VideoCodecKind::H265 => VideoCodec::H265,
                VideoCodecKind::Av1 => VideoCodec::Av1,
            },
        };
        let mut sinks = video_sinks.lock().await;
        sinks.retain(|sink| sink.send(encoded.clone()));
        // A failed/cancelled window does not end the media track. If no sink
        // accepts this frame, its completion returns the admission token and
        // the existing receiver recovery remains available for a retry.
    }
}
