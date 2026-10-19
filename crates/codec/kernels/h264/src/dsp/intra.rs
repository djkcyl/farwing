// SPDX-License-Identifier: LGPL-2.1-or-later
// FFmpeg 8.0.3 h264pred_template.c, Copyright (c) 2003-2011 Michael Niedermayer.
use super::{Block, clip};
use crate::{Error, Result};

#[derive(Clone)]
pub struct Edges {
    pub(crate) top: [u8; 32],
    pub(crate) left: [u8; 16],
    pub(crate) corner: u8,
    has_top: bool,
    has_left: bool,
    has_corner: bool,
}
impl Edges {
    pub fn new(size: usize, top: &[u8], left: &[u8], corner: Option<u8>) -> Result<Self> {
        if !matches!(size, 4 | 8 | 16)
            || (!top.is_empty() && top.len() < size)
            || (!left.is_empty() && left.len() < size)
        {
            return Err(Error::Invalid(crate::Fault::IntraEdgeGeometry));
        }
        let mut result = Self {
            top: [128; 32],
            left: [128; 16],
            corner: corner.unwrap_or(128),
            has_top: !top.is_empty(),
            has_left: !left.is_empty(),
            has_corner: corner.is_some(),
        };
        if !top.is_empty() {
            let count = top.len().min(result.top.len());
            result.top[..count].copy_from_slice(&top[..count]);
            result.top[count..].fill(top[count - 1]);
        }
        if !left.is_empty() {
            result.left[..size].copy_from_slice(&left[..size]);
            result.left[size..].fill(left[size - 1]);
        }
        Ok(result)
    }
    fn t(&self, i: i32) -> i32 {
        if i < 0 {
            self.corner as i32
        } else {
            self.top[i.min(31) as usize] as i32
        }
    }
    fn l(&self, i: i32, n: usize) -> i32 {
        if i < 0 {
            self.corner as i32
        } else {
            self.left[(i as usize).min(n - 1)] as i32
        }
    }
    fn filtered8(&self, mode: u8) -> Self {
        let mut out = self.clone();
        let top_needed = mode != 1 && mode != 8;
        let left_needed = !matches!(mode, 0 | 3 | 7);
        #[cfg(not(all(target_arch = "x86_64", not(feature = "scalar-dsp"))))]
        {
            if self.has_top && top_needed {
                out.top[0] = quarter(
                    if self.has_corner {
                        self.corner as i32
                    } else {
                        self.t(0)
                    },
                    self.t(0),
                    self.t(1),
                ) as u8;
                for i in 1..16 {
                    out.top[i] = quarter(
                        self.t(i as i32 - 1),
                        self.t(i as i32),
                        self.t((i + 1).min(15) as i32),
                    ) as u8;
                }
            }
            if self.has_left && left_needed {
                out.left[0] = quarter(
                    if self.has_corner {
                        self.corner as i32
                    } else {
                        self.l(0, 8)
                    },
                    self.l(0, 8),
                    self.l(1, 8),
                ) as u8;
                for i in 1..8 {
                    out.left[i] = quarter(
                        self.l(i as i32 - 1, 8),
                        self.l(i as i32, 8),
                        self.l(i as i32 + 1, 8),
                    ) as u8;
                }
            }
        }
        #[cfg(all(target_arch = "x86_64", not(feature = "scalar-dsp")))]
        {
            if self.has_top && top_needed {
                let mut line = [self.top[15]; 18];
                line[0] = if self.has_corner {
                    self.corner
                } else {
                    self.top[0]
                };
                line[1..17].copy_from_slice(&self.top[..16]);
                out.top[..16].copy_from_slice(&super::x86::average3(&line));
            }
            if self.has_left && left_needed {
                let mut line = [self.left[7]; 18];
                line[0] = if self.has_corner {
                    self.corner
                } else {
                    self.left[0]
                };
                line[1..9].copy_from_slice(&self.left[..8]);
                out.left[..8].copy_from_slice(&super::x86::average3(&line)[..8]);
            }
        }
        if self.has_corner && matches!(mode, 4..=6) {
            out.corner = quarter(
                if self.has_left {
                    self.l(0, 8)
                } else {
                    self.corner as i32
                },
                self.corner as i32,
                if self.has_top {
                    self.t(0)
                } else {
                    self.corner as i32
                },
            ) as u8;
        }
        out
    }
}
#[inline]
fn quarter(a: i32, b: i32, c: i32) -> i32 {
    (a + 2 * b + c + 2) >> 2
}
#[inline]
fn half(a: i32, b: i32) -> i32 {
    (a + b + 1) >> 1
}

fn require(e: &Edges, top: bool, left: bool, corner: bool) -> Result<()> {
    if top && !e.has_top || left && !e.has_left || corner && !e.has_corner {
        Err(Error::Invalid(crate::Fault::UnavailableIntraNeighbour))
    } else {
        Ok(())
    }
}
fn dc(e: &Edges, n: usize) -> u8 {
    let sum_t: i32 = e.top[..n].iter().map(|&v| v as i32).sum();
    let sum_l: i32 = e.left[..n].iter().map(|&v| v as i32).sum();
    match (e.has_top, e.has_left) {
        (true, true) => ((sum_t + sum_l + n as i32) / (2 * n) as i32) as u8,
        (true, false) => ((sum_t + n as i32 / 2) / n as i32) as u8,
        (false, true) => ((sum_l + n as i32 / 2) / n as i32) as u8,
        _ => 128,
    }
}
fn vertical_right(x: i32, y: i32, top: impl Fn(i32) -> i32, left: impl Fn(i32) -> i32) -> i32 {
    let z = 2 * x - y;
    if z >= 0 {
        let k = x - y / 2;
        if z & 1 == 0 {
            half(top(k - 1), top(k))
        } else {
            quarter(top(k - 2), top(k - 1), top(k))
        }
    } else if z == -1 {
        quarter(left(0), top(-1), top(0))
    } else {
        let k = y - 2 * x;
        quarter(left(k - 3), left(k - 2), left(k - 1))
    }
}
/// H.264 luma modes 0..8; 4:4:4 Cb/Cr use this exact process.
pub fn nxn(dst: &mut Block<'_>, edges: &Edges, mode: u8) -> Result<()> {
    let n = dst.width;
    if !matches!(n, 4 | 8) || dst.height != n || mode > 8 {
        return Err(Error::Invalid(crate::Fault::IntraNxnMode));
    }
    let filtered;
    let e = if n == 8 {
        filtered = edges.filtered8(mode);
        &filtered
    } else {
        edges
    };
    match mode {
        0 | 3 | 7 => require(e, true, false, false)?,
        1 | 8 => require(e, false, true, false)?,
        4..=6 => require(e, true, true, true)?,
        _ => {}
    }
    match mode {
        0 => {
            for y in 0..n {
                dst.row_mut(y).copy_from_slice(&e.top[..n]);
            }
            return Ok(());
        }
        1 => {
            for y in 0..n {
                dst.row_mut(y).fill(e.left[y]);
            }
            return Ok(());
        }
        2 => {
            let d = dc(e, n);
            for y in 0..n {
                dst.row_mut(y).fill(d);
            }
            return Ok(());
        }
        _ => {}
    }
    #[cfg(all(target_arch = "x86_64", not(feature = "scalar-dsp")))]
    if super::x86::intra_nxn(dst, e, mode) {
        return Ok(());
    }
    let d = dc(e, n);
    let top = |i: i32| e.t(i.min((2 * n - 1) as i32));
    let left = |i: i32| e.l(i, n);
    for y in 0..n {
        for x in 0..n {
            let (xx, yy) = (x as i32, y as i32);
            let v = match mode {
                0 => top(xx),
                1 => left(yy),
                2 => d as i32,
                3 => quarter(top(xx + yy), top(xx + yy + 1), top(xx + yy + 2)),
                4 => {
                    let z = xx - yy;
                    if z > 0 {
                        quarter(top(z - 2), top(z - 1), top(z))
                    } else if z < 0 {
                        quarter(left(-z - 2), left(-z - 1), left(-z))
                    } else {
                        quarter(left(0), e.corner as i32, top(0))
                    }
                }
                5 => vertical_right(xx, yy, top, left),
                6 => vertical_right(yy, xx, left, top),
                7 => {
                    let k = xx + yy / 2;
                    if yy & 1 == 0 {
                        half(top(k), top(k + 1))
                    } else {
                        quarter(top(k), top(k + 1), top(k + 2))
                    }
                }
                8 => {
                    let k = yy + xx / 2;
                    if xx & 1 == 0 {
                        half(left(k), left(k + 1))
                    } else {
                        quarter(left(k), left(k + 1), left(k + 2))
                    }
                }
                _ => unreachable!(),
            };
            dst.row_mut(y)[x] = v as u8;
        }
    }
    Ok(())
}
pub fn luma16(dst: &mut Block<'_>, e: &Edges, mode: u8) -> Result<()> {
    if dst.width != 16 || dst.height != 16 || mode > 3 {
        return Err(Error::Invalid(crate::Fault::Intra16Mode));
    }
    match mode {
        0 => require(e, true, false, false)?,
        1 => require(e, false, true, false)?,
        3 => require(e, true, true, true)?,
        _ => {}
    }
    match mode {
        0 => {
            for y in 0..16 {
                dst.row_mut(y).copy_from_slice(&e.top[..16]);
            }
            return Ok(());
        }
        1 => {
            for y in 0..16 {
                dst.row_mut(y).fill(e.left[y]);
            }
            return Ok(());
        }
        2 => {
            let d = dc(e, 16);
            for y in 0..16 {
                dst.row_mut(y).fill(d);
            }
            return Ok(());
        }
        _ => {}
    }
    let mut h = 0;
    let mut v = 0;
    if mode == 3 {
        for i in 1..=8 {
            h += i * (e.t(7 + i) - e.t(7 - i));
            v += i * (e.l(7 + i, 16) - e.l(7 - i, 16));
        }
    }
    let (a, b, c) = (
        16 * (e.t(15) + e.l(15, 16)),
        (5 * h + 32) >> 6,
        (5 * v + 32) >> 6,
    );
    #[cfg(all(target_arch = "x86_64", not(feature = "scalar-dsp")))]
    if super::x86::plane(dst, a, b, c, 7) {
        return Ok(());
    }
    let d = dc(e, 16);
    for y in 0..16 {
        for x in 0..16 {
            dst.row_mut(y)[x] = match mode {
                0 => e.top[x],
                1 => e.left[y],
                2 => d,
                _ => clip((a + b * (x as i32 - 7) + c * (y as i32 - 7) + 16) >> 5),
            };
        }
    }
    Ok(())
}
/// 4:2:0's 8x8 chroma DC has four quadrant predictions; it is not luma DC.
pub fn chroma8(dst: &mut Block<'_>, e: &Edges, mode: u8) -> Result<()> {
    if dst.width != 8 || dst.height != 8 || mode > 3 {
        return Err(Error::Invalid(crate::Fault::ChromaIntraMode));
    }
    match mode {
        1 => require(e, false, true, false)?,
        2 => require(e, true, false, false)?,
        3 => require(e, true, true, true)?,
        _ => {}
    }
    match mode {
        1 => {
            for y in 0..8 {
                dst.row_mut(y).fill(e.left[y]);
            }
            return Ok(());
        }
        2 => {
            for y in 0..8 {
                dst.row_mut(y).copy_from_slice(&e.top[..8]);
            }
            return Ok(());
        }
        _ => {}
    }
    let t: [i32; 2] =
        std::array::from_fn(|i| e.top[i * 4..i * 4 + 4].iter().map(|&v| v as i32).sum());
    let l: [i32; 2] =
        std::array::from_fn(|i| e.left[i * 4..i * 4 + 4].iter().map(|&v| v as i32).sum());
    let mut h = 0;
    let mut v = 0;
    if mode == 3 {
        for i in 1..=4 {
            h += i * (e.t(3 + i) - e.t(3 - i));
            v += i * (e.l(3 + i, 8) - e.l(3 - i, 8));
        }
    }
    let (a, b, c) = (
        16 * (e.t(7) + e.l(7, 8)),
        (17 * h + 16) >> 5,
        (17 * v + 16) >> 5,
    );
    #[cfg(all(target_arch = "x86_64", not(feature = "scalar-dsp")))]
    if mode == 3 && super::x86::plane(dst, a, b, c, 3) {
        return Ok(());
    }
    if mode == 0 {
        for qy in 0..2 {
            for qx in 0..2 {
                let d = match (e.has_top, e.has_left) {
                    (true, true) => {
                        if qx == qy {
                            (t[qx] + l[qy] + 4) >> 3
                        } else if qx == 1 {
                            (t[1] + 2) >> 2
                        } else {
                            (l[1] + 2) >> 2
                        }
                    }
                    (true, false) => (t[qx] + 2) >> 2,
                    (false, true) => (l[qy] + 2) >> 2,
                    _ => 128,
                };
                for y in qy * 4..qy * 4 + 4 {
                    dst.row_mut(y)[qx * 4..qx * 4 + 4].fill(d as u8);
                }
            }
        }
        return Ok(());
    }
    for y in 0..8 {
        for x in 0..8 {
            let (qx, qy) = (x / 4, y / 4);
            let d = match (e.has_top, e.has_left) {
                (true, true) => {
                    if qx == qy {
                        (t[qx] + l[qy] + 4) >> 3
                    } else if qx == 1 {
                        (t[1] + 2) >> 2
                    } else {
                        (l[1] + 2) >> 2
                    }
                }
                (true, false) => (t[qx] + 2) >> 2,
                (false, true) => (l[qy] + 2) >> 2,
                _ => 128,
            };
            dst.row_mut(y)[x] = match mode {
                0 => d as u8,
                1 => e.left[y],
                2 => e.top[x],
                _ => clip((a + b * (x as i32 - 3) + c * (y as i32 - 3) + 16) >> 5),
            };
        }
    }
    Ok(())
}
