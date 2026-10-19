#[cfg(test)]
mod twcc_test;

pub mod receiver;
pub mod sender;

use std::collections::BTreeMap;

use rtcp::transport_feedbacks::transport_layer_cc::{
    PacketStatusChunk, RecvDelta, RunLengthChunk, StatusChunkTypeTcc, StatusVectorChunk,
    SymbolSizeTypeTcc, SymbolTypeTcc, TransportLayerCc,
};

// A transport-wide, bounded sequence history, retained after feedback to
// recognize duplicates and late arrivals.
const HISTORY_SPAN: i64 = 32768;
const HISTORY_WINDOW_US: i64 = 500_000;
const REFERENCE_PERIOD_US: i64 = (1 << 24) * 64_000;

#[derive(Default, Debug, PartialEq, Clone)]
pub struct Recorder {
    arrivals: BTreeMap<i64, i64>,
    history_begin: i64,
    history_end: i64,
    last_sequence: Option<i64>,
    periodic_start: Option<i64>,
    sender_ssrc: u32,
    media_ssrc: u32,
    fb_pkt_cnt: u8,
}

impl Recorder {
    pub fn new(sender_ssrc: u32) -> Self {
        Self {
            sender_ssrc,
            ..Default::default()
        }
    }

    pub fn record(&mut self, media_ssrc: u32, sequence_number: u16, arrival_time: i64) {
        if arrival_time < 0 {
            return;
        }
        self.media_ssrc = media_ssrc;
        let sequence = match self.last_sequence {
            None => i64::from(sequence_number),
            Some(last) => {
                let previous = last as u16;
                let forward = sequence_number.wrapping_sub(previous);
                let delta = if forward < 32768 || (forward == 32768 && sequence_number > previous) {
                    i64::from(forward)
                } else {
                    i64::from(forward) - 65536
                };
                last + delta
            }
        };
        self.last_sequence = Some(sequence);

        if self
            .periodic_start
            .is_some_and(|start| start >= self.history_end)
            && arrival_time >= HISTORY_WINDOW_US
        {
            let cutoff = arrival_time - HISTORY_WINDOW_US;
            while self.history_begin < sequence.min(self.history_end) {
                if self
                    .arrivals
                    .get(&self.history_begin)
                    .is_some_and(|time| *time > cutoff)
                {
                    break;
                }
                self.arrivals.remove(&self.history_begin);
                self.history_begin += 1;
            }
        }
        // Native updates the pending start even for a duplicate. Its existing
        // timestamp is never overwritten by a later delivery.
        self.periodic_start = Some(
            self.periodic_start
                .map_or(sequence, |start| start.min(sequence)),
        );
        if self.arrivals.contains_key(&sequence) {
            return;
        }

        if self.arrivals.is_empty() && self.history_begin == self.history_end {
            self.history_begin = sequence;
            self.history_end = sequence + 1;
        } else if sequence < self.history_begin {
            if self.history_end - sequence > HISTORY_SPAN {
                self.periodic_start = self
                    .periodic_start
                    .map(|start| start.max(self.history_begin));
                return;
            }
            self.history_begin = sequence;
        } else if sequence >= self.history_end {
            if sequence >= self.history_end + HISTORY_SPAN - 1 {
                self.arrivals.clear();
                self.history_begin = sequence;
            } else {
                let begin = self.history_begin.max(sequence + 1 - HISTORY_SPAN);
                while self
                    .arrivals
                    .first_key_value()
                    .is_some_and(|(&key, _)| key < begin)
                {
                    self.arrivals.pop_first();
                }
                self.history_begin = begin;
            }
            self.history_end = sequence + 1;
        }
        self.arrivals.insert(sequence, arrival_time);
        self.periodic_start = self
            .periodic_start
            .map(|start| start.max(self.history_begin));
    }

    pub fn build_feedback_packet(&mut self) -> Vec<Box<dyn rtcp::packet::Packet + Send + Sync>> {
        let Some(mut start) = self.periodic_start else {
            return vec![];
        };
        let end = self.history_end;
        let mut packets: Vec<Box<dyn rtcp::packet::Packet + Send + Sync>> = Vec::new();
        while start < end {
            let mut feedback: Option<Feedback> = None;
            let mut next = start;
            for (&sequence, &arrival) in self.arrivals.range(start.max(self.history_begin)..end) {
                let builder = feedback.get_or_insert_with(|| {
                    let mut builder =
                        Feedback::new(self.sender_ssrc, self.media_ssrc, self.fb_pkt_cnt);
                    self.fb_pkt_cnt = self.fb_pkt_cnt.wrapping_add(1);
                    builder.set_base(start as u16, arrival);
                    builder
                });
                if !builder.add_received(sequence as u16, arrival) {
                    break;
                }
                next = sequence + 1;
            }
            let Some(mut feedback) = feedback else {
                break;
            };
            if next == start {
                break;
            }
            packets.push(Box::new(feedback.get_rtcp()));
            start = next;
            self.periodic_start = Some(start);
        }
        packets
    }
}

#[derive(Default, Debug, PartialEq, Clone)]
struct Feedback {
    rtcp: TransportLayerCc,
    base_sequence_number: u16,
    ref_timestamp64ms: i64,
    last_timestamp_us: i64,
    next_sequence_number: u16,
    sequence_number_count: u16,
    last_chunk: Chunk,
    chunks: Vec<PacketStatusChunk>,
    deltas: Vec<RecvDelta>,
}

impl Feedback {
    fn new(sender_ssrc: u32, media_ssrc: u32, fb_pkt_count: u8) -> Self {
        Self {
            rtcp: TransportLayerCc {
                sender_ssrc,
                media_ssrc,
                fb_pkt_count,
                ..Default::default()
            },
            ..Default::default()
        }
    }

    fn set_base(&mut self, sequence_number: u16, time_us: i64) {
        self.base_sequence_number = sequence_number;
        self.next_sequence_number = sequence_number;
        self.ref_timestamp64ms = time_us.div_euclid(64000);
        self.last_timestamp_us = self.ref_timestamp64ms * 64000;
    }

    fn get_rtcp(&mut self) -> TransportLayerCc {
        self.rtcp.packet_status_count = self.sequence_number_count;
        self.rtcp.reference_time = (self.ref_timestamp64ms & 0xffffff) as u32;
        self.rtcp.base_sequence_number = self.base_sequence_number;
        while !self.last_chunk.deltas.is_empty() {
            self.chunks.push(self.last_chunk.encode());
        }
        self.rtcp.packet_chunks.clone_from(&self.chunks);
        self.rtcp.recv_deltas.clone_from(&self.deltas);
        self.rtcp.clone()
    }

    fn add_status(&mut self, status: SymbolTypeTcc) {
        if !self.last_chunk.can_add(status as u16) {
            self.chunks.push(self.last_chunk.encode());
        }
        self.last_chunk.add(status as u16);
        self.sequence_number_count += 1;
        self.next_sequence_number = self.next_sequence_number.wrapping_add(1);
    }

    fn add_received(&mut self, sequence_number: u16, timestamp_us: i64) -> bool {
        let missing = sequence_number.wrapping_sub(self.next_sequence_number);
        if missing != 0 {
            let previous = self.next_sequence_number.wrapping_sub(1);
            let forward = sequence_number.wrapping_sub(previous);
            if forward == 0 || forward > 32768 || (forward == 32768 && sequence_number <= previous)
            {
                return false;
            }
        }
        if u32::from(self.sequence_number_count) + u32::from(missing) + 1 > u32::from(u16::MAX) {
            return false;
        }
        // Unwrap the 24-bit reference period, round to the nearest 250 us,
        // and advance by the encoded delta, carrying the remainder.
        let mut delta = (timestamp_us - self.last_timestamp_us).rem_euclid(REFERENCE_PERIOD_US);
        if delta > REFERENCE_PERIOD_US / 2 {
            delta -= REFERENCE_PERIOD_US;
        }
        let ticks = if delta < 0 {
            (delta - 125) / 250
        } else {
            (delta + 125) / 250
        };
        if i16::try_from(ticks).is_err() {
            return false;
        }
        for _ in 0..missing {
            self.add_status(SymbolTypeTcc::PacketNotReceived);
        }
        let status = if (0..=255).contains(&ticks) {
            SymbolTypeTcc::PacketReceivedSmallDelta
        } else {
            SymbolTypeTcc::PacketReceivedLargeDelta
        };
        self.add_status(status);
        self.deltas.push(RecvDelta {
            type_tcc_packet: status,
            delta: ticks * 250,
        });
        self.last_timestamp_us += ticks * 250;
        // <=65535 statuses, <=2 bytes/delta and at least seven symbols per
        // full chunk stay below the native 0x40000-byte builder bound.
        true
    }
}

const MAX_RUN_LENGTH_CAP: usize = 0x1fff; // 13 bits
const MAX_ONE_BIT_CAP: usize = 14; // bits
const MAX_TWO_BIT_CAP: usize = 7; // bits

#[derive(Default, Debug, PartialEq, Clone)]
struct Chunk {
    has_large_delta: bool,
    has_different_types: bool,
    deltas: Vec<u16>,
}

impl Chunk {
    fn can_add(&self, delta: u16) -> bool {
        if self.deltas.len() < MAX_TWO_BIT_CAP {
            return true;
        }
        if self.deltas.len() < MAX_ONE_BIT_CAP
            && !self.has_large_delta
            && delta != SymbolTypeTcc::PacketReceivedLargeDelta as u16
        {
            return true;
        }
        if self.deltas.len() < MAX_RUN_LENGTH_CAP
            && !self.has_different_types
            && delta == self.deltas[0]
        {
            return true;
        }
        false
    }

    fn add(&mut self, delta: u16) {
        self.deltas.push(delta);
        self.has_large_delta =
            self.has_large_delta || delta == SymbolTypeTcc::PacketReceivedLargeDelta as u16;
        self.has_different_types = self.has_different_types || delta != self.deltas[0];
    }

    fn encode(&mut self) -> PacketStatusChunk {
        if !self.has_different_types {
            let p = PacketStatusChunk::RunLengthChunk(RunLengthChunk {
                type_tcc: StatusChunkTypeTcc::RunLengthChunk,
                packet_status_symbol: self.deltas[0].into(),
                run_length: self.deltas.len() as u16,
            });
            self.reset();
            return p;
        }
        if self.deltas.len() == MAX_ONE_BIT_CAP {
            let p = PacketStatusChunk::StatusVectorChunk(StatusVectorChunk {
                type_tcc: StatusChunkTypeTcc::StatusVectorChunk,
                symbol_size: SymbolSizeTypeTcc::OneBit,
                symbol_list: self
                    .deltas
                    .iter()
                    .map(|x| SymbolTypeTcc::from(*x))
                    .collect::<Vec<SymbolTypeTcc>>(),
            });
            self.reset();
            return p;
        }

        let min_cap = std::cmp::min(MAX_TWO_BIT_CAP, self.deltas.len());
        let svc = PacketStatusChunk::StatusVectorChunk(StatusVectorChunk {
            type_tcc: StatusChunkTypeTcc::StatusVectorChunk,
            symbol_size: SymbolSizeTypeTcc::TwoBit,
            symbol_list: self.deltas[..min_cap]
                .iter()
                .map(|x| SymbolTypeTcc::from(*x))
                .collect::<Vec<SymbolTypeTcc>>(),
        });
        self.deltas.drain(..min_cap);
        self.has_different_types = false;
        self.has_large_delta = false;

        if !self.deltas.is_empty() {
            let tmp = self.deltas[0];
            for d in &self.deltas {
                if tmp != *d {
                    self.has_different_types = true;
                }
                if *d == SymbolTypeTcc::PacketReceivedLargeDelta as u16 {
                    self.has_large_delta = true;
                }
            }
        }

        svc
    }

    fn reset(&mut self) {
        self.deltas = vec![];
        self.has_large_delta = false;
        self.has_different_types = false;
    }
}
