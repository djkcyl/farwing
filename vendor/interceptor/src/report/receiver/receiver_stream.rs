use std::collections::HashMap;
use std::time::{Duration, Instant, SystemTime};

use async_trait::async_trait;
use util::sync::Mutex;

use super::reception_statistics::ReceptionStatistics;
use super::*;
use crate::stream_info::{ReceiveRtcpParameters, RtcpMode};
use crate::{Attributes, RTPReader};

pub(crate) struct ReceiverStream {
    pub(super) ssrc: u32,
    pub(super) rtx_for: Option<u32>,
    parent_rtp_reader: Arc<dyn RTPReader + Send + Sync>,
    now: Option<FnTimeGen>,
    clock_anchor: (Instant, SystemTime),
    clock_rates: HashMap<u8, u32>,
    statistics: Mutex<ReceptionStatistics>,
    sender_report: Mutex<Option<(u32, SystemTime)>>,
    receiver_ssrc: u32,
    report_schedule: Mutex<(Instant, Duration, bool)>,
    rtcp_parameters: Mutex<ReceiveRtcpParameters>,
    audio_compound: bool,
    pub(super) report_lock: tokio::sync::Mutex<()>,
}

impl ReceiverStream {
    pub(crate) fn new(
        ssrc: u32,
        clock_rate: u32,
        reader: Arc<dyn RTPReader + Send + Sync>,
        now: Option<FnTimeGen>,
    ) -> Self {
        Self {
            ssrc,
            rtx_for: None,
            parent_rtp_reader: reader,
            now,
            clock_anchor: (Instant::now(), SystemTime::now()),
            clock_rates: HashMap::new(),
            statistics: Mutex::new(ReceptionStatistics::new(ssrc, clock_rate)),
            sender_report: Mutex::new(None),
            receiver_ssrc: rand::random(),
            report_schedule: Mutex::new((Instant::now(), Duration::from_secs(1), true)),
            rtcp_parameters: Mutex::new(ReceiveRtcpParameters::default()),
            audio_compound: false,
            report_lock: tokio::sync::Mutex::new(()),
        }
    }

    pub(super) fn configure(&mut self, info: &StreamInfo, compound_audio_reports: bool) {
        self.clock_rates = info.payload_clock_rates.iter().copied().collect();
        self.rtx_for = info
            .associated_stream
            .as_ref()
            .filter(|_| info.mime_type.eq_ignore_ascii_case("video/rtx"))
            .map(|associated| associated.ssrc);
        let media_video = info.associated_stream.is_none() && info.mime_type.starts_with("video/");
        let nack = info
            .rtcp_feedback
            .iter()
            .any(|fb| fb.typ == "nack" && fb.parameter.is_empty());
        self.statistics
            .lock()
            .configure(if media_video && nack { 1000 } else { 50 }, media_video);
        self.audio_compound = compound_audio_reports && info.mime_type.starts_with("audio/");
        self.update_rtcp(info.receiver_rtcp.unwrap_or_default());
    }

    pub(super) fn update_rtcp(&self, mut parameters: ReceiveRtcpParameters) {
        // The audio receive module does not apply remote rtcp-rsize;
        // the video receive module does.
        if self.audio_compound && parameters.mode != RtcpMode::Off {
            parameters.mode = RtcpMode::Compound;
        }
        let mut current = self.rtcp_parameters.lock();
        if current.mode == RtcpMode::Off && parameters.mode != RtcpMode::Off {
            let mut schedule = self.report_schedule.lock();
            schedule.0 = Instant::now() + schedule.1 / 2;
        }
        *current = parameters;
    }

    pub(super) fn rtcp_mode(&self) -> RtcpMode {
        self.rtcp_parameters.lock().mode
    }

    pub(super) fn reports_enabled(&self) -> bool {
        self.rtcp_mode() != RtcpMode::Off && self.report_schedule.lock().2
    }

    pub(super) fn set_retransmission_detection(&self, enabled: bool) {
        self.statistics.lock().set_retransmission_detection(enabled);
    }

    pub(crate) fn set_report_interval(&self, interval: Duration) {
        let interval = interval.max(Duration::from_millis(1));
        *self.report_schedule.lock() = (Instant::now() + interval / 2, interval, true);
    }

    pub(crate) fn next_report_at(&self) -> Option<Instant> {
        if self.rtcp_mode() == RtcpMode::Off {
            return None;
        }
        let schedule = self.report_schedule.lock();
        (schedule.2 && self.rtx_for.is_none()).then_some(schedule.0)
    }

    pub(crate) fn claim_scheduled_report(&self, now: Instant) -> bool {
        if self.rtcp_mode() == RtcpMode::Off {
            return false;
        }
        let mut schedule = self.report_schedule.lock();
        if !schedule.2 || self.rtx_for.is_some() || now < schedule.0 {
            return false;
        }
        Self::schedule_next(&mut schedule, now);
        true
    }

    fn schedule_next(schedule: &mut (Instant, Duration, bool), now: Instant) {
        let base_ms = ((schedule.1.as_micros() + 500) / 1_000) as u64;
        let next_ms = rand::random_range(base_ms / 2..=base_ms.saturating_mul(3) / 2).max(1);
        schedule.0 = now + Duration::from_millis(next_ms);
    }

    pub(super) fn report_sent_with_feedback(&self) {
        Self::schedule_next(&mut self.report_schedule.lock(), Instant::now());
    }

    pub(crate) fn stop_reports(&self) {
        self.report_schedule.lock().2 = false;
    }

    pub(crate) fn process_rtp(&self, now: SystemTime, pkt: &rtp::packet::Packet) {
        let mut statistics = self.statistics.lock();
        if let Some(rate) = self.clock_rates.get(&pkt.header.payload_type) {
            statistics.set_clock_rate(*rate);
        }
        statistics.update(now, pkt.header.sequence_number, pkt.header.timestamp);
    }

    pub(crate) fn process_sender_report(
        &self,
        now: SystemTime,
        sr: &rtcp::sender_report::SenderReport,
    ) {
        *self.sender_report.lock() = Some(((sr.ntp_time >> 16) as u32, now));
    }

    pub(crate) fn generate_report(&self, now: SystemTime) -> rtcp::receiver_report::ReceiverReport {
        let reports = self.statistics.lock().report(now).into_iter().collect();
        let mut report = rtcp::receiver_report::ReceiverReport {
            ssrc: self
                .rtcp_parameters
                .lock()
                .local_ssrc
                .unwrap_or(self.receiver_ssrc),
            reports,
            ..Default::default()
        };
        self.apply_sender_report(now, &mut report);
        report
    }

    // RTCPSender::CreateReportBlocks applies its module's SR sample to every
    // block, including the separately counted RTX SSRC.
    pub(super) fn apply_sender_report(
        &self,
        now: SystemTime,
        report: &mut rtcp::receiver_report::ReceiverReport,
    ) {
        let (last, delay) = self
            .sender_report
            .lock()
            .map_or((0, 0), |(last, received)| {
                (last, compact_ntp(now).wrapping_sub(compact_ntp(received)))
            });
        for block in &mut report.reports {
            block.last_sender_report = last;
            block.delay = delay;
        }
    }
}

fn compact_ntp(time: SystemTime) -> u32 {
    let elapsed = time
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or(Duration::ZERO);
    let seconds = elapsed.as_secs().wrapping_add(2_208_988_800);
    ((seconds << 16) | ((u64::from(elapsed.subsec_nanos()) << 16) / 1_000_000_000)) as u32
}

#[async_trait]
impl RTPReader for ReceiverStream {
    async fn read(
        &self,
        buf: &mut [u8],
        a: &Attributes,
    ) -> Result<(rtp::packet::Packet, Attributes)> {
        let (pkt, attr) = self.parent_rtp_reader.read(buf, a).await?;
        let now = self.now.as_ref().map_or_else(
            || self.clock_anchor.1 + self.clock_anchor.0.elapsed(),
            |f| f(),
        );
        self.process_rtp(now, &pkt);
        Ok((pkt, attr))
    }
}
