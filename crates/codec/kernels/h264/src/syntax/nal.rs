// SPDX-License-Identifier: MIT
// Derived from oxideav-h264 0.1.8, Copyright (c) 2026 Karpelès Lab Inc.
// Retained syntax/metadata primitives used by this client; see COPYING.OxideAV in this crate.

use std::borrow::Cow;

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum NalError {
    #[error("NAL unit is empty (no header byte)")]
    EmptyNal,
    #[error("forbidden_zero_bit is not 0 (got {0})")]
    ForbiddenZeroBitNotZero(u8),
    #[error("extended AVC NAL units are not supported")]
    UnsupportedExtension,
}

#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum NalUnitType {
    Unspecified(u8),
    SliceNonIdr,
    SliceDataPartitionA,
    SliceDataPartitionB,
    SliceDataPartitionC,
    SliceIdr,
    Sei,
    Sps,
    Pps,
    AccessUnitDelimiter,
    EndOfSequence,
    EndOfStream,
    FillerData,
    SpsExtension,
    PrefixNalUnit,
    SubsetSps,
    DepthParameterSet,
    Reserved(u8),
    SliceAuxiliary,
    SliceExtension,
    SliceExtensionDepth,
    UnspecifiedRange(u8),
}

impl NalUnitType {
    pub fn from_u8(v: u8) -> Self {
        match v & 0x1F {
            0 => NalUnitType::Unspecified(0),
            1 => NalUnitType::SliceNonIdr,
            2 => NalUnitType::SliceDataPartitionA,
            3 => NalUnitType::SliceDataPartitionB,
            4 => NalUnitType::SliceDataPartitionC,
            5 => NalUnitType::SliceIdr,
            6 => NalUnitType::Sei,
            7 => NalUnitType::Sps,
            8 => NalUnitType::Pps,
            9 => NalUnitType::AccessUnitDelimiter,
            10 => NalUnitType::EndOfSequence,
            11 => NalUnitType::EndOfStream,
            12 => NalUnitType::FillerData,
            13 => NalUnitType::SpsExtension,
            14 => NalUnitType::PrefixNalUnit,
            15 => NalUnitType::SubsetSps,
            16 => NalUnitType::DepthParameterSet,
            17 | 18 | 22 | 23 => NalUnitType::Reserved(v & 0x1F),
            19 => NalUnitType::SliceAuxiliary,
            20 => NalUnitType::SliceExtension,
            21 => NalUnitType::SliceExtensionDepth,
            24..=31 => NalUnitType::UnspecifiedRange(v & 0x1F),
            _ => unreachable!(),
        }
    }

    pub fn as_u8(self) -> u8 {
        match self {
            NalUnitType::Unspecified(v) => v,
            NalUnitType::SliceNonIdr => 1,
            NalUnitType::SliceDataPartitionA => 2,
            NalUnitType::SliceDataPartitionB => 3,
            NalUnitType::SliceDataPartitionC => 4,
            NalUnitType::SliceIdr => 5,
            NalUnitType::Sei => 6,
            NalUnitType::Sps => 7,
            NalUnitType::Pps => 8,
            NalUnitType::AccessUnitDelimiter => 9,
            NalUnitType::EndOfSequence => 10,
            NalUnitType::EndOfStream => 11,
            NalUnitType::FillerData => 12,
            NalUnitType::SpsExtension => 13,
            NalUnitType::PrefixNalUnit => 14,
            NalUnitType::SubsetSps => 15,
            NalUnitType::DepthParameterSet => 16,
            NalUnitType::Reserved(v) => v,
            NalUnitType::SliceAuxiliary => 19,
            NalUnitType::SliceExtension => 20,
            NalUnitType::SliceExtensionDepth => 21,
            NalUnitType::UnspecifiedRange(v) => v,
        }
    }

    pub fn is_vcl(self) -> bool {
        matches!(
            self,
            NalUnitType::SliceNonIdr
                | NalUnitType::SliceDataPartitionA
                | NalUnitType::SliceDataPartitionB
                | NalUnitType::SliceDataPartitionC
                | NalUnitType::SliceIdr
        )
    }

    pub fn is_idr(self) -> bool {
        matches!(self, NalUnitType::SliceIdr)
    }
}

#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub struct NalHeader {
    pub forbidden_zero_bit: u8,
    pub nal_ref_idc: u8,
    pub nal_unit_type: NalUnitType,
}

#[derive(Debug, Clone)]
pub struct NalUnit<'a> {
    pub header: NalHeader,
    pub rbsp: Cow<'a, [u8]>,
}

pub fn parse_nal_unit(payload: &[u8]) -> Result<NalUnit<'_>, NalError> {
    if payload.is_empty() {
        return Err(NalError::EmptyNal);
    }
    let header_byte = payload[0];
    let forbidden_zero_bit = (header_byte >> 7) & 1;
    if forbidden_zero_bit != 0 {
        return Err(NalError::ForbiddenZeroBitNotZero(forbidden_zero_bit));
    }
    let header = NalHeader {
        forbidden_zero_bit,
        nal_ref_idc: (header_byte >> 5) & 3,
        nal_unit_type: NalUnitType::from_u8(header_byte),
    };
    if matches!(
        header.nal_unit_type,
        NalUnitType::PrefixNalUnit | NalUnitType::SliceExtension | NalUnitType::SliceExtensionDepth
    ) {
        return Err(NalError::UnsupportedExtension);
    }
    let rbsp = rbsp_from_nal_payload(&payload[1..]);
    Ok(NalUnit { header, rbsp })
}

pub fn rbsp_from_nal_payload(nal_payload: &[u8]) -> Cow<'_, [u8]> {
    if !contains_emulation_byte(nal_payload) {
        return Cow::Borrowed(nal_payload);
    }
    let mut out = Vec::with_capacity(nal_payload.len());
    let mut i = 0;
    while i < nal_payload.len() {
        // §7.3.1: `i + 2 < N && next_bits(24) == 0x000003` ⇒ keep the
        // two leading zeros, skip the 0x03 emulation byte.
        if i + 2 < nal_payload.len()
            && nal_payload[i] == 0
            && nal_payload[i + 1] == 0
            && nal_payload[i + 2] == 0x03
        {
            out.push(0);
            out.push(0);
            i += 3;
        } else {
            out.push(nal_payload[i]);
            i += 1;
        }
    }
    Cow::Owned(out)
}

fn contains_emulation_byte(bytes: &[u8]) -> bool {
    let mut i = 0;
    while i + 2 < bytes.len() {
        if bytes[i] == 0 && bytes[i + 1] == 0 && bytes[i + 2] == 0x03 {
            return true;
        }
        i += 1;
    }
    false
}

pub struct AnnexBSplitter<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> AnnexBSplitter<'a> {
    pub fn new(data: &'a [u8]) -> Self {
        Self { data, pos: 0 }
    }
}

impl<'a> Iterator for AnnexBSplitter<'a> {
    type Item = &'a [u8];

    fn next(&mut self) -> Option<Self::Item> {
        // Find the next start-code prefix from `self.pos`.
        let start_offset = find_start_code_prefix(&self.data[self.pos..])?;
        let prefix_pos = self.pos + start_offset;
        let after_prefix = prefix_pos + 3;
        // Find the start of the next prefix to bound this NAL.
        let next_prefix = find_start_code_prefix(&self.data[after_prefix..])
            .map(|off| after_prefix + off)
            .unwrap_or(self.data.len());
        // §B.1 trailing_zero_8bits — strip trailing zeros so the NAL
        // ends at its real last byte. Also strip the optional zero byte
        // that turns a 3-byte prefix into the canonical 4-byte form
        // for the *next* NAL (we'll find that prefix again next time).
        let mut nal_end = next_prefix;
        while nal_end > after_prefix && self.data[nal_end - 1] == 0 {
            nal_end -= 1;
        }
        self.pos = next_prefix;
        Some(&self.data[after_prefix..nal_end])
    }
}

fn find_start_code_prefix(bytes: &[u8]) -> Option<usize> {
    if bytes.len() < 3 {
        return None;
    }
    let mut i = 0;
    while i + 2 < bytes.len() {
        if bytes[i] == 0 && bytes[i + 1] == 0 && bytes[i + 2] == 1 {
            return Some(i);
        }
        i += 1;
    }
    None
}
