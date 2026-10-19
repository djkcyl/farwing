// SPDX-License-Identifier: LGPL-2.1-or-later
// FFmpeg 8.0.3 h264dsp_template.c / h264_loopfilter.c.
// Copyright (c) 2003-2011 Michael Niedermayer. Rust adaptation OpenUUYC.
use super::clip;
use crate::{Error, Result, tables};

#[derive(Clone, Copy)]
pub struct Filter {
    pub strength: [u8; 4],
    pub qp: i32,
    pub alpha_offset: i8,
    pub beta_offset: i8,
    /// Luma rules apply to all three 4:4:4 planes. Only subsampled
    /// chroma uses the shorter filter and tc0+1 rule.
    pub subsampled_chroma: bool,
}
#[derive(Clone, Copy)]
pub struct Edge {
    pub x: usize,
    pub y: usize,
    pub vertical: bool,
    /// Samples per strength segment: 4 luma, 2 for 420 chroma.
    pub segment_len: usize,
}
/// Constructed only after this module validates the full footprint. The type
/// parameter separates the four-tap chroma and eight-tap luma rectangles.
#[cfg(all(target_arch = "x86_64", not(feature = "scalar-dsp")))]
pub(super) struct ValidatedEdge<'a, const CHROMA: bool> {
    data: &'a mut [u8],
    stride: usize,
    edge: Edge,
}
#[cfg(all(target_arch = "x86_64", not(feature = "scalar-dsp")))]
impl<'a, const CHROMA: bool> ValidatedEdge<'a, CHROMA> {
    pub(super) fn into_parts(self) -> (&'a mut [u8], usize, Edge) {
        (self.data, self.stride, self.edge)
    }
}
#[inline]
pub fn apply(data: &mut [u8], stride: usize, edge: Edge, filter: Filter) -> Result<()> {
    apply_inner::<true>(data, stride, edge, filter)
}
#[inline(always)]
fn apply_inner<const SIMD: bool>(
    data: &mut [u8],
    stride: usize,
    edge: Edge,
    filter: Filter,
) -> Result<()> {
    if stride == 0
        || !matches!(edge.segment_len, 2 | 4)
        || filter.strength.iter().any(|&s| s > 4)
        || !(-12..=12).contains(&filter.alpha_offset)
        || !(-12..=12).contains(&filter.beta_offset)
    {
        return Err(Error::Invalid(crate::Fault::DeblockParameters));
    }
    let width = stride;
    let height = data.len() / stride;
    let reach = if filter.subsampled_chroma { 2 } else { 4 };
    let length = 4 * edge.segment_len;
    let (dx, dy) = if edge.vertical {
        (1, stride)
    } else {
        (stride, 1)
    };
    let valid = if edge.vertical {
        edge.x >= reach
            && edge.x.checked_add(reach).is_some_and(|x| x <= width)
            && edge.y.checked_add(length - 1).is_some_and(|y| y < height)
    } else {
        edge.y >= reach
            && edge.y.checked_add(reach - 1).is_some_and(|y| y < height)
            && edge.x.checked_add(length).is_some_and(|x| x <= width)
    };
    if !valid {
        return Err(Error::Invalid(crate::Fault::DeblockFootprint));
    }
    let ia = (filter.qp + filter.alpha_offset as i32).clamp(0, 51) as usize + 52;
    let ib = (filter.qp + filter.beta_offset as i32).clamp(0, 51) as usize + 52;
    let (alpha, beta) = (tables::ALPHA[ia] as i32, tables::BETA[ib] as i32);
    if alpha == 0 || beta == 0 {
        return Ok(());
    }
    #[cfg(all(target_arch = "x86_64", not(feature = "scalar-dsp")))]
    if SIMD && !filter.subsampled_chroma && edge.segment_len == 4 {
        let tc = filter.strength.map(|bs| {
            if bs > 0 && bs < 4 {
                tables::TC0[ia * 4 + bs as usize] as i16
            } else {
                0
            }
        });
        super::x86::luma(
            ValidatedEdge::<false> { data, stride, edge },
            alpha,
            beta,
            filter.strength,
            tc,
        );
        return Ok(());
    }
    #[cfg(all(target_arch = "x86_64", not(feature = "scalar-dsp")))]
    if SIMD && filter.subsampled_chroma && edge.segment_len == 2 {
        let tc = filter.strength.map(|bs| {
            if bs > 0 && bs < 4 {
                tables::TC0[ia * 4 + bs as usize] as i16
            } else {
                0
            }
        });
        super::x86::chroma_filter(
            ValidatedEdge::<true> { data, stride, edge },
            alpha,
            beta,
            filter.strength,
            tc,
        );
        return Ok(());
    }
    let base = edge.y * stride + edge.x;
    for (segment, &bs) in filter.strength.iter().enumerate() {
        if bs == 0 {
            continue;
        }
        let tc0 = if bs < 4 {
            tables::TC0[ia * 4 + bs as usize] as i32
        } else {
            0
        };
        for d in 0..edge.segment_len {
            let q = base + (segment * edge.segment_len + d) * dy;
            let (p0, p1, q0, q1) = (
                data[q - dx] as i32,
                data[q - 2 * dx] as i32,
                data[q] as i32,
                data[q + dx] as i32,
            );
            if (p0 - q0).abs() >= alpha || (p1 - p0).abs() >= beta || (q1 - q0).abs() >= beta {
                continue;
            }
            if filter.subsampled_chroma {
                if bs == 4 {
                    data[q - dx] = ((2 * p1 + p0 + q1 + 2) >> 2) as u8;
                    data[q] = ((2 * q1 + q0 + p1 + 2) >> 2) as u8;
                } else {
                    let tc = tc0 + 1;
                    let delta = (((q0 - p0) * 4 + p1 - q1 + 4) >> 3).clamp(-tc, tc);
                    data[q - dx] = clip(p0 + delta);
                    data[q] = clip(q0 - delta);
                }
                continue;
            }
            let (p2, q2) = (data[q - 3 * dx] as i32, data[q + 2 * dx] as i32);
            if bs < 4 {
                let ap = (p2 - p0).abs() < beta;
                let aq = (q2 - q0).abs() < beta;
                let tc = tc0 + i32::from(ap) + i32::from(aq);
                if ap && tc0 != 0 {
                    data[q - 2 * dx] =
                        (p1 + (((p2 + ((p0 + q0 + 1) >> 1)) >> 1) - p1).clamp(-tc0, tc0)) as u8;
                }
                if aq && tc0 != 0 {
                    data[q + dx] =
                        (q1 + (((q2 + ((p0 + q0 + 1) >> 1)) >> 1) - q1).clamp(-tc0, tc0)) as u8;
                }
                let delta = (((q0 - p0) * 4 + p1 - q1 + 4) >> 3).clamp(-tc, tc);
                data[q - dx] = clip(p0 + delta);
                data[q] = clip(q0 - delta);
            } else {
                let small = (p0 - q0).abs() < (alpha >> 2) + 2;
                if small && (p2 - p0).abs() < beta {
                    let p3 = data[q - 4 * dx] as i32;
                    data[q - dx] = ((p2 + 2 * p1 + 2 * p0 + 2 * q0 + q1 + 4) >> 3) as u8;
                    data[q - 2 * dx] = ((p2 + p1 + p0 + q0 + 2) >> 2) as u8;
                    data[q - 3 * dx] = ((2 * p3 + 3 * p2 + p1 + p0 + q0 + 4) >> 3) as u8;
                } else {
                    data[q - dx] = ((2 * p1 + p0 + q1 + 2) >> 2) as u8;
                }
                if small && (q2 - q0).abs() < beta {
                    let q3 = data[q + 3 * dx] as i32;
                    data[q] = ((p1 + 2 * p0 + 2 * q0 + 2 * q1 + q2 + 4) >> 3) as u8;
                    data[q + dx] = ((p0 + q0 + q1 + q2 + 2) >> 2) as u8;
                    data[q + 2 * dx] = ((2 * q3 + 3 * q2 + q1 + q0 + p0 + 4) >> 3) as u8;
                } else {
                    data[q] = ((2 * q1 + q0 + p1 + 2) >> 2) as u8;
                }
            }
        }
    }
    Ok(())
}

#[derive(Clone, Copy)]
pub struct Motion {
    pub reference: u8,
    pub x: i16,
    pub y: i16,
}
impl Default for Motion {
    fn default() -> Self {
        Self {
            reference: u8::MAX,
            x: 0,
            y: 0,
        }
    }
}
/// Compare stable DPB positions for this picture.
/// Slice-list indices alone do not imply equal reference pictures.
pub fn inter_strength(p: Motion, q: Motion, nonzero: bool, vertical_limit: i32) -> u8 {
    if nonzero {
        return 2;
    }
    u8::from(
        p.reference != q.reference
            || (p.reference != u8::MAX
                && ((p.x as i32 - q.x as i32).abs() >= 4
                    || (p.y as i32 - q.y as i32).abs() >= vertical_limit)),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn packed_edges_match_scalar_and_preserve_footprint() {
        let mut seed = 0x8344abcd_u32;
        for chroma in [false, true] {
            for vertical in [false, true] {
                for qp in 0..=51 {
                    for pattern in 0..128 {
                        let mut input = vec![0u8; 31 * 27];
                        for v in &mut input {
                            seed = seed.wrapping_mul(1664525).wrapping_add(1013904223);
                            *v = if pattern % 2 == 0 {
                                ((pattern * 7 + (seed % 25) as i32 - 12).clamp(0, 255)) as u8
                            } else {
                                (seed >> 24) as u8
                            };
                        }
                        let mut a = input.clone();
                        let mut b = input;
                        let filter = Filter {
                            strength: std::array::from_fn(|i| {
                                ((pattern as usize / (i + 1)) % 5) as u8
                            }),
                            qp,
                            alpha_offset: (pattern % 25 - 12) as i8,
                            beta_offset: ((pattern / 5) % 25 - 12) as i8,
                            subsampled_chroma: chroma,
                        };
                        let edge = Edge {
                            x: 7,
                            y: 7,
                            vertical,
                            segment_len: if chroma { 2 } else { 4 },
                        };
                        apply_inner::<true>(&mut a, 31, edge, filter).unwrap();
                        apply_inner::<false>(&mut b, 31, edge, filter).unwrap();
                        assert_eq!(a, b, "vertical={vertical} qp={qp} pattern={pattern}");
                    }
                }
            }
        }
    }
}
