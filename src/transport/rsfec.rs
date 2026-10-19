//! Farwing RS-FEC (`video/x-farwing-rsfec`) wire format and systematic CM256
//! recovery. There is no online re-encoding acceptance gate.

use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::sync::{Arc, OnceLock};

/// RTP payload format name of Farwing RS-FEC repair packets.
pub(crate) const MIME_TYPE: &str = webrtc::RSFEC_MIME_TYPE;

use anyhow::{Result, bail, ensure};
use bytes::Bytes;
use webrtc::rtp::packet::Packet as RtpPacket;
use webrtc::util::marshal::{Marshal, MarshalSize};

const MAX_K: u8 = 109;
const MAX_SHARD_SIZE: usize = 1_500;
const MIN_SHARD_SIZE: usize = 14;
const MAX_COMPLETED_BLOCKS: usize = 1_024;

#[derive(Clone, Copy, Debug)]
pub(crate) struct RsFecConfig {
    pub max_k: u8,
    pub rtx_as_source: bool,
}

impl RsFecConfig {
    pub(crate) fn from_fmtp(fmtp: &str) -> Self {
        let parameter = |name: &str| {
            fmtp.split(';').find_map(|part| {
                let (key, value) = part.trim().split_once('=')?;
                (key == name).then_some(value)
            })
        };
        let max_k = parameter("max-k")
            .and_then(|value| value.parse::<i32>().ok())
            .filter(|value| (48..=i32::from(MAX_K)).contains(value))
            .unwrap_or(48) as u8;
        Self {
            max_k,
            rtx_as_source: parameter("rtx-as-source") == Some("1"),
        }
    }
}

/// This applies only to the FEC source copy. Failure must not discard the
/// already reconstructed RTX media packet.
pub(crate) fn normalize_rtx_source(
    packet: &RtpPacket,
    rid: Option<u8>,
    repaired_rid: Option<u8>,
    allow_mixed: bool,
) -> Result<Bytes> {
    let mut packet = packet.clone();
    if let Some(source) = repaired_rid
        && rid != Some(source)
        && let Some(index) = packet
            .header
            .extensions
            .iter()
            .position(|ext| ext.id == source)
    {
        let target = rid.ok_or_else(|| anyhow::anyhow!("replacement RID is not registered"))?;
        ensure!(
            !packet.header.extensions.iter().any(|ext| ext.id == target),
            "replacement RID already exists"
        );
        packet.header.extensions[index].id = target;
        ensure!(
            packet
                .header
                .extensions
                .iter()
                .all(|ext| !ext.payload.is_empty() && ext.payload.len() <= 255),
            "cannot rebuild invalid RTP extension"
        );
        let two_byte = packet
            .header
            .extensions
            .iter()
            .any(|ext| ext.id >= 15 || ext.payload.len() > 16);
        ensure!(
            !two_byte || allow_mixed,
            "replacement requires two-byte extensions without allow-mixed"
        );
        packet.header.extension_profile = if two_byte { 0x1000 } else { 0xbede };
        packet.header.extensions_padding = (4 - packet.header.get_extension_payload_len() % 4) % 4;
        ensure!(
            packet.marshal_size() <= MAX_SHARD_SIZE,
            "replacement exceeds RTP packet capacity"
        );
    }
    packet.marshal().map_err(Into::into)
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct RsFecHeader {
    pub block_id: u16,
    pub k: u8,
    pub m: u8,
    pub repair_index: u8,
    pub protected_ssrc: u32,
    pub shard_size: usize,
    pub base_sequence: u16,
    pub data_sequences: Vec<u16>,
    pub header_size: usize,
}

#[derive(Default)]
pub(crate) struct RsFecRecovery {
    pub recovered_packets: Vec<Vec<u8>>,
}

struct RsFecBlock {
    header: RsFecHeader,
    sources: BTreeMap<u8, Arc<[u8; MAX_SHARD_SIZE]>>,
    repairs: BTreeMap<u8, Vec<u8>>,
}

pub(crate) struct RsFecReceiver {
    protected_ssrc: u32,
    max_k: u8,
    media_capacity: usize,
    repair_capacity: usize,
    media_packets: HashMap<u16, Arc<[u8; MAX_SHARD_SIZE]>>,
    media_order: VecDeque<u16>,
    blocks: BTreeMap<u16, RsFecBlock>,
    block_order: VecDeque<u16>,
    repair_shards: usize,
    completed_blocks: HashSet<u16>,
    completed_order: VecDeque<u16>,
    mutable_extensions: HashMap<u8, usize>,
}

impl RsFecHeader {
    pub(crate) fn parse(payload: &[u8]) -> Result<Self> {
        ensure!(
            payload.len() >= 15,
            "RSFEC payload is shorter than its minimum header"
        );
        let block_id = u16::from_be_bytes([payload[0], payload[1]]);
        let k = payload[2];
        let m = payload[3];
        let repair_index = payload[4];
        let protected_ssrc = u32::from_be_bytes(payload[5..9].try_into().expect("fixed field"));
        let shard_size =
            u16::from_be_bytes(payload[9..11].try_into().expect("fixed field")) as usize;
        let base_sequence = u16::from_be_bytes(payload[11..13].try_into().expect("fixed field"));

        ensure!((1..=MAX_K).contains(&k), "RSFEC k is outside 1..={MAX_K}");
        ensure!(
            m != 0 && u16::from(k) + u16::from(m) <= 256,
            "invalid RSFEC k+m"
        );
        ensure!(
            repair_index >= k && u16::from(repair_index) < u16::from(k) + u16::from(m),
            "invalid RSFEC repair index"
        );
        ensure!(
            (MIN_SHARD_SIZE..=MAX_SHARD_SIZE).contains(&shard_size),
            "invalid RSFEC shard size"
        );

        let mask_size = if payload[13] & 0x80 != 0 {
            2
        } else if payload.len() >= 19 && payload[15] & 0x80 != 0 {
            6
        } else if payload.len() >= 27 && payload[19] & 0x80 != 0 {
            14
        } else {
            bail!("invalid RSFEC compressed mask selector");
        };
        let header_size = 13 + mask_size;
        ensure!(
            payload.len() >= header_size + shard_size,
            "truncated RSFEC repair shard"
        );
        let bit_count = match mask_size {
            2 => 15,
            6 => 46,
            14 => 109,
            _ => unreachable!(),
        };
        let wire_mask = &payload[13..header_size];
        let mut data_sequences = Vec::with_capacity(k as usize);
        for normalized_bit in 0..bit_count {
            let skipped_selectors = if normalized_bit < 15 {
                1
            } else if normalized_bit < 46 {
                2
            } else {
                3
            };
            let wire_bit = normalized_bit + skipped_selectors;
            if wire_mask[wire_bit / 8] & (0x80 >> (wire_bit % 8)) != 0 {
                data_sequences.push(base_sequence.wrapping_add(normalized_bit as u16));
            }
        }
        ensure!(
            data_sequences.len() == k as usize,
            "RSFEC mask popcount does not equal k"
        );

        Ok(Self {
            block_id,
            k,
            m,
            repair_index,
            protected_ssrc,
            shard_size,
            base_sequence,
            data_sequences,
            header_size,
        })
    }
}

impl RsFecReceiver {
    pub(crate) fn new(
        protected_ssrc: u32,
        negotiated_max_k: u8,
        mutable_extensions: impl IntoIterator<Item = (u8, usize)>,
    ) -> Self {
        let max_k = if (48..=MAX_K).contains(&negotiated_max_k) {
            negotiated_max_k
        } else {
            48
        };
        let packet_capacity = usize::from(max_k) * 10;
        Self {
            protected_ssrc,
            max_k,
            media_capacity: packet_capacity,
            repair_capacity: packet_capacity,
            media_packets: HashMap::with_capacity(packet_capacity),
            media_order: VecDeque::with_capacity(packet_capacity),
            blocks: BTreeMap::new(),
            block_order: VecDeque::new(),
            repair_shards: 0,
            completed_blocks: HashSet::new(),
            completed_order: VecDeque::new(),
            mutable_extensions: mutable_extensions.into_iter().collect(),
        }
    }

    pub(crate) fn remember_media(
        &mut self,
        sequence_number: u16,
        raw_rtp: &[u8],
    ) -> Result<RsFecRecovery> {
        if self.media_order.len() == self.media_capacity
            && self.media_order.back().is_some_and(|newest| {
                usize::from(sequence_distance(*newest, sequence_number)) > self.media_capacity
            })
        {
            self.reset_recent_state();
        }
        let mut recovery = RsFecRecovery::default();
        // A duplicate does not retry a block or replace its retained source.
        if !self.media_packets.contains_key(&sequence_number) && raw_rtp.len() <= MAX_SHARD_SIZE - 2
        {
            let normalized = zero_mutable_extensions(raw_rtp, &self.mutable_extensions)
                .ok_or_else(|| anyhow::anyhow!("invalid RSFEC source RTP header"))?;
            let mut shard = [0u8; MAX_SHARD_SIZE];
            shard[..2].copy_from_slice(&(normalized.len() as u16).to_be_bytes());
            shard[2..2 + normalized.len()].copy_from_slice(&normalized);
            let shard = Arc::new(shard);
            self.media_packets
                .insert(sequence_number, Arc::clone(&shard));
            insert_sequence_ordered(&mut self.media_order, sequence_number);
            if self.media_order.len() > self.media_capacity
                && let Some(expired) = self.media_order.pop_front()
            {
                self.media_packets.remove(&expired);
            }
            // Stop after inserting into the first eligible block, but skip
            // blocks that already own this source index.
            let matching = self.block_order.iter().find_map(|block_id| {
                let block = self.blocks.get(block_id)?;
                let index = block
                    .header
                    .data_sequences
                    .iter()
                    .position(|&value| value == sequence_number)? as u8;
                (!block.sources.contains_key(&index)).then_some((*block_id, index))
            });
            if let Some((block_id, index)) = matching {
                self.blocks
                    .get_mut(&block_id)
                    .expect("located RSFEC block")
                    .sources
                    .insert(index, shard);
                recovery = self.attempt_block(block_id);
            }
        } else if raw_rtp.len() > MAX_SHARD_SIZE - 2 {
            tracing::debug!(bytes = raw_rtp.len(), "oversized RSFEC source ignored");
        }
        self.expire_stale_blocks(sequence_number);
        Ok(recovery)
    }

    pub(crate) fn receive_repair(&mut self, payload: &[u8]) -> Result<RsFecRecovery> {
        let header = RsFecHeader::parse(payload)?;
        ensure!(
            header.protected_ssrc == self.protected_ssrc,
            "RSFEC protected SSRC mismatch"
        );
        ensure!(header.k <= self.max_k, "RSFEC k exceeds negotiated max-k");
        if self.completed_blocks.contains(&header.block_id) {
            return Ok(RsFecRecovery::default());
        }
        if !self.blocks.contains_key(&header.block_id) {
            let sources = header
                .data_sequences
                .iter()
                .enumerate()
                .filter_map(|(index, seq)| {
                    Some((index as u8, Arc::clone(self.media_packets.get(seq)?)))
                })
                .collect();
            self.blocks.insert(
                header.block_id,
                RsFecBlock {
                    header: header.clone(),
                    sources,
                    repairs: BTreeMap::new(),
                },
            );
            insert_sequence_ordered(&mut self.block_order, header.block_id);
        }
        let block = self
            .blocks
            .get_mut(&header.block_id)
            .expect("inserted RSFEC block");
        // A later repair's mask is parsed/validated, but the original block
        // owns its source mapping. only k/m/length/base are compared here.
        ensure!(
            same_block(&block.header, &header),
            "inconsistent RSFEC block parameters"
        );
        let index = header.repair_index - header.k;
        if let std::collections::btree_map::Entry::Vacant(entry) = block.repairs.entry(index) {
            entry.insert(
                payload[header.header_size..header.header_size + header.shard_size].to_vec(),
            );
            self.repair_shards += 1;
        }
        let recovery = self.attempt_block(header.block_id);
        if let Some(latest) = self.block_order.back().and_then(|id| self.blocks.get(id)) {
            self.expire_stale_blocks(latest.header.base_sequence);
        }
        // One trim per incoming repair.
        if self.repair_shards > self.repair_capacity
            && let Some(oldest) = self.block_order.front().copied()
        {
            self.remove_block(oldest);
        }
        Ok(recovery)
    }

    fn attempt_block(&mut self, block_id: u16) -> RsFecRecovery {
        let Some(block) = self.blocks.get(&block_id) else {
            return RsFecRecovery::default();
        };
        let header = block.header.clone();
        if block.sources.len() == usize::from(header.k) {
            self.complete_block(block_id);
            return RsFecRecovery::default();
        }
        if block.sources.len() + block.repairs.len() < usize::from(header.k) {
            return RsFecRecovery::default();
        }
        let existing: HashSet<_> = block.sources.keys().copied().collect();
        let mut selected: Vec<_> = block
            .sources
            .iter()
            .map(|(index, source)| (*index, source[..header.shard_size].to_vec()))
            .chain(
                block
                    .repairs
                    .iter()
                    .map(|(index, repair)| (header.k + index, repair.clone())),
            )
            .take(usize::from(header.k))
            .collect();
        let mut recovery = RsFecRecovery::default();
        match cm256_decode_originals(header.k, header.m, header.shard_size, &mut selected) {
            Ok(originals) => {
                for (index, shard) in originals.into_iter().enumerate() {
                    if existing.contains(&(index as u8)) {
                        continue;
                    }
                    let size = usize::from(u16::from_be_bytes([shard[0], shard[1]]));
                    if !(12..=header.shard_size - 2).contains(&size) {
                        tracing::debug!(
                            block_id,
                            index,
                            size,
                            "corrupt RSFEC recovered length; skip this item"
                        );
                        continue;
                    }
                    // The Call reparses/routes by the recovered RTP SSRC.
                    // Mask-sequence equality is deliberately not rejected.
                    recovery.recovered_packets.push(shard[2..2 + size].to_vec());
                }
            }
            Err(error) => {
                tracing::debug!(%error,block_id, "CM256 decode failed; completing RSFEC block")
            }
        }
        self.complete_block(block_id);
        recovery
    }

    fn expire_stale_blocks(&mut self, sequence_number: u16) {
        while let Some(oldest) = self.block_order.front().copied() {
            let Some(block) = self.blocks.get(&oldest) else {
                break;
            };
            if sequence_distance(block.header.base_sequence, sequence_number) < 0x4000 {
                break;
            }
            self.remove_block(oldest);
        }
    }

    fn complete_block(&mut self, block_id: u16) {
        self.remove_block(block_id);
        if self.completed_blocks.insert(block_id) {
            insert_sequence_ordered(&mut self.completed_order, block_id);
        }
        if self.completed_order.len() > MAX_COMPLETED_BLOCKS
            && let Some(expired) = self.completed_order.pop_front()
        {
            self.completed_blocks.remove(&expired);
        }
    }

    fn remove_block(&mut self, block_id: u16) {
        if let Some(block) = self.blocks.remove(&block_id) {
            self.repair_shards -= block.repairs.len();
        }
        self.block_order.retain(|value| *value != block_id);
    }

    fn reset_recent_state(&mut self) {
        self.media_packets.clear();
        self.media_order.clear();
        self.blocks.clear();
        self.block_order.clear();
        self.repair_shards = 0;
        self.completed_blocks.clear();
        self.completed_order.clear();
    }
}

fn insert_sequence_ordered(order: &mut VecDeque<u16>, sequence: u16) {
    let position = order
        .iter()
        .position(|&value| (sequence.wrapping_sub(value) as i16) < 0)
        .unwrap_or(order.len());
    order.insert(position, sequence);
}
fn sequence_distance(left: u16, right: u16) -> u16 {
    left.wrapping_sub(right).min(right.wrapping_sub(left))
}

pub(crate) fn zero_mutable_extensions(
    raw_rtp: &[u8],
    mutable_extensions: &HashMap<u8, usize>,
) -> Option<Vec<u8>> {
    if raw_rtp.len() < 12 || raw_rtp[0] >> 6 != 2 {
        return None;
    }
    let extension_offset = 12_usize.checked_add(usize::from(raw_rtp[0] & 0x0f) * 4)?;
    if raw_rtp.len() < extension_offset || raw_rtp[0] & 0x10 == 0 {
        return Some(raw_rtp.to_vec());
    }
    if raw_rtp.len() < extension_offset + 4 {
        return None;
    }
    let profile = u16::from_be_bytes([raw_rtp[extension_offset], raw_rtp[extension_offset + 1]]);
    let extension_bytes = usize::from(u16::from_be_bytes([
        raw_rtp[extension_offset + 2],
        raw_rtp[extension_offset + 3],
    ]))
    .checked_mul(4)?;
    let data_start = extension_offset + 4;
    let data_end = data_start.checked_add(extension_bytes)?;
    if raw_rtp.len() < data_end {
        return None;
    }
    let mut normalized = raw_rtp.to_vec();
    let mut cursor = data_start;
    if profile == 0xbede {
        while cursor < data_end {
            let header = normalized[cursor];
            cursor += 1;
            let id = header >> 4;
            if id == 0 {
                continue;
            }
            if id == 15 {
                break;
            }
            let length = usize::from(header & 0x0f) + 1;
            if cursor + length > data_end {
                return None;
            }
            if let Some(preserve) = mutable_extensions.get(&id).copied() {
                normalized[cursor + preserve.min(length)..cursor + length].fill(0);
            }
            cursor += length;
        }
    } else if profile & 0xfff0 == 0x1000 {
        while cursor < data_end {
            let id = normalized[cursor];
            cursor += 1;
            if id == 0 {
                continue;
            }
            if cursor >= data_end {
                return None;
            }
            let length = usize::from(normalized[cursor]);
            cursor += 1;
            if cursor + length > data_end {
                return None;
            }
            if let Some(preserve) = mutable_extensions.get(&id).copied() {
                normalized[cursor + preserve.min(length)..cursor + length].fill(0);
            }
            cursor += length;
        }
    }
    Some(normalized)
}

fn same_block(left: &RsFecHeader, right: &RsFecHeader) -> bool {
    left.block_id == right.block_id
        && left.k == right.k
        && left.m == right.m
        && left.protected_ssrc == right.protected_ssrc
        && left.shard_size == right.shard_size
        && left.base_sequence == right.base_sequence
}

pub(crate) fn cm256_decode_originals(
    k: u8,
    m: u8,
    shard_size: usize,
    selected: &mut [(u8, Vec<u8>)],
) -> Result<Vec<Vec<u8>>> {
    let count = usize::from(k);
    ensure!(
        count != 0 && selected.len() == count,
        "CM256 requires exactly k shards"
    );
    ensure!(u16::from(k) + u16::from(m) <= 256, "invalid CM256 k+m");
    ensure!(
        selected.iter().all(|(_, shard)| shard.len() >= shard_size),
        "short CM256 shard"
    );

    let mut augmented = vec![vec![0_u8; count * 2]; count];
    for (row, (index, _)) in selected.iter().enumerate() {
        ensure!(
            u16::from(*index) < u16::from(k) + u16::from(m),
            "CM256 shard index is outside k+m"
        );
        if *index < k {
            augmented[row][usize::from(*index)] = 1;
        } else {
            for (column, value) in augmented[row][..count].iter_mut().enumerate() {
                *value = matrix_element(*index, k, column as u8);
            }
        }
        augmented[row][count + row] = 1;
    }
    invert_matrix(&mut augmented, count)?;

    let mut originals = vec![vec![0_u8; shard_size]; count];
    for original in 0..count {
        for (received, (_, shard)) in selected.iter().enumerate() {
            multiply_add(
                &mut originals[original],
                &shard[..shard_size],
                augmented[original][count + received],
            );
        }
    }
    Ok(originals)
}

pub(crate) fn cm256_encode_repairs(
    originals: &[Vec<u8>],
    repair_count: u8,
    shard_size: usize,
) -> Result<Vec<Vec<u8>>> {
    let original_count = u8::try_from(originals.len())
        .map_err(|_| anyhow::anyhow!("CM256 original count exceeds 255"))?;
    ensure!(
        original_count != 0,
        "CM256 requires at least one original shard"
    );
    ensure!(
        repair_count != 0,
        "CM256 requires at least one repair shard"
    );
    ensure!(
        u16::from(original_count) + u16::from(repair_count) <= 256,
        "invalid CM256 original+repair count"
    );
    ensure!(
        originals.iter().all(|shard| shard.len() >= shard_size),
        "short CM256 original shard"
    );

    let mut repairs = Vec::with_capacity(usize::from(repair_count));
    for repair in 0..repair_count {
        let row = original_count.wrapping_add(repair);
        let mut output = vec![0_u8; shard_size];
        for (column, shard) in originals.iter().enumerate() {
            multiply_add(
                &mut output,
                &shard[..shard_size],
                matrix_element(row, original_count, column as u8),
            );
        }
        repairs.push(output);
    }
    Ok(repairs)
}

fn invert_matrix(matrix: &mut [Vec<u8>], count: usize) -> Result<()> {
    for column in 0..count {
        let pivot = (column..count)
            .find(|row| matrix[*row][column] != 0)
            .ok_or_else(|| anyhow::anyhow!("singular CM256 matrix"))?;
        matrix.swap(column, pivot);
        let inverse = gf_inverse(matrix[column][column]);
        for value in &mut matrix[column] {
            *value = gf_multiply(*value, inverse);
        }
        for row in 0..count {
            if row == column {
                continue;
            }
            let coefficient = matrix[row][column];
            if coefficient == 0 {
                continue;
            }
            let pivot_row = matrix[column].clone();
            for (target, source) in matrix[row].iter_mut().zip(pivot_row) {
                *target ^= gf_multiply(source, coefficient);
            }
        }
    }
    Ok(())
}

fn matrix_element(row: u8, first_repair_row: u8, column: u8) -> u8 {
    gf_divide(first_repair_row ^ column, row ^ column)
}

fn multiply_add(output: &mut [u8], input: &[u8], coefficient: u8) {
    if coefficient == 0 {
        return;
    }
    if coefficient == 1 {
        for (output, input) in output.iter_mut().zip(input) {
            *output ^= *input;
        }
        return;
    }
    let table = multiplication_table();
    let row = usize::from(coefficient) * 256;
    for (output, input) in output.iter_mut().zip(input) {
        *output ^= table[row + usize::from(*input)];
    }
}

fn gf_multiply(left: u8, right: u8) -> u8 {
    multiplication_table()[usize::from(right) * 256 + usize::from(left)]
}

fn gf_divide(numerator: u8, denominator: u8) -> u8 {
    if numerator == 0 {
        return 0;
    }
    assert_ne!(denominator, 0, "CM256 Cauchy denominator must be non-zero");
    let (logarithm, exponent) = gf_tables();
    let power = (u16::from(logarithm[usize::from(numerator)]) + 255
        - u16::from(logarithm[usize::from(denominator)]))
        % 255;
    exponent[usize::from(power)]
}

fn gf_inverse(value: u8) -> u8 {
    assert_ne!(value, 0, "zero has no GF(256) inverse");
    gf_divide(1, value)
}

fn multiplication_table() -> &'static [u8; 65_536] {
    static TABLE: OnceLock<Box<[u8; 65_536]>> = OnceLock::new();
    TABLE.get_or_init(|| {
        let (logarithm, exponent) = gf_tables();
        let mut table = Box::new([0_u8; 65_536]);
        for right in 1..256 {
            for left in 1..256 {
                let power = usize::from(logarithm[left]) + usize::from(logarithm[right]);
                table[right * 256 + left] = exponent[power % 255];
            }
        }
        table
    })
}

fn gf_tables() -> &'static ([u8; 256], [u8; 255]) {
    static TABLES: OnceLock<([u8; 256], [u8; 255])> = OnceLock::new();
    TABLES.get_or_init(|| {
        let mut logarithm = [0_u8; 256];
        let mut exponent = [0_u8; 255];
        let mut value = 1_u16;
        for power in 0..255_u16 {
            exponent[usize::from(power)] = value as u8;
            logarithm[usize::from(value as u8)] = power as u8;
            value <<= 1;
            if value & 0x100 != 0 {
                value ^= 0x14d;
            }
        }
        (logarithm, exponent)
    })
}
