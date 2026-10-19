// SPDX-License-Identifier: MIT
// Derived from oxideav-h264 0.1.8, Copyright (c) 2026 Karpelès Lab Inc.
// Retained syntax/metadata primitives used by this client; see COPYING.OxideAV in this crate.

use thiserror::Error;

#[derive(Debug, Error, PartialEq, Eq)]
pub enum PocError {
    #[error("pic_order_cnt_type {0} is out of range (must be 0..=2)")]
    InvalidType(u32),
    #[error("log2_max_pic_order_cnt_lsb_minus4 {0} exceeds 12")]
    LsbWidthOutOfRange(u32),
    #[error("log2_max_frame_num_minus4 {0} exceeds 12")]
    FrameNumWidthOutOfRange(u32),
    #[error("POC derivation overflowed i32 representable range (clause §8.2.1)")]
    Overflow,
}

#[derive(Debug, Clone)]
pub struct PocSps {
    pub pic_order_cnt_type: u32,
    pub log2_max_frame_num_minus4: u32,
    pub log2_max_pic_order_cnt_lsb_minus4: u32,
    pub delta_pic_order_always_zero_flag: bool,
    pub offset_for_non_ref_pic: i32,
    pub offset_for_top_to_bottom_field: i32,
    pub num_ref_frames_in_pic_order_cnt_cycle: u32,
    pub offset_for_ref_frame: Vec<i32>,
    pub frame_mbs_only_flag: bool,
}

#[derive(Debug, Clone, Copy)]
pub struct PocSlice {
    pub is_reference: bool,
    pub is_idr: bool,
    pub frame_num: u32,
    pub field_pic_flag: bool,
    pub bottom_field_flag: bool,
    pub pic_order_cnt_lsb: u32,
    pub delta_pic_order_cnt_bottom: i32,
    pub delta_pic_order_cnt: [i32; 2],
    pub prev_had_mmco5: bool,
    pub prev_reference_top_foc_for_mmco5: i32,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PocState {
    pub prev_pic_order_cnt_msb: i32,
    pub prev_pic_order_cnt_lsb: u32,
    pub prev_frame_num: u32,
    pub prev_frame_num_offset: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PocResult {
    pub top_field_order_cnt: i32,
    pub bottom_field_order_cnt: i32,
    pub pic_order_cnt: i32,
}

pub fn derive_poc(
    sps: &PocSps,
    slice: &PocSlice,
    state: &mut PocState,
) -> Result<PocResult, PocError> {
    if sps.log2_max_frame_num_minus4 > 12 {
        return Err(PocError::FrameNumWidthOutOfRange(
            sps.log2_max_frame_num_minus4,
        ));
    }
    if sps.log2_max_pic_order_cnt_lsb_minus4 > 12 {
        return Err(PocError::LsbWidthOutOfRange(
            sps.log2_max_pic_order_cnt_lsb_minus4,
        ));
    }
    match sps.pic_order_cnt_type {
        0 => derive_type0(sps, slice, state),
        1 => derive_type1(sps, slice, state),
        2 => derive_type2(sps, slice, state),
        other => Err(PocError::InvalidType(other)),
    }
}

fn derive_type0(
    sps: &PocSps,
    slice: &PocSlice,
    state: &mut PocState,
) -> Result<PocResult, PocError> {
    // eq. 7-11: MaxPicOrderCntLsb = 2^(log2_max_pic_order_cnt_lsb_minus4 + 4).
    let max_pic_order_cnt_lsb: i32 = 1i32 << (sps.log2_max_pic_order_cnt_lsb_minus4 + 4);

    // §8.2.1.1 — derive prevPicOrderCntMsb and prevPicOrderCntLsb.
    //
    // * If the current picture is an IDR, both are 0.
    // * Else if the previous reference picture had MMCO-5:
    //     — if that previous reference picture was not a bottom field,
    //       prevPicOrderCntMsb = 0, prevPicOrderCntLsb = its top FOC.
    //     — otherwise both are 0.
    // * Otherwise carry prev* forward from `state`.
    let (prev_msb, prev_lsb) = if slice.is_idr {
        (0i32, 0u32)
    } else if slice.prev_had_mmco5 {
        // The caller tells us, via `prev_reference_top_foc_for_mmco5`,
        // what the previous (MMCO-5) reference picture's TopFieldOrderCnt
        // was, assuming it was not a bottom field. A bottom field path
        // collapses to (0, 0). We encode "was bottom field" as the
        // caller setting that parameter to 0 — both arms of the spec
        // arrive at the same numbers in that case, so no extra flag is
        // needed.
        let top_foc = slice.prev_reference_top_foc_for_mmco5;
        let lsb = top_foc.rem_euclid(max_pic_order_cnt_lsb) as u32;
        (0i32, lsb)
    } else {
        (state.prev_pic_order_cnt_msb, state.prev_pic_order_cnt_lsb)
    };

    // eq. 8-3 — PicOrderCntMsb wrap-around logic. Done in i64 so that
    // a long fuzz-driven sequence of wrap events can saturate the
    // accumulator past i32 without panicking in debug builds; the
    // i32::try_from on the way out structurally maps that to
    // PocError::Overflow per Annex C's i32-PicOrderCnt invariant.
    let max_pic_order_cnt_lsb_i64 = max_pic_order_cnt_lsb as i64;
    let cur_lsb_i64 = slice.pic_order_cnt_lsb as i64;
    let prev_lsb_i64 = prev_lsb as i64;
    let prev_msb_i64 = prev_msb as i64;
    let half = max_pic_order_cnt_lsb_i64 / 2;
    let pic_order_cnt_msb_i64 =
        if cur_lsb_i64 < prev_lsb_i64 && (prev_lsb_i64 - cur_lsb_i64) >= half {
            prev_msb_i64 + max_pic_order_cnt_lsb_i64
        } else if cur_lsb_i64 > prev_lsb_i64 && (cur_lsb_i64 - prev_lsb_i64) > half {
            prev_msb_i64 - max_pic_order_cnt_lsb_i64
        } else {
            prev_msb_i64
        };

    // eq. 8-4 — TopFieldOrderCnt (present unless current picture is a
    // bottom field).
    // eq. 8-5 — BottomFieldOrderCnt (present unless current picture is a
    // top field).
    //
    // "Current picture is a bottom field" ⇔ field_pic_flag && bottom_field_flag.
    // "Current picture is a top field"    ⇔ field_pic_flag && !bottom_field_flag.
    // A frame has both fields.
    let is_bottom_field = slice.field_pic_flag && slice.bottom_field_flag;
    let is_top_field = slice.field_pic_flag && !slice.bottom_field_flag;
    let delta_bot_i64 = slice.delta_pic_order_cnt_bottom as i64;

    let top_i64 = if !is_bottom_field {
        pic_order_cnt_msb_i64 + cur_lsb_i64 // eq. 8-4
    } else {
        0
    };
    let bottom_i64 = if !is_top_field {
        if !slice.field_pic_flag {
            top_i64 + delta_bot_i64 // eq. 8-5 (frame branch)
        } else {
            pic_order_cnt_msb_i64 + cur_lsb_i64 // eq. 8-5 (bottom field branch)
        }
    } else {
        0
    };

    let top = i32::try_from(top_i64).map_err(|_| PocError::Overflow)?;
    let bottom = i32::try_from(bottom_i64).map_err(|_| PocError::Overflow)?;
    let pic_order_cnt_msb = i32::try_from(pic_order_cnt_msb_i64).map_err(|_| PocError::Overflow)?;

    // eq. 8-1 — PicOrderCnt() projection onto current picture.
    let pic = if !slice.field_pic_flag {
        top.min(bottom)
    } else if is_bottom_field {
        bottom
    } else {
        top
    };

    // §8.2.1 — carry state forward only for reference pictures and only
    // when the current picture does *not* include MMCO-5 (MMCO-5 is
    // handled by the caller setting `prev_had_mmco5` on the *next*
    // slice). Non-reference pictures do not update the POC anchor.
    if slice.is_reference {
        state.prev_pic_order_cnt_msb = pic_order_cnt_msb;
        // eq. 8-3 consumes `pic_order_cnt_lsb` directly, so store it.
        state.prev_pic_order_cnt_lsb = slice.pic_order_cnt_lsb;
    }
    state.prev_frame_num = slice.frame_num;

    Ok(PocResult {
        top_field_order_cnt: top,
        bottom_field_order_cnt: bottom,
        pic_order_cnt: pic,
    })
}

fn derive_type1(
    sps: &PocSps,
    slice: &PocSlice,
    state: &mut PocState,
) -> Result<PocResult, PocError> {
    // eq. 7-10: MaxFrameNum = 2^(log2_max_frame_num_minus4 + 4).
    let max_frame_num: i64 = 1i64 << (sps.log2_max_frame_num_minus4 + 4);

    // §8.2.1.2 — prevFrameNumOffset.
    //
    // For non-IDR pictures:
    //   — if the previous picture had MMCO-5, prevFrameNumOffset = 0.
    //   — otherwise it equals FrameNumOffset of the previous picture.
    // (For IDRs, prevFrameNumOffset is irrelevant — FrameNumOffset = 0.)
    let prev_frame_num_offset: i64 = if slice.is_idr || slice.prev_had_mmco5 {
        0
    } else {
        state.prev_frame_num_offset
    };

    // §7.4.1.2.4 — when the previous picture carried MMCO-5, its
    // frame_num is inferred to 0 for the purposes of subsequent
    // PrevRefFrameNum comparisons. Without this override the
    // wrap-detection (prev_frame_num > slice.frame_num) would fire
    // against the pre-reset MMCO-5 frame_num and inflate
    // FrameNumOffset by MaxFrameNum.
    let prev_frame_num: i64 = if slice.prev_had_mmco5 {
        0
    } else {
        state.prev_frame_num as i64
    };

    // eq. 8-6 — FrameNumOffset.
    let frame_num_offset: i64 = if slice.is_idr {
        0
    } else if prev_frame_num > (slice.frame_num as i64) {
        prev_frame_num_offset + max_frame_num
    } else {
        prev_frame_num_offset
    };

    // eq. 8-7 — absFrameNum.
    let mut abs_frame_num: i64 = if sps.num_ref_frames_in_pic_order_cnt_cycle != 0 {
        frame_num_offset + slice.frame_num as i64
    } else {
        0
    };
    if !slice.is_reference && abs_frame_num > 0 {
        abs_frame_num -= 1;
    }

    // eq. 7-12 — ExpectedDeltaPerPicOrderCntCycle = sum(offset_for_ref_frame[]).
    // We recompute it here rather than caching it in `PocSps`; it is a
    // tiny vector and the crate's POC hot path is already dwarfed by
    // the rest of slice decoding.
    let expected_delta_per_cycle: i64 = sps
        .offset_for_ref_frame
        .iter()
        .take(sps.num_ref_frames_in_pic_order_cnt_cycle as usize)
        .map(|&v| v as i64)
        .sum();

    // eq. 8-8 — picOrderCntCycleCnt / frameNumInPicOrderCntCycle.
    // eq. 8-9 — expectedPicOrderCnt.
    let expected_pic_order_cnt: i64 = if abs_frame_num > 0 {
        let cycle_len = sps.num_ref_frames_in_pic_order_cnt_cycle as i64;
        // cycle_len == 0 ⇒ abs_frame_num == 0 by eq. 8-7, so this branch
        // is only reached when cycle_len > 0.
        let pic_order_cnt_cycle_cnt = (abs_frame_num - 1) / cycle_len;
        let frame_num_in_cycle = ((abs_frame_num - 1) % cycle_len) as usize;
        let mut accum: i64 = pic_order_cnt_cycle_cnt * expected_delta_per_cycle;
        for i in 0..=frame_num_in_cycle {
            accum += sps.offset_for_ref_frame[i] as i64;
        }
        accum
    } else {
        0
    };
    let expected_pic_order_cnt: i64 = if !slice.is_reference {
        expected_pic_order_cnt + sps.offset_for_non_ref_pic as i64
    } else {
        expected_pic_order_cnt
    };

    // eq. 8-10 — TopFieldOrderCnt / BottomFieldOrderCnt.
    //
    // Per §7.4.2.1.1 + §7.4.3, when `delta_pic_order_always_zero_flag`
    // is set, both `delta_pic_order_cnt[0]` and `delta_pic_order_cnt[1]`
    // are inferred to be 0. We honour that by zeroing the deltas here
    // so callers do not have to.
    let (d0, d1) = if sps.delta_pic_order_always_zero_flag {
        (0i64, 0i64)
    } else {
        (
            slice.delta_pic_order_cnt[0] as i64,
            slice.delta_pic_order_cnt[1] as i64,
        )
    };
    let offset_top_to_bottom = sps.offset_for_top_to_bottom_field as i64;

    let (top_i64, bot_i64) = if !slice.field_pic_flag {
        // Frame: both fields are derived.
        let top = expected_pic_order_cnt + d0;
        let bot = top + offset_top_to_bottom + d1;
        (top, bot)
    } else if !slice.bottom_field_flag {
        // Top field only.
        let top = expected_pic_order_cnt + d0;
        (top, 0)
    } else {
        // Bottom field only.
        let bot = expected_pic_order_cnt + offset_top_to_bottom + d0;
        (0, bot)
    };

    // Annex C bounds TopFieldOrderCnt / BottomFieldOrderCnt to i32.
    // A silent `as i32` truncation would let fuzz-driven offsets walk
    // past 2^31 and surface as a wrong but plausible POC; mapping the
    // overflow to a structured error keeps the contract typed.
    let top = i32::try_from(top_i64).map_err(|_| PocError::Overflow)?;
    let bottom = i32::try_from(bot_i64).map_err(|_| PocError::Overflow)?;

    let pic = if !slice.field_pic_flag {
        top.min(bottom)
    } else if slice.bottom_field_flag {
        bottom
    } else {
        top
    };

    // §8.2.1 state update: FrameNumOffset is derived from *every*
    // picture's frame_num regardless of reference status.
    state.prev_frame_num = slice.frame_num;
    state.prev_frame_num_offset = frame_num_offset;

    Ok(PocResult {
        top_field_order_cnt: top,
        bottom_field_order_cnt: bottom,
        pic_order_cnt: pic,
    })
}

fn derive_type2(
    sps: &PocSps,
    slice: &PocSlice,
    state: &mut PocState,
) -> Result<PocResult, PocError> {
    // eq. 7-10.
    let max_frame_num: i64 = 1i64 << (sps.log2_max_frame_num_minus4 + 4);

    // §8.2.1.3 — prevFrameNumOffset (same rule as type 1).
    let prev_frame_num_offset: i64 = if slice.is_idr || slice.prev_had_mmco5 {
        0
    } else {
        state.prev_frame_num_offset
    };

    // §7.4.1.2.4 — see derive_type1; the MMCO-5 picture's frame_num
    // is inferred to 0 before computing FrameNumOffset.
    let prev_frame_num: i64 = if slice.prev_had_mmco5 {
        0
    } else {
        state.prev_frame_num as i64
    };

    // eq. 8-11 — FrameNumOffset.
    let frame_num_offset: i64 = if slice.is_idr {
        0
    } else if prev_frame_num > (slice.frame_num as i64) {
        prev_frame_num_offset + max_frame_num
    } else {
        prev_frame_num_offset
    };

    // eq. 8-12 — tempPicOrderCnt.
    let temp_pic_order_cnt: i64 = if slice.is_idr {
        0
    } else if !slice.is_reference {
        2 * (frame_num_offset + slice.frame_num as i64) - 1
    } else {
        2 * (frame_num_offset + slice.frame_num as i64)
    };

    // eq. 8-13 — TopFieldOrderCnt / BottomFieldOrderCnt. Annex C bounds
    // these to i32; a silent `as i32` truncation would let a long
    // fuzz-driven frame_num-wrap chain push tempPicOrderCnt past
    // 2^31 and produce a wrong-but-plausible POC instead of a typed
    // error.
    let temp = i32::try_from(temp_pic_order_cnt).map_err(|_| PocError::Overflow)?;
    let (top, bottom) = if !slice.field_pic_flag {
        (temp, temp)
    } else if slice.bottom_field_flag {
        (0, temp)
    } else {
        (temp, 0)
    };

    let pic = if !slice.field_pic_flag {
        top.min(bottom)
    } else if slice.bottom_field_flag {
        bottom
    } else {
        top
    };

    state.prev_frame_num = slice.frame_num;
    state.prev_frame_num_offset = frame_num_offset;

    Ok(PocResult {
        top_field_order_cnt: top,
        bottom_field_order_cnt: bottom,
        pic_order_cnt: pic,
    })
}
