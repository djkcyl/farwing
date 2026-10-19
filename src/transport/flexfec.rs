//! FlexFEC-03 header reader; recovery ownership is shared with ULPFEC.

use crate::transport::xor_fec::{ParityPacket, XorFecReceiver};
use anyhow::{Result, ensure};

const FLEXFEC_BASE_HEADER_SIZE: usize = 12;
const FLEXFEC_STREAM_HEADER_SIZE: usize = 6;
const FLEXFEC_PACKET_MASK_OFFSET: usize = FLEXFEC_BASE_HEADER_SIZE + FLEXFEC_STREAM_HEADER_SIZE;
const FLEXFEC_PACKET_MASK_SIZES: [usize; 3] = [2, 6, 14];
const FLEXFEC_HEADER_SIZES: [usize; 3] = [20, 24, 32];
const MAX_RTP_PACKET_SIZE: usize = 1500;

#[derive(Debug)]
pub(crate) struct FlexFecReceiver {
    receiver: XorFecReceiver,
}

impl FlexFecReceiver {
    pub(crate) fn new(
        protected_ssrc: u32,
        mutable_extensions: impl IntoIterator<Item = (u8, usize)>,
    ) -> Self {
        Self {
            receiver: XorFecReceiver::new(protected_ssrc, mutable_extensions),
        }
    }
    pub(crate) fn remember_media(&mut self, sequence: u16, raw_rtp: &[u8]) -> Result<Vec<Vec<u8>>> {
        self.receiver.remember_media(sequence, raw_rtp)
    }
    pub(crate) fn receive_repair(
        &mut self,
        fec_ssrc: u32,
        sequence: u16,
        payload: &[u8],
    ) -> Result<Vec<Vec<u8>>> {
        self.receiver
            .receive_repair(fec_ssrc, sequence, || parse_flexfec_packet(payload))
    }
}

fn parse_flexfec_packet(payload: &[u8]) -> Result<ParityPacket> {
    ensure!(
        payload.len() >= FLEXFEC_HEADER_SIZES[0],
        "truncated FlexFEC packet"
    );
    ensure!(payload[0] & 0x80 == 0, "FlexFEC retransmission bit is set");
    ensure!(
        payload[0] & 0x40 == 0,
        "FlexFEC flexible generator matrix is not supported"
    );
    ensure!(payload[8] == 1, "FlexFEC protects multiple media SSRCs");
    let protected_ssrc = u32::from_be_bytes(payload[12..16].try_into().expect("fixed field"));
    let sequence_base = u16::from_be_bytes(payload[16..18].try_into().expect("fixed field"));

    let mut mask = [0_u8; FLEXFEC_PACKET_MASK_SIZES[2]];
    let available_mask_bytes = payload
        .len()
        .min(FLEXFEC_PACKET_MASK_OFFSET + FLEXFEC_PACKET_MASK_SIZES[2])
        .saturating_sub(FLEXFEC_PACKET_MASK_OFFSET);
    mask[..available_mask_bytes].copy_from_slice(
        &payload[FLEXFEC_PACKET_MASK_OFFSET..FLEXFEC_PACKET_MASK_OFFSET + available_mask_bytes],
    );
    let (mask_size, header_size) = normalize_packet_mask(&mut mask, payload.len())?;
    let protected_sequences = (0..mask_size * 8)
        .filter(|bit| mask[bit / 8] & (0x80 >> (bit % 8)) != 0)
        .map(|bit| sequence_base.wrapping_add(bit as u16))
        .collect::<Vec<_>>();
    ensure!(
        !protected_sequences.is_empty(),
        "FlexFEC packet mask is empty"
    );
    let protection_length = payload.len().saturating_sub(header_size);
    ensure!(
        protection_length <= MAX_RTP_PACKET_SIZE - 12,
        "FlexFEC protection payload exceeds the RTP packet limit"
    );
    Ok(ParityPacket {
        protected_sequences,
        protected_ssrc,
        header_size,
        protection_length,
        payload: payload.to_vec(),
    })
}

fn normalize_packet_mask(mask: &mut [u8; 14], payload_len: usize) -> Result<(usize, usize)> {
    let k_bit0 = mask[0] & 0x80 != 0;
    let part0 = u16::from_be_bytes([mask[0], mask[1]]) << 1;
    mask[..2].copy_from_slice(&part0.to_be_bytes());
    if k_bit0 {
        return Ok((FLEXFEC_PACKET_MASK_SIZES[0], FLEXFEC_HEADER_SIZES[0]));
    }
    ensure!(
        payload_len >= FLEXFEC_HEADER_SIZES[1],
        "truncated FlexFEC medium packet mask"
    );
    let k_bit1 = mask[2] & 0x80 != 0;
    let bit15 = (mask[2] >> 6) & 1;
    mask[1] |= bit15;
    let part1 = u32::from_be_bytes([mask[2], mask[3], mask[4], mask[5]]) << 2;
    mask[2..6].copy_from_slice(&part1.to_be_bytes());
    if k_bit1 {
        return Ok((FLEXFEC_PACKET_MASK_SIZES[1], FLEXFEC_HEADER_SIZES[1]));
    }
    ensure!(
        payload_len >= FLEXFEC_HEADER_SIZES[2],
        "truncated FlexFEC long packet mask"
    );
    ensure!(mask[6] & 0x80 != 0, "malformed FlexFEC long packet mask");
    let tail_bits = (mask[6] >> 5) & 0x03;
    mask[5] |= tail_bits;
    let part2 = u64::from_be_bytes(mask[6..14].try_into().expect("fixed field")) << 3;
    mask[6..14].copy_from_slice(&part2.to_be_bytes());
    Ok((FLEXFEC_PACKET_MASK_SIZES[2], FLEXFEC_HEADER_SIZES[2]))
}
