// SPDX-License-Identifier: LGPL-2.1-or-later
// FFmpeg 8.0.3 h264idct_template.c / h264_ps.c.
// Copyright (c) 2004-2011 Michael Niedermayer. Rust adaptation OpenUUYC.
use super::{Block, clip};
use crate::{Error, Result, tables};
use std::sync::Arc;

/// Tables are transposed exactly as the inverse-transform input. Identical
/// scaling lists share storage; no quantizer division or matrix multiplication
/// is repeated while decoding a residual coefficient.
pub struct Dequant {
    pub four: [Arc<[[i32; 16]; 52]>; 6],
    pub eight: [Arc<[[i32; 64]; 52]>; 6],
}
impl Dequant {
    pub fn new(four: &[[u8; 16]; 6], eight: &[[u8; 64]; 6]) -> Result<Self> {
        if four
            .iter()
            .flatten()
            .chain(eight.iter().flatten())
            .any(|&x| x == 0)
        {
            return Err(Error::Invalid(crate::Fault::ZeroScalingWeight));
        }
        let mut f: Vec<Arc<[[i32; 16]; 52]>> = Vec::with_capacity(6);
        let mut e: Vec<Arc<[[i32; 64]; 52]>> = Vec::with_capacity(6);
        for i in 0..6 {
            if let Some(j) = (0..i).find(|&j| four[j] == four[i]) {
                f.push(f[j].clone());
            } else {
                let mut t = [[0; 16]; 52];
                for (q, row) in t.iter_mut().enumerate() {
                    for x in 0..16 {
                        row[(x >> 2) | ((x & 3) << 2)] =
                            (tables::DEQUANT4[(q % 6) * 3 + (x & 1) + ((x >> 2) & 1)] as i32
                                * four[i][x] as i32)
                                << (q / 6 + 2);
                    }
                }
                f.push(Arc::new(t));
            }
            if let Some(j) = (0..i).find(|&j| eight[j] == eight[i]) {
                e.push(e[j].clone());
            } else {
                let mut t = [[0; 64]; 52];
                for (q, row) in t.iter_mut().enumerate() {
                    for x in 0..64 {
                        let index = tables::DEQUANT8_SCAN[((x >> 1) & 12) | (x & 3)] as usize;
                        row[(x >> 3) | ((x & 7) << 3)] =
                            (tables::DEQUANT8[(q % 6) * 6 + index] as i32 * eight[i][x] as i32)
                                << (q / 6);
                    }
                }
                e.push(Arc::new(t));
            }
        }
        Ok(Self {
            four: f.try_into().ok().unwrap(),
            eight: e.try_into().ok().unwrap(),
        })
    }
    #[inline]
    pub fn coefficient(level: i32, multiplier: i32) -> i16 {
        (level.wrapping_mul(multiplier).wrapping_add(32) >> 6) as i16
    }
}

fn shape(dst: &Block<'_>, n: usize) -> Result<()> {
    if dst.width != n || dst.height != n {
        Err(Error::Invalid(crate::Fault::TransformBlockShape))
    } else {
        Ok(())
    }
}
/// FFmpeg add16/add16intra/add8/add4 scheduling: the whole plane prediction
/// must already exist. Intra4/8 callers retain their per-block dependencies.
pub fn add_plane(
    dst: &mut Block<'_>,
    coefficients: &mut [i16; 256],
    nonzero: &[u8; 16],
    transform8: bool,
    separate_dc: bool,
) -> Result<()> {
    if !matches!(dst.width, 8 | 16) || dst.height != dst.width || (transform8 && dst.width != 16) {
        return Err(Error::Invalid(crate::Fault::TransformBlockShape));
    }
    #[cfg(all(target_arch = "x86_64", not(feature = "scalar-dsp")))]
    {
        super::x86::add_plane(dst, coefficients, nonzero, transform8, separate_dc);
        return Ok(());
    }
    #[cfg(not(all(target_arch = "x86_64", not(feature = "scalar-dsp"))))]
    {
        let n = if transform8 { 8 } else { 4 };
        for i in 0..dst.width * dst.width / (n * n) {
            let count = if transform8 {
                nonzero[i * 4..i * 4 + 4].iter().map(|&v| v as u16).sum()
            } else {
                nonzero[i] as u16
            };
            let offset = i * n * n;
            let dc = coefficients[offset];
            if count == 0 && dc == 0 {
                continue;
            }
            let (x, y) = plane_block_position(i, dst.width, n);
            let at = y * dst.stride + x;
            let mut block = Block::new(&mut dst.data[at..], dst.stride, n, n)?;
            if count == 0 || (!separate_dc && count == 1 && dc != 0) {
                add_dc(&mut block, &mut coefficients[offset])?;
            } else if transform8 {
                add8(
                    &mut block,
                    (&mut coefficients[offset..offset + 64]).try_into().unwrap(),
                )?;
            } else {
                add4(
                    &mut block,
                    (&mut coefficients[offset..offset + 16]).try_into().unwrap(),
                )?;
            }
        }
        Ok(())
    }
}
#[inline]
pub(crate) fn plane_block_position(i: usize, size: usize, n: usize) -> (usize, usize) {
    if n == 8 {
        (i % 2 * 8, i / 2 * 8)
    } else if size == 8 {
        (i % 2 * 4, i / 2 * 4)
    } else {
        ((i / 4 % 2) * 8 + i % 2 * 4, i / 8 * 8 + i % 4 / 2 * 4)
    }
}
/// FFmpeg's input is transposed. The first pass truncates to signed 16 bits,
/// matching its dctcoef storage; the final add clips to unsigned byte pixels.
pub fn add4(dst: &mut Block<'_>, block: &mut [i16; 16]) -> Result<()> {
    shape(dst, 4)?;
    #[cfg(all(target_arch = "x86_64", not(feature = "scalar-dsp")))]
    if super::x86::add4(dst, block) {
        return Ok(());
    }
    block[0] = block[0].wrapping_add(32);
    for i in 0..4 {
        let a = block[i] as i32;
        let b = block[i + 4] as i32;
        let c = block[i + 8] as i32;
        let d = block[i + 12] as i32;
        let (z0, z1, z2, z3) = (a + c, a - c, (b >> 1) - d, b + (d >> 1));
        for (j, v) in [z0 + z3, z1 + z2, z1 - z2, z0 - z3].into_iter().enumerate() {
            block[i + j * 4] = v as i16;
        }
    }
    for i in 0..4 {
        let [a, b, c, d] = std::array::from_fn(|j| block[4 * i + j] as i32);
        let (z0, z1, z2, z3) = (a + c, a - c, (b >> 1) - d, b + (d >> 1));
        for (y, v) in [z0 + z3, z1 + z2, z1 - z2, z0 - z3].into_iter().enumerate() {
            let row = dst.row_mut(y);
            row[i] = clip(row[i] as i32 + (v >> 6));
        }
    }
    block.fill(0);
    Ok(())
}
#[inline]
fn butterfly8(v: [i32; 8]) -> [i32; 8] {
    let a0 = v[0] + v[4];
    let a2 = v[0] - v[4];
    let a4 = (v[2] >> 1) - v[6];
    let a6 = (v[6] >> 1) + v[2];
    let (b0, b2, b4, b6) = (a0 + a6, a2 + a4, a2 - a4, a0 - a6);
    let a1 = -v[3] + v[5] - v[7] - (v[7] >> 1);
    let a3 = v[1] + v[7] - v[3] - (v[3] >> 1);
    let a5 = -v[1] + v[7] + v[5] + (v[5] >> 1);
    let a7 = v[3] + v[5] + v[1] + (v[1] >> 1);
    let (b1, b3, b5, b7) = (
        (a7 >> 2) + a1,
        a3 + (a5 >> 2),
        (a3 >> 2) - a5,
        a7 - (a1 >> 2),
    );
    [
        b0 + b7,
        b2 + b5,
        b4 + b3,
        b6 + b1,
        b6 - b1,
        b4 - b3,
        b2 - b5,
        b0 - b7,
    ]
}
pub fn add8(dst: &mut Block<'_>, block: &mut [i16; 64]) -> Result<()> {
    shape(dst, 8)?;
    #[cfg(all(target_arch = "x86_64", not(feature = "scalar-dsp")))]
    if super::x86::add8(dst, block) {
        return Ok(());
    }
    block[0] = block[0].wrapping_add(32);
    for i in 0..8 {
        let v = butterfly8(std::array::from_fn(|j| block[i + j * 8] as i32));
        for j in 0..8 {
            block[i + j * 8] = v[j] as i16;
        }
    }
    for i in 0..8 {
        let v = butterfly8(std::array::from_fn(|j| block[i * 8 + j] as i32));
        for (y, &value) in v.iter().enumerate() {
            let row = dst.row_mut(y);
            row[i] = clip(row[i] as i32 + (value >> 6));
        }
    }
    block.fill(0);
    Ok(())
}
pub fn add_dc(dst: &mut Block<'_>, dc: &mut i16) -> Result<()> {
    if !matches!(dst.width, 4 | 8) || dst.width != dst.height {
        return Err(Error::Invalid(crate::Fault::DcBlockShape));
    }
    #[cfg(all(target_arch = "x86_64", not(feature = "scalar-dsp")))]
    if super::x86::add_dc(dst, dc) {
        return Ok(());
    }
    let value = (*dc as i32 + 32) >> 6;
    for y in 0..dst.height {
        for p in dst.row_mut(y) {
            *p = clip(*p as i32 + value);
        }
    }
    *dc = 0;
    Ok(())
}
pub fn bypass(
    dst: &mut Block<'_>,
    residual: &mut [i16],
    stride: usize,
    vertical: bool,
    horizontal: bool,
) -> Result<()> {
    if vertical && horizontal
        || stride < dst.width
        || (dst.height - 1)
            .checked_mul(stride)
            .and_then(|n| n.checked_add(dst.width))
            .is_none_or(|n| n > residual.len())
    {
        return Err(Error::Invalid(crate::Fault::BypassResidualGeometry));
    }
    let mut above = [0i32; 16];
    for y in 0..dst.height {
        let mut left = 0;
        for x in 0..dst.width {
            let v = residual[y * stride + x] as i32
                + if vertical {
                    above[x]
                } else if horizontal {
                    left
                } else {
                    0
                };
            above[x] = v;
            left = v;
            let row = dst.row_mut(y);
            row[x] = clip(row[x] as i32 + v);
            residual[y * stride + x] = 0;
        }
    }
    Ok(())
}
pub fn luma_dc(coeff: &[i16; 16], qmul: i32) -> [i16; 16] {
    let mut temp = [0i32; 16];
    let mut out = [0; 16];
    for i in 0..4 {
        let [a, b, c, d] = std::array::from_fn(|j| coeff[i * 4 + j] as i32);
        let (z0, z1, z2, z3) = (a + b, a - b, c - d, c + d);
        temp[i * 4..i * 4 + 4].copy_from_slice(&[z0 + z3, z0 - z3, z1 - z2, z1 + z2]);
    }
    for i in 0..4 {
        let (z0, z1, z2, z3) = (
            temp[i] + temp[8 + i],
            temp[i] - temp[8 + i],
            temp[4 + i] - temp[12 + i],
            temp[4 + i] + temp[12 + i],
        );
        // Canonical spatial block raster; the slice's scan maps into this.
        for (j, v) in [z0 + z3, z1 + z2, z1 - z2, z0 - z3].into_iter().enumerate() {
            out[j * 4 + i] = (v.wrapping_mul(qmul).wrapping_add(128) >> 8) as i16;
        }
    }
    out
}
pub fn chroma_dc(coeff: [i16; 4], qmul: i32) -> [i16; 4] {
    let [a, b, c, d] = coeff.map(i32::from);
    let (e, f, g, h) = (a - b, a + b, c - d, c + d);
    [f + h, e + g, f - h, e - g].map(|v| (v.wrapping_mul(qmul) >> 7) as i16)
}
