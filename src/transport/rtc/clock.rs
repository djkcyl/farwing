//! RTP/NTP clock mapping and sender timing.
use super::tracks::FrameSenderTiming;
use crate::transport::rtcp_timing::RtcpTiming;
use std::collections::VecDeque;
use std::sync::{Arc, Mutex as StdMutex};
use std::time::{Duration, Instant};
use webrtc::rtp_transceiver::rtp_receiver::RTCRtpReceiver;

#[derive(Clone, Copy)]
pub(super) struct RtcpClockMeasurement {
    pub(super) unwrapped_rtp: i64,
    pub(super) remote_ntp_ms: f64,
}

pub(super) struct RemoteNtpEstimator {
    pub(super) anchor: Instant,
    pub(super) last_unwrapped_rtp: Option<i64>,
    pub(super) measurements: VecDeque<RtcpClockMeasurement>,
    pub(super) clock_offsets_ms: VecDeque<f64>,
    pub(super) consecutive_invalid: u8,
    pub(super) mapping_established: bool,
}

impl RemoteNtpEstimator {
    const MAX_MEASUREMENTS: usize = 20;
    const MAX_RTP_JUMP: i64 = 1 << 25;
    const MAX_NTP_JUMP_MS: f64 = 60.0 * 60.0 * 1_000.0;
    const MAX_INVALID_SAMPLES: u8 = 3;

    pub(super) fn new(now: Instant) -> Self {
        Self {
            anchor: now,
            last_unwrapped_rtp: None,
            measurements: VecDeque::with_capacity(Self::MAX_MEASUREMENTS),
            clock_offsets_ms: VecDeque::with_capacity(Self::MAX_MEASUREMENTS),
            consecutive_invalid: 0,
            mapping_established: false,
        }
    }

    pub(super) fn update(
        &mut self,
        ntp_time: u64,
        rtp_timestamp: u32,
        rtt: Duration,
        received_at: Instant,
    ) -> bool {
        if ntp_time == 0 {
            return false;
        }
        let unwrapped_rtp = unwrap_rtp_timestamp(self.last_unwrapped_rtp, rtp_timestamp);
        let remote_ntp_ms = ntp_to_millis(ntp_time);
        if self.measurements.iter().any(|measurement| {
            measurement.unwrapped_rtp == unwrapped_rtp || measurement.remote_ntp_ms == remote_ntp_ms
        }) {
            return false;
        }

        let invalid = self.measurements.front().is_some_and(|latest| {
            remote_ntp_ms <= latest.remote_ntp_ms
                || remote_ntp_ms - latest.remote_ntp_ms > Self::MAX_NTP_JUMP_MS
                || unwrapped_rtp <= latest.unwrapped_rtp
                || unwrapped_rtp - latest.unwrapped_rtp > Self::MAX_RTP_JUMP
        });
        if invalid {
            self.consecutive_invalid = self.consecutive_invalid.saturating_add(1);
            if self.consecutive_invalid < Self::MAX_INVALID_SAMPLES {
                return false;
            }
            self.measurements.clear();
            self.clock_offsets_ms.clear();
            self.last_unwrapped_rtp = None;
            self.mapping_established = false;
        }
        self.consecutive_invalid = 0;
        self.last_unwrapped_rtp = Some(unwrapped_rtp);
        let local_arrival_ms = received_at
            .saturating_duration_since(self.anchor)
            .as_secs_f64()
            * 1_000.0;
        let sender_arrival_ms = remote_ntp_ms + rtt.as_secs_f64() * 500.0;
        tracing::trace!(target: "farwing::transport::rtc::clock", ntp_time, rtp_timestamp,
            unwrapped_rtp, remote_ntp_ms, local_arrival_ms, rtt_ms = rtt.as_secs_f64() * 1000.0,
            invalid_reset = invalid, "RTCP clock mapping sample");
        self.measurements.push_front(RtcpClockMeasurement {
            unwrapped_rtp,
            remote_ntp_ms,
        });
        self.clock_offsets_ms
            .push_back(local_arrival_ms - sender_arrival_ms);
        if self.measurements.len() > Self::MAX_MEASUREMENTS {
            self.measurements.pop_back();
        }
        if self.clock_offsets_ms.len() > Self::MAX_MEASUREMENTS {
            self.clock_offsets_ms.pop_front();
        }
        let established = self.measurements.len() >= 2;
        let became_established = established && !self.mapping_established;
        self.mapping_established = established;
        became_established
    }

    pub(super) fn estimate(&self, rtp_timestamp: u32) -> Option<Instant> {
        if self.measurements.len() < 2 || self.clock_offsets_ms.len() < 2 {
            return None;
        }
        let latest = self.measurements.front()?;
        let unwrapped = unwrap_rtp_timestamp(Some(latest.unwrapped_rtp), rtp_timestamp);
        let count = self.measurements.len() as f64;
        let base_x = latest.unwrapped_rtp as f64;
        let base_y = latest.remote_ntp_ms;
        let average_x = self
            .measurements
            .iter()
            .map(|measurement| measurement.unwrapped_rtp as f64 - base_x)
            .sum::<f64>()
            / count;
        let average_y = self
            .measurements
            .iter()
            .map(|measurement| measurement.remote_ntp_ms - base_y)
            .sum::<f64>()
            / count;
        let (variance, covariance) =
            self.measurements
                .iter()
                .fold((0.0, 0.0), |(variance, covariance), measurement| {
                    let x = measurement.unwrapped_rtp as f64 - base_x - average_x;
                    let y = measurement.remote_ntp_ms - base_y - average_y;
                    (variance + x * x, covariance + x * y)
                });
        if variance.abs() < 1.0e-8 {
            return None;
        }
        let slope = covariance / variance;
        let remote_capture_ms =
            base_y + average_y + (unwrapped as f64 - base_x - average_x) * slope;
        let mut offsets = self.clock_offsets_ms.iter().copied().collect::<Vec<_>>();
        offsets.sort_by(f64::total_cmp);
        let clock_offset_ms = offsets[offsets.len() / 2];
        add_signed_millis(self.anchor, remote_capture_ms + clock_offset_ms)
    }
}

pub(super) fn unwrap_rtp_timestamp(previous: Option<i64>, timestamp: u32) -> i64 {
    previous.map_or(i64::from(timestamp), |previous| {
        previous + i64::from(timestamp.wrapping_sub(previous as u32) as i32)
    })
}

pub(super) fn ntp_to_millis(ntp_time: u64) -> f64 {
    let seconds = (ntp_time >> 32) as u32;
    let fractions = ntp_time as u32;
    f64::from(seconds) * 1_000.0 + f64::from(fractions) * (1_000.0 / 4_294_967_296.0)
}

pub(super) fn add_signed_millis(anchor: Instant, milliseconds: f64) -> Option<Instant> {
    if !milliseconds.is_finite() {
        return None;
    }
    if milliseconds >= 0.0 {
        anchor.checked_add(Duration::from_secs_f64(milliseconds / 1_000.0))
    } else {
        anchor.checked_sub(Duration::from_secs_f64(-milliseconds / 1_000.0))
    }
}

pub(super) async fn observe_remote_ntp(
    receiver: Arc<RTCRtpReceiver>,
    media_ssrc: u32,
    estimator: Arc<StdMutex<RemoteNtpEstimator>>,
    rtcp_timing: RtcpTiming,
) {
    while receiver.read_rtcp().await.is_ok() {
        if let Some((report, rtt)) = rtcp_timing.fresh_sender_clock(media_ssrc) {
            let mapping_established = estimator
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .update(report.ntp_time, report.rtp_time, rtt, report.received_at);
            if mapping_established {
                tracing::info!(media_ssrc, "remote RTP-to-local-NTP mapping established");
            }
        }
    }
    tracing::debug!(media_ssrc, "remote RTCP sender-report stream ended");
}

pub(super) fn frame_sender_timing(
    capture_at: Option<Instant>,
    timing: Option<crate::transport::video_receiver::VideoSendTiming>,
    frame_sending_delay_ms: Option<u16>,
    last_received_at: Instant,
) -> FrameSenderTiming {
    // Like WebRTC's ReceiveStatisticsProxy::OnTimingFrameInfoUpdated, admit
    // cross-clock phase/E2E samples only with the measured flag (bit 2).
    // A sender report alone does not establish that an arbitrary RTP frame
    // carries a valid capture-domain timing measurement.
    let timing = timing.filter(|sample| sample.flags & 4 != 0);
    let capture_at = timing.and(capture_at);
    let millis = |value: u16| Duration::from_millis(u64::from(value));
    let capture_delay = timing.map(|timing| millis(timing.encode_start_delta_ms));
    let encode_delay = timing.and_then(|timing| {
        timing
            .encode_finish_delta_ms
            .checked_sub(timing.encode_start_delta_ms)
            .map(millis)
    });
    let pacer_delay = timing.and_then(|timing| {
        timing
            .pacer_exit_delta_ms
            .checked_sub(timing.packetization_finish_delta_ms)
            .map(millis)
    });
    let sending_delay = frame_sending_delay_ms
        .map(millis)
        .or_else(|| timing.map(|timing| millis(timing.pacer_exit_delta_ms)));
    let transport_delay = capture_at
        .zip(sending_delay)
        .and_then(|(capture_at, sending_delay)| capture_at.checked_add(sending_delay))
        .and_then(|pacer_exit| last_received_at.checked_duration_since(pacer_exit));
    FrameSenderTiming {
        capture_at,
        capture_delay,
        encode_delay,
        pacer_delay,
        sending_delay,
        transport_delay,
    }
}
