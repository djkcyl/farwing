//! Shared receive state for the FlexFEC and ULPFEC readers. One
//! ForwardErrorCorrection object owns the same packet/ref-counted recovery
//! lists for both formats.

use std::collections::{HashMap, VecDeque};
use std::sync::Arc;

use anyhow::{Result, ensure};

use crate::transport::rsfec::zero_mutable_extensions;

const MAX_MEDIA_PACKETS: usize = 192;
const MAX_REPAIR_PACKETS: usize = 48;
const MAX_SEQUENCE_DISTANCE: u16 = 0x4000;
const MAX_RTP_PACKET_SIZE: usize = 1500;

#[derive(Debug)]
pub(crate) struct ParityPacket {
    pub(crate) protected_ssrc: u32,
    pub(crate) protected_sequences: Vec<u16>,
    pub(crate) header_size: usize,
    pub(crate) protection_length: usize,
    pub(crate) payload: Vec<u8>,
}

#[derive(Debug)]
struct MediaPacket {
    sequence: u16,
    data: Arc<[u8]>,
    returned: bool,
}

#[derive(Debug)]
struct RepairPacket {
    ssrc: u32,
    sequence: u16,
    parity: ParityPacket,
    // A repair retains known source packets even after the main media list
    // evicts them. Looking them up in a global cache at recovery time is not
    // equivalent to the original per-protected-packet reference ownership.
    sources: Vec<Option<Arc<[u8]>>>,
}

#[derive(Debug)]
pub(crate) struct XorFecReceiver {
    protected_ssrc: u32,
    media: VecDeque<MediaPacket>,
    repairs: VecDeque<RepairPacket>,
    mutable_extensions: HashMap<u8, usize>,
}

impl XorFecReceiver {
    pub(crate) fn new(
        protected_ssrc: u32,
        mutable_extensions: impl IntoIterator<Item = (u8, usize)>,
    ) -> Self {
        Self {
            protected_ssrc,
            media: VecDeque::new(),
            repairs: VecDeque::new(),
            mutable_extensions: mutable_extensions.into_iter().collect(),
        }
    }

    fn prepare(&mut self, ssrc: u32, sequence: u16) {
        if ssrc == self.protected_ssrc
            && self.media.len() == MAX_MEDIA_PACKETS
            && self
                .media
                .back()
                .is_some_and(|last| distance(last.sequence, sequence) > MAX_MEDIA_PACKETS as u16)
        {
            self.media.clear();
            self.repairs.clear();
        }
        while self.repairs.front().is_some_and(|oldest| {
            oldest.ssrc == ssrc && distance(oldest.sequence, sequence) >= MAX_SEQUENCE_DISTANCE
        }) {
            self.repairs.pop_front();
        }
    }

    pub(crate) fn remember_media(&mut self, sequence: u16, raw_rtp: &[u8]) -> Result<Vec<Vec<u8>>> {
        ensure!(
            raw_rtp.len() >= 12,
            "XOR FEC media packet is shorter than its RTP header"
        );
        self.prepare(self.protected_ssrc, sequence);
        if !self.media.iter().any(|packet| packet.sequence == sequence) {
            // ULPFEC's ProcessReceivedFec keeps the original packet
            // when RTP reparse fails, so do not invent a mandatory failure
            // policy for this normalization helper.
            let normalized = zero_mutable_extensions(raw_rtp, &self.mutable_extensions)
                .unwrap_or_else(|| raw_rtp.to_vec());
            self.insert_media(sequence, Arc::from(normalized), true);
        }
        self.recover_available();
        Ok(self.take_recovered())
    }

    pub(crate) fn receive_repair(
        &mut self,
        ssrc: u32,
        sequence: u16,
        parse: impl FnOnce() -> Result<ParityPacket>,
    ) -> Result<Vec<Vec<u8>>> {
        self.prepare(ssrc, sequence);
        if self
            .repairs
            .iter()
            .any(|packet| packet.sequence == sequence)
        {
            return Ok(Vec::new());
        }
        let parity = parse()?;
        ensure!(
            parity.protected_ssrc == self.protected_ssrc,
            "XOR FEC protected SSRC mismatch"
        );
        let sources = parity
            .protected_sequences
            .iter()
            .map(|sequence| {
                self.media
                    .iter()
                    .find(|packet| packet.sequence == *sequence)
                    .map(|packet| Arc::clone(&packet.data))
            })
            .collect();
        let position = self
            .repairs
            .iter()
            .position(|packet| ahead_of(packet.sequence, sequence))
            .unwrap_or(self.repairs.len());
        self.repairs.insert(
            position,
            RepairPacket {
                ssrc,
                sequence,
                parity,
                sources,
            },
        );
        if self.repairs.len() > MAX_REPAIR_PACKETS {
            self.repairs.pop_front();
        }
        self.recover_available();
        Ok(self.take_recovered())
    }

    fn insert_media(&mut self, sequence: u16, data: Arc<[u8]>, returned: bool) {
        for repair in &mut self.repairs {
            for (protected, source) in repair
                .parity
                .protected_sequences
                .iter()
                .zip(&mut repair.sources)
            {
                if *protected == sequence {
                    *source = Some(Arc::clone(&data));
                }
            }
        }
        let position = self
            .media
            .iter()
            .position(|packet| ahead_of(packet.sequence, sequence))
            .unwrap_or(self.media.len());
        self.media.insert(
            position,
            MediaPacket {
                sequence,
                data,
                returned,
            },
        );
        if self.media.len() > MAX_MEDIA_PACKETS {
            self.media.pop_front();
        }
    }

    fn recover_available(&mut self) {
        let mut index = 0;
        while let Some(repair) = self.repairs.get(index) {
            let mut missing = repair
                .sources
                .iter()
                .enumerate()
                .filter(|(_, source)| source.is_none());
            let first = missing.next().map(|(index, _)| index);
            let multiple_missing = missing.next().is_some();
            let Some(missing_index) = first else {
                self.repairs.remove(index);
                continue;
            };
            if !multiple_missing {
                let repair = self.repairs.remove(index).expect("current repair exists");
                match recover_packet(&repair, missing_index) {
                    Ok(data) => {
                        self.insert_media(
                            repair.parity.protected_sequences[missing_index],
                            Arc::from(data),
                            false,
                        );
                        // A success can unlock an earlier repair. Failure only
                        // removes that repair; earlier successes remain owned
                        // by the media list and are still returned below.
                        index = 0;
                    }
                    Err(error) => {
                        tracing::debug!(%error, sequence = repair.sequence, "discard corrupted XOR FEC recovery")
                    }
                }
            } else if self
                .media
                .back()
                .zip(repair.parity.protected_sequences.last())
                .is_some_and(|(media, last_protected)| {
                    distance(media.sequence, *last_protected) >= MAX_SEQUENCE_DISTANCE
                })
            {
                self.repairs.remove(index);
            } else {
                index += 1;
            }
        }
    }

    fn take_recovered(&mut self) -> Vec<Vec<u8>> {
        self.media
            .iter_mut()
            .filter_map(|packet| {
                if packet.returned {
                    return None;
                }
                packet.returned = true;
                Some(packet.data.to_vec())
            })
            .collect()
    }
}

fn distance(left: u16, right: u16) -> u16 {
    left.wrapping_sub(right).min(right.wrapping_sub(left))
}

fn ahead_of(left: u16, right: u16) -> bool {
    let delta = left.wrapping_sub(right);
    if delta == 0x8000 {
        left > right
    } else {
        delta != 0 && delta < 0x8000
    }
}

fn recover_packet(repair: &RepairPacket, missing_index: usize) -> Result<Vec<u8>> {
    let fec = &repair.parity;
    ensure!(
        fec.payload.len() >= fec.header_size + fec.protection_length,
        "truncated FEC protection payload"
    );
    ensure!(
        fec.protection_length
            <= MAX_RTP_PACKET_SIZE
                .saturating_sub(fec.header_size)
                .min(MAX_RTP_PACKET_SIZE - 12),
        "FEC protection length exceeds packet size"
    );
    let mut recovered = vec![0_u8; 12 + fec.protection_length];
    recovered[..12].copy_from_slice(&fec.payload[..12]);
    recovered[12..]
        .copy_from_slice(&fec.payload[fec.header_size..fec.header_size + fec.protection_length]);
    for packet in repair.sources.iter().flatten() {
        ensure!(packet.len() >= 12, "invalid protected RTP length");
        let length = ((packet.len() - 12) as u16).to_be_bytes();
        recovered[0] ^= packet[0];
        recovered[1] ^= packet[1];
        recovered[2] ^= length[0];
        recovered[3] ^= length[1];
        for i in 4..8 {
            recovered[i] ^= packet[i];
        }
        // The original XOR helper grows the output when a known media packet
        // extends past the parity's protection length.
        if recovered.len() < packet.len() {
            recovered.resize(packet.len(), 0);
        }
        for i in 12..packet.len() {
            recovered[i] ^= packet[i];
        }
    }
    let length = usize::from(u16::from_be_bytes([recovered[2], recovered[3]])) + 12;
    ensure!(
        length <= MAX_RTP_PACKET_SIZE,
        "recovered RTP length exceeds packet size"
    );
    recovered[0] = (recovered[0] & 0x3f) | 0x80;
    recovered[2..4].copy_from_slice(&fec.protected_sequences[missing_index].to_be_bytes());
    recovered[8..12].copy_from_slice(&fec.protected_ssrc.to_be_bytes());
    recovered.resize(length, 0);
    Ok(recovered)
}
