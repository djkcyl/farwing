//! Farwing RS-FEC sender, using the systematic CM256 implementation.
//!
//! Redundancy follows `transport::fec_policy` (binomial loss model).
use crate::transport::rsfec::{RsFecConfig, cm256_encode_repairs, zero_mutable_extensions};
use anyhow::{Context, Result, ensure};
use std::collections::HashMap;
use webrtc::{interceptor::stream_info::StreamInfo, rtp::packet::Packet, util::marshal::Marshal};

pub(crate) struct Sender {
    max_k: usize,
    mutable: HashMap<u8, usize>,
    sources: Vec<(u16, Vec<u8>)>,
    block: u16,
    frames: u32,
    keyframe: bool,
}
pub(crate) struct Block {
    id: u16,
    sources: Vec<(u16, Vec<u8>)>,
    protected_ssrc: u32,
    pub desired: usize,
    pub shard_size: usize,
    pub keyframe: bool,
}
impl Sender {
    pub fn new(info: &StreamInfo) -> Self {
        let config = RsFecConfig::from_fmtp(&info.sdp_fmtp_line);
        let mutable = info
            .rtp_header_extensions
            .iter()
            .filter_map(|e| {
                let preserve = match e.uri.as_str() {
                    "http://www.ietf.org/id/draft-holmer-rmcat-transport-wide-cc-extensions-01"
                    | "urn:farwing:rtp-hdrext:sending-delay" => 0,
                    "http://www.webrtc.org/experiments/rtp-hdrext/video-timing" => 7,
                    _ => return None,
                };
                u8::try_from(e.id).ok().map(|id| (id, preserve))
            })
            .collect();
        Self {
            max_k: config.max_k as usize,
            mutable,
            sources: Vec::new(),
            block: rand::random(),
            frames: 0,
            keyframe: false,
        }
    }
    pub fn clear(&mut self) {
        self.sources.clear();
        self.frames = 0;
        self.keyframe = false;
    }
    pub fn push(
        &mut self,
        packet: &Packet,
        keyframe: bool,
        loss: f64,
        rtt_ms: u64,
    ) -> Result<Option<Block>> {
        let raw = packet.marshal()?;
        // Sized while FEC was off (it just turned on): this block goes
        // without; the next frames leave room for the repair header.
        if raw.len() + 2 > 1500 {
            self.clear();
            return Ok(None);
        }
        let raw = zero_mutable_extensions(&raw, &self.mutable)
            .context("RSFEC source extensions invalid")?;
        if self.sources.first().is_some_and(|(base, _)| {
            packet.header.sequence_number.wrapping_sub(*base) as usize >= self.max_k
        }) {
            self.clear();
        }
        let mut shard = Vec::with_capacity(raw.len() + 2);
        shard.extend_from_slice(&(raw.len() as u16).to_be_bytes());
        shard.extend_from_slice(&raw);
        self.sources.push((packet.header.sequence_number, shard));
        self.keyframe |= keyframe;
        self.frames += u32::from(packet.header.marker);
        // One block per frame: its repair packets follow the frame at once,
        // so a loss is repaired within the frame instead of after the next
        // few frames (about one RTT, no faster than NACK). The FEC budget
        // limits the overhead of small frames.
        if self.sources.len() < self.max_k && !packet.header.marker {
            return Ok(None);
        }
        let sources = std::mem::take(&mut self.sources);
        let k = sources.len();
        let shard_size = sources.iter().map(|(_, s)| s.len()).max().unwrap_or(0);
        // Keyframes and high-RTT links cannot afford a retransmission round
        // trip; protect them to a stricter target.
        let strict = rtt_ms > 120 || self.keyframe || k >= 71;
        let target = if strict {
            crate::transport::fec_policy::VIDEO_STRICT_TARGET
        } else {
            crate::transport::fec_policy::VIDEO_TARGET
        };
        // Redundancy stays between ~5 % and ~35 % of the block.
        let floor = ((k * 12 + 128) >> 8).max(1);
        let ceiling = ((k * 89 + 128) >> 8).max(1);
        let mut desired =
            crate::transport::fec_policy::repair_count(k, loss, target, floor, ceiling);
        if !self.keyframe && rtt_ms < 20 {
            // On a very short RTT a NACK/RTX round trip is cheaper.
            desired = floor;
        }
        desired = desired.clamp(floor, ceiling).min(k).min(256 - k);
        let block = Block {
            id: self.block,
            sources,
            protected_ssrc: packet.header.ssrc,
            desired,
            shard_size,
            keyframe: self.keyframe,
        };
        self.block = self.block.wrapping_add(1);
        self.frames = 0;
        self.keyframe = false;
        Ok(Some(block))
    }
}
impl Block {
    pub fn payloads(mut self, count: usize) -> Result<Vec<Vec<u8>>> {
        let k = self.sources.len();
        let count = count.min(self.desired).min(k).min(256 - k);
        if count == 0 {
            return Ok(Vec::new());
        }
        let base = self.sources[0].0;
        let span = self.sources.last().unwrap().0.wrapping_sub(base) as usize + 1;
        ensure!(span <= 109, "RSFEC sequence span exceeds mask");
        let mask_size = if span <= 15 {
            2
        } else if span <= 46 {
            6
        } else {
            14
        };
        let mut header = vec![0u8; 13 + mask_size];
        header[..2].copy_from_slice(&self.id.to_be_bytes());
        header[2] = k as u8;
        header[3] = count as u8;
        header[5..9].copy_from_slice(&self.protected_ssrc.to_be_bytes());
        header[9..11].copy_from_slice(&(self.shard_size as u16).to_be_bytes());
        header[11..13].copy_from_slice(&base.to_be_bytes());
        header[13
            + match mask_size {
                2 => 0,
                6 => 2,
                _ => 6,
            }] = 0x80;
        for (seq, shard) in &mut self.sources {
            let bit = seq.wrapping_sub(base) as usize;
            let wire = bit
                + if bit < 15 {
                    1
                } else if bit < 46 {
                    2
                } else {
                    3
                };
            header[13 + wire / 8] |= 0x80 >> (wire % 8);
            shard.resize(self.shard_size, 0);
        }
        let sources: Vec<_> = self.sources.into_iter().map(|(_, s)| s).collect();
        let repairs = cm256_encode_repairs(&sources, count as u8, self.shard_size)?;
        Ok(repairs
            .into_iter()
            .enumerate()
            .map(|(i, repair)| {
                let mut payload = header.clone();
                payload[4] = (k + i) as u8;
                payload.extend(repair);
                payload
            })
            .collect())
    }
}
