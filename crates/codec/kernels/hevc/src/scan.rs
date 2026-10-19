// SPDX-License-Identifier: MIT
// Derived from oxideav-h265 0.0.10, Copyright (c) 2026 Karpelès Lab Inc.
// Retained syntax/metadata primitives used by this client; see COPYING.OxideAV in this crate.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ScanPos {
    pub x: u8,
    pub y: u8,
}

pub fn up_right_diagonal(blk_size: usize) -> Vec<ScanPos> {
    let n = blk_size * blk_size;
    let mut diag = Vec::with_capacity(n);

    // The 6-11 pseudocode uses a signed `y` that the inner loop drives
    // below 0 as its termination test, so the coordinates are tracked
    // as `i32` and only the in-bounds cells are recorded.
    let blk = blk_size as i32;
    let mut x: i32 = 0;
    let mut y: i32 = 0;
    let mut stop = false;
    while !stop {
        while y >= 0 {
            if x < blk && y < blk {
                diag.push(ScanPos {
                    x: x as u8,
                    y: y as u8,
                });
            }
            y -= 1;
            x += 1;
        }
        y = x;
        x = 0;
        if diag.len() >= n {
            stop = true;
        }
    }

    diag
}
