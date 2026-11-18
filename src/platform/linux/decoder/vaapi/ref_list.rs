// SPDX-License-Identifier: MIT
// Derived from oxideav-h264 0.1.8, Copyright (c) 2026 Karpelès Lab Inc.
// See crates/h264-core/COPYING.OxideAV.
//! §8.2.4.2 / §8.2.4.3 reference picture list construction for frames.
//!
//! DXVA's short slice format leaves list construction to the driver, so the
//! shared H.264 syntax crate does not carry it; VA-API wants the lists in
//! its slice parameters, so the Linux decoder keeps these here.
use openuuyc_h264::syntax::ref_list::{DpbEntry, PicStructure, RefMarking};

/// `modification_of_pic_nums_idc` operation from §7.3.3.1 / Table 7-7.
/// Mirrors the shape of `crate::slice_header::RefPicListModificationOp`
/// but re-declared here so this module has no upward dependency.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RplmOp {
    /// idc 0 — `abs_diff_pic_num_minus1`, subtract from picNumPred.
    Subtract(u32),
    /// idc 1 — `abs_diff_pic_num_minus1`, add to picNumPred.
    Add(u32),
    /// idc 2 — `long_term_pic_num` of the picture to splice in.
    LongTerm(u32),
}

// §8.2.4.2 — Initialisation of reference picture lists.
// ---------------------------------------------------------------------

/// §8.2.4.2.1 — Initialisation process for the reference picture list
/// for P and SP slices in frames.
///
/// Ordering:
///   1. Short-term frames/complementary field pairs, sorted by
///      descending `PicNum`.
///   2. Long-term frames/complementary field pairs, sorted by
///      ascending `LongTermPicNum`.
///
/// Returns a list of `dpb_key` values in the order they should occupy
/// RefPicList0.
pub fn init_ref_pic_list_p(
    dpb: &[DpbEntry],
    current_frame_num: u32,
    max_frame_num: u32,
    current_structure: PicStructure,
    current_bottom: bool,
) -> Vec<u32> {
    let current_is_field = current_structure.is_field();

    // 1. Short-term, descending PicNum.
    let mut short: Vec<(i32, u32)> = dpb
        .iter()
        .filter(|e| matches!(e.marking, RefMarking::ShortTerm))
        .map(|e| {
            (
                e.pic_num(
                    current_frame_num,
                    max_frame_num,
                    current_is_field,
                    current_bottom,
                ),
                e.dpb_key,
            )
        })
        .collect();
    short.sort_by_key(|e| std::cmp::Reverse(e.0));

    // 2. Long-term, ascending LongTermPicNum.
    let mut long: Vec<(i32, u32)> = dpb
        .iter()
        .filter(|e| e.is_long_term())
        .map(|e| {
            (
                e.long_term_pic_num(current_is_field, current_bottom),
                e.dpb_key,
            )
        })
        .collect();
    long.sort_by_key(|a| a.0);

    let mut out: Vec<u32> = Vec::with_capacity(short.len() + long.len());
    out.extend(short.into_iter().map(|(_, k)| k));
    out.extend(long.into_iter().map(|(_, k)| k));
    out
}

/// §8.2.4.2.3 — Initialisation process for reference picture lists for
/// B slices in frames.
///
/// RefPicList0 ordering:
///   1a. Short-term refs with `PicOrderCnt(entryShortTerm)` less than
///       `PicOrderCnt(CurrPic)`, sorted by descending PicOrderCnt.
///   1b. Then short-term refs with `PicOrderCnt >= PicOrderCnt(CurrPic)`,
///       sorted by ascending PicOrderCnt.
///   2.  Then long-term refs, sorted by ascending `LongTermPicNum`.
///
/// RefPicList1 ordering:
///   1a. Short-term refs with `PicOrderCnt(entryShortTerm)` greater than
///       `PicOrderCnt(CurrPic)`, sorted by ascending PicOrderCnt.
///   1b. Then short-term refs with `PicOrderCnt <= PicOrderCnt(CurrPic)`,
///       sorted by descending PicOrderCnt.
///   2.  Then long-term refs, sorted by ascending `LongTermPicNum`.
///
/// Plus the tie-breaker: if `RefPicList1 == RefPicList0` and the list
/// has more than one entry, swap positions [0] and [1] of List1.
pub fn init_ref_pic_lists_b(
    dpb: &[DpbEntry],
    current_poc: i32,
    current_structure: PicStructure,
    current_bottom: bool,
) -> (Vec<u32>, Vec<u32>) {
    let current_is_field = current_structure.is_field();

    // Partition short-term refs by POC vs current.
    let mut st_less: Vec<(i32, u32)> = Vec::new();
    let mut st_geq: Vec<(i32, u32)> = Vec::new();
    let mut st_greater: Vec<(i32, u32)> = Vec::new();
    let mut st_leq: Vec<(i32, u32)> = Vec::new();

    for e in dpb
        .iter()
        .filter(|e| matches!(e.marking, RefMarking::ShortTerm))
    {
        let poc = e.pic_order_cnt;
        if poc < current_poc {
            st_less.push((poc, e.dpb_key));
        } else {
            st_geq.push((poc, e.dpb_key));
        }
        if poc > current_poc {
            st_greater.push((poc, e.dpb_key));
        } else {
            st_leq.push((poc, e.dpb_key));
        }
    }

    // RefPicList0: st_less desc by POC, then st_geq asc by POC.
    st_less.sort_by_key(|e| std::cmp::Reverse(e.0));
    st_geq.sort_by_key(|a| a.0);

    // RefPicList1: st_greater asc, then st_leq desc.
    st_greater.sort_by_key(|a| a.0);
    st_leq.sort_by_key(|e| std::cmp::Reverse(e.0));

    // Long-term refs, ascending LongTermPicNum (shared by both lists).
    let mut long: Vec<(i32, u32)> = dpb
        .iter()
        .filter(|e| e.is_long_term())
        .map(|e| {
            (
                e.long_term_pic_num(current_is_field, current_bottom),
                e.dpb_key,
            )
        })
        .collect();
    long.sort_by_key(|a| a.0);

    let mut list0: Vec<u32> = Vec::new();
    list0.extend(st_less.iter().map(|(_, k)| *k));
    list0.extend(st_geq.iter().map(|(_, k)| *k));
    list0.extend(long.iter().map(|(_, k)| *k));

    let mut list1: Vec<u32> = Vec::new();
    list1.extend(st_greater.iter().map(|(_, k)| *k));
    list1.extend(st_leq.iter().map(|(_, k)| *k));
    list1.extend(long.iter().map(|(_, k)| *k));

    // §8.2.4.2.3 final rule — when RefPicList1 has more than one entry
    // and is identical to RefPicList0, swap [0] and [1] of List1.
    if list1.len() > 1 && list1 == list0 {
        list1.swap(0, 1);
    }

    (list0, list1)
}

// ---------------------------------------------------------------------
// §8.2.4.3 — Modification process for reference picture lists.
// ---------------------------------------------------------------------

/// §8.2.4.3 — apply RPLM ops to a reference picture list.
///
/// The initial list provided by the caller is first truncated to the
/// target `num_active` entries (per §8.2.4.2) if it has more, or padded
/// with a sentinel `u32::MAX` ("no reference picture") if it has fewer.
/// Each op then either splices a short-term or long-term ref into the
/// current `refIdxLX` position per §8.2.4.3.1 / §8.2.4.3.2.
///
/// Implementation notes:
///  - picNumLXPred is initialised to `CurrPicNum` and updated after
///    each short-term modification (§8.2.4.3.1).
///  - The temporary "length + 1" trick from the spec is reproduced
///    here: we push the new entry, shift tail entries, then truncate
///    back to `num_active`.
///  - `u32::MAX` is used as the sentinel for "no reference picture".
pub fn modify_ref_pic_list(
    list: &mut Vec<u32>,
    ops: &[RplmOp],
    dpb: &[DpbEntry],
    num_active: u32,
    current_frame_num: u32,
    max_frame_num: u32,
    current_is_field: bool,
    current_bottom: bool,
) {
    // Normalise to exactly `num_active` entries (§8.2.4.2 fall-through).
    let target = num_active as usize;
    if list.len() > target {
        list.truncate(target);
    }
    while list.len() < target {
        list.push(u32::MAX); // sentinel — "no reference picture".
    }

    if ops.is_empty() {
        return;
    }

    // §7.4.3 — CurrPicNum derivation.
    let curr_pic_num: i32 = if current_is_field {
        (2 * current_frame_num + 1) as i32
    } else {
        current_frame_num as i32
    };
    // §7.4.3 — MaxPicNum derivation.
    let max_pic_num: i32 = if current_is_field {
        (2 * max_frame_num) as i32
    } else {
        max_frame_num as i32
    };

    let mut ref_idx_lx: usize = 0;
    let mut pic_num_lx_pred: i32 = curr_pic_num;

    for op in ops {
        match *op {
            RplmOp::Subtract(abs_diff) | RplmOp::Add(abs_diff) => {
                // §8.2.4.3.1 — short-term modification.
                let delta = (abs_diff + 1) as i32;

                // eq. 8-34 / 8-35 — picNumLXNoWrap.
                let pic_num_lx_no_wrap: i32 = if matches!(op, RplmOp::Subtract(_)) {
                    if pic_num_lx_pred - delta < 0 {
                        pic_num_lx_pred - delta + max_pic_num
                    } else {
                        pic_num_lx_pred - delta
                    }
                } else {
                    // Add
                    if pic_num_lx_pred + delta >= max_pic_num {
                        pic_num_lx_pred + delta - max_pic_num
                    } else {
                        pic_num_lx_pred + delta
                    }
                };

                pic_num_lx_pred = pic_num_lx_no_wrap;

                // eq. 8-36 — picNumLX.
                let pic_num_lx: i32 = if pic_num_lx_no_wrap > curr_pic_num {
                    pic_num_lx_no_wrap - max_pic_num
                } else {
                    pic_num_lx_no_wrap
                };

                // Locate the matching short-term ref in the DPB.
                let target_key = dpb
                    .iter()
                    .find(|e| {
                        matches!(e.marking, RefMarking::ShortTerm)
                            && e.pic_num(
                                current_frame_num,
                                max_frame_num,
                                current_is_field,
                                current_bottom,
                            ) == pic_num_lx
                    })
                    .map(|e| e.dpb_key)
                    .unwrap_or(u32::MAX);

                splice_into_list(list, ref_idx_lx, num_active as usize, target_key, |k| {
                    // PicNumF filter — for this routine, the
                    // "short-term matching" path should skip the
                    // entry we just inserted. An entry that isn't
                    // marked short-term gets PicNumF = MaxPicNum
                    // (per §8.2.4.3.1), which won't equal any
                    // short-term picNumLX — so we only need to
                    // suppress re-matching the same dpb_key. The
                    // spec uses the "PicNumF(RefPicListX[cIdx]) !=
                    // picNumLX" test; we implement the equivalent
                    // "dbp_key != target" check because an entry
                    // at a new list index corresponds to exactly
                    // one DPB pic per our mapping.
                    k != target_key
                });
                ref_idx_lx += 1;
            }
            RplmOp::LongTerm(long_term_pic_num) => {
                // §8.2.4.3.2 — long-term modification.
                let target_key = dpb
                    .iter()
                    .find(|e| {
                        e.is_long_term()
                            && e.long_term_pic_num(current_is_field, current_bottom)
                                == long_term_pic_num as i32
                    })
                    .map(|e| e.dpb_key)
                    .unwrap_or(u32::MAX);

                splice_into_list(list, ref_idx_lx, num_active as usize, target_key, |k| {
                    k != target_key
                });
                ref_idx_lx += 1;
            }
        }
    }
}

/// Common helper for §8.2.4.3.1 eq. 8-37 and §8.2.4.3.2 eq. 8-38 —
/// shift entries right from `ref_idx_lx`, insert `target_key`, then
/// drop any existing occurrence of `target_key` further along in the
/// list (the `filter_retain` predicate picks which entries survive
/// the compact step).
///
/// The spec temporarily extends the list length by 1 during the
/// procedure and truncates back to `num_active` at the end; we emulate
/// that by working on a temporary vec.
fn splice_into_list<F>(
    list: &mut Vec<u32>,
    ref_idx_lx: usize,
    num_active: usize,
    target_key: u32,
    filter_retain: F,
) where
    F: Fn(u32) -> bool,
{
    if ref_idx_lx >= num_active {
        return;
    }
    // Temporary "one element longer" vec per the spec's pseudo-code.
    let mut tmp: Vec<u32> = Vec::with_capacity(num_active + 1);
    // Copy [0 .. ref_idx_lx] unchanged.
    tmp.extend_from_slice(&list[..ref_idx_lx]);
    // Insert the target.
    tmp.push(target_key);
    // Append the rest, filtering out the duplicate of target_key.
    for &k in &list[ref_idx_lx..] {
        if filter_retain(k) {
            tmp.push(k);
        }
    }
    // Re-pad / truncate to num_active.
    while tmp.len() < num_active {
        tmp.push(u32::MAX);
    }
    tmp.truncate(num_active);
    *list = tmp;
}
