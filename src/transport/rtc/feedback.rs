use crate::transport::rtcp_timing::DEFAULT_RECEIVER_SSRC;
use anyhow::{Context, Result};
use std::collections::HashMap;
use std::time::{Duration, Instant};
use webrtc::peer_connection::RTCPeerConnection;
use webrtc::rtcp::packet::Packet as RtcpPacket;
use webrtc::rtcp::payload_feedbacks::full_intra_request::{FirEntry, FullIntraRequest};
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

/// 首个包之前补发 NACK 的窗口大小。UU 的 macOS host 只在会话开始时下发
/// 一次 SPS/PPS，且 RTP 订阅建立往往晚于流起点（首包即关键帧中段），/// 需要让支持 RTX 的 host 重传这段订阅前窗口。
/// 1080p 关键帧约 25-120 个 FU-A 片 + 一个 STAP-A 参数集包：
/// 窗口必须覆盖整帧，否则 IDR 头部（含 S=1 首片）永远不可恢复。
/// 128 个条目的 NACK ~600B，仍在单个 RTCP 包内。
/// 必须与 packets.rs 的 PRE_WINDOW_NACK 保持一致。
pub(super) const RTP_STARTUP_NACK_WINDOW: u16 = 128;

pub(super) const RTP_NACK_BACKOFF_BASE: f64 = 1.25;

pub(super) const RTP_NACK_BACKOFF_START: Duration = Duration::from_millis(50);

pub(super) const KEYFRAME_PACKET_WINDOW: Duration = Duration::from_millis(200);

pub(super) const ACTIVE_STREAM_WINDOW: Duration = Duration::from_secs(5);

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
            // 同时发 PLI 与 FIR：部分 host（macOS）只对 FIR 生成关键帧。
            let pli = send_picture_loss_indication(connection, media_ssrc).await;
            if let Err(error) = pli {
                tracing::warn!(%error, "buffered keyframe request failed");
            }
            let fir = send_full_intra_request(connection, media_ssrc).await;
            if let Err(error) = fir {
                tracing::warn!(%error, "buffered FIR request failed");
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
        }
    }

    pub(super) fn set_rtt(&mut self, rtt: Duration) {
        self.rtt = rtt;
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
            // 订阅建立时流的开头（SPS/PPS、关键帧首片）可能已经发出：
            // 日志里首包常是关键帧的中段 FU-A（S=0）。对首包之前的窗口
            // 立即补发 NACK，host 的 RTX 重传可补齐这部分包。首包本身就是
            // 关键帧开头时没有缺口，不去要旧帧的包。
            if !starts_keyframe {
                self.add_missing(
                    sequence_number.wrapping_sub(RTP_STARTUP_NACK_WINDOW),
                    sequence_number,
                );
            }
            let batch = self.get_batch(NackFilter::Sequence, now);
            return NackReceiveResult {
                batch,
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
        NackReceiveResult {
            batch,
            nack_count: 0,
        }
    }

    pub(super) fn process(&mut self) -> NackBatch {
        self.get_batch(NackFilter::Time, Instant::now())
    }

    pub(super) fn clear_pending(&mut self) {
        // Clearing pending repairs preserves RTT, sequence anchors,
        // recovered/keyframe history and cumulative diagnostics.
        self.entries.clear();
    }

    pub(super) fn clear_up_to(&mut self, sequence_number: u16) {
        self.entries
            .retain(|sequence, _| sequence_ahead_or_at(*sequence, sequence_number));
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
        NackBatch {
            sequences,
            request_keyframe: false,
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
    // 1D84C3..1D8510: RTT rounded to milliseconds, exponential retry term
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

/// FIR（RFC 5104 §3.5.1）。部分 host 只对 FIR 生成关键帧，
/// 因此与 PLI 一起发送。序号按媒体 SSRC 单调递增，host 用它去重；
/// 所有发送路径共用一个计数，重复的序号会被当作同一请求丢弃。
pub(super) async fn send_full_intra_request(
    connection: &RTCPeerConnection,
    media_ssrc: u32,
) -> Result<()> {
    static SEQUENCES: std::sync::LazyLock<std::sync::Mutex<HashMap<u32, u8>>> =
        std::sync::LazyLock::new(Default::default);
    let sequence = {
        let mut sequences = SEQUENCES
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let next = sequences.entry(media_ssrc).or_default();
        let sequence = *next;
        *next = next.wrapping_add(1);
        sequence
    };
    tracing::debug!(media_ssrc, sequence, "sending RTCP FIR");
    let fir: Box<dyn RtcpPacket + Send + Sync> = Box::new(FullIntraRequest {
        sender_ssrc: DEFAULT_RECEIVER_SSRC,
        media_ssrc,
        fir: vec![FirEntry {
            ssrc: media_ssrc,
            sequence_number: sequence,
        }],
    });
    connection
        .write_rtcp(&[fir])
        .await
        .context("send RTCP full intra request")?;
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
        tracing::enabled!(target: "openuuyc::nack_audit", tracing::Level::DEBUG).then(Instant::now);
    tracing::debug!(target: "openuuyc::nack_audit", media_ssrc, sequences = ?sorted,
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
        tracing::debug!(target: "openuuyc::nack_audit", media_ssrc,
            write_us = start.elapsed().as_micros(), "packet-repair request write completed");
    }
    Ok(())
}
