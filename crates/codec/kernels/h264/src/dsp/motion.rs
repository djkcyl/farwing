// SPDX-License-Identifier: LGPL-2.1-or-later
// FFmpeg h264qpel_template.c / h264chroma_template.c specialized byte kernels.
// Copyright (c) 2003-2011 Michael Niedermayer. Rust adaptation OpenUUYC.
#[cfg(not(all(target_arch = "x86_64", not(feature = "scalar-dsp"))))]
use super::avg;
use super::{Block, clip};
use crate::{Error, Result, picture::Plane};
#[inline]
#[cfg(not(all(target_arch = "x86_64", not(feature = "scalar-dsp"))))]
fn tap(a: i32, b: i32, c: i32, d: i32, e: i32, f: i32) -> i32 {
    (c + d) * 20 - (b + e) * 5 + a + f
}

pub fn luma(src: &Plane, x_qpel: i32, y_qpel: i32, dst: &mut Block<'_>) -> Result<()> {
    if !matches!(dst.width, 4 | 8 | 16) || !matches!(dst.height, 4 | 8 | 16) {
        return Err(Error::Invalid(crate::Fault::LumaPartition));
    }
    // Signed arithmetic shifts implement floor division by powers of two,
    // including negative MVs; masks give the corresponding positive fraction.
    let (ix, iy) = (x_qpel >> 2, y_qpel >> 2);
    let (fx, fy) = (x_qpel & 3, y_qpel & 3);
    if fx == 0 && fy == 0 {
        copy(src, ix, iy, dst);
        return Ok(());
    }
    if let Some((input, stride)) = src.footprint(ix - 2, iy - 2, dst.width + 5, dst.height + 5) {
        return luma_input(input, stride, fx, fy, dst);
    }
    luma_edge(src, ix, iy, fx, fy, dst)
}
#[cold]
#[inline(never)]
fn luma_edge(src: &Plane, ix: i32, iy: i32, fx: i32, fy: i32, dst: &mut Block<'_>) -> Result<()> {
    let mut edge = [0u8; 21 * 21];
    for y in 0..dst.height + 5 {
        for x in 0..dst.width + 5 {
            edge[y * 21 + x] = src.sample(ix + x as i32 - 2, iy + y as i32 - 2);
        }
    }
    luma_input(&edge, 21, fx, fy, dst)
}
#[inline]
fn luma_input(input: &[u8], stride: usize, fx: i32, fy: i32, dst: &mut Block<'_>) -> Result<()> {
    #[cfg(all(target_arch = "x86_64", not(feature = "scalar-dsp")))]
    {
        if super::x86::qpel(input, stride, fx as u8, fy as u8, dst) {
            Ok(())
        } else {
            Err(Error::Invalid(crate::Fault::LumaPartition))
        }
    }
    #[cfg(not(all(target_arch = "x86_64", not(feature = "scalar-dsp"))))]
    {
        macro_rules! fraction {
            ($x:literal,$y:literal) => {
                match dst.width {
                    4 => qpel::<4, $x, $y>(input, stride, dst),
                    8 => qpel::<8, $x, $y>(input, stride, dst),
                    16 => qpel::<16, $x, $y>(input, stride, dst),
                    _ => unreachable!(),
                }
            };
        }
        match (fx, fy) {
            (1, 0) => fraction!(1, 0),
            (2, 0) => fraction!(2, 0),
            (3, 0) => fraction!(3, 0),
            (0, 1) => fraction!(0, 1),
            (1, 1) => fraction!(1, 1),
            (2, 1) => fraction!(2, 1),
            (3, 1) => fraction!(3, 1),
            (0, 2) => fraction!(0, 2),
            (1, 2) => fraction!(1, 2),
            (2, 2) => fraction!(2, 2),
            (3, 2) => fraction!(3, 2),
            (0, 3) => fraction!(0, 3),
            (1, 3) => fraction!(1, 3),
            (2, 3) => fraction!(2, 3),
            (3, 3) => fraction!(3, 3),
            _ => unreachable!(),
        }
        Ok(())
    }
}
fn copy(src: &Plane, x: i32, y: i32, dst: &mut Block<'_>) {
    if let Some((data, stride)) = src.footprint(x, y, dst.width, dst.height) {
        #[cfg(all(target_arch = "x86_64", not(feature = "scalar-dsp")))]
        if super::x86::copy(data, stride, dst) {
            return;
        }
        let w = dst.width;
        for row in 0..dst.height {
            dst.row_mut(row)
                .copy_from_slice(&data[row * stride..row * stride + w]);
        }
    } else {
        for row in 0..dst.height {
            for column in 0..dst.width {
                dst.row_mut(row)[column] = src.sample(x + column as i32, y + row as i32);
            }
        }
    }
}
/// Integer-motion weighted prediction can write its final values once. The
/// ordinary path copied a complete partition and then loaded/stored it again
/// for weighting; fractional motion keeps the existing interpolation rules.
pub fn weighted_luma(
    src: &Plane,
    x_qpel: i32,
    y_qpel: i32,
    dst: &mut Block<'_>,
    denom: u8,
    factor: i16,
    offset: i16,
) -> Result<()> {
    if denom > 7 || !(-128..=128).contains(&factor) || !(-128..=127).contains(&offset) {
        return Err(Error::Invalid(crate::Fault::PredictionWeight));
    }
    if !matches!(dst.width, 4 | 8 | 16) || !matches!(dst.height, 4 | 8 | 16) {
        return Err(Error::Invalid(crate::Fault::LumaPartition));
    }
    #[cfg(all(target_arch = "x86_64", not(feature = "scalar-dsp")))]
    if (x_qpel | y_qpel) & 3 == 0 {
        if let Some((input, stride)) =
            src.footprint(x_qpel >> 2, y_qpel >> 2, dst.width, dst.height)
        {
            if super::x86::weighted_copy(input, stride, dst, denom, factor, offset) {
                return Ok(());
            }
        }
    }
    luma(src, x_qpel, y_qpel, dst)?;
    weight(dst, denom, factor, offset)
}
// Separate monomorphizations hoist fractional-position and width decisions out
// of every pixel. Fixed contiguous rows allow LLVM's packed integer kernels.
#[cfg(not(all(target_arch = "x86_64", not(feature = "scalar-dsp"))))]
fn qpel<const W: usize, const X: u8, const Y: u8>(src: &[u8], stride: usize, dst: &mut Block<'_>) {
    let diagonal = X != 0 && Y != 0 && (X == 2 || Y == 2);
    let mut strip = [[0i16; 16]; 21];
    if diagonal {
        for y in 0..dst.height + 5 {
            let row = &src[y * stride..y * stride + W + 5];
            let output = &mut strip[y];
            for x in 0..W {
                output[x] = tap(
                    row[x] as i32,
                    row[x + 1] as i32,
                    row[x + 2] as i32,
                    row[x + 3] as i32,
                    row[x + 4] as i32,
                    row[x + 5] as i32,
                ) as i16;
            }
        }
    }
    for y in 0..dst.height {
        let hs = (y + 2 + usize::from(Y == 3)) * stride;
        let hr = &src[hs..hs + W + 5];
        let vshift = 2 + usize::from(X == 3);
        let vr: [&[u8]; 6] =
            std::array::from_fn(|i| &src[(y + i) * stride + vshift..(y + i) * stride + vshift + W]);
        let gr = (y + 2 + usize::from(Y == 3)) * stride + 2 + usize::from(X == 3);
        let integer = &src[gr..gr + W];
        let output = &mut dst.data[y * dst.stride..y * dst.stride + W];
        for x in 0..W {
            let h = || {
                clip(
                    (tap(
                        hr[x] as i32,
                        hr[x + 1] as i32,
                        hr[x + 2] as i32,
                        hr[x + 3] as i32,
                        hr[x + 4] as i32,
                        hr[x + 5] as i32,
                    ) + 16)
                        >> 5,
                )
            };
            let v = || {
                clip(
                    (tap(
                        vr[0][x] as i32,
                        vr[1][x] as i32,
                        vr[2][x] as i32,
                        vr[3][x] as i32,
                        vr[4][x] as i32,
                        vr[5][x] as i32,
                    ) + 16)
                        >> 5,
                )
            };
            let j = || {
                clip(
                    (tap(
                        strip[y][x] as i32,
                        strip[y + 1][x] as i32,
                        strip[y + 2][x] as i32,
                        strip[y + 3][x] as i32,
                        strip[y + 4][x] as i32,
                        strip[y + 5][x] as i32,
                    ) + 512)
                        >> 10,
                )
            };
            output[x] = match (X, Y) {
                (2, 0) => h(),
                (0, 2) => v(),
                (2, 2) => j(),
                (1 | 3, 0) => avg(integer[x], h()),
                (0, 1 | 3) => avg(integer[x], v()),
                (2, 1 | 3) => avg(h(), j()),
                (1 | 3, 2) => avg(v(), j()),
                (1 | 3, 1 | 3) => avg(h(), v()),
                _ => unreachable!(),
            };
        }
    }
}

pub fn chroma(src: &Plane, x_eighth: i32, y_eighth: i32, dst: &mut Block<'_>) -> Result<()> {
    if !matches!(dst.width, 2 | 4 | 8) || !matches!(dst.height, 2 | 4 | 8) {
        return Err(Error::Invalid(crate::Fault::ChromaPartition));
    }
    let (ix, iy) = (x_eighth >> 3, y_eighth >> 3);
    let (fx, fy) = (x_eighth & 7, y_eighth & 7);
    if fx == 0 && fy == 0 {
        copy(src, ix, iy, dst);
        return Ok(());
    }
    let mut edge;
    let (input, stride) = if let Some(v) = src.footprint(ix, iy, dst.width + 1, dst.height + 1) {
        v
    } else {
        edge = [0u8; 9 * 9];
        for y in 0..=dst.height {
            for x in 0..=dst.width {
                edge[y * 9 + x] = src.sample(ix + x as i32, iy + y as i32);
            }
        }
        (&edge[..], 9)
    };
    #[cfg(all(target_arch = "x86_64", not(feature = "scalar-dsp")))]
    if super::x86::chroma(input, stride, fx, fy, dst) {
        return Ok(());
    }
    match dst.width {
        2 => chroma_block::<2>(input, stride, fx, fy, dst),
        4 => chroma_block::<4>(input, stride, fx, fy, dst),
        8 => chroma_block::<8>(input, stride, fx, fy, dst),
        _ => unreachable!(),
    };
    Ok(())
}
fn chroma_block<const W: usize>(src: &[u8], stride: usize, fx: i32, fy: i32, dst: &mut Block<'_>) {
    let weights = [(8 - fx) * (8 - fy), fx * (8 - fy), (8 - fx) * fy, fx * fy];
    for y in 0..dst.height {
        let a = &src[y * stride..y * stride + W + 1];
        let b = &src[(y + 1) * stride..(y + 1) * stride + W + 1];
        let output = &mut dst.data[y * dst.stride..y * dst.stride + W];
        for x in 0..W {
            output[x] = ((a[x] as i32 * weights[0]
                + a[x + 1] as i32 * weights[1]
                + b[x] as i32 * weights[2]
                + b[x + 1] as i32 * weights[3]
                + 32)
                >> 6) as u8;
        }
    }
}
pub fn weight(dst: &mut Block<'_>, denom: u8, weight: i16, offset: i16) -> Result<()> {
    if denom > 7 || !(-128..=128).contains(&weight) || !(-128..=127).contains(&offset) {
        return Err(Error::Invalid(crate::Fault::PredictionWeight));
    }
    if weight == 1 << denom && offset == 0 {
        return Ok(());
    }
    #[cfg(all(target_arch = "x86_64", not(feature = "scalar-dsp")))]
    if super::x86::weight(dst, denom, weight, offset) {
        return Ok(());
    }
    let round = if denom == 0 { 0 } else { 1 << (denom - 1) };
    for y in 0..dst.height {
        for v in dst.row_mut(y) {
            *v = clip(((*v as i32 * weight as i32 + round) >> denom) + offset as i32);
        }
    }
    Ok(())
}
