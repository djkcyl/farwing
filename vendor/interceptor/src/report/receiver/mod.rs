mod receiver_stream;
#[cfg(test)]
mod receiver_test;
mod reception_statistics;

use std::collections::HashMap;
use std::time::{Duration, SystemTime};

use receiver_stream::ReceiverStream;
use tokio::sync::{mpsc, Mutex};
use waitgroup::WaitGroup;

use super::*;
use crate::error::Error;
use crate::stream_info::{ReceiveRtcpParameters, RtcpMode};
use crate::*;
use rtcp::payload_feedbacks::picture_loss_indication::PictureLossIndication;
use rtcp::transport_feedbacks::transport_layer_nack::TransportLayerNack;

/// Local interceptor context, never serialized into an RTCP packet. Needed
/// when an inactive module emits XR without any reception report blocks.
pub const RECEIVER_REPORT_MEDIA_SSRC: usize = 0x5555_5252;
/// Context for the downstream XR/framing adapter. In reduced-size mode an
/// empty RR is only a report-cycle marker: generate XR, but omit RR on wire.
pub const RECEIVER_REPORT_MODE: usize = 0x5555_524D;

fn module_report(
    stream: &ReceiverStream,
    streams: &[Arc<ReceiverStream>],
    now: SystemTime,
) -> (rtcp::receiver_report::ReceiverReport, Attributes) {
    let mut report = stream.generate_report(now);
    let mut blocks = Vec::new();
    for repair in streams
        .iter()
        .filter(|repair| repair.rtx_for == Some(stream.ssrc))
    {
        blocks.extend(repair.generate_report(now).reports);
    }
    blocks.append(&mut report.reports);
    report.reports = blocks;
    stream.apply_sender_report(now, &mut report);
    let mut attributes = Attributes::new();
    attributes.insert(RECEIVER_REPORT_MEDIA_SSRC, stream.ssrc as usize);
    attributes.insert(
        RECEIVER_REPORT_MODE,
        match stream.rtcp_mode() {
            RtcpMode::Off => 0,
            RtcpMode::Compound => 1,
            RtcpMode::ReducedSize => 2,
        },
    );
    (report, attributes)
}

pub(crate) struct ReceiverReportInternal {
    pub(crate) interval: Duration,
    pub(crate) now: Option<FnTimeGen>,
    pub(crate) streams: Mutex<HashMap<u32, Arc<ReceiverStream>>>,
    pub(crate) close_rx: Mutex<Option<mpsc::Receiver<()>>>,
    pub(crate) media_intervals: Option<(Duration, Duration)>,
    pub(crate) streams_changed: tokio::sync::Notify,
}

pub(crate) struct ReceiverReportRtcpReader {
    pub(crate) internal: Arc<ReceiverReportInternal>,
    pub(crate) parent_rtcp_reader: Arc<dyn RTCPReader + Send + Sync>,
}

struct ReceiverReportRtcpWriter {
    internal: Arc<ReceiverReportInternal>,
    parent: Arc<dyn RTCPWriter + Send + Sync>,
}

fn feedback_ssrc(packet: &(dyn rtcp::packet::Packet + Send + Sync)) -> Option<u32> {
    packet
        .as_any()
        .downcast_ref::<PictureLossIndication>()
        .map(|packet| packet.media_ssrc)
        .or_else(|| {
            packet
                .as_any()
                .downcast_ref::<TransportLayerNack>()
                .map(|packet| packet.media_ssrc)
        })
}

#[async_trait]
impl RTCPWriter for ReceiverReportRtcpWriter {
    async fn write(
        &self,
        packets: &[Box<dyn rtcp::packet::Packet + Send + Sync>],
        attributes: &Attributes,
    ) -> Result<usize> {
        let mut total = 0;
        let mut index = 0;
        while index < packets.len() {
            let Some(ssrc) = feedback_ssrc(packets[index].as_ref()) else {
                // Transport-wide feedback belongs to Call, not to a video
                // receive module. Never manufacture RR/XR for it. Preserve
                // compound groups supplied by senders (SR + SDES); splitting
                // these into individual writes destroys their RTCP contract.
                let end = (index + 1..packets.len())
                    .find(|&next| feedback_ssrc(packets[next].as_ref()).is_some())
                    .unwrap_or(packets.len());
                total += self.parent.write(&packets[index..end], attributes).await?;
                index = end;
                continue;
            };
            let end = (index + 1..packets.len())
                .find(|&next| feedback_ssrc(packets[next].as_ref()) != Some(ssrc))
                .unwrap_or(packets.len());
            let streams: Vec<_> = self
                .internal
                .streams
                .lock()
                .await
                .values()
                .cloned()
                .collect();
            let Some(stream) = streams
                .iter()
                .find(|stream| stream.ssrc == ssrc && stream.rtx_for.is_none())
            else {
                total += self.parent.write(&packets[index..end], attributes).await?;
                index = end;
                continue;
            };
            let _report = stream.report_lock.lock().await;
            if !stream.reports_enabled() {
                return Err(Error::Other("RTCP receive module is disabled".to_owned()));
            }
            let mut frame: Vec<Box<dyn rtcp::packet::Packet + Send + Sync>> = Vec::new();
            let mut context = attributes.clone();
            context.insert(RECEIVER_REPORT_MEDIA_SSRC, ssrc as usize);
            if stream.rtcp_mode() == RtcpMode::Compound {
                let now = self
                    .internal
                    .now
                    .as_ref()
                    .map_or_else(SystemTime::now, |f| f());
                let (report, report_context) = module_report(stream, &streams, now);
                context.extend(report_context.iter().map(|(&key, &value)| (key, value)));
                frame.push(Box::new(report));
                stream.report_sent_with_feedback();
                self.internal.streams_changed.notify_one();
            }
            let mut feedback: Vec<_> = packets[index..end]
                .iter()
                .map(|packet| packet.cloned())
                .collect();
            // RTCP feedback is ordered by flag: PLI=0x20, NACK=0x40.
            feedback.sort_by_key(|packet| !packet.as_any().is::<PictureLossIndication>());
            frame.extend(feedback);
            // The state lock is released before the final transport
            // callback. Do not block SDP updates on socket I/O.
            drop(_report);
            total += self.parent.write(&frame, &context).await?;
            index = end;
        }
        Ok(total)
    }
}

#[async_trait]
impl RTCPReader for ReceiverReportRtcpReader {
    async fn read(
        &self,
        buf: &mut [u8],
        a: &Attributes,
    ) -> Result<(Vec<Box<dyn rtcp::packet::Packet + Send + Sync>>, Attributes)> {
        let (pkts, attr) = self.parent_rtcp_reader.read(buf, a).await?;

        let now = if let Some(f) = &self.internal.now {
            f()
        } else {
            SystemTime::now()
        };

        for p in &pkts {
            if let Some(sr) = p
                .as_any()
                .downcast_ref::<rtcp::sender_report::SenderReport>()
            {
                let stream = {
                    let m = self.internal.streams.lock().await;
                    m.get(&sr.ssrc).cloned()
                };
                if let Some(stream) = stream {
                    stream.process_sender_report(now, sr);
                }
            }
        }

        Ok((pkts, attr))
    }
}

/// ReceiverReport interceptor generates receiver reports.
pub struct ReceiverReport {
    pub(crate) internal: Arc<ReceiverReportInternal>,

    pub(crate) wg: Mutex<Option<WaitGroup>>,
    pub(crate) close_tx: Mutex<Option<mpsc::Sender<()>>>,
}

impl ReceiverReport {
    /// builder returns a new ReportBuilder.
    pub fn builder() -> ReportBuilder {
        ReportBuilder {
            is_rr: true,
            ..Default::default()
        }
    }

    async fn is_closed(&self) -> bool {
        let close_tx = self.close_tx.lock().await;
        close_tx.is_none()
    }

    async fn run(
        rtcp_writer: Arc<dyn RTCPWriter + Send + Sync>,
        internal: Arc<ReceiverReportInternal>,
    ) -> Result<()> {
        if internal.media_intervals.is_some() {
            return Self::run_scheduled(rtcp_writer, internal).await;
        }
        let mut ticker = tokio::time::interval(internal.interval);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

        let mut close_rx = {
            let mut close_rx = internal.close_rx.lock().await;
            if let Some(close) = close_rx.take() {
                close
            } else {
                return Err(Error::ErrInvalidCloseRx);
            }
        };

        loop {
            tokio::select! {
                _ = ticker.tick() =>{
                    // TODO(cancel safety): This branch isn't cancel safe

                    let now = if let Some(f) = &internal.now {
                        f()
                    } else {
                        SystemTime::now()
                    };
                    let streams:Vec<Arc<ReceiverStream>> = {
                        let m = internal.streams.lock().await;
                        m.values().cloned().collect()
                    };
                    for stream in &streams {
                        if stream.rtx_for.is_some() { continue; }
                        let (pkt, a) = module_report(stream, &streams, now);
                        if let Err(err) = rtcp_writer.write(&[Box::new(pkt)], &a).await{
                            log::warn!("failed sending: {err}");
                        }
                    }
                }
                _ = close_rx.recv() =>{
                    return Ok(());
                }
            }
        }
    }

    async fn run_scheduled(
        writer: Arc<dyn RTCPWriter + Send + Sync>,
        internal: Arc<ReceiverReportInternal>,
    ) -> Result<()> {
        let mut close_rx = internal
            .close_rx
            .lock()
            .await
            .take()
            .ok_or(Error::ErrInvalidCloseRx)?;
        loop {
            let changed = internal.streams_changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            let streams: Vec<_> = internal.streams.lock().await.values().cloned().collect();
            let deadline = streams
                .iter()
                .filter_map(|stream| stream.next_report_at())
                .min();
            let wait = async {
                if let Some(deadline) = deadline {
                    tokio::time::sleep_until(deadline.into()).await;
                } else {
                    std::future::pending::<()>().await;
                }
            };
            tokio::select! {
                _ = close_rx.recv() => return Ok(()),
                _ = &mut changed => continue,
                _ = wait => {}
            }
            for stream in &streams {
                let _report = tokio::select! {
                    _ = close_rx.recv() => return Ok(()),
                    guard = stream.report_lock.lock() => guard,
                };
                if !stream.claim_scheduled_report(std::time::Instant::now()) {
                    continue;
                }
                let now = internal.now.as_ref().map_or_else(SystemTime::now, |f| f());
                let (report, attributes) = module_report(stream, &streams, now);
                let packets: Vec<Box<dyn rtcp::packet::Packet + Send + Sync>> =
                    vec![Box::new(report)];
                drop(_report);
                tokio::select! {
                    _ = close_rx.recv() => return Ok(()),
                    result = writer.write(&packets, &attributes) => {
                        if let Err(error) = result {
                            log::debug!("receiver report send failed: {error}");
                        }
                    }
                }
            }
        }
    }
}

#[async_trait]
impl Interceptor for ReceiverReport {
    /// bind_rtcp_reader lets you modify any incoming RTCP packets. It is called once per sender/receiver, however this might
    /// change in the future. The returned method will be called once per packet batch.
    async fn bind_rtcp_reader(
        &self,
        reader: Arc<dyn RTCPReader + Send + Sync>,
    ) -> Arc<dyn RTCPReader + Send + Sync> {
        Arc::new(ReceiverReportRtcpReader {
            internal: Arc::clone(&self.internal),
            parent_rtcp_reader: reader,
        })
    }

    /// bind_rtcp_writer lets you modify any outgoing RTCP packets. It is called once per PeerConnection. The returned method
    /// will be called once per packet batch.
    async fn bind_rtcp_writer(
        &self,
        writer: Arc<dyn RTCPWriter + Send + Sync>,
    ) -> Arc<dyn RTCPWriter + Send + Sync> {
        if self.is_closed().await {
            return writer;
        }

        let mut w = {
            let wait_group = self.wg.lock().await;
            wait_group.as_ref().map(|wg| wg.worker())
        };
        let writer2 = Arc::clone(&writer);
        let internal = Arc::clone(&self.internal);
        tokio::spawn(async move {
            let _d = w.take();
            if let Err(err) = ReceiverReport::run(writer2, internal).await {
                log::warn!("bind_rtcp_writer ReceiverReport::run got error: {err}");
            }
        });

        if self.internal.media_intervals.is_some() {
            Arc::new(ReceiverReportRtcpWriter {
                internal: Arc::clone(&self.internal),
                parent: writer,
            })
        } else {
            writer
        }
    }

    /// bind_local_stream lets you modify any outgoing RTP packets. It is called once for per LocalStream. The returned method
    /// will be called once per rtp packet.
    async fn bind_local_stream(
        &self,
        _info: &StreamInfo,
        writer: Arc<dyn RTPWriter + Send + Sync>,
    ) -> Arc<dyn RTPWriter + Send + Sync> {
        writer
    }

    /// UnbindLocalStream is called when the Stream is removed. It can be used to clean up any data related to that track.
    async fn unbind_local_stream(&self, _info: &StreamInfo) {}

    /// bind_remote_stream lets you modify any incoming RTP packets. It is called once for per RemoteStream. The returned method
    /// will be called once per rtp packet.
    async fn bind_remote_stream(
        &self,
        info: &StreamInfo,
        reader: Arc<dyn RTPReader + Send + Sync>,
    ) -> Arc<dyn RTPReader + Send + Sync> {
        let mut stream = ReceiverStream::new(
            info.ssrc,
            info.clock_rate,
            reader,
            self.internal.now.clone(),
        );
        stream.configure(info, self.internal.media_intervals.is_some());
        let stream = Arc::new(stream);
        if let Some((video, audio)) = self.internal.media_intervals {
            stream.set_report_interval(if info.mime_type.starts_with("audio/") {
                audio
            } else {
                video
            });
        }
        {
            let mut streams = self.internal.streams.lock().await;
            if let Some(primary) = stream.rtx_for.and_then(|ssrc| streams.get(&ssrc)) {
                primary.set_retransmission_detection(false);
            }
            if streams
                .values()
                .any(|repair| repair.rtx_for == Some(stream.ssrc))
            {
                stream.set_retransmission_detection(false);
            }
            streams.insert(info.ssrc, Arc::clone(&stream));
        }
        self.internal.streams_changed.notify_one();

        stream
    }

    /// unbind_remote_stream is called when the Stream is removed. It can be used to clean up any data related to that track.
    async fn unbind_remote_stream(&self, info: &StreamInfo) {
        let mut streams = self.internal.streams.lock().await;
        if let Some(stream) = streams.remove(&info.ssrc) {
            stream.stop_reports();
        }
        self.internal.streams_changed.notify_one();
    }

    async fn update_remote_rtcp(&self, ssrc: u32, parameters: ReceiveRtcpParameters) {
        let stream = self.internal.streams.lock().await.get(&ssrc).cloned();
        if let Some(stream) = stream {
            let _report = stream.report_lock.lock().await;
            stream.update_rtcp(parameters);
        }
        self.internal.streams_changed.notify_one();
    }

    /// close closes the Interceptor, cleaning up any data if necessary.
    async fn close(&self) -> Result<()> {
        {
            let mut close_tx = self.close_tx.lock().await;
            close_tx.take();
        }

        {
            let mut wait_group = self.wg.lock().await;
            if let Some(wg) = wait_group.take() {
                wg.wait().await;
            }
        }

        Ok(())
    }
}
