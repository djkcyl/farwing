//! Mirrors the behavior of libwebrtc's ReceiveStatisticsImpl/StreamStatisticianImpl.
use rtcp::reception_report::ReceptionReport;
use std::time::{Duration, SystemTime};

pub(super) struct ReceptionStatistics {
    ssrc: u32,
    clock_rate: u32,
    reorder_threshold: i64,
    detect_retransmissions: bool,
    packets: u32,
    retransmissions: u32,
    highest: Option<i64>,
    pending_restart: Option<u16>,
    last_timestamp: u32,
    last_in_order_ms: i64,
    jitter_q4: u32,
    jitter_clock_rate: u32,
    cumulative_loss: i32,
    loss_offset: i32,
    last_report_loss: i32,
    last_report_highest: i64,
}

impl ReceptionStatistics {
    pub(super) fn new(ssrc: u32, clock_rate: u32) -> Self {
        Self {
            ssrc,
            clock_rate,
            reorder_threshold: 50,
            detect_retransmissions: false,
            packets: 0,
            retransmissions: 0,
            highest: None,
            pending_restart: None,
            last_timestamp: 0,
            last_in_order_ms: 0,
            jitter_q4: 0,
            jitter_clock_rate: 0,
            cumulative_loss: 0,
            loss_offset: 0,
            last_report_loss: 0,
            last_report_highest: -1,
        }
    }

    pub(super) fn configure(&mut self, threshold: i64, detect_retransmissions: bool) {
        self.reorder_threshold = threshold;
        self.detect_retransmissions = detect_retransmissions;
    }

    pub(super) fn set_retransmission_detection(&mut self, enabled: bool) {
        self.detect_retransmissions = enabled;
    }

    pub(super) fn set_clock_rate(&mut self, rate: u32) {
        self.clock_rate = rate;
    }

    pub(super) fn update(&mut self, now: SystemTime, sequence: u16, timestamp: u32) {
        let now_ms = millis(now);
        self.packets = self.packets.wrapping_add(1);
        self.cumulative_loss = self.cumulative_loss.wrapping_sub(1);
        let sequence_unwrapped = self.unwrap_without_update(sequence);
        let previous_highest = match self.highest {
            None => {
                self.last_report_highest = sequence_unwrapped - 1;
                sequence_unwrapped - 1
            }
            Some(highest) => {
                let restart = self.pending_restart.take().is_some_and(|pending| {
                    // The provisional packet did not decrement loss on arrival.
                    self.cumulative_loss = self.cumulative_loss.wrapping_sub(1);
                    sequence == pending.wrapping_add(1)
                });
                if restart {
                    self.last_report_highest = sequence_unwrapped - 2;
                    sequence_unwrapped - 2
                } else {
                    if (highest - sequence_unwrapped).abs() > self.reorder_threshold {
                        self.pending_restart = Some(sequence);
                        self.cumulative_loss = self.cumulative_loss.wrapping_add(1);
                        return;
                    }
                    if sequence_unwrapped <= highest {
                        if self.detect_retransmissions {
                            let khz = self.clock_rate / 1000;
                            if khz != 0 {
                                let timestamp_age =
                                    timestamp.wrapping_sub(self.last_timestamp) / khz;
                                let jitter_ms = (2.0_f32 * ((self.jitter_q4 >> 4) as f32).sqrt()
                                    / khz as f32)
                                    as i64;
                                if now_ms - self.last_in_order_ms
                                    > i64::from(timestamp_age) + jitter_ms.max(1)
                                {
                                    self.retransmissions = self.retransmissions.wrapping_add(1);
                                }
                            }
                        }
                        return;
                    }
                    highest
                }
            }
        };
        // Keep the sequence reference unchanged until the in-order/restart gate accepts it.
        self.cumulative_loss = self
            .cumulative_loss
            .wrapping_add((sequence_unwrapped - previous_highest) as i32);
        self.highest = Some(sequence_unwrapped);
        if timestamp != self.last_timestamp && self.packets.wrapping_sub(self.retransmissions) >= 2
        {
            // The multiply/divide happens in 64 bits, then the low 32 bits
            // are added to the RTP timestamp, to avoid narrowing errors.
            let arrival_ticks = ((now_ms - self.last_in_order_ms)
                .wrapping_mul(i64::from(self.clock_rate as i32))
                / 1000) as i32;
            let deviation = arrival_ticks
                .wrapping_add(self.last_timestamp as i32)
                .wrapping_sub(timestamp as i32)
                .unsigned_abs();
            if self.clock_rate != 0 && self.clock_rate != self.jitter_clock_rate {
                if self.jitter_clock_rate != 0 {
                    self.jitter_q4 = (u64::from(self.clock_rate) * u64::from(self.jitter_q4)
                        / u64::from(self.jitter_clock_rate))
                        as u32;
                }
                self.jitter_clock_rate = self.clock_rate;
            }
            if deviation < 450_000 {
                let adjustment =
                    ((deviation * 16 + 8) as i32).wrapping_sub(self.jitter_q4 as i32) >> 4;
                self.jitter_q4 = self.jitter_q4.wrapping_add(adjustment as u32);
            }
        }
        self.last_timestamp = timestamp;
        self.last_in_order_ms = now_ms;
    }

    fn unwrap_without_update(&self, sequence: u16) -> i64 {
        let Some(last) = self.highest else {
            return i64::from(sequence);
        };
        let low = last as u16;
        let mut delta = i64::from(sequence) - i64::from(low);
        let diff = sequence.wrapping_sub(low);
        let ahead = if diff == 0x8000 {
            sequence > low
        } else {
            diff != 0 && diff < 0x8000
        };
        if ahead {
            if delta < 0 {
                delta += 65_536;
            }
        } else if delta > 0 && last + delta >= 65_536 {
            delta -= 65_536;
        }
        last + delta
    }

    pub(super) fn report(&mut self, now: SystemTime) -> Option<ReceptionReport> {
        let highest = self.highest?;
        if millis(now) - self.last_in_order_ms >= 8_000 {
            return None;
        }
        let expected = highest - self.last_report_highest;
        let lost = self.cumulative_loss.wrapping_sub(self.last_report_loss);
        let fraction_lost = if expected > 0 && lost > 0 {
            (255 * i64::from(lost) / expected) as u8
        } else {
            0
        };
        let mut cumulative = self.cumulative_loss.wrapping_add(self.loss_offset);
        if cumulative < 0 {
            self.loss_offset = self.cumulative_loss.wrapping_neg();
            cumulative = 0;
        }
        self.last_report_loss = self.cumulative_loss;
        self.last_report_highest = highest;
        Some(ReceptionReport {
            ssrc: self.ssrc,
            fraction_lost,
            total_lost: cumulative.min(0x7f_ffff) as u32,
            last_sequence_number: highest as u32,
            jitter: self.jitter_q4 >> 4,
            ..Default::default()
        })
    }
}

fn millis(time: SystemTime) -> i64 {
    let elapsed = time
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or(Duration::ZERO);
    ((elapsed.as_micros() + 500) / 1000) as i64
}
