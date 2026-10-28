//! One media stream's capture, encoder, RTP, RTCP and cancellation owners.
use super::capture_worker::capture_loop;
use super::{Published, maximum_header, video_timestamp};
use crate::features::host::{Lease, VideoConfig, capture, encoder, lock};
use crate::i18n::t;
use anyhow::Result;
use bytes::Bytes;
use std::{
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU16, Ordering},
    },
    time::{Duration, Instant},
};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use webrtc::{
    peer_connection::RTCPeerConnection,
    rtp::{codecs::h264::H264Payloader, header::Header, packet::Packet, packetizer::Payloader},
    rtp_transceiver::rtp_sender::RTCRtpSender,
};

/// Removes a track from the participant's feedback routing.
struct Unregister(
    Arc<crate::features::host::recovery::Inbox>,
    Arc<crate::features::host::recovery::TrackFeedback>,
);
impl Drop for Unregister {
    fn drop(&mut self) {
        self.0.unregister(&self.1);
    }
}

pub(super) struct Timeline {
    pub(super) started: Instant,
    origin: u32,
    sequence: Arc<AtomicU16>,
    sent: Arc<Mutex<Option<(u32, Instant, u64, u64)>>>,
}
impl Timeline {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            started: Instant::now(),
            origin: rand::random(),
            sequence: Arc::new(AtomicU16::new(rand::random())),
            sent: Arc::default(),
        })
    }
}
/// Who a stream sends for: a participant (its lease) or the forwarder uplink.
pub(super) trait Owner: Clone + Send + Sync + 'static {
    /// The stream may send.
    fn requested(&self) -> bool;
    fn fail(&self, message: String);
    /// A complete frame went out.
    fn frame(&self);
}
impl Owner for Lease {
    fn requested(&self) -> bool {
        Lease::requested(self)
    }
    fn fail(&self, message: String) {
        Lease::fail(self, message);
    }
    fn frame(&self) {
        Lease::frame(self);
    }
}

/// Sender reports and the source description of one video stream.
pub(super) fn spawn_reports<O: Owner>(
    index: usize,
    timeline: &Timeline,
    report_sender: Arc<RTCRtpSender>,
    report_connection: std::sync::Weak<RTCPeerConnection>,
    report_cancel: CancellationToken,
    report_handle: O,
    report_connected: Arc<AtomicBool>,
) -> tokio::task::JoinHandle<()> {
    let capture_clock = timeline.started;
    let origin = timeline.origin;
    let report_clock = timeline.sent.clone();
    tokio::spawn(async move {
        use webrtc::rtcp::{
            sender_report::SenderReport,
            source_description::{
                SdesType, SourceDescription, SourceDescriptionChunk, SourceDescriptionItem,
            },
        };
        let anchor = (Instant::now(), std::time::SystemTime::now());
        let mut previous = (Instant::now(), 0u64);
        let mut delay = Duration::from_millis(500);
        loop {
            tokio::select! {_ = report_cancel.cancelled()=>break,_=tokio::time::sleep(delay)=>{}}
            if !report_handle.requested() || !report_connected.load(Ordering::Acquire) {
                continue;
            }
            let Some((_, _, packets, octets)) = *lock(&report_clock) else {
                continue;
            };
            let elapsed = previous.0.elapsed().as_secs_f64().max(0.001);
            let bps = octets.saturating_sub(previous.1) as f64 * 8.0 / elapsed;
            previous = (Instant::now(), octets);
            let base = if bps >= 1000.0 {
                (360_000.0 / (bps / 1000.0)).min(1000.0)
            } else {
                1000.0
            };
            delay = Duration::from_secs_f64(
                (base * (0.5 + rand::random::<f64>()) / 1000.0).max(0.001),
            );
            let Some(connection) = report_connection.upgrade() else {
                break;
            };
            let parameters = report_sender.get_parameters().await;
            let Some(encoding) = parameters.encodings.first() else {
                continue;
            };
            let report = SenderReport {
                ssrc: encoding.ssrc,
                ntp_time: webrtc::rtp::extension::abs_send_time_extension::unix2ntp(
                    anchor.1 + anchor.0.elapsed(),
                ),
                rtp_time: video_timestamp(
                    origin,
                    (capture_clock.elapsed().as_nanos() / 100) as u64,
                ),
                packet_count: packets as u32,
                octet_count: octets as u32,
                ..Default::default()
            };
            let sdes = SourceDescription {
                chunks: vec![SourceDescriptionChunk {
                    source: encoding.ssrc,
                    items: vec![SourceDescriptionItem {
                        sdes_type: SdesType::SdesCname,
                        text: Bytes::from(format!("video_{index}")),
                    }],
                }],
            };
            let packets: Vec<Box<dyn webrtc::rtcp::packet::Packet + Send + Sync>> =
                vec![Box::new(report), Box::new(sdes)];
            let result = tokio::select! {_=report_cancel.cancelled()=>break,r=connection.write_rtcp(&packets)=>r};
            if let Err(error) = result {
                tracing::debug!(%error,"host sender report failed");
            }
        }
    })
}

/// The receiver's RTCP for one video stream: repairs, round trip and loss,
/// keyframe requests.
pub(super) fn spawn_feedback(
    index: usize,
    sender: Arc<RTCRtpSender>,
    feedback_transport: crate::features::host::transport::Transport,
    feedback_cancel: CancellationToken,
    recovery: Arc<crate::features::host::recovery::Inbox>,
    feedback_track: Arc<crate::features::host::recovery::TrackFeedback>,
    request_keyframe: Arc<AtomicBool>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut keyframes = crate::features::host::keyframe::Feedback::default();
        // Until a receiver report measures it, assume a typical WAN RTT.
        let mut latest_rtt = Duration::from_millis(100);
        // `VideoFeedback` names the track by its SSRC.
        let mut registered = None;
        let mut register = |params: &webrtc::rtp_transceiver::RTCRtpSendParameters| {
            if let Some(encoding) = params.encodings.first()
                && registered != Some(encoding.ssrc)
            {
                recovery.register(encoding.ssrc, index, &feedback_track);
                registered = Some(encoding.ssrc);
            }
        };
        register(&sender.get_parameters().await);
        let _unregister = Unregister(recovery.clone(), feedback_track.clone());
        loop {
            let reports =
                tokio::select! {_ = feedback_cancel.cancelled()=>break,r=sender.read_rtcp()=>r};
            let reports = match reports {
                Ok((reports, _)) => reports,
                Err(webrtc::Error::Interceptor(webrtc::interceptor::Error::Rtcp(error))) => {
                    tracing::debug!(%error,"discarded malformed host video feedback");
                    continue;
                }
                Err(_) => break,
            };
            let params = sender.get_parameters().await;
            register(&params);
            for report in reports {
                if let Some(nack)=report.as_any().downcast_ref::<webrtc::rtcp::transport_feedbacks::transport_layer_nack::TransportLayerNack>() {feedback_transport.nack(nack);}
                let reports = report
                    .as_any()
                    .downcast_ref::<webrtc::rtcp::receiver_report::ReceiverReport>()
                    .map(|r| r.reports.as_slice())
                    .or_else(|| {
                        report
                            .as_any()
                            .downcast_ref::<webrtc::rtcp::sender_report::SenderReport>()
                            .map(|r| r.reports.as_slice())
                    });
                if let Some(reports) = reports {
                    for report in reports {
                        if !params.encodings.iter().any(|e| e.ssrc == report.ssrc)
                            || report.last_sender_report == 0
                        {
                            continue;
                        }
                        let now = (webrtc::rtp::extension::abs_send_time_extension::unix2ntp(
                            std::time::SystemTime::now(),
                        ) >> 16) as u32;
                        let rtt = now
                            .wrapping_sub(report.last_sender_report)
                            .wrapping_sub(report.delay);
                        if rtt < 0x80000000 {
                            let measured =
                                Duration::from_micros(u64::from(rtt) * 1_000_000 / 65536);
                            feedback_transport.remote_report(report.fraction_lost, measured);
                            latest_rtt = measured;
                            feedback_track.set_rtt(measured);
                        }
                    }
                }
                if params.encodings.first().is_some_and(|encoding| {
                    keyframes.accept(report.as_ref(), encoding.ssrc, Instant::now(), latest_rtt)
                }) {
                    // A PLI asks for a keyframe: libwebrtc ignores a
                    // recovery frame sent instead and asks again ~200 ms
                    // later (measured), so it is not offered one.
                    tracing::info!(track = index, "controller requested a keyframe");
                    request_keyframe.store(true, Ordering::Release);
                    feedback_track.note_keyframe_request();
                }
            }
        }
    })
}

/// What sending one video stream's encoded frames needs.
pub(super) struct Sending<O> {
    pub index: usize,
    pub capture_index: u16,
    pub timeline: Arc<Timeline>,
    pub track: Arc<crate::features::host::track::VideoTrack>,
    pub extension_sender: Arc<RTCRtpSender>,
    pub sending_formats: Arc<crate::features::host::format::Negotiated>,
    pub sending_keyframe: Arc<AtomicBool>,
    pub transport: crate::features::host::transport::Transport,
    pub send_handle: O,
    pub sent_feedback: Arc<crate::features::host::recovery::TrackFeedback>,
    pub send_cancel: CancellationToken,
    pub sent_first_frame: Arc<AtomicBool>,
    pub worker_started: Instant,
    /// End-to-end forwarding: frames are encrypted before packetization.
    pub encrypt: Option<Arc<Mutex<crate::media::frame_crypto::Encryptor>>>,
}

/// Packetizes and sends encoded frames until cancelled.
pub(super) fn spawn_sending<O: Owner>(
    sending: Sending<O>,
    mut frames: mpsc::Receiver<encoder::Encoded>,
) -> tokio::task::JoinHandle<()> {
    let Sending {
        index,
        capture_index,
        timeline,
        track,
        extension_sender,
        sending_formats,
        sending_keyframe,
        transport,
        send_handle,
        sent_feedback,
        send_cancel,
        sent_first_frame,
        worker_started,
        encrypt,
    } = sending;
    let origin = timeline.origin;
    let sent = timeline.sent.clone();
    tokio::spawn(async move {
        let mut payloader = H264Payloader::default();
        let sequence = timeline.sequence.clone();
        let mut awaiting_keyframe = false;
        // Dependency Descriptor numbering (browser viewers).
        let mut dependencies = crate::media::dependency_descriptor::Numbering::default();
        // `FARWING_HOST_VIDEO_LOSS`: frames this track does not send.
        let mut loss = HostLoss::from_environment();
        loop {
            let value = tokio::select! {_ = send_cancel.cancelled()=>break,v=frames.recv()=>v};
            let Some(frame) = value else { break };
            if !sending_formats.permits_format(frame.format) {
                awaiting_keyframe = true;
                sending_keyframe.store(true, Ordering::Release);
                continue;
            }
            // An in-flight encoder may have queued a dependent P frame
            // before a reoffer dropped its reference. Do not send that old
            // dependency chain while the requested keyframe is being built.
            if awaiting_keyframe && !frame.keyframe {
                continue;
            }
            // Recovery frames get keyframe protection (FEC target, RTX).
            transport.frame(
                video_timestamp(origin, frame.timestamp_100ns as u64),
                frame.keyframe || frame.meta.is_some_and(|meta| meta.recovery()),
                frame.timing.map(|t| t.captured),
                frame.format,
            );
            if !send_handle.requested() {
                break;
            }
            let parameters = extension_sender.get_parameters().await.rtp_parameters;
            transport.negotiated_codecs(&parameters.codecs);
            let extensions = parameters.header_extensions;
            let color =
                Bytes::from(crate::features::host::format::color_extension(frame.color));
            let headers:Vec<_>=extensions.iter().filter(|e|frame.timing.is_some() || !matches!(e.uri.as_str(),
                "http://www.webrtc.org/experiments/rtp-hdrext/video-timing"|"urn:farwing:rtp-hdrext:sending-delay")).cloned().collect();
            let base: Vec<_> = headers
                .iter()
                .filter(|e| {
                    !matches!(
                        e.uri.as_str(),
                        "http://www.webrtc.org/experiments/rtp-hdrext/color-space"
                            | "http://www.webrtc.org/experiments/rtp-hdrext/video-timing"
                    )
                })
                .cloned()
                .collect();
            let repair:Vec<_>=extensions.iter().filter(|e|matches!(e.uri.as_str(),"urn:ietf:params:rtp-hdrext:sdes:mid"|"http://www.ietf.org/id/draft-holmer-rmcat-transport-wide-cc-extensions-01")).cloned().collect();
            let mixed = color.len() > 16 || extensions.iter().any(|e| e.id > 14);
            // Browsers decide decodability from declared dependencies.
            let generic = extensions
                .iter()
                .any(|e| e.uri == crate::media::dependency_descriptor::URI);
            // Chrome does not decode H.265 recovery frames (measured; the
            // cause is inside Chrome): that browser gets keyframes, at once.
            sent_feedback.set_generic(
                generic && frame.format.codec != crate::features::host::format::Codec::H265,
            );
            sent_feedback.set_browser(
                !extensions.iter().any(|e| e.uri == crate::media::frame_meta::URI),
            );
            transport.set_generic_dependencies(generic);
            let descriptor = generic.then(|| dependencies.frame(frame.keyframe, frame.meta));
            if let Some(meta) = frame.meta.filter(|_| generic) {
                sent_feedback.sent(
                    video_timestamp(origin, frame.timestamp_100ns as u64),
                    meta,
                    frame.keyframe,
                );
            }
            let dropped = loss.as_mut().is_some_and(|loss| loss.drops(frame.keyframe));
            let header = maximum_header(&headers, track.mid_len(), color.len(), mixed);
            let base_header = maximum_header(&base, track.mid_len(), color.len(), mixed);
            let repair_header = maximum_header(&repair, track.mid_len(), color.len(), mixed);
            let mtu = transport.payload_limit(header, repair_header);
            // End-to-end forwarding: only the participants read the frame.
            let data = match &encrypt {
                Some(cipher) => {
                    use crate::media::frame_crypto::Codec as Wire;
                    let codec = match frame.format.codec {
                        crate::features::host::format::Codec::H264 => Wire::H264,
                        crate::features::host::format::Codec::H265 => Wire::H265,
                        crate::features::host::format::Codec::Av1 => Wire::Av1,
                    };
                    match lock(cipher).encrypt(codec, &frame.data) {
                        Ok(sealed) => sealed,
                        Err(error) => {
                            tracing::warn!(%error, track = index, "frame not encrypted; waiting for a keyframe");
                            awaiting_keyframe = true;
                            sending_keyframe.store(true, Ordering::Release);
                            continue;
                        }
                    }
                }
                None => frame.data,
            };
            let payloads = match frame.format.codec {
                crate::features::host::format::Codec::H264 => payloader
                    .payload(mtu, &Bytes::from(data))
                    .map_err(anyhow::Error::from),
                crate::features::host::format::Codec::H265 => {
                    crate::features::host::hevc::payloads(mtu, &data)
                }
                crate::features::host::format::Codec::Av1 => {
                    crate::media::av1::payloads(mtu, &data, frame.keyframe)
                }
            };
            let payloads = match payloads {
                Ok(v) => v,
                Err(error) => {
                    send_handle.fail((t().features.peer_packetize_failed)(&error.to_string()));
                    send_cancel.cancel();
                    break;
                }
            };
            let count = payloads.len();
            let mut complete = count > 0;
            let packetized_at = Instant::now();
            let mut queued = transport.queued_frame(
                payloads
                    .iter()
                    .enumerate()
                    .map(|(i, p)| {
                        transport.wire_size(
                            p.len() + if i + 1 == count { header } else { base_header },
                        )
                    })
                    .sum(),
            );
            for (index, payload) in payloads.into_iter().enumerate() {
                if send_cancel.is_cancelled() || !send_handle.requested() {
                    break;
                }
                let mut packet = Packet {
                    header: Header {
                        version: 2,
                        extension: color.len() > 16 || extensions.iter().any(|e| e.id > 14),
                        extension_profile: if color.len() > 16
                            || extensions.iter().any(|e| e.id > 14)
                        {
                            0x1000
                        } else {
                            0
                        },
                        sequence_number: sequence.fetch_add(1, Ordering::Relaxed),
                        timestamp: video_timestamp(origin, frame.timestamp_100ns as u64),
                        marker: index + 1 == count,
                        ..Default::default()
                    },
                    payload,
                };
                for ext in &extensions {
                    let value = match ext.uri.as_str() {
                        "urn:farwing:rtp-hdrext:sending-delay"
                            if frame.timing.is_some() =>
                        {
                            Some(Bytes::from_static(&[0, 0]))
                        }
                        "http://www.webrtc.org/experiments/rtp-hdrext/video-timing"
                            if index + 1 == count && frame.timing.is_some() =>
                        {
                            let timing = frame.timing.unwrap();
                            let mut value = [0u8; 13];
                            value[0] = 4;
                            for (i, at) in
                                [timing.encode_started, timing.encode_finished, packetized_at]
                                    .into_iter()
                                    .enumerate()
                            {
                                let delta = ((at
                                    .saturating_duration_since(timing.captured)
                                    .as_micros()
                                    + 500)
                                    / 1000)
                                    .min(u16::MAX.into())
                                    as u16;
                                value[1 + i * 2..3 + i * 2]
                                    .copy_from_slice(&delta.to_be_bytes());
                            }
                            Some(Bytes::copy_from_slice(&value))
                        }
                        "urn:farwing:rtp-hdrext:capture-index" => {
                            // Source identity, not a frame counter. It stays fixed
                            // for this capture owner.
                            Some(Bytes::copy_from_slice(&capture_index.to_be_bytes()))
                        }
                        "urn:farwing:rtp-hdrext:new-frame" => {
                            Some(Bytes::from_static(if frame.is_new { &[1] } else { &[0] }))
                        }
                        // RTX keeps it; only the first packet carries it.
                        "urn:farwing:rtp-hdrext:frame-meta" if index == 0 => {
                            frame.meta.map(|meta| Bytes::copy_from_slice(&meta.encode()))
                        }
                        crate::media::dependency_descriptor::URI => {
                            descriptor.map(|(number, dependency)| {
                                Bytes::from(crate::media::dependency_descriptor::encode(
                                    number, dependency, index, count,
                                ))
                            })
                        }
                        "http://www.webrtc.org/experiments/rtp-hdrext/color-space"
                            if index + 1 == count =>
                        {
                            Some(color.clone())
                        }
                        _ => None,
                    };
                    if let Some(value) = value {
                        if let Err(error) = packet.header.set_extension(ext.id as u8, value) {
                            tracing::warn!(%error,"host extension encoding failed");
                        }
                    }
                }
                // Test loss: the sequence number is used, nothing is sent
                // or kept for repair.
                if dropped {
                    continue;
                }
                match tokio::select! { _=send_cancel.cancelled()=>break, result=track.write(frame.format, &packet)=>result }
                {
                    Ok(0) => {
                        // Unbound during renegotiation: the encoder has
                        // advanced its references, but the peer missed this
                        // frame. Resume with an IDR instead of a broken P chain.
                        awaiting_keyframe = true;
                        sending_keyframe.store(true, Ordering::Release);
                        complete = false;
                        break;
                    }
                    Ok(_) => {}
                    Err(webrtc::Error::ErrUnsupportedCodec) => {
                        // A reoffer may retire this already queued format.
                        awaiting_keyframe = true;
                        sending_keyframe.store(true, Ordering::Release);
                        complete = false;
                        break;
                    }
                    Err(error) => {
                        send_handle.fail((t().features.peer_send_failed)(&error.to_string()));
                        send_cancel.cancel();
                        complete = false;
                        break;
                    }
                }
                queued.sent(transport.wire_size(
                    packet.payload.len()
                        + if index + 1 == count {
                            header
                        } else {
                            base_header
                        },
                ));
                let mut clock = lock(&sent);
                let (packets, octets) = clock.as_ref().map_or((0, 0), |v| (v.2, v.3));
                *clock = Some((
                    packet.header.timestamp,
                    Instant::now(),
                    packets + 1,
                    octets + packet.payload.len() as u64,
                ));
                drop(clock);
            }
            if complete && !send_cancel.is_cancelled() && send_handle.requested() {
                if frame.keyframe {
                    awaiting_keyframe = false;
                }
                if !sent_first_frame.swap(true, Ordering::AcqRel) {
                    tracing::info!(
                        track = index,
                        elapsed_ms = worker_started.elapsed().as_millis(),
                        "host first video frame submitted"
                    );
                }
                send_handle.frame();
            }
        }
    })
}

pub(super) struct Worker {
    cancel: CancellationToken,
    source_missing: Arc<AtomicBool>,
    first_frame: Arc<AtomicBool>,
    tasks: Vec<tokio::task::JoinHandle<()>>,
    capture: Option<std::thread::JoinHandle<()>>,
}
impl Worker {
    pub(super) fn spawn(
        index: usize,
        capture_index: u16,
        timeline: Arc<Timeline>,
        connection: Arc<RTCPeerConnection>,
        track: Arc<crate::features::host::track::VideoTrack>,
        sender: Arc<RTCRtpSender>,
        screen: capture::Screen,
        handle: Lease,
        cancel: CancellationToken,
        connected: Arc<AtomicBool>,
        business_ready: Arc<super::screens::Ready>,
        config: Arc<Mutex<VideoConfig>>,
        negotiated: Arc<crate::features::host::format::Negotiated>,
        transport: crate::features::host::transport::Transport,
        publication: tokio::sync::watch::Sender<Published>,
        pointer: Arc<Mutex<Option<crate::platform::windows::cursor_shape::Snapshot>>>,
        recovery: Arc<crate::features::host::recovery::Inbox>,
        forwarded: Arc<crate::features::host::forwarding::Flags>,
    ) -> Result<Self> {
        let worker_started = Instant::now();
        let track_feedback = Arc::new(crate::features::host::recovery::TrackFeedback::default());
        let sent_feedback = track_feedback.clone();
        let keyframe = Arc::new(AtomicBool::new(true));
        let capture_clock = timeline.started;
        let reports = spawn_reports(
            index,
            &timeline,
            sender.clone(),
            Arc::downgrade(&connection),
            cancel.clone(),
            handle.clone(),
            connected.clone(),
        );
        let feedback = spawn_feedback(
            index,
            sender.clone(),
            transport.clone(),
            cancel.clone(),
            recovery,
            track_feedback.clone(),
            keyframe.clone(),
        );
        // A frame must be consumed before capturing another. There is no historical frame backlog.
        let (frames_tx, frames) = mpsc::channel::<encoder::Encoded>(1);
        let owner_cancel = cancel.clone();
        let owner_handle = handle.clone();
        let owner_keyframe = keyframe.clone();
        let owner_transport = transport.clone();
        let failed_publication = publication.clone();
        let sending_formats = negotiated.clone();
        let sending_keyframe = keyframe.clone();
        let source_missing = Arc::new(AtomicBool::new(false));
        let missing = source_missing.clone();
        let capture = std::thread::Builder::new()
            .name("host-screen".into())
            .spawn(move || {
                let result = capture_loop(
                    screen,
                    owner_handle.clone(),
                    owner_cancel.clone(),
                    connected,
                    business_ready,
                    config,
                    owner_keyframe,
                    frames_tx,
                    capture_clock,
                    owner_transport,
                    negotiated,
                    publication,
                    pointer.clone(),
                    track_feedback,
                    forwarded,
                    index,
                );
                *lock(&pointer) = None;
                if let Err(error) = result {
                    if owner_cancel.is_cancelled() || !owner_handle.requested() {
                        return;
                    }
                    missing.store(error.is::<capture::SourceGone>(), Ordering::Release);
                    let message = (t().features.peer_capture_stopped)(&format!("{error:#}"));
                    failed_publication.send_modify(|state| {
                        state.capturing = false;
                        state.visible = false;
                        state.encoder = None;
                    });
                    owner_handle.video(None);
                    owner_handle.fail(message);
                    owner_cancel.cancel();
                }
            })
            .map_err(|e| {
                cancel.cancel();
                e
            })?;
        let send_cancel = cancel.clone();
        let send_handle = handle.clone();
        let first_frame = Arc::new(AtomicBool::new(false));
        let sent_first_frame = first_frame.clone();
        let sending = spawn_sending(
            Sending {
                index,
                capture_index,
                timeline,
                track,
                extension_sender: sender.clone(),
                sending_formats,
                sending_keyframe,
                transport,
                send_handle,
                sent_feedback,
                send_cancel,
                sent_first_frame,
                worker_started,
                encrypt: None,
            },
            frames,
        );

        Ok(Self {
            cancel,
            source_missing,
            first_frame,
            tasks: vec![reports, feedback, sending],
            capture: Some(capture),
        })
    }
    pub(super) async fn close(mut self) {
        self.cancel.cancel();
        for task in self.tasks.drain(..) {
            let _ = task.await;
        }
        if let Some(owner) = self.capture.take() {
            let _ = tokio::task::spawn_blocking(move || owner.join()).await;
        }
    }
    pub(super) fn source_missing(&self) -> bool {
        self.source_missing.load(Ordering::Acquire)
    }
    pub(super) fn has_frame(&self) -> bool {
        self.first_frame.load(Ordering::Acquire) && !self.ended()
    }
    pub(super) fn ended(&self) -> bool {
        self.cancel.is_cancelled()
    }
}
impl Drop for Worker {
    fn drop(&mut self) {
        self.cancel.cancel();
        for task in &self.tasks {
            task.abort();
        }
        if let Some(owner) = self.capture.take() {
            if let Ok(runtime) = tokio::runtime::Handle::try_current() {
                runtime.spawn_blocking(move || {
                    let _ = owner.join();
                });
            } else {
                let _ = owner.join();
            }
        }
    }
}

/// `FARWING_HOST_VIDEO_LOSS=N,M`: each video track skips its Nth sent frame
/// and every Mth after it (keyframes excepted). The packets keep their
/// sequence numbers but are neither sent nor kept for retransmission, so the
/// viewer cannot repair them: a measurement aid for viewers the controller
/// side cannot impair (browsers).
struct HostLoss {
    first: u64,
    every: u64,
    frames: u64,
}

impl HostLoss {
    fn from_environment() -> Option<Self> {
        let spec = std::env::var("FARWING_HOST_VIDEO_LOSS").ok()?;
        let (first, every) = spec.split_once(',').unwrap_or((&spec, "0"));
        let (Ok(first), Ok(every)) = (first.trim().parse::<u64>(), every.trim().parse::<u64>()) else {
            tracing::warn!(%spec, "ignoring FARWING_HOST_VIDEO_LOSS (expected N,M)");
            return None;
        };
        tracing::warn!(first, every, "host video loss injection is on");
        Some(Self {
            first: first.max(1),
            every,
            frames: 0,
        })
    }

    fn drops(&mut self, keyframe: bool) -> bool {
        self.frames += 1;
        let frames = self.frames;
        !keyframe
            && (frames == self.first
                || (self.every > 0 && frames > self.first && (frames - self.first) % self.every == 0))
    }
}
