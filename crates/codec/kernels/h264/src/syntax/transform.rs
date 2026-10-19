// SPDX-License-Identifier: MIT
// Derived from oxideav-h264 0.1.8, Copyright (c) 2026 Karpelès Lab Inc.
// Retained syntax/metadata primitives used by this client; see COPYING.OxideAV in this crate.

use thiserror::Error;

use crate::syntax::pps::Pps;

use crate::syntax::sps::{ScalingListEntry, Sps};

#[derive(Debug, Error, PartialEq, Eq)]
pub enum TransformError {
    #[error("QP out of range (got {0}, must be 0..=51+QpBdOffset for the bit depth)")]
    QpOutOfRange(i32),
    #[error("bit_depth out of range (got {0}, must be 8..=14)")]
    BitDepthOutOfRange(u32),
}

#[inline]
pub fn qp_bd_offset(bit_depth_minus8: u32) -> i32 {
    6 * bit_depth_minus8 as i32
}

const NORM_ADJUST_4X4_V: [[i32; 3]; 6] = [
    [10, 16, 13],
    [11, 18, 14],
    [13, 20, 16],
    [14, 23, 18],
    [16, 25, 20],
    [18, 29, 23],
];

#[inline]
fn norm_adjust_4x4(m: usize, i: usize, j: usize) -> i32 {
    let row = &NORM_ADJUST_4X4_V[m];
    match (i & 1, j & 1) {
        (0, 0) => row[0],
        (1, 1) => row[1],
        _ => row[2],
    }
}

#[inline]
fn level_scale_4x4(scaling_list: &[i32; 16], m: usize, i: usize, j: usize) -> i32 {
    scaling_list[i * 4 + j] * norm_adjust_4x4(m, i, j)
}

pub const FLAT_4X4_16: [i32; 16] = [16; 16];

pub const FLAT_8X8_16: [i32; 64] = [16; 64];

pub const DEFAULT_4X4_INTRA: [i32; 16] = [
    6, 13, 13, 20, 20, 20, 28, 28, 28, 28, 32, 32, 32, 37, 37, 42,
];

pub const DEFAULT_4X4_INTER: [i32; 16] = [
    10, 14, 14, 20, 20, 20, 24, 24, 24, 24, 27, 27, 27, 30, 30, 34,
];

pub const DEFAULT_8X8_INTRA: [i32; 64] = [
    6, 10, 10, 13, 11, 13, 16, 16, 16, 16, 18, 18, 18, 18, 18, 23, 23, 23, 23, 23, 23, 25, 25, 25,
    25, 25, 25, 25, 27, 27, 27, 27, 27, 27, 27, 27, 29, 29, 29, 29, 29, 29, 29, 31, 31, 31, 31, 31,
    31, 33, 33, 33, 33, 33, 36, 36, 36, 36, 38, 38, 38, 40, 40, 42,
];

pub const DEFAULT_8X8_INTER: [i32; 64] = [
    9, 13, 13, 15, 13, 15, 17, 17, 17, 17, 19, 19, 19, 19, 19, 21, 21, 21, 21, 21, 21, 22, 22, 22,
    22, 22, 22, 22, 24, 24, 24, 24, 24, 24, 24, 24, 25, 25, 25, 25, 25, 25, 25, 27, 27, 27, 27, 27,
    27, 28, 28, 28, 28, 28, 30, 30, 30, 30, 32, 32, 32, 33, 33, 35,
];

fn default_4x4_for(idx: usize) -> [i32; 16] {
    // idx 0..=2 are Intra (Y/Cb/Cr), idx 3..=5 are Inter.
    if idx < 3 {
        DEFAULT_4X4_INTRA
    } else {
        DEFAULT_4X4_INTER
    }
}

fn default_8x8_for(idx: usize) -> [i32; 64] {
    // Even `idx` (0, 2, 4) = Intra; odd (1, 3, 5) = Inter.
    if idx & 1 == 0 {
        DEFAULT_8X8_INTRA
    } else {
        DEFAULT_8X8_INTER
    }
}

fn chain_prev_4x4(idx: usize) -> Option<usize> {
    match idx {
        1 => Some(0),
        2 => Some(1),
        4 => Some(3),
        5 => Some(4),
        _ => None, // 0 and 3 are class heads
    }
}

fn chain_prev_8x8(idx: usize) -> Option<usize> {
    match idx {
        2 => Some(0), // i=8 → i=6
        3 => Some(1), // i=9 → i=7
        4 => Some(2), // i=10 → i=8
        5 => Some(3), // i=11 → i=9
        _ => None,    // 0 (i=6), 1 (i=7) are class heads
    }
}

fn derive_sps_4x4(idx: usize, sps: &Sps) -> [i32; 16] {
    if !sps.seq_scaling_matrix_present_flag {
        // §7.4.2.1.1 — inferred Flat_4x4_16.
        return FLAT_4X4_16;
    }
    let lists = match sps.seq_scaling_lists.as_ref() {
        Some(l) => l,
        None => return FLAT_4X4_16,
    };
    let entry = lists.entries.get(idx);
    match entry {
        Some(ScalingListEntry::Explicit(vals)) => vec_to_16(vals),
        Some(ScalingListEntry::UseDefault) => default_4x4_for(idx),
        Some(ScalingListEntry::NotPresent) => {
            // Fall-back rule A for the sequence-level list.
            match chain_prev_4x4(idx) {
                Some(prev) => derive_sps_4x4(prev, sps),
                None => default_4x4_for(idx),
            }
        }
        None => FLAT_4X4_16,
    }
}

fn derive_sps_8x8(idx: usize, sps: &Sps) -> [i32; 64] {
    if !sps.seq_scaling_matrix_present_flag {
        return FLAT_8X8_16;
    }
    let lists = match sps.seq_scaling_lists.as_ref() {
        Some(l) => l,
        None => return FLAT_8X8_16,
    };
    // The SPS entries vector layout: [0..6) = 4x4 lists; [6..n) = 8x8
    // lists where n ∈ {8, 12}. We want entries[6 + idx].
    let spec_idx = 6 + idx;
    let entry = lists.entries.get(spec_idx);
    match entry {
        Some(ScalingListEntry::Explicit(vals)) => vec_to_64(vals),
        Some(ScalingListEntry::UseDefault) => default_8x8_for(idx),
        Some(ScalingListEntry::NotPresent) => {
            // Fall-back rule A for the sequence-level list.
            match chain_prev_8x8(idx) {
                Some(prev) => derive_sps_8x8(prev, sps),
                None => default_8x8_for(idx),
            }
        }
        None => FLAT_8X8_16,
    }
}

pub fn select_scaling_list_4x4(list_idx: usize, sps: &Sps, pps: &Pps) -> [i32; 16] {
    if list_idx >= 6 {
        return FLAT_4X4_16;
    }
    // PPS override: when pic_scaling_matrix_present_flag == 1 the
    // picture-level list is derived from the PPS per-index entries
    // with the appropriate fall-back rule set.
    let ext = pps.extension.as_ref();
    let pic_present = ext.is_some_and(|e| e.pic_scaling_matrix_present_flag);
    // §8.5.9 — the derived list is in bitstream scan order; weightScale
    // is its §8.5.6 inverse scan (row-major). Flat lists are invariant.
    let scan_list = if !pic_present {
        // §7.4.2.2: pic_scaling_matrix_present_flag == 0 → picture-level
        // lists equal the sequence-level lists.
        derive_sps_4x4(list_idx, sps)
    } else {
        match ext.and_then(|e| e.pic_scaling_lists.as_ref()) {
            Some(l) => derive_pps_4x4(list_idx, sps, l),
            None => derive_sps_4x4(list_idx, sps),
        }
    };
    weight_scale_4x4_from_scan(&scan_list)
}

pub fn select_scaling_list_8x8(list_idx: usize, sps: &Sps, pps: &Pps) -> [i32; 64] {
    if list_idx >= 6 {
        return FLAT_8X8_16;
    }
    let ext = pps.extension.as_ref();
    let pic_present = ext.is_some_and(|e| e.pic_scaling_matrix_present_flag);
    // §8.5.9 — inverse-scan the derived scan-order list into the
    // row-major weightScale8x8 every consumer indexes.
    let scan_list = if !pic_present {
        derive_sps_8x8(list_idx, sps)
    } else {
        match ext.and_then(|e| e.pic_scaling_lists.as_ref()) {
            Some(l) => derive_pps_8x8(list_idx, sps, l),
            None => derive_sps_8x8(list_idx, sps),
        }
    };
    weight_scale_8x8_from_scan(&scan_list)
}

fn derive_pps_4x4(idx: usize, sps: &Sps, pic: &crate::syntax::pps::PicScalingLists) -> [i32; 16] {
    let entry = pic.entries.get(idx);
    match entry {
        Some(ScalingListEntry::Explicit(vals)) => vec_to_16(vals),
        Some(ScalingListEntry::UseDefault) => default_4x4_for(idx),
        Some(ScalingListEntry::NotPresent) => {
            // §7.4.2.2 semantics on pic_scaling_list_present_flag[i]:
            //   seq_scaling_matrix_present_flag == 0 → rule A
            //   otherwise                            → rule B
            if !sps.seq_scaling_matrix_present_flag {
                // Rule A
                match chain_prev_4x4(idx) {
                    Some(prev) => derive_pps_4x4(prev, sps, pic),
                    None => default_4x4_for(idx),
                }
            } else {
                // Rule B — for class heads (i=0, 3) use the
                // sequence-level list; for i=1/2/4/5 inherit from the
                // previous PPS index (same PPS context).
                match chain_prev_4x4(idx) {
                    Some(prev) => derive_pps_4x4(prev, sps, pic),
                    None => derive_sps_4x4(idx, sps),
                }
            }
        }
        None => derive_sps_4x4(idx, sps),
    }
}

fn derive_pps_8x8(idx: usize, sps: &Sps, pic: &crate::syntax::pps::PicScalingLists) -> [i32; 64] {
    let spec_idx = 6 + idx;
    let entry = pic.entries.get(spec_idx);
    match entry {
        Some(ScalingListEntry::Explicit(vals)) => vec_to_64(vals),
        Some(ScalingListEntry::UseDefault) => default_8x8_for(idx),
        Some(ScalingListEntry::NotPresent) => {
            if !sps.seq_scaling_matrix_present_flag {
                // Rule A
                match chain_prev_8x8(idx) {
                    Some(prev) => derive_pps_8x8(prev, sps, pic),
                    None => default_8x8_for(idx),
                }
            } else {
                // Rule B
                match chain_prev_8x8(idx) {
                    Some(prev) => derive_pps_8x8(prev, sps, pic),
                    None => derive_sps_8x8(idx, sps),
                }
            }
        }
        None => derive_sps_8x8(idx, sps),
    }
}

#[inline]
fn vec_to_16(v: &[i32]) -> [i32; 16] {
    let mut out = [0i32; 16];
    let n = v.len().min(16);
    out[..n].copy_from_slice(&v[..n]);
    if n < 16 {
        // Defensive: pad with the last value (shouldn't happen with a
        // valid parse, since scaling_list() always fills exactly n
        // entries). Fall back to flat.
        for slot in out.iter_mut().skip(n) {
            *slot = 16;
        }
    }
    out
}

#[inline]
fn vec_to_64(v: &[i32]) -> [i32; 64] {
    let mut out = [0i32; 64];
    let n = v.len().min(64);
    out[..n].copy_from_slice(&v[..n]);
    if n < 64 {
        for slot in out.iter_mut().skip(n) {
            *slot = 16;
        }
    }
    out
}

const ZIGZAG_4X4: [(usize, usize); 16] = [
    (0, 0),
    (0, 1),
    (1, 0),
    (2, 0),
    (1, 1),
    (0, 2),
    (0, 3),
    (1, 2),
    (2, 1),
    (3, 0),
    (3, 1),
    (2, 2),
    (1, 3),
    (2, 3),
    (3, 2),
    (3, 3),
];

pub fn weight_scale_4x4_from_scan(list: &[i32; 16]) -> [i32; 16] {
    inverse_scan_4x4_zigzag(list)
}

pub fn weight_scale_8x8_from_scan(list: &[i32; 64]) -> [i32; 64] {
    let mut out = [0i32; 64];
    for (k, &raster) in ZIGZAG_8X8_SCAN_TO_RASTER.iter().enumerate() {
        out[raster] = list[k];
    }
    out
}

const ZIGZAG_8X8_SCAN_TO_RASTER: [usize; 64] = [
    0, 1, 8, 16, 9, 2, 3, 10, 17, 24, 32, 25, 18, 11, 4, 5, 12, 19, 26, 33, 40, 48, 41, 34, 27, 20,
    13, 6, 7, 14, 21, 28, 35, 42, 49, 56, 57, 50, 43, 36, 29, 22, 15, 23, 30, 37, 44, 51, 58, 59,
    52, 45, 38, 31, 39, 46, 53, 60, 61, 54, 47, 55, 62, 63,
];

pub fn inverse_scan_4x4_zigzag(levels: &[i32; 16]) -> [i32; 16] {
    let mut out = [0i32; 16];
    for (k, &(i, j)) in ZIGZAG_4X4.iter().enumerate() {
        out[i * 4 + j] = levels[k];
    }
    out
}

#[inline]
fn check_qp(qp: i32, bit_depth: u32) -> Result<(), TransformError> {
    let bd_minus8 = bit_depth.saturating_sub(8) as i32;
    let qp_bd_offset = 6 * bd_minus8;
    if !(0..=51 + qp_bd_offset).contains(&qp) {
        Err(TransformError::QpOutOfRange(qp))
    } else {
        Ok(())
    }
}

#[inline]
fn check_bit_depth(bit_depth: u32) -> Result<(), TransformError> {
    if !(8..=14).contains(&bit_depth) {
        Err(TransformError::BitDepthOutOfRange(bit_depth))
    } else {
        Ok(())
    }
}

pub fn inverse_hadamard_luma_dc_16x16(
    dc_coeffs: &[i32; 16],
    qp: i32,
    scaling_list: &[i32; 16],
    bit_depth: u32,
) -> Result<[i32; 16], TransformError> {
    check_qp(qp, bit_depth)?;
    check_bit_depth(bit_depth)?;

    // §8.5.10 Eq. 8-320 — f = H * c * H, with
    //     H = [[1, 1, 1, 1],
    //          [1, 1,-1,-1],
    //          [1,-1,-1, 1],
    //          [1,-1, 1,-1]].
    // Being a Hadamard this is self-inverse up to a factor of 4; the
    // factor is absorbed into the scaling step below.
    let c = dc_coeffs;
    let mut t = [0i32; 16]; // t = H * c
    for j in 0..4 {
        // Row 0 of H: [ 1,  1,  1,  1]
        t[j] = c[j] + c[4 + j] + c[8 + j] + c[12 + j];
        // Row 1: [ 1,  1, -1, -1]
        t[4 + j] = c[j] + c[4 + j] - c[8 + j] - c[12 + j];
        // Row 2: [ 1, -1, -1,  1]
        t[8 + j] = c[j] - c[4 + j] - c[8 + j] + c[12 + j];
        // Row 3: [ 1, -1,  1, -1]
        t[12 + j] = c[j] - c[4 + j] + c[8 + j] - c[12 + j];
    }
    let mut f = [0i32; 16]; // f = t * H
    for i in 0..4 {
        let base = i * 4;
        let t0 = t[base];
        let t1 = t[base + 1];
        let t2 = t[base + 2];
        let t3 = t[base + 3];
        // Right-multiplying by H above yields:
        //   f[i][0] = t[i][0]+t[i][1]+t[i][2]+t[i][3]
        //   f[i][1] = t[i][0]+t[i][1]-t[i][2]-t[i][3]
        //   f[i][2] = t[i][0]-t[i][1]-t[i][2]+t[i][3]
        //   f[i][3] = t[i][0]-t[i][1]+t[i][2]-t[i][3]
        f[base] = t0 + t1 + t2 + t3;
        f[base + 1] = t0 + t1 - t2 - t3;
        f[base + 2] = t0 - t1 - t2 + t3;
        f[base + 3] = t0 - t1 + t2 - t3;
    }

    // §8.5.10 — Scale per Eq. 8-321 / 8-322 using LevelScale4x4(qP%6, 0, 0).
    let qp_mod = (qp % 6) as usize;
    let qp_div = qp / 6;
    let ls = level_scale_4x4(scaling_list, qp_mod, 0, 0);

    let mut dc_y = [0i32; 16];
    if qp >= 36 {
        // Eq. 8-321.
        let shift = qp_div - 6;
        for k in 0..16 {
            dc_y[k] = (f[k] * ls) << shift;
        }
    } else {
        // Eq. 8-322.
        let shift = 6 - qp_div;
        let round = 1 << (5 - qp_div);
        for k in 0..16 {
            dc_y[k] = (f[k] * ls + round) >> shift;
        }
    }
    Ok(dc_y)
}
