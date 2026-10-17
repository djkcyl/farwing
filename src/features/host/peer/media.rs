//! One media stream's capture, encoder, RTP, RTCP and cancellation owners.
use super::capture_worker::capture_loop;
use super::{Published, maximum_header, video_timestamp};
use crate::features::host::{Lease, VideoConfig, capture, encoder, lock};
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

pub(super) struct Timeline {
    started: Instant,
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
pub(super) struct Worker {
    cancel: CancellationToken,
    encode_cancel: Arc<AtomicBool>,
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
        business_ready: Arc<AtomicBool>,
        config: Arc<Mutex<VideoConfig>>,
        negotiated: Arc<crate::features::host::format::Negotiated>,
        transport: crate::features::host::transport::Transport,
        publication: tokio::sync::watch::Sender<Published>,
        pointer: Arc<Mutex<Option<crate::platform::cursor_shape::Snapshot>>>,
    ) -> Result<Self> {
        let worker_started = Instant::now();
        let keyframe = Arc::new(AtomicBool::new(true));
        let request_keyframe = keyframe.clone();
        let feedback_cancel = cancel.clone();
        let feedback_transport = transport.clone();
        let extension_sender = sender.clone();
        let capture_clock = timeline.started;
        let origin = timeline.origin;
        let report_sender = sender.clone();
        let report_connection = Arc::downgrade(&connection);
        let report_cancel = cancel.clone();
        let report_handle = handle.clone();
        let sent = timeline.sent.clone();
        let report_clock = sent.clone();
        let report_connected = connected.clone();
        let reports = tokio::spawn(async move {
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
        });
        let feedback = tokio::spawn(async move {
            let mut keyframes = crate::features::host::keyframe::Feedback::default();
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
                                feedback_transport.remote_report(
                                    report.fraction_lost,
                                    Duration::from_micros(u64::from(rtt) * 1_000_000 / 65536),
                                );
                            }
                        }
                    }
                    if params.encodings.first().is_some_and(|encoding| {
                        keyframes.accept(report.as_ref(), encoding.ssrc, Instant::now())
                    }) {
                        request_keyframe.store(true, Ordering::Release);
                    }
                }
            }
        });
        // A frame must be consumed before capturing another. There is no historical frame backlog.
        let (frames_tx, mut frames) = mpsc::channel::<encoder::Encoded>(1);
        let owner_cancel = cancel.clone();
        let owner_handle = handle.clone();
        let owner_keyframe = keyframe.clone();
        let owner_transport = transport.clone();
        let failed_publication = publication.clone();
        let sending_formats = negotiated.clone();
        let sending_keyframe = keyframe.clone();
        let source_missing = Arc::new(AtomicBool::new(false));
        let missing = source_missing.clone();
        let encode_cancel = Arc::new(AtomicBool::new(cancel.is_cancelled()));
        let owner_encode_cancel = encode_cancel.clone();
        let capture = std::thread::Builder::new()
            .name("host-screen".into())
            .spawn(move || {
                let result = capture_loop(
                    screen,
                    owner_handle.clone(),
                    owner_cancel.clone(),
                    owner_encode_cancel,
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
                );
                *lock(&pointer) = None;
                if let Err(error) = result {
                    if owner_cancel.is_cancelled() || !owner_handle.requested() {
                        return;
                    }
                    let source_gone = error.is::<capture::SourceGone>();
                    missing.store(source_gone, Ordering::Release);
                    failed_publication.send_modify(|state| {
                        state.capturing = false;
                        state.visible = false;
                        state.encoder = None;
                    });
                    owner_handle.video(None);
                    if source_gone {
                        // The screen pool owns physical return and headless
                        // handoff. Losing one source is not a terminal failure
                        // of the connection; real recovery errors are reported
                        // by that owner instead.
                        tracing::info!(error = %format!("{error:#}"),
                            "host capture source disappeared; waiting for display handoff");
                    } else {
                        owner_handle.fail(format!("画面采集已停止：{error:#}"));
                    }
                    owner_cancel.cancel();
                }
            })
            .map_err(|e| {
                cancel.cancel();
                e
            })?;
        let encoding_cancel_token = cancel.clone();
        let encoding_cancel_flag = encode_cancel.clone();
        let encoding_cancellation = tokio::spawn(async move {
            encoding_cancel_token.cancelled().await;
            encoding_cancel_flag.store(true, Ordering::Release);
        });
        let send_cancel = cancel.clone();
        let send_handle = handle.clone();
        let first_frame = Arc::new(AtomicBool::new(false));
        let sent_first_frame = first_frame.clone();
        let sending = tokio::spawn(async move {
            let mut payloader = H264Payloader::default();
            let sequence = timeline.sequence.clone();
            let mut awaiting_keyframe = false;
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
                transport.frame(
                    video_timestamp(origin, frame.timestamp_100ns as u64),
                    frame.keyframe,
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
                    "http://www.webrtc.org/experiments/rtp-hdrext/video-timing"|"http://www.webrtc.org/experiments/rtp-hdrext/video-frame-sending-delay")).cloned().collect();
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
                let header = maximum_header(&headers, track.mid_len(), color.len(), mixed);
                let base_header = maximum_header(&base, track.mid_len(), color.len(), mixed);
                let repair_header = maximum_header(&repair, track.mid_len(), color.len(), mixed);
                let mtu = transport.payload_limit(header, repair_header);
                let payloads = match frame.format.codec {
                    crate::features::host::format::Codec::H264 => payloader
                        .payload(mtu, &Bytes::from(frame.data))
                        .map_err(anyhow::Error::from),
                    crate::features::host::format::Codec::H265 => {
                        crate::features::host::hevc::payloads(mtu, &frame.data)
                    }
                    crate::features::host::format::Codec::Av1 => {
                        crate::media::av1::payloads(mtu, &frame.data, frame.keyframe)
                    }
                };
                let payloads = match payloads {
                    Ok(v) => v,
                    Err(error) => {
                        send_handle.fail(format!("视频分包失败：{error}"));
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
                            "http://www.webrtc.org/experiments/rtp-hdrext/abs-send-time"
                            | "urn:ietf:params:rtp-hdrext:toffset" => {
                                Some(Bytes::from_static(&[0, 0, 0]))
                            }
                            "http://www.webrtc.org/experiments/rtp-hdrext/video-frame-sending-delay"
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
                            "http://www.webrtc.org/experiments/rtp-hdrext/video-capture-index" => {
                                // T397272 -> TD08870/TD0B440: source identity,
                                // not a frame counter. It stays fixed for this capture owner.
                                Some(Bytes::copy_from_slice(&capture_index.to_be_bytes()))
                            }
                            "http://www.webrtc.org/experiments/rtp-hdrext/video-frame-is-new-frame" => {
                                Some(Bytes::from_static(if frame.is_new { &[1] } else { &[0] }))
                            }
                            "http://www.webrtc.org/experiments/rtp-hdrext/playout-delay" => {
                                Some(Bytes::from_static(&[0, 0, 0]))
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
                            send_handle.fail(format!("画面发送失败：{error}"));
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
        });

        Ok(Self {
            cancel,
            encode_cancel,
            source_missing,
            first_frame,
            tasks: vec![reports, feedback, sending, encoding_cancellation],
            capture: Some(capture),
        })
    }
    pub(super) async fn close(mut self) {
        self.encode_cancel.store(true, Ordering::Release);
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
        self.encode_cancel.store(true, Ordering::Release);
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
