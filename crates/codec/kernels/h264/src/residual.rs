// SPDX-License-Identifier: LGPL-2.1-or-later
// FFmpeg 8.0.3 h264_cabac.c residual significance/level decoder.
// Copyright (c) 2003 Michael Niedermayer. Rust adaptation OpenUUYC.
use crate::{
    Error, Result,
    cabac::{Cabac, Contexts},
    dsp::transform::Dequant,
    tables,
};
const CBF: [usize; 14] = [
    85, 89, 93, 97, 101, 1012, 460, 464, 468, 1016, 472, 476, 480, 1020,
];
const SIG: [usize; 14] = [
    105, 120, 134, 149, 152, 402, 484, 499, 513, 660, 528, 543, 557, 718,
];
const LAST: [usize; 14] = [
    166, 181, 195, 210, 213, 417, 572, 587, 601, 690, 616, 631, 645, 748,
];
const LEVEL: [usize; 14] = [
    227, 237, 247, 257, 266, 426, 952, 962, 972, 708, 982, 992, 1002, 766,
];
const ONE: [usize; 8] = [1, 2, 3, 4, 0, 0, 0, 0];
const GREATER: [usize; 8] = [5, 5, 5, 5, 6, 7, 8, 9];
const NEXT_ONE: [usize; 8] = [1, 2, 3, 3, 4, 5, 6, 7];
const NEXT_GREATER: [usize; 8] = [4, 4, 4, 4, 5, 6, 7, 7];

/// Each call directly stores decoded levels in the current macroblock's
/// transposed scratch. No per-coefficient vector or deferred residual object.
#[allow(clippy::too_many_arguments)]
pub fn cabac(
    c: &mut Cabac,
    ctx: &mut Contexts,
    category: usize,
    cbf: Option<(bool, bool)>,
    scan: &crate::scan::Scan,
    qmul: Option<&[i32]>,
    dst: &mut [i16],
) -> Result<u8> {
    if category >= 14 || !scan.fits(dst, qmul) {
        return Err(Error::Invalid(crate::Fault::CabacResidualGeometry));
    }
    // Scan is validated at compile time. The private entropy driver supplies
    // zeroed scratch: reconstruction consumes and clears each coded block.
    // Errors abandon the slice scratch rather than reusing partial residuals.
    let scan = scan.positions();
    if let Some((left, top)) = cbf {
        if c.decision(&mut ctx.0[CBF[category] + usize::from(left) + 2 * usize::from(top)]) == 0 {
            c.check()?;
            return Ok(0);
        }
    }
    let mut positions = [0u8; 64];
    let mut count = 0;
    let mut terminated = false;
    for position in 0..scan.len() - 1 {
        let sig = if scan.len() == 64 {
            tables::SIGNIFICANT8[position] as usize
        } else {
            position
        };
        if c.decision(&mut ctx.0[SIG[category] + sig]) != 0 {
            positions[count] = position as u8;
            count += 1;
            let last = if scan.len() == 64 {
                tables::CABAC[1280 + position] as usize
            } else {
                position
            };
            if c.decision(&mut ctx.0[LAST[category] + last]) != 0 {
                terminated = true;
                break;
            }
        }
    }
    if !terminated {
        positions[count] = (scan.len() - 1) as u8;
        count += 1;
    }
    let mut node = 0;
    for &position in positions[..count].iter().rev() {
        let mut magnitude = 1i32;
        if c.decision(&mut ctx.0[LEVEL[category] + ONE[node]]) == 0 {
            node = NEXT_ONE[node];
        } else {
            magnitude = 2;
            let index = LEVEL[category] + GREATER[node];
            node = NEXT_GREATER[node];
            while magnitude < 15 && c.decision(&mut ctx.0[index]) != 0 {
                magnitude += 1;
            }
            if magnitude == 15 {
                let mut prefix = 0;
                while c.bypass() != 0 {
                    prefix += 1;
                    if prefix > 23 {
                        return Err(Error::Invalid(crate::Fault::CabacLevelPrefix));
                    }
                }
                magnitude = 1;
                for _ in 0..prefix {
                    magnitude = (magnitude << 1) + c.bypass() as i32;
                }
                magnitude += 14;
            }
        }
        let level = if c.bypass() != 0 {
            -magnitude
        } else {
            magnitude
        };
        let index = scan[position as usize] as usize;
        dst[index] = match qmul {
            Some(q) => Dequant::coefficient(level, q[index]),
            None => i16::try_from(level).map_err(|_| Error::Invalid(crate::Fault::CabacDcRange))?,
        };
    }
    c.check()?;
    Ok(count as u8)
}
