//! Shared RTCP receiver measurements and sender RRTR/DLRR responses. No
//! ICE/default RTT is admitted as an RTCP measurement or as an RTP/NTP clock
//! calibration input.

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use webrtc::interceptor::report::receiver::{RECEIVER_REPORT_MEDIA_SSRC, RECEIVER_REPORT_MODE};
use webrtc::interceptor::stream_info::{ReceiveRtcpParameters, RtcpMode, StreamInfo};
use webrtc::interceptor::{
    Attributes, Error, Interceptor, InterceptorBuilder, RTCPReader, RTCPWriter, RTPReader,
    RTPWriter,
};
use webrtc::rtcp::compound_packet::CompoundPacket;
use webrtc::rtcp::extended_report::{
    DLRRReport, DLRRReportBlock, ExtendedReport, ReceiverReferenceTimeReportBlock,
};
use webrtc::rtcp::packet::Packet;
use webrtc::rtcp::payload_feedbacks::picture_loss_indication::PictureLossIndication;
use webrtc::rtcp::receiver_report::ReceiverReport;
use webrtc::rtcp::sender_report::SenderReport;
use webrtc::rtcp::transport_feedbacks::transport_layer_cc::TransportLayerCc;
use webrtc::rtcp::transport_feedbacks::transport_layer_nack::TransportLayerNack;

type RtcpPacket = Box<dyn Packet + Send + Sync>;
pub(crate) const DEFAULT_RECEIVER_SSRC: u32 = 1;
// ModuleRtpRtcpImpl2 overwrites RTCPSender's constructor default
// with 1460. Receive-only modules have no video-sender MTU override.
const RECEIVE_RTCP_PACKET_SIZE: usize = 1460;

fn frame_rtcp_packets(packets: Vec<RtcpPacket>) -> Result<Vec<Vec<RtcpPacket>>, Error> {
    let mut frames = Vec::new();
    let mut current = Vec::new();
    let mut size = 0;
    for packet in packets {
        if let Some(nack) = packet.as_any().downcast_ref::<TransportLayerNack>() {
            let mut offset = 0;
            while offset < nack.nacks.len() {
                // Nack::Create fills the remaining compound buffer,
                // then starts a new feedback packet in the next datagram.
                if RECEIVE_RTCP_PACKET_SIZE - size < 16 {
                    frames.push(std::mem::take(&mut current));
                    size = 0;
                }
                let count =
                    ((RECEIVE_RTCP_PACKET_SIZE - size - 12) / 4).min(nack.nacks.len() - offset);
                current.push(Box::new(TransportLayerNack {
                    sender_ssrc: nack.sender_ssrc,
                    media_ssrc: nack.media_ssrc,
                    nacks: nack.nacks[offset..offset + count].to_vec(),
                }) as RtcpPacket);
                size += 12 + 4 * count;
                offset += count;
            }
        } else {
            let packet_size = packet.marshal_size();
            if packet_size > RECEIVE_RTCP_PACKET_SIZE {
                return Err(Error::Other(format!(
                    "RTCP block exceeds receive-module packet size: {packet_size}"
                )));
            }
            if size + packet_size > RECEIVE_RTCP_PACKET_SIZE {
                frames.push(std::mem::take(&mut current));
                size = 0;
            }
            size += packet_size;
            current.push(packet);
        }
    }
    if !current.is_empty() {
        frames.push(current);
    }
    Ok(frames)
}

#[derive(Clone)]
pub(crate) struct RtcpTiming(Arc<Mutex<State>>);

struct State {
    anchor: Instant,
    anchor_ntp: u64,
    streams: HashMap<u32, StreamTiming>,
    local_senders: Vec<u32>,
    sender_rrtr: HashMap<u32, VecDeque<ReceivedRrtr>>,
    receive_order: Vec<u32>,
    selected_video: Option<u32>,
}

struct StreamTiming {
    rtcp_mode: RtcpMode,
    rrtr_enabled: bool,
    local_ssrc: Option<u32>,
    received_rrtr: VecDeque<ReceivedRrtr>,
    pending_rtt_ms: Option<(u64, Instant)>,
    published_rtt_ms: Option<u64>,
    published_rtt_at: Option<Instant>,
    sender_report: Option<SenderClockSample>,
}

struct ReceivedRrtr {
    sender_ssrc: u32,
    remote_compact_ntp: u32,
    arrival_compact_ntp: u32,
}

#[derive(Clone, Copy)]
pub(crate) struct SenderClockSample {
    pub(crate) ntp_time: u64,
    pub(crate) rtp_time: u32,
    pub(crate) received_at: Instant,
}

impl RtcpTiming {
    pub(crate) fn new() -> Self {
        let unix = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default();
        let seconds = unix.as_secs().wrapping_add(2_208_988_800) as u32;
        let fraction = ((u64::from(unix.subsec_nanos()) << 32) / 1_000_000_000) as u32;
        Self(Arc::new(Mutex::new(State {
            anchor: Instant::now(),
            anchor_ntp: (u64::from(seconds) << 32) | u64::from(fraction),
            streams: HashMap::new(),
            local_senders: Vec::new(),
            sender_rrtr: HashMap::new(),
            receive_order: Vec::new(),
            selected_video: None,
        })))
    }

    pub(crate) fn select_video(&self, ssrc: u32) {
        self.0
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .selected_video = Some(ssrc);
    }

    pub(crate) fn rtt_for(&self, ssrc: u32) -> Option<Duration> {
        self.0
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .streams
            .get(&ssrc)?
            .published_rtt_ms
            .map(Duration::from_millis)
    }

    /// A stream whose sender cannot be measured from here (it comes through
    /// the forwarder): the round trip to the sender measured elsewhere.
    pub(crate) fn assume_rtt(&self, ssrc: u32, rtt: Duration) {
        let mut state = self.0.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(stream) = state.streams.get_mut(&ssrc) {
            stream.published_rtt_ms = Some((rtt.as_millis() as u64).max(1));
            stream.published_rtt_at = Some(Instant::now());
        }
    }

    /// ModuleRtpRtcpImpl2 publishes pending non-sender RTT once per second.
    /// Absence of a new sample does not clear the module's last valid RTT.
    pub(crate) fn publish_rtt(&self) -> Option<Duration> {
        let mut state = self.0.lock().unwrap_or_else(|e| e.into_inner());
        for (&ssrc, stream) in &mut state.streams {
            if let Some((ms, at)) = stream.pending_rtt_ms.take() {
                stream.published_rtt_ms = Some(ms);
                stream.published_rtt_at = Some(at);
                tracing::debug!(
                    media_ssrc = ssrc,
                    rtt_ms = ms,
                    "published measured RTCP non-sender RTT"
                );
            }
        }
        state
            .streams
            .get(&state.selected_video?)?
            .published_rtt_ms
            .map(Duration::from_millis)
    }

    pub(crate) fn fresh_sender_clock(&self, ssrc: u32) -> Option<(SenderClockSample, Duration)> {
        let state = self.0.lock().unwrap_or_else(|e| e.into_inner());
        let stream = state.streams.get(&ssrc)?;
        let sample = stream.sender_report?;
        // RtpVideoStreamReceiver2::DeliverRtcp permits a newly received SR
        // only (now_ms - last_sr_receive_ms <= 1) and requires measured RTT.
        let elapsed_ms = state.anchor.elapsed().as_millis();
        let received_ms = sample.received_at.duration_since(state.anchor).as_millis();
        if elapsed_ms.saturating_sub(received_ms) > 1 {
            return None;
        }
        Some((sample, Duration::from_millis(stream.published_rtt_ms?)))
    }

    /// Diagnostics freshness only; does not change the protocol's retained RTT.
    pub(crate) fn published_rtt_at(&self) -> Option<Instant> {
        let state = self.0.lock().unwrap_or_else(|e| e.into_inner());
        state.streams.get(&state.selected_video?)?.published_rtt_at
    }

    pub(crate) fn rtt_observed_at_for(&self, ssrc: u32) -> Option<Instant> {
        self.0
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .streams
            .get(&ssrc)?
            .published_rtt_at
    }

    fn observe(&self, packet: &(dyn Packet + Send + Sync), received_at: Instant) {
        if let Some(compound) = packet.as_any().downcast_ref::<CompoundPacket>() {
            for packet in &compound.0 {
                self.observe(packet.as_ref(), received_at);
            }
            return;
        }
        let mut state = self.0.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(sr) = packet.as_any().downcast_ref::<SenderReport>()
            && let Some(stream) = state.streams.get_mut(&sr.ssrc)
        {
            stream.sender_report = Some(SenderClockSample {
                ntp_time: sr.ntp_time,
                rtp_time: sr.rtp_time,
                received_at,
            });
        }
        let Some(xr) = packet.as_any().downcast_ref::<ExtendedReport>() else {
            return;
        };
        if xr
            .reports
            .iter()
            .filter(|block| block.as_any().is::<ReceiverReferenceTimeReportBlock>())
            .count()
            > 1
        {
            tracing::debug!(
                sender_ssrc = xr.sender_ssrc,
                "discard XR containing duplicate RRTR blocks"
            );
            return;
        }
        let now = state.compact_ntp(received_at);
        for block in &xr.reports {
            if let Some(rrtr) = block
                .as_any()
                .downcast_ref::<ReceiverReferenceTimeReportBlock>()
            {
                for pending in state.sender_rrtr.values_mut() {
                    let remote = (rrtr.ntp_timestamp >> 16) as u32;
                    if let Some(entry) = pending
                        .iter_mut()
                        .find(|entry| entry.sender_ssrc == xr.sender_ssrc)
                    {
                        // One compound datagram can be delivered to several local SSRCs.
                        if entry.remote_compact_ntp != remote {
                            entry.remote_compact_ntp = remote;
                            entry.arrival_compact_ntp = now;
                        }
                    } else if pending.len() < 300 {
                        pending.push_back(ReceivedRrtr {
                            sender_ssrc: xr.sender_ssrc,
                            remote_compact_ntp: remote,
                            arrival_compact_ntp: now,
                        });
                    }
                }
            }
        }
        // RTCP is routed to each receive module. Each module
        // owns its RRTR list and validates DLRR against its own local SSRC.
        for stream in state.streams.values_mut() {
            for block in &xr.reports {
                if let Some(rrtr) = block
                    .as_any()
                    .downcast_ref::<ReceiverReferenceTimeReportBlock>()
                {
                    let remote = (rrtr.ntp_timestamp >> 16) as u32;
                    if let Some(entry) = stream
                        .received_rrtr
                        .iter_mut()
                        .find(|entry| entry.sender_ssrc == xr.sender_ssrc)
                    {
                        entry.remote_compact_ntp = remote;
                        entry.arrival_compact_ntp = now;
                    } else if stream.received_rrtr.len() < 300 {
                        stream.received_rrtr.push_back(ReceivedRrtr {
                            sender_ssrc: xr.sender_ssrc,
                            remote_compact_ntp: remote,
                            arrival_compact_ntp: now,
                        });
                    }
                }
                if !stream.rrtr_enabled {
                    continue;
                }
                if let Some(dlrr) = block.as_any().downcast_ref::<DLRRReportBlock>() {
                    for report in &dlrr.reports {
                        if stream.local_ssrc != Some(report.ssrc) || report.last_rr == 0 {
                            continue;
                        }
                        let compact = now.wrapping_sub(report.last_rr.wrapping_add(report.dlrr));
                        let micros = if compact > 0x8000_0000 {
                            1_000
                        } else {
                            ((u64::from(compact) * 1_000_000 + 0x8000) >> 16).max(1_000)
                        };
                        stream.pending_rtt_ms = Some(((micros + 500) / 1_000, received_at));
                        tracing::debug!(
                            sender_ssrc = xr.sender_ssrc,
                            receiver_ssrc = report.ssrc,
                            rtt_micros = micros,
                            "received valid RTCP DLRR measurement"
                        );
                    }
                }
            }
        }
    }

    fn sender_extensions(&self, ssrc: u32) -> Option<RtcpPacket> {
        let mut state = self.0.lock().unwrap_or_else(|e| e.into_inner());
        let now = state.compact_ntp(Instant::now());
        let pending = state.sender_rrtr.get_mut(&ssrc)?;
        let count = pending.len().min(50);
        if count == 0 {
            return None;
        }
        let reports = pending
            .drain(..count)
            .map(|entry| DLRRReport {
                ssrc: entry.sender_ssrc,
                last_rr: entry.remote_compact_ntp,
                dlrr: now.wrapping_sub(entry.arrival_compact_ntp),
            })
            .collect();
        Some(Box::new(ExtendedReport {
            sender_ssrc: ssrc,
            reports: vec![Box::new(DLRRReportBlock { reports })],
        }))
    }

    fn report_extensions(
        &self,
        rr: &mut ReceiverReport,
        module_ssrc: Option<u32>,
    ) -> Vec<RtcpPacket> {
        let mut state = self.0.lock().unwrap_or_else(|e| e.into_inner());
        let now = state.ntp(Instant::now());
        let mut packets: Vec<RtcpPacket> = Vec::new();
        if let Some(module_ssrc) = module_ssrc {
            let Some(stream) = state.streams.get_mut(&module_ssrc) else {
                return packets;
            };
            if let Some(local_ssrc) = stream.local_ssrc {
                rr.ssrc = local_ssrc;
            } else {
                stream.local_ssrc = Some(rr.ssrc);
            }
            let mut blocks: Vec<RtcpPacket> = Vec::new();
            if stream.rrtr_enabled {
                blocks.push(Box::new(ReceiverReferenceTimeReportBlock {
                    ntp_timestamp: now,
                }));
            }
            let count = stream.received_rrtr.len().min(50);
            let reports: Vec<_> = stream
                .received_rrtr
                .drain(..count)
                .map(|entry| DLRRReport {
                    ssrc: entry.sender_ssrc,
                    last_rr: entry.remote_compact_ntp,
                    dlrr: ((now >> 16) as u32).wrapping_sub(entry.arrival_compact_ntp),
                })
                .collect();
            if !reports.is_empty() {
                blocks.push(Box::new(DLRRReportBlock { reports }));
            }
            if !blocks.is_empty() {
                tracing::debug!(
                    media_ssrc = module_ssrc,
                    local_ssrc = rr.ssrc,
                    rrtr = stream.rrtr_enabled,
                    dlrr_reports = count,
                    "sending RTCP XR timing report"
                );
                packets.push(Box::new(ExtendedReport {
                    sender_ssrc: rr.ssrc,
                    reports: blocks,
                }));
            }
        }
        packets
    }

    fn local_ssrc(&self, media_ssrc: u32) -> Option<u32> {
        self.0
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .streams
            .get(&media_ssrc)?
            .local_ssrc
    }
}

impl State {
    fn ntp(&self, now: Instant) -> u64 {
        let elapsed = now.duration_since(self.anchor).as_nanos();
        self.anchor_ntp
            .wrapping_add(((elapsed << 32) / 1_000_000_000) as u64)
    }

    fn compact_ntp(&self, now: Instant) -> u32 {
        (self.ntp(now) >> 16) as u32
    }
}

impl InterceptorBuilder for RtcpTiming {
    fn build(&self, _id: &str) -> Result<Arc<dyn Interceptor + Send + Sync>, Error> {
        Ok(Arc::new(self.clone()))
    }
}

struct TimingReader {
    inner: Arc<dyn RTCPReader + Send + Sync>,
    timing: RtcpTiming,
}
struct TimingWriter {
    inner: Arc<dyn RTCPWriter + Send + Sync>,
    timing: RtcpTiming,
}

#[async_trait]
impl RTCPReader for TimingReader {
    async fn read(
        &self,
        buf: &mut [u8],
        attributes: &Attributes,
    ) -> Result<(Vec<RtcpPacket>, Attributes), Error> {
        let (packets, attributes) = self.inner.read(buf, attributes).await?;
        let now = Instant::now();
        for packet in &packets {
            self.timing.observe(packet.as_ref(), now);
        }
        Ok((packets, attributes))
    }
}

#[async_trait]
impl RTCPWriter for TimingWriter {
    async fn write(&self, packets: &[RtcpPacket], attributes: &Attributes) -> Result<usize, Error> {
        let mut outgoing: Vec<RtcpPacket> = Vec::new();
        let mut report_extensions = Vec::new();
        for packet in packets {
            if let Some(sr) = packet.as_any().downcast_ref::<SenderReport>() {
                report_extensions.extend(self.timing.sender_extensions(sr.ssrc));
            }
            if let Some(rr) = packet.as_any().downcast_ref::<ReceiverReport>() {
                let mut rr = rr.clone();
                let module_ssrc = attributes
                    .get(&RECEIVER_REPORT_MEDIA_SSRC)
                    .copied()
                    .and_then(|ssrc| u32::try_from(ssrc).ok());
                report_extensions.extend(self.timing.report_extensions(&mut rr, module_ssrc));
                let emit_rr =
                    attributes.get(&RECEIVER_REPORT_MODE) != Some(&2) || !rr.reports.is_empty();
                tracing::debug!(module_ssrc, mode=attributes.get(&RECEIVER_REPORT_MODE), emit_rr, local_ssrc=rr.ssrc, reports=?rr.reports,
                    "prepared module RTCP report cycle");
                if emit_rr {
                    outgoing.push(Box::new(rr));
                }
            } else if let Some(pli) = packet.as_any().downcast_ref::<PictureLossIndication>() {
                let mut pli = pli.clone();
                if let Some(ssrc) = self.timing.local_ssrc(pli.media_ssrc) {
                    pli.sender_ssrc = ssrc;
                }
                outgoing.push(Box::new(pli));
            } else if let Some(nack) = packet.as_any().downcast_ref::<TransportLayerNack>() {
                let mut nack = nack.clone();
                if let Some(ssrc) = self.timing.local_ssrc(nack.media_ssrc) {
                    nack.sender_ssrc = ssrc;
                }
                outgoing.push(Box::new(nack));
            } else if let Some(feedback) = packet.as_any().downcast_ref::<TransportLayerCc>() {
                // PacketRouter uses the first enabled send module,
                // otherwise the first receive module; RTCPSender
                // overwrites the feedback builder's sender SSRC.
                let state = self.timing.0.lock().unwrap_or_else(|e| e.into_inner());
                let sender = state.local_senders.first().copied().or_else(|| {
                    state.receive_order.iter().find_map(|ssrc| {
                        let stream = state.streams.get(ssrc)?;
                        (stream.rtcp_mode != RtcpMode::Off)
                            .then_some(stream.local_ssrc)
                            .flatten()
                    })
                });
                if let Some(sender_ssrc) = sender {
                    let mut feedback = feedback.clone();
                    feedback.sender_ssrc = sender_ssrc;
                    outgoing.push(Box::new(feedback));
                }
            } else {
                outgoing.push(packet.cloned());
            }
        }
        // RTCPSender builds XR after its PLI/NACK flags. Generate it once per
        // report cycle, without adding any report to reduced-size feedback.
        outgoing.extend(report_extensions);
        if outgoing.is_empty() {
            return Ok(0);
        }
        // Call-level transport feedback also goes through RTCPSender's
        // datagram size limit, but must not acquire an unsolicited RR/XR.
        let mut total = 0;
        let mut failure = None;
        for frame in frame_rtcp_packets(outgoing)? {
            // OnBufferFull does not abandon later datagrams after a send
            // failure. Success is recorded if any send succeeded.
            match self.inner.write(&frame, attributes).await {
                Ok(0) => {
                    failure = Some(Error::Other(
                        "RTCP transport did not accept the datagram".to_owned(),
                    ))
                }
                Ok(bytes) => {
                    total += bytes;
                    for packet in &frame {
                        if let Some(feedback) = packet.as_any().downcast_ref::<TransportLayerCc>() {
                            tracing::trace!(
                                sender_ssrc = feedback.sender_ssrc,
                                media_ssrc = feedback.media_ssrc,
                                base = feedback.base_sequence_number,
                                statuses = feedback.packet_status_count,
                                received = feedback.recv_deltas.len(),
                                reference = feedback.reference_time,
                                feedback_count = feedback.fb_pkt_count,
                                bytes,
                                "sent TWCC transport feedback"
                            );
                        }
                    }
                }
                Err(error) => {
                    tracing::debug!(%error, "RTCP datagram send failed; continuing remaining fragments");
                    failure = Some(error);
                }
            }
        }
        if total == 0
            && let Some(failure) = failure
        {
            Err(failure)
        } else {
            Ok(total)
        }
    }
}

#[async_trait]
impl Interceptor for RtcpTiming {
    async fn bind_rtcp_reader(
        &self,
        reader: Arc<dyn RTCPReader + Send + Sync>,
    ) -> Arc<dyn RTCPReader + Send + Sync> {
        Arc::new(TimingReader {
            inner: reader,
            timing: self.clone(),
        })
    }
    async fn bind_rtcp_writer(
        &self,
        writer: Arc<dyn RTCPWriter + Send + Sync>,
    ) -> Arc<dyn RTCPWriter + Send + Sync> {
        Arc::new(TimingWriter {
            inner: writer,
            timing: self.clone(),
        })
    }
    async fn bind_local_stream(
        &self,
        info: &StreamInfo,
        writer: Arc<dyn RTPWriter + Send + Sync>,
    ) -> Arc<dyn RTPWriter + Send + Sync> {
        let mut state = self.0.lock().unwrap_or_else(|e| e.into_inner());
        if !state.local_senders.contains(&info.ssrc) {
            state.local_senders.push(info.ssrc);
        }
        if info.associated_stream.is_none()
            && (info.mime_type.starts_with("video/") || info.mime_type.starts_with("audio/"))
        {
            state.sender_rrtr.entry(info.ssrc).or_default();
        }
        writer
    }
    async fn unbind_local_stream(&self, info: &StreamInfo) {
        let mut state = self.0.lock().unwrap_or_else(|e| e.into_inner());
        state.local_senders.retain(|ssrc| *ssrc != info.ssrc);
        state.sender_rrtr.remove(&info.ssrc);
    }
    async fn bind_remote_stream(
        &self,
        info: &StreamInfo,
        reader: Arc<dyn RTPReader + Send + Sync>,
    ) -> Arc<dyn RTPReader + Send + Sync> {
        let is_audio = info.mime_type.starts_with("audio/");
        if (info.associated_stream.is_none()
            && (info.mime_type.eq_ignore_ascii_case("video/H264")
                || info.mime_type.eq_ignore_ascii_case("video/H265")
                || info.mime_type.eq_ignore_ascii_case("video/AV1")
                || is_audio))
            || info.mime_type.eq_ignore_ascii_case(crate::transport::rsfec::MIME_TYPE)
        {
            let mut state = self.0.lock().unwrap_or_else(|e| e.into_inner());
            if !state.receive_order.contains(&info.ssrc) {
                state.receive_order.push(info.ssrc);
            }
            let enabled = info
                .rtcp_feedback
                .iter()
                .any(|feedback| feedback.typ == "rrtr" && feedback.parameter.is_empty());
            state
                .streams
                .entry(info.ssrc)
                .and_modify(|stream| stream.rrtr_enabled = enabled)
                .or_insert_with(|| StreamTiming {
                    rtcp_mode: info
                        .receiver_rtcp
                        .map_or(RtcpMode::Compound, |parameters| parameters.mode),
                    rrtr_enabled: enabled,
                    // The video channel starts with RR SSRC 1. AddRecvStream
                    // chooses 2 only when the remote media SSRC itself is 1.
                    // A local video sender would supply its SSRC instead, but
                    // this client deliberately negotiates video recvonly.
                    local_ssrc: info
                        .receiver_rtcp
                        .and_then(|parameters| parameters.local_ssrc)
                        .or_else(|| {
                            (!is_audio).then_some(
                                if info
                                    .associated_stream
                                    .as_ref()
                                    .map_or(info.ssrc, |stream| stream.ssrc)
                                    == DEFAULT_RECEIVER_SSRC
                                {
                                    2
                                } else {
                                    DEFAULT_RECEIVER_SSRC
                                },
                            )
                        }),
                    received_rrtr: VecDeque::new(),
                    pending_rtt_ms: None,
                    published_rtt_ms: None,
                    published_rtt_at: None,
                    sender_report: None,
                });
            tracing::debug!(
                media_ssrc = info.ssrc,
                rrtr_enabled = enabled,
                "registered receive RTCP timing module"
            );
        }
        reader
    }
    async fn update_remote_rtcp(&self, ssrc: u32, parameters: ReceiveRtcpParameters) {
        if let Some(stream) = self
            .0
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .streams
            .get_mut(&ssrc)
        {
            stream.rtcp_mode = parameters.mode;
            if let Some(local_ssrc) = parameters.local_ssrc {
                stream.local_ssrc = Some(local_ssrc);
            }
        }
    }
    async fn unbind_remote_stream(&self, info: &StreamInfo) {
        let mut state = self.0.lock().unwrap_or_else(|e| e.into_inner());
        state.streams.remove(&info.ssrc);
        state.receive_order.retain(|ssrc| *ssrc != info.ssrc);
        if state.selected_video == Some(info.ssrc) {
            state.selected_video = None;
        }
    }
    async fn close(&self) -> Result<(), Error> {
        let mut state = self.0.lock().unwrap_or_else(|e| e.into_inner());
        state.streams.clear();
        state.local_senders.clear();
        state.sender_rrtr.clear();
        state.receive_order.clear();
        state.selected_video = None;
        Ok(())
    }
}
