// SPDX-License-Identifier: MIT
// Derived from oxideav-h265 0.0.10, Copyright (c) 2026 Karpelès Lab Inc.
// Retained syntax/metadata primitives used by this client; see COPYING.OxideAV in this crate.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NalKind(u8);

impl NalKind {
    pub const TRAIL_N: u8 = 0;
    pub const TRAIL_R: u8 = 1;
    pub const TSA_N: u8 = 2;
    pub const TSA_R: u8 = 3;
    pub const STSA_N: u8 = 4;
    pub const STSA_R: u8 = 5;
    pub const RADL_N: u8 = 6;
    pub const RADL_R: u8 = 7;
    pub const RASL_N: u8 = 8;
    pub const RASL_R: u8 = 9;
    pub const BLA_W_LP: u8 = 16;
    pub const BLA_W_RADL: u8 = 17;
    pub const BLA_N_LP: u8 = 18;
    pub const IDR_W_RADL: u8 = 19;
    pub const IDR_N_LP: u8 = 20;
    pub const CRA_NUT: u8 = 21;
    pub const RSV_IRAP_VCL23: u8 = 23;

    #[inline]
    #[must_use]
    pub fn new(nal_unit_type: u8) -> Self {
        Self(nal_unit_type)
    }

    #[inline]
    #[must_use]
    pub fn value(self) -> u8 {
        self.0
    }

    #[inline]
    #[must_use]
    pub fn is_vcl(self) -> bool {
        (self.0 <= Self::RASL_R) || (Self::BLA_W_LP..=31).contains(&self.0)
    }

    #[inline]
    #[must_use]
    pub fn is_irap(self) -> bool {
        (Self::BLA_W_LP..=Self::RSV_IRAP_VCL23).contains(&self.0)
    }

    #[inline]
    #[must_use]
    pub fn is_idr(self) -> bool {
        self.0 == Self::IDR_W_RADL || self.0 == Self::IDR_N_LP
    }

    #[inline]
    #[must_use]
    pub fn is_bla(self) -> bool {
        (Self::BLA_W_LP..=Self::BLA_N_LP).contains(&self.0)
    }

    #[inline]
    #[must_use]
    pub fn is_cra(self) -> bool {
        self.0 == Self::CRA_NUT
    }

    #[inline]
    #[must_use]
    pub fn is_rasl(self) -> bool {
        self.0 == Self::RASL_N || self.0 == Self::RASL_R
    }

    #[inline]
    #[must_use]
    pub fn is_radl(self) -> bool {
        self.0 == Self::RADL_N || self.0 == Self::RADL_R
    }

    #[inline]
    #[must_use]
    pub fn is_slnr(self) -> bool {
        self.0 <= Self::RADL_R && self.0 % 2 == 0
            || self.0 == 10
            || self.0 == 12
            || self.0 == 14
            || self.0 == Self::RASL_N
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct PocState {
    prev_poc_lsb: u32,
    prev_poc_msb: i32,
    seen: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PicOrderCnt {
    pub msb: i32,
    pub lsb: u32,
    pub val: i32,
}

impl PocState {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    #[must_use]
    pub fn derive(
        &self,
        nal_kind: NalKind,
        no_rasl_output: bool,
        poc_lsb: u32,
        max_poc_lsb: u32,
    ) -> PicOrderCnt {
        let msb = if nal_kind.is_irap() && no_rasl_output {
            // §8.3.1 — IRAP with NoRaslOutputFlag == 1 resets the MSB.
            0
        } else if !self.seen {
            // No prevTid0Pic yet (and not an MSB-reset IRAP): prev values
            // are 0 per the IDR NOTE-1 boundary condition.
            poc_msb(poc_lsb, 0, 0, max_poc_lsb)
        } else {
            poc_msb(poc_lsb, self.prev_poc_lsb, self.prev_poc_msb, max_poc_lsb)
        };
        let val = msb + poc_lsb as i32;
        PicOrderCnt {
            msb,
            lsb: poc_lsb,
            val,
        }
    }

    pub fn update_prev_tid0(&mut self, poc: PicOrderCnt) {
        self.prev_poc_lsb = poc.lsb;
        self.prev_poc_msb = poc.msb;
        self.seen = true;
    }

    pub fn decode_picture_poc(
        &mut self,
        nal_kind: NalKind,
        temporal_id: u8,
        no_rasl_output: bool,
        poc_lsb: u32,
        max_poc_lsb: u32,
    ) -> PicOrderCnt {
        let poc = self.derive(nal_kind, no_rasl_output, poc_lsb, max_poc_lsb);
        if temporal_id == 0 && !(nal_kind.is_rasl() || nal_kind.is_radl() || nal_kind.is_slnr()) {
            self.update_prev_tid0(poc);
        }
        poc
    }
}

#[inline]
fn poc_msb(poc_lsb: u32, prev_poc_lsb: u32, prev_poc_msb: i32, max_poc_lsb: u32) -> i32 {
    let half = (max_poc_lsb / 2) as i32;
    let lsb = poc_lsb as i32;
    let prev_lsb = prev_poc_lsb as i32;
    let max = max_poc_lsb as i32;
    if lsb < prev_lsb && (prev_lsb - lsb) >= half {
        prev_poc_msb + max
    } else if lsb > prev_lsb && (lsb - prev_lsb) > half {
        prev_poc_msb - max
    } else {
        prev_poc_msb
    }
}
