// SPDX-License-Identifier: MIT
// Derived from oxideav-h265 0.0.10, Copyright (c) 2026 Karpelès Lab Inc.
// Retained syntax/metadata primitives used by this client; see COPYING.OxideAV in this crate.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NalError {
    NoStartCode,
    ForbiddenZeroBitSet,
    TemporalIdPlus1Zero,
    TruncatedHeader,
}

impl core::fmt::Display for NalError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::NoStartCode => f.write_str("no Annex B start code in input"),
            Self::ForbiddenZeroBitSet => f.write_str("forbidden_zero_bit was set in NAL header"),
            Self::TemporalIdPlus1Zero => f.write_str("nuh_temporal_id_plus1 was zero"),
            Self::TruncatedHeader => f.write_str("NAL unit shorter than two-byte header"),
        }
    }
}

impl std::error::Error for NalError {}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NalHeader {
    pub nal_unit_type: u8,
    pub nuh_layer_id: u8,
    pub temporal_id: u8,
}

impl NalHeader {
    pub fn parse(bytes: &[u8]) -> Result<Self, NalError> {
        if bytes.len() < 2 {
            return Err(NalError::TruncatedHeader);
        }
        let b0 = bytes[0];
        let b1 = bytes[1];

        if b0 & 0x80 != 0 {
            return Err(NalError::ForbiddenZeroBitSet);
        }

        let nal_unit_type = (b0 >> 1) & 0x3F;
        let nuh_layer_id = ((b0 & 0x01) << 5) | ((b1 >> 3) & 0x1F);
        let temporal_id_plus1 = b1 & 0x07;
        if temporal_id_plus1 == 0 {
            return Err(NalError::TemporalIdPlus1Zero);
        }
        Ok(Self {
            nal_unit_type,
            nuh_layer_id,
            temporal_id: temporal_id_plus1 - 1,
        })
    }

    pub fn is_vcl(&self) -> bool {
        self.nal_unit_type < 32
    }
}

pub fn strip_emulation_prevention(escaped: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(escaped.len());
    let mut zero_run: u8 = 0;
    for &b in escaped {
        if zero_run >= 2 && b == 0x03 {
            // §7.4.1.1: drop the 0x03 escape byte after `00 00`.
            zero_run = 0;
            continue;
        }
        out.push(b);
        if b == 0x00 {
            zero_run = zero_run.saturating_add(1);
        } else {
            zero_run = 0;
        }
    }
    out
}
