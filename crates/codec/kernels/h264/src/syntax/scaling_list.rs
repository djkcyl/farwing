// SPDX-License-Identifier: MIT
// Derived from oxideav-h264 0.1.8, Copyright (c) 2026 Karpelès Lab Inc.
// Retained syntax/metadata primitives used by this client; see COPYING.OxideAV in this crate.

use crate::syntax::bitstream::{BitError, BitReader};

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ScalingListError {
    #[error("bitstream read failed: {0}")]
    Bitstream(#[from] BitError),
    #[error("scaling_list size must be 16 or 64 (got {0})")]
    InvalidSize(u32),
    #[error("delta_scale out of range (got {0}, must be in -128..=127)")]
    DeltaScaleOutOfRange(i32),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScalingListResult {
    pub scaling_list: Vec<i32>,
    pub use_default: bool,
}

pub fn parse_scaling_list(
    r: &mut BitReader<'_>,
    size_of_scaling_list: u32,
) -> Result<ScalingListResult, ScalingListError> {
    if size_of_scaling_list != 16 && size_of_scaling_list != 64 {
        return Err(ScalingListError::InvalidSize(size_of_scaling_list));
    }

    // §7.3.2.1.1.1 — direct transcription of the spec pseudo-code.
    let n = size_of_scaling_list as usize;
    let mut scaling_list: Vec<i32> = Vec::with_capacity(n);
    let mut use_default = false;
    let mut last_scale: i32 = 8;
    let mut next_scale: i32 = 8;

    for j in 0..n {
        if next_scale != 0 {
            // §7.4.2.1.1.1: "The value of delta_scale shall be in the
            // range of −128 to +127, inclusive." Enforce the range
            // explicitly — out-of-range values from a malformed stream
            // would overflow the `last_scale + delta_scale + 256`
            // arithmetic below (both operands are i32 and an attacker-
            // controlled `delta_scale` could be near i32::MAX after
            // se(v) decode of an oversized codeNum).
            let delta_scale = r.se()?;
            if !(-128..=127).contains(&delta_scale) {
                return Err(ScalingListError::DeltaScaleOutOfRange(delta_scale));
            }
            next_scale = (last_scale + delta_scale + 256).rem_euclid(256);
            if j == 0 && next_scale == 0 {
                use_default = true;
            }
        }
        let entry = if next_scale == 0 {
            last_scale
        } else {
            next_scale
        };
        scaling_list.push(entry);
        last_scale = entry;
    }

    Ok(ScalingListResult {
        scaling_list,
        use_default,
    })
}
