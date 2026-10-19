// SPDX-License-Identifier: MIT
// Derived from oxideav-h265 0.0.10, Copyright (c) 2026 Karpelès Lab Inc.
// Retained syntax/metadata primitives used by this client; see COPYING.OxideAV in this crate.

use crate::bitreader::{BitReader, BitReaderError};

use crate::hrd::HrdError;

pub const HEVC_MAX_SUB_LAYERS: usize = 7;

pub const HEVC_VPS_MAX_NUM_LAYER_SETS: usize = 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VpsError {
    Truncated,
    ReservedFieldMismatch { got: u16 },
    ValueOutOfRange { field: &'static str, got: u32 },
    Bitstream(BitReaderError),
    Hrd(HrdError),
}

impl core::fmt::Display for VpsError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Truncated => f.write_str("VPS RBSP truncated"),
            Self::ReservedFieldMismatch { got } => write!(
                f,
                "vps_reserved_0xffff_16bits was 0x{got:04X}, expected 0xFFFF"
            ),
            Self::ValueOutOfRange { field, got } => {
                write!(f, "syntax element {field} out of range: {got}")
            }
            Self::Bitstream(e) => write!(f, "bitstream error during VPS parse: {e}"),
            Self::Hrd(e) => write!(f, "hrd_parameters() error inside VPS: {e}"),
        }
    }
}

impl std::error::Error for VpsError {}

impl From<BitReaderError> for VpsError {
    fn from(e: BitReaderError) -> Self {
        match e {
            BitReaderError::EndOfBuffer => Self::Truncated,
            other => Self::Bitstream(other),
        }
    }
}

impl From<HrdError> for VpsError {
    fn from(e: HrdError) -> Self {
        // Surface a bitstream-truncation reported through the HRD path
        // as the VPS-level Truncated variant so callers can keep their
        // single-pattern truncation handler. Any other HRD failure mode
        // is opaque to the VPS — preserve it verbatim.
        match e {
            HrdError::Truncated => Self::Truncated,
            other => Self::Hrd(other),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProfileTierLevel {
    pub general_profile_space: u8,
    pub general_tier_flag: bool,
    pub general_profile_idc: u8,
    pub general_level_idc: u8,
    pub sub_layer_profile_present: [bool; HEVC_MAX_SUB_LAYERS],
    pub sub_layer_level_present: [bool; HEVC_MAX_SUB_LAYERS],
    pub sub_layer_level_idc: [u8; HEVC_MAX_SUB_LAYERS],
}

impl ProfileTierLevel {
    pub fn parse(
        br: &mut BitReader<'_>,
        profile_present_flag: bool,
        max_num_sub_layers_minus1: u8,
    ) -> Result<Self, VpsError> {
        let mut ptl = Self {
            general_profile_space: 0,
            general_tier_flag: false,
            general_profile_idc: 0,
            general_level_idc: 0,
            sub_layer_profile_present: [false; HEVC_MAX_SUB_LAYERS],
            sub_layer_level_present: [false; HEVC_MAX_SUB_LAYERS],
            sub_layer_level_idc: [0; HEVC_MAX_SUB_LAYERS],
        };

        if profile_present_flag {
            ptl.general_profile_space = br.u(2)? as u8;
            ptl.general_tier_flag = br.u1()? != 0;
            ptl.general_profile_idc = br.u(5)? as u8;
            // 32 compatibility flags — skipped wholesale; the calling
            // application can re-parse them from the bit position if
            // needed later.
            br.skip(32)?;
            // progressive / interlaced / non_packed / frame_only
            br.skip(4)?;
            // The conditional block beneath these flags always consumes
            // exactly 43 bits regardless of profile_idc (per the
            // `/* not affected by this condition */` comment in §7.3.3
            // — the chroma-constraint, range-extension, and reserved
            // alternatives all sum to 43 bits).
            br.skip(43)?;
            // general_inbld_flag OR general_reserved_zero_bit — always 1 bit.
            br.skip(1)?;
        }
        // general_level_idc is always present (no `profilePresentFlag`
        // guard in the §7.3.3 syntax).
        ptl.general_level_idc = br.u(8)? as u8;

        // Per-sub-layer present-flag gates: 2 bits per sublayer up to
        // (but excluding) maxNumSubLayersMinus1.
        let max = max_num_sub_layers_minus1 as usize;
        for i in 0..max {
            let prof = br.u1()? != 0;
            let lvl = br.u1()? != 0;
            ptl.sub_layer_profile_present[i] = prof;
            ptl.sub_layer_level_present[i] = lvl;
        }

        // §7.3.3: if maxNumSubLayersMinus1 > 0, then for i in
        // max..8: reserved_zero_2bits — exactly 2 bits each — to keep
        // the per-sub-layer body byte-aligned regardless of how many
        // sublayers were actually signalled.
        if max_num_sub_layers_minus1 > 0 {
            for _ in max..8 {
                br.skip(2)?;
            }
        }

        // Per-sub-layer profile/level body for each i in 0..max.
        for i in 0..max {
            if ptl.sub_layer_profile_present[i] {
                // 2 + 1 + 5 + 32 + 4 + 43 + 1 = 88 bits, identical
                // layout to the general profile block above.
                br.skip(88)?;
            }
            if ptl.sub_layer_level_present[i] {
                ptl.sub_layer_level_idc[i] = br.u(8)? as u8;
            }
        }

        Ok(ptl)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct SubLayerOrderingInfo {
    pub max_dec_pic_buffering_minus1: u32,
    pub max_num_reorder_pics: u32,
    pub max_latency_increase_plus1: u32,
}
