//! ULPFEC header reader carried in a single-block RTP/RED packet.

use crate::transport::xor_fec::{ParityPacket, XorFecReceiver};
use anyhow::{Result, ensure};

const ULPFEC_SHORT_MASK_SIZE: usize = 2;
const ULPFEC_LONG_MASK_SIZE: usize = 6;
const ULPFEC_HEADER_SIZE_SHORT: usize = 14;
const ULPFEC_HEADER_SIZE_LONG: usize = 18;
const MAX_RTP_PACKET_SIZE: usize = 1500;

#[derive(Debug)]
pub(crate) struct UlpfecReceiver {
    protected_ssrc: u32,
    receiver: XorFecReceiver,
}

impl UlpfecReceiver {
    pub(crate) fn new(
        protected_ssrc: u32,
        mutable_extensions: impl IntoIterator<Item = (u8, usize)>,
    ) -> Self {
        Self {
            protected_ssrc,
            receiver: XorFecReceiver::new(protected_ssrc, mutable_extensions),
        }
    }
    pub(crate) fn remember_media(&mut self, sequence: u16, raw_rtp: &[u8]) -> Result<Vec<Vec<u8>>> {
        self.receiver.remember_media(sequence, raw_rtp)
    }
    pub(crate) fn receive_repair(&mut self, sequence: u16, payload: &[u8]) -> Result<Vec<Vec<u8>>> {
        let ssrc = self.protected_ssrc;
        self.receiver
            .receive_repair(ssrc, sequence, || parse_ulpfec_packet(ssrc, payload))
    }
}

fn parse_ulpfec_packet(protected_ssrc: u32, payload: &[u8]) -> Result<ParityPacket> {
    ensure!(
        payload.len() >= ULPFEC_HEADER_SIZE_SHORT,
        "truncated ULPFEC packet"
    );
    let long_mask = payload[0] & 0x40 != 0;
    let mask_size = if long_mask {
        ULPFEC_LONG_MASK_SIZE
    } else {
        ULPFEC_SHORT_MASK_SIZE
    };
    let header_size = if long_mask {
        ULPFEC_HEADER_SIZE_LONG
    } else {
        ULPFEC_HEADER_SIZE_SHORT
    };
    ensure!(payload.len() >= header_size, "truncated ULPFEC packet mask");
    let sequence_base = u16::from_be_bytes([payload[2], payload[3]]);
    let protection_length = usize::from(u16::from_be_bytes([payload[10], payload[11]]));
    ensure!(
        protection_length <= MAX_RTP_PACKET_SIZE - 12
            && payload.len() >= header_size + protection_length,
        "ULPFEC protection length is invalid"
    );
    let protected_sequences = (0..mask_size * 8)
        .filter(|bit| payload[12 + bit / 8] & (0x80 >> (bit % 8)) != 0)
        .map(|bit| sequence_base.wrapping_add(bit as u16))
        .collect::<Vec<_>>();
    ensure!(
        !protected_sequences.is_empty(),
        "ULPFEC packet mask is empty"
    );
    Ok(ParityPacket {
        protected_ssrc,
        protected_sequences,
        header_size,
        protection_length,
        payload: {
            let mut normalized = payload.to_vec();
            // UlpfecHeaderReader replaces SN base by length recovery
            // before handing the common header to ForwardErrorCorrection.
            normalized[2..4].copy_from_slice(&payload[8..10]);
            normalized
        },
    })
}
