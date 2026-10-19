// SPDX-License-Identifier: MIT
// Derived from oxideav-h265 0.0.10, Copyright (c) 2026 Karpelès Lab Inc.
// Retained syntax/metadata primitives used by this client; see COPYING.OxideAV in this crate.

use crate::sps::MaterializedShortTermRefPicSet;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RpsPocLists {
    pub st_curr_before: Vec<i32>,
    pub st_curr_after: Vec<i32>,
    pub st_foll: Vec<i32>,
    pub lt_curr: Vec<i32>,
    pub lt_foll: Vec<i32>,
    pub curr_delta_poc_msb_present: Vec<bool>,
    pub foll_delta_poc_msb_present: Vec<bool>,
}

#[derive(Debug, Clone, Copy)]
pub struct LongTermEntry {
    pub poc_lsb_lt: u32,
    pub used_by_curr_pic_lt: bool,
    pub delta_poc_msb_present: bool,
    pub delta_poc_msb_cycle_lt: u32,
}

#[must_use]
pub fn build_rps_poc_lists(
    is_idr: bool,
    poc: i32,
    max_poc_lsb: u32,
    st_rps: &MaterializedShortTermRefPicSet,
    lt_entries: &[LongTermEntry],
) -> RpsPocLists {
    let mut out = RpsPocLists::default();
    if is_idr {
        return out;
    }
    // Negative (S0) pics — equation 8-5 first loop.
    for (i, &delta) in st_rps.delta_poc_s0.iter().enumerate() {
        let p = poc + delta;
        if st_rps.used_by_curr_pic_s0[i] {
            out.st_curr_before.push(p);
        } else {
            out.st_foll.push(p);
        }
    }
    // Positive (S1) pics — equation 8-5 second loop.
    for (i, &delta) in st_rps.delta_poc_s1.iter().enumerate() {
        let p = poc + delta;
        if st_rps.used_by_curr_pic_s1[i] {
            out.st_curr_after.push(p);
        } else {
            out.st_foll.push(p);
        }
    }
    // Long-term — equation 8-5 third loop.
    for e in lt_entries {
        let mut poc_lt = e.poc_lsb_lt as i32;
        if e.delta_poc_msb_present {
            poc_lt += poc
                - (e.delta_poc_msb_cycle_lt as i32) * (max_poc_lsb as i32)
                - (poc & (max_poc_lsb as i32 - 1));
        }
        if e.used_by_curr_pic_lt {
            out.lt_curr.push(poc_lt);
            out.curr_delta_poc_msb_present.push(e.delta_poc_msb_present);
        } else {
            out.lt_foll.push(poc_lt);
            out.foll_delta_poc_msb_present.push(e.delta_poc_msb_present);
        }
    }
    out
}
