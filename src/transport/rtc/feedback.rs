use crate::transport::rtcp_timing::DEFAULT_RECEIVER_SSRC;
use anyhow::{Context, Result};
use std::collections::HashMap;
use std::time::{Duration, Instant};
use webrtc::peer_connection::RTCPeerConnection;
use webrtc::rtcp::packet::Packet as RtcpPacket;
use webrtc::rtcp::payload_feedbacks::picture_loss_indication::PictureLossIndication;
use webrtc::rtcp::transport_feedbacks::transport_layer_nack::{
    TransportLayerNack, nack_pairs_from_sequence_numbers,
};
// NACK/keyframe recovery and RTCP feedback scheduling.

pub(super) const RTP_MAX_PACKET_AGE: u16 = 10_000;

pub(super) const RTP_MAX_NACK_PACKETS: usize = 1_000;

pub(super) const RTP_NACK_PROCESS_INTERVAL: Duration = Duration::from_millis(20);

pub(super) const RTP_DEFAULT_RTT: Duration = Duration::from_millis(50);

pub(super) const RTP_NACK_MAX_RETRIES: u8 = 17;

pub(super) const RTP_NACK_BACKOFF_BASE: f64 = 1.25;

pub(super) const RTP_NACK_BACKOFF_START: Duration = Duration::from_millis(50);

pub(super) const KEYFRAME_PACKET_WINDOW: Duration = Duration::from_millis(200);

pub(super) const ACTIVE_STREAM_WINDOW: Duration = Duration::from_secs(5);

/// A packet missing longer than this is treated as unrecoverable: waiting for
/// the decoder timeout (about a second) would freeze the picture instead.
const UNRECOVERABLE_FLOOR: Duration = Duration::from_millis(60);

/// When a missing packet can no longer be repaired by NACK.
pub(super) fn unrecoverable_after(rtt: Duration) -> Duration {
    rtt.mul_f64(2.5).max(UNRECOVERABLE_FLOOR)
}

/// A recovery frame is cheap: ask for one once repair is unlikely, before
/// it is hopeless (`docs/protocol/rfi-ltr-brief.md` §6.5 point 4). Half a
/// round trip of margin lets a first retransmission still win.
const RECOVERY_FLOOR: Duration = Duration::from_millis(40);

pub(super) fn recovery_after(rtt: Duration) -> Duration {
    rtt.mul_f64(1.5).max(RECOVERY_FLOOR)
}

/// The shortest spacing between keyframe requests for one loss episode.
pub(super) fn keyframe_request_spacing(rtt: Duration) -> Duration {
    rtt.mul_f64(1.5).max(Duration::from_millis(100))
}

#[derive(Clone, Copy)]
pub(super) enum NackFilter {
    Sequence,
    Time,
}

pub(super) struct NackEntry {
    pub(super) sequence_number: u16,
    pub(super) send_at_sequence_number: u16,
    pub(super) created_at: Instant,
    pub(super) sent_at: Option<Instant>,
    pub(super) retries: u8,
    pub(super) retries_because_of_sequence: u8,
    pub(super) retries_because_of_rtt: u8,
}

#[derive(Default)]
pub(super) struct NackBatch {
    pub(super) sequences: Vec<u16>,
    pub(super) request_keyframe: bool,
    /// The keyframe is asked for because repair of a loss gave up (not
    /// because the NACK list overflowed): a recovery frame may do instead.
    pub(super) loss: bool,
    /// A packet has been missing past `recovery_after`: repair is unlikely.
    pub(super) late: bool,
}

#[derive(Default)]
pub(super) struct RtcpFeedbackBuffer {
    pub(super) request_keyframe: bool,
    pub(super) nack_sequences: Vec<u16>,
}

impl RtcpFeedbackBuffer {
    pub(super) fn buffer(&mut self, batch: NackBatch) {
        self.request_keyframe |= batch.request_keyframe;
        self.nack_sequences.extend(batch.sequences);
    }

    pub(super) async fn flush(&mut self, connection: &RTCPeerConnection, media_ssrc: u32) {
        let request_keyframe = std::mem::take(&mut self.request_keyframe);
        let nack_sequences = std::mem::take(&mut self.nack_sequences);
        if request_keyframe {
            if let Err(error) = send_picture_loss_indication(connection, media_ssrc).await {
                tracing::warn!(%error, "buffered keyframe request failed");
            }
        } else if !nack_sequences.is_empty()
            && let Err(error) =
                send_transport_layer_nack(connection, media_ssrc, &nack_sequences).await
        {
            tracing::warn!(%error, count = nack_sequences.len(), "buffered RTCP NACK send failed");
        }
    }
}

pub(super) struct NackReceiveResult {
    pub(super) batch: NackBatch,
    pub(super) nack_count: u8,
}

pub(super) struct NackRequester {
    pub(super) initialized: bool,
    pub(super) newest_sequence_number: u16,
    pub(super) rtt: Duration,
    pub(super) entries: HashMap<u16, NackEntry>,
    pub(super) keyframes: Vec<u16>,
    pub(super) recovered: Vec<u16>,
    pub(super) final_lost_packets: u64,
    /// The last keyframe this requester asked for on unrecoverable loss.
    pub(super) keyframe_requested: Option<Instant>,
    /// A missing packet counts as lost no sooner than this: a forwarded
    /// stream watched in cinema mode waits like a live stream.
    pub(super) patience: Duration,
}

impl NackRequester {
    pub(super) fn new() -> Self {
        Self {
            initialized: false,
            newest_sequence_number: 0,
            rtt: RTP_DEFAULT_RTT,
            entries: HashMap::new(),
            keyframes: Vec::new(),
            recovered: Vec::new(),
            final_lost_packets: 0,
            keyframe_requested: None,
            patience: Duration::ZERO,
        }
    }

    pub(super) fn set_rtt(&mut self, rtt: Duration) {
        self.rtt = rtt;
    }

    pub(super) fn set_patience(&mut self, patience: Duration) {
        self.patience = patience;
    }

    pub(super) fn on_received(
        &mut self,
        sequence_number: u16,
        starts_keyframe: bool,
        recovered: bool,
    ) -> NackReceiveResult {
        let now = Instant::now();
        if !self.initialized {
            self.initialized = true;
            self.newest_sequence_number = sequence_number;
            if starts_keyframe {
                self.keyframes.push(sequence_number);
            }
            return NackReceiveResult {
                batch: NackBatch::default(),
                nack_count: 0,
            };
        }
        if sequence_number == self.newest_sequence_number {
            return NackReceiveResult {
                batch: NackBatch::default(),
                nack_count: 0,
            };
        }
        if sequence_ahead_of(self.newest_sequence_number, sequence_number) {
            let nack_count = self
                .entries
                .remove(&sequence_number)
                .map_or(0, |entry| entry.retries);
            return NackReceiveResult {
                batch: NackBatch::default(),
                nack_count,
            };
        }

        if starts_keyframe && !self.keyframes.contains(&sequence_number) {
            self.keyframes.push(sequence_number);
        }
        Self::erase_before_cutoff(&mut self.keyframes, sequence_number);
        if recovered {
            if !self.recovered.contains(&sequence_number) {
                self.recovered.push(sequence_number);
            }
            Self::erase_before_cutoff(&mut self.recovered, sequence_number);
            return NackReceiveResult {
                batch: NackBatch::default(),
                nack_count: 0,
            };
        }

        let missing_start = self.newest_sequence_number.wrapping_add(1);
        let request_keyframe = self.add_missing(missing_start, sequence_number);
        self.newest_sequence_number = sequence_number;
        let mut batch = self.get_batch(NackFilter::Sequence, now);
        batch.request_keyframe |= request_keyframe;
        batch.loss &= !request_keyframe;
        NackReceiveResult {
            batch,
            nack_count: 0,
        }
    }

    pub(super) fn process(&mut self) -> NackBatch {
        self.get_batch(NackFilter::Time, Instant::now())
    }

    pub(super) fn clear_pending(&mut self) {
        // Clearing pending repairs preserves RTT, sequence anchors and
        // recovered/keyframe history; what it gives up on was never repaired.
        self.final_lost_packets = self
            .final_lost_packets
            .saturating_add(self.entries.len() as u64);
        self.entries.clear();
    }

    pub(super) fn clear_up_to(&mut self, sequence_number: u16) {
        // Packets still missing below a continuous point were skipped.
        let before = self.entries.len();
        self.entries
            .retain(|sequence, _| sequence_ahead_or_at(*sequence, sequence_number));
        self.final_lost_packets = self
            .final_lost_packets
            .saturating_add((before - self.entries.len()) as u64);
        self.keyframes
            .retain(|sequence| sequence_ahead_or_at(*sequence, sequence_number));
        self.recovered
            .retain(|sequence| sequence_ahead_or_at(*sequence, sequence_number));
    }

    pub(super) fn add_missing(&mut self, start: u16, end: u16) -> bool {
        let before_age_cleanup = self.entries.len();
        let cutoff = end.wrapping_sub(RTP_MAX_PACKET_AGE);
        self.entries
            .retain(|sequence, _| sequence_ahead_or_at(*sequence, cutoff));
        self.final_lost_packets = self
            .final_lost_packets
            .saturating_add((before_age_cleanup - self.entries.len()) as u64);
        let count = usize::from(end.wrapping_sub(start));
        while self.entries.len().saturating_add(count) > RTP_MAX_NACK_PACKETS
            && self.remove_until_keyframe()
        {}
        if self.entries.len().saturating_add(count) > RTP_MAX_NACK_PACKETS {
            let existing = self.entries.len();
            self.final_lost_packets = self.final_lost_packets.saturating_add(existing as u64);
            self.entries.clear();
            tracing::warn!(
                max_packets = RTP_MAX_NACK_PACKETS,
                existing,
                missing_start = start,
                received_sequence = end,
                apparent_gap = count,
                "NACK list full; clearing it and requesting keyframe"
            );
            return true;
        }
        let mut sequence = start;
        while sequence != end {
            if !self.recovered.contains(&sequence) {
                self.entries.entry(sequence).or_insert(NackEntry {
                    sequence_number: sequence,
                    send_at_sequence_number: sequence,
                    created_at: Instant::now(),
                    sent_at: None,
                    retries: 0,
                    retries_because_of_sequence: 0,
                    retries_because_of_rtt: 0,
                });
            }
            sequence = sequence.wrapping_add(1);
        }
        false
    }

    pub(super) fn remove_until_keyframe(&mut self) -> bool {
        self.keyframes.sort_unstable_by(sequence_order);
        self.keyframes.dedup();
        while let Some(keyframe) = self.keyframes.first().copied() {
            let before = self.entries.len();
            self.entries
                .retain(|sequence, _| sequence_ahead_or_at(*sequence, keyframe));
            if self.entries.len() != before {
                self.final_lost_packets = self
                    .final_lost_packets
                    .saturating_add((before - self.entries.len()) as u64);
                return true;
            }
            self.keyframes.remove(0);
        }
        false
    }

    pub(super) fn get_batch(&mut self, filter: NackFilter, now: Instant) -> NackBatch {
        let newest = self.newest_sequence_number;
        let rtt = self.rtt;
        let mut sequences = Vec::new();
        let before = self.entries.len();
        let unrecoverable = unrecoverable_after(rtt).max(self.patience);
        let early = recovery_after(rtt).max(self.patience);
        let mut stale = false;
        let mut late = false;
        self.entries.retain(|_, entry| {
            let retry_delay = nack_retry_delay(rtt, entry.retries);
            let should_send = match filter {
                NackFilter::Sequence => {
                    entry.sent_at.is_none()
                        && sequence_ahead_or_at(newest, entry.send_at_sequence_number)
                }
                NackFilter::Time => entry
                    .sent_at
                    .is_none_or(|sent_at| now.duration_since(sent_at) >= retry_delay),
            };
            if should_send {
                sequences.push(entry.sequence_number);
                entry.retries = entry.retries.saturating_add(1);
                match filter {
                    NackFilter::Sequence => {
                        entry.retries_because_of_sequence =
                            entry.retries_because_of_sequence.saturating_add(1);
                    }
                    NackFilter::Time => {
                        entry.retries_because_of_rtt =
                            entry.retries_because_of_rtt.saturating_add(1);
                    }
                }
                entry.sent_at = Some(now);
            }
            let keep = entry.retries < RTP_NACK_MAX_RETRIES;
            stale |= !keep || now.duration_since(entry.created_at) >= unrecoverable;
            late |= !keep || now.duration_since(entry.created_at) >= early;
            if !keep {
                tracing::warn!(
                    sequence_number = entry.sequence_number,
                    current_size = before,
                    in_nack_list_ms = now.duration_since(entry.created_at).as_millis(),
                    rtt_ms = rtt.as_secs_f64() * 1000.0,
                    retries_because_of_sequence = entry.retries_because_of_sequence,
                    retries_because_of_rtt = entry.retries_because_of_rtt,
                    "sequence number removed from NACK list due to max retries"
                );
            }
            keep
        });
        self.final_lost_packets = self
            .final_lost_packets
            .saturating_add((before - self.entries.len()) as u64);
        sequences.sort_unstable_by(sequence_order);
        // Ask for a keyframe once repair is hopeless, then keep repairing
        // with NACK until it arrives; repeat only after the request spacing.
        let request_keyframe = stale
            && self
                .keyframe_requested
                .is_none_or(|at| {
                    now.duration_since(at) >= keyframe_request_spacing(rtt).max(self.patience)
                });
        if request_keyframe {
            self.keyframe_requested = Some(now);
            tracing::debug!(
                outstanding = self.entries.len(),
                rtt_ms = rtt.as_millis(),
                "unrecoverable video loss; requesting a keyframe"
            );
        }
        NackBatch {
            sequences,
            request_keyframe,
            loss: request_keyframe,
            late,
        }
    }

    pub(super) fn erase_before_cutoff(sequences: &mut Vec<u16>, newest: u16) {
        let cutoff = newest.wrapping_sub(RTP_MAX_PACKET_AGE);
        sequences.retain(|sequence| sequence_ahead_or_at(*sequence, cutoff));
    }

    pub(super) fn outstanding(&self) -> usize {
        self.entries.len()
    }

    pub(super) fn final_lost_packets(&self) -> u64 {
        self.final_lost_packets
    }
}

/// Packets the sequence numbers say were sent on the primary stream, counted
/// as they arrive (RFC 3550 A.3): with the arrivals themselves this gives the
/// network's loss, before RTX or FEC repairs any of it.
#[derive(Default)]
pub(super) struct ArrivalCounter {
    newest: Option<u16>,
}

impl ArrivalCounter {
    /// Packets newly expected once `sequence_number` arrived: the advance of
    /// the newest sequence number, nothing for a late or repeated one.
    pub(super) fn expected_by(&mut self, sequence_number: u16) -> u64 {
        let Some(newest) = self.newest else {
            self.newest = Some(sequence_number);
            return 1;
        };
        if !sequence_ahead_of(sequence_number, newest) {
            return 0;
        }
        self.newest = Some(sequence_number);
        let advance = sequence_number.wrapping_sub(newest);
        // A jump this far is a new sequence, not that much loss.
        if advance >= RTP_MAX_PACKET_AGE {
            1
        } else {
            u64::from(advance)
        }
    }
}

pub(super) fn sequence_ahead_of(newer: u16, older: u16) -> bool {
    let distance = newer.wrapping_sub(older);
    if distance == 0x8000 {
        newer > older
    } else {
        distance != 0 && distance < 0x8000
    }
}

pub(super) fn sequence_ahead_or_at(newer: u16, older: u16) -> bool {
    newer == older || sequence_ahead_of(newer, older)
}

pub(super) fn sequence_order(left: &u16, right: &u16) -> std::cmp::Ordering {
    if left == right {
        std::cmp::Ordering::Equal
    } else if sequence_ahead_of(*left, *right) {
        std::cmp::Ordering::Greater
    } else {
        std::cmp::Ordering::Less
    }
}

pub(super) fn nack_retry_delay(rtt: Duration, retries: u8) -> Duration {
    // RTT rounded to milliseconds, exponential retry term
    // truncated to milliseconds; compare their minimum, not sub-ms floats.
    let rtt_ms = ((rtt.as_micros() + 500) / 1_000) as u64;
    let backoff_ms = (RTP_NACK_BACKOFF_START.as_millis() as f64
        * RTP_NACK_BACKOFF_BASE.powi(i32::from(retries))) as u64;
    Duration::from_millis(rtt_ms.min(backoff_ms))
}

pub(super) async fn send_picture_loss_indication(
    connection: &RTCPeerConnection,
    media_ssrc: u32,
) -> Result<()> {
    tracing::debug!(media_ssrc, "sending RTCP PLI");
    super::impair::record_keyframe_request();
    let pli: Box<dyn RtcpPacket + Send + Sync> = Box::new(PictureLossIndication {
        sender_ssrc: DEFAULT_RECEIVER_SSRC,
        media_ssrc,
    });
    connection
        .write_rtcp(&[pli])
        .await
        .context("send RTCP picture-loss indication")?;
    Ok(())
}

pub(super) async fn send_transport_layer_nack(
    connection: &RTCPeerConnection,
    media_ssrc: u32,
    missing_sequences: &[u16],
) -> Result<()> {
    if missing_sequences.is_empty() {
        return Ok(());
    }
    let mut sorted = missing_sequences.to_vec();
    sorted.sort_unstable_by(sequence_order);
    sorted.dedup();
    let pairs = nack_pairs_from_sequence_numbers(&sorted);
    let diagnostic_start =
        tracing::enabled!(target: "farwing::nack_audit", tracing::Level::DEBUG).then(Instant::now);
    tracing::debug!(target: "farwing::nack_audit", media_ssrc, sequences = ?sorted,
        "sending packet-repair request");
    let nack: Box<dyn RtcpPacket + Send + Sync> = Box::new(TransportLayerNack {
        sender_ssrc: DEFAULT_RECEIVER_SSRC,
        media_ssrc,
        nacks: pairs,
    });
    connection
        .write_rtcp(&[nack])
        .await
        .context("send RTCP transport-layer NACK")?;
    if let Some(start) = diagnostic_start {
        tracing::debug!(target: "farwing::nack_audit", media_ssrc,
            write_us = start.elapsed().as_micros(), "packet-repair request write completed");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn arrivals_count_the_sequence_advance() {
        let mut counter = ArrivalCounter::default();
        assert_eq!(counter.expected_by(65_534), 1);
        assert_eq!(counter.expected_by(65_535), 1);
        // 0 and 1 lost across the wrap.
        assert_eq!(counter.expected_by(2), 3);
        // A late 1 and a repeated 2 add nothing.
        assert_eq!(counter.expected_by(1), 0);
        assert_eq!(counter.expected_by(2), 0);
        assert_eq!(counter.expected_by(3 + RTP_MAX_PACKET_AGE), 1);
    }

    #[test]
    fn given_up_repairs_count_as_unrepaired() {
        let mut nack = NackRequester::new();
        nack.on_received(10, true, false);
        nack.on_received(15, false, false);
        assert_eq!(nack.outstanding(), 4);
        // 11 arrives by RTX: repaired.
        nack.on_received(11, false, true);
        assert_eq!(nack.outstanding(), 3);
        nack.clear_up_to(13);
        assert_eq!(nack.final_lost_packets(), 1);
        nack.clear_pending();
        assert_eq!(nack.final_lost_packets(), 3);
        assert_eq!(nack.outstanding(), 0);
    }
}
