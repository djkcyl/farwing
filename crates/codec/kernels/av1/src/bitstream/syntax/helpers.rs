// Copyright 2023 The ChromiumOS Authors
// Use of this source code is governed by a BSD-style license that can be
// found in ../../COPYING.ChromiumOS.

use super::parser::NUM_REF_FRAMES;

/// Implements FloorLog2(x), which is defined to be the floor of the base 2
/// logarithm of the input x.
///
/// The input x will always be an integer, and will always be greater than or equal to 1.
/// This function extracts the location of the most significant bit in x.
pub fn floor_log2(mut x: u32) -> u32 {
    assert!(x > 0);
    let mut s = 0;

    while x != 0 {
        x >>= 1;
        s += 1;
    }

    s - 1
}

/// Implements 5.9.3. Get relative distance function
pub fn get_relative_dist(enable_order_hint: bool, order_hint_bits: i32, a: i32, b: i32) -> i32 {
    if !enable_order_hint {
        0
    } else {
        let diff = a - b;
        let m = 1 << (order_hint_bits - 1);
        (diff & (m - 1)) - (diff & m)
    }
}

/// Implements find_latest_backward from section 7.8.
pub fn find_latest_backward(
    shifted_order_hints: &[i32; NUM_REF_FRAMES],
    used_frame: &[bool; NUM_REF_FRAMES],
    cur_frame_hint: i32,
    latest_order_hint: &mut i32,
) -> i32 {
    let mut _ref = -1;

    for i in 0..NUM_REF_FRAMES {
        let hint = shifted_order_hints[i];
        if !used_frame[i] && hint >= cur_frame_hint && (_ref < 0 || hint >= *latest_order_hint) {
            _ref = i as i32;
            *latest_order_hint = hint;
        }
    }

    _ref
}

/// Implements find_earliest_backward from section 7.8.
pub fn find_earliest_backward(
    shifted_order_hints: &[i32; NUM_REF_FRAMES],
    used_frame: &[bool; NUM_REF_FRAMES],
    cur_frame_hint: i32,
    earliest_order_hint: &mut i32,
) -> i32 {
    let mut _ref = -1;

    for i in 0..NUM_REF_FRAMES {
        let hint = shifted_order_hints[i];
        if !used_frame[i] && hint >= cur_frame_hint && (_ref < 0 || hint < *earliest_order_hint) {
            _ref = i as i32;
            *earliest_order_hint = hint;
        }
    }

    _ref
}

/// Implements find_latest_forward from section 7.8.
pub fn find_latest_forward(
    shifted_order_hints: &[i32; NUM_REF_FRAMES],
    used_frame: &[bool; NUM_REF_FRAMES],
    cur_frame_hint: i32,
    latest_order_hint: &mut i32,
) -> i32 {
    let mut _ref = -1;
    for i in 0..NUM_REF_FRAMES {
        let hint = shifted_order_hints[i];
        if !used_frame[i] && hint < cur_frame_hint && (_ref < 0 || hint >= *latest_order_hint) {
            _ref = i as i32;
            *latest_order_hint = hint;
        }
    }

    _ref
}

pub fn tile_log2(blk_size: u32, target: u32) -> u32 {
    let mut k = 0;

    while (blk_size << k) < target {
        k += 1;
    }

    k
}

pub fn clip3(x: i32, y: i32, z: i32) -> i32 {
    if z < x {
        x
    } else if z > y {
        y
    } else {
        z
    }
}

/// 5.9.29
pub fn inverse_recenter(r: i32, v: i32) -> i32 {
    if v > 2 * r {
        v
    } else if v & 1 != 0 {
        r - ((v + 1) >> 1)
    } else {
        r + (v >> 1)
    }
}
