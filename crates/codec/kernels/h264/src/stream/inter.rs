// SPDX-License-Identifier: LGPL-2.1-or-later
// FFmpeg h264_mvpred.h and H.264 P partition syntax. Macroblock-local motion
// scratch feeds reconstruction immediately, with no frame-thread holdback.
use super::*;
use crate::reconstruct::{Partition, Prediction, Weights};

#[derive(Clone, Copy, Default)]
struct NeighbourMv {
    available: bool,
    reference: i8,
    mv: [i16; 2],
    mvd: [i8; 2],
}

struct MotionCache {
    cells: [NeighbourMv; 40],
}
impl MotionCache {
    fn new(grid: &SyntaxGrid, address: usize, slice: u32) -> Self {
        let none = NeighbourMv {
            reference: -2,
            ..Default::default()
        };
        let mut cache = Self { cells: [none; 40] };
        let item = |n: &Neighbour, i: usize| NeighbourMv {
            available: true,
            reference: n.motion[i].reference.map_or(-1, |r| r as i8),
            mv: n.motion[i].mv,
            mvd: n.mvd[i],
        };
        if let Some(left) = grid.left(address, slice) {
            for y in 0..4 {
                cache.cells[11 + y * 8] = item(left, y * 4 + 3);
            }
        }
        if let Some(top) = grid.top(address, slice) {
            for x in 0..4 {
                cache.cells[4 + x] = item(top, 12 + x);
            }
        }
        let col = address % grid.width;
        if address >= grid.width {
            if col > 0 {
                if let Some(n) = grid
                    .at(address - grid.width - 1)
                    .filter(|n| n.slice == slice)
                {
                    cache.cells[3] = item(n, 15);
                }
            }
            if col + 1 < grid.width {
                if let Some(n) = grid
                    .at(address - grid.width + 1)
                    .filter(|n| n.slice == slice)
                {
                    cache.cells[8] = item(n, 12);
                }
            }
        }
        cache
    }
    #[inline]
    fn at(&self, x: i32, y: i32, require_mv: bool) -> NeighbourMv {
        let index = ((y + 1) * 8 + x + 4) as usize;
        let value = self.cells.get(index).copied().unwrap_or(NeighbourMv {
            reference: -2,
            ..Default::default()
        });
        if require_mv && !value.available {
            NeighbourMv {
                reference: -2,
                ..Default::default()
            }
        } else {
            value
        }
    }
    fn reference(&mut self, x: usize, y: usize, w: usize, h: usize, index: u8) {
        for yy in y..y + h {
            for cell in &mut self.cells[12 + yy * 8 + x..12 + yy * 8 + x + w] {
                cell.reference = index as i8;
            }
        }
    }
    fn decoded(&mut self, x: usize, y: usize, w: usize, h: usize, p: Prediction, mvd: [i32; 2]) {
        let value = NeighbourMv {
            available: true,
            reference: p.reference.map_or(-1, |v| v as i8),
            mv: p.mv,
            mvd: mvd.map(|v| v.clamp(-70, 70) as i8),
        };
        for yy in y..y + h {
            self.cells[12 + yy * 8 + x..12 + yy * 8 + x + w].fill(value);
        }
    }
    fn predict(&self, x: usize, y: usize, w: usize, shape: u32, reference: u8) -> [i16; 2] {
        let a = self.at(x as i32 - 1, y as i32, true);
        let b = self.at(x as i32, y as i32 - 1, true);
        let mut c = self.at((x + w) as i32, y as i32 - 1, true);
        if !c.available {
            c = self.at(x as i32 - 1, y as i32 - 1, true);
        }
        let matches = |n: NeighbourMv| n.reference == reference as i8;
        if shape == 1 {
            if y == 0 && matches(b) {
                return b.mv;
            }
            if y != 0 && matches(a) {
                return a.mv;
            }
        }
        if shape == 2 {
            if x == 0 && matches(a) {
                return a.mv;
            }
            if x != 0 && matches(c) {
                return c.mv;
            }
        }
        if u8::from(matches(a)) + u8::from(matches(b)) + u8::from(matches(c)) == 1 {
            return if matches(a) {
                a.mv
            } else if matches(b) {
                b.mv
            } else {
                c.mv
            };
        }
        if !b.available && !c.available && a.available {
            return a.mv;
        }
        std::array::from_fn(|i| a.mv[i].max(b.mv[i].min(c.mv[i])).min(b.mv[i].max(c.mv[i])))
    }
}
pub(super) fn weights(header: &SliceHeader, index: u8) -> [Option<Weights>; 3] {
    std::array::from_fn(|plane| {
        header.pred_weight_table.as_ref().and_then(|table| {
            let denominator = if plane == 0 {
                table.luma_log2_weight_denom
            } else {
                table.chroma_log2_weight_denom
            } as u8;
            let (w, o) = if plane == 0 {
                table
                    .luma_weights_l0
                    .get(index as usize)
                    .copied()
                    .flatten()
                    .unwrap_or((1 << denominator, 0))
            } else {
                table
                    .chroma_weights_l0
                    .get(index as usize)
                    .copied()
                    .flatten()
                    .map(|p| p[plane - 1])
                    .unwrap_or((1 << denominator, 0))
            };
            if denominator <= 7 && w == 1 << denominator && o == 0 {
                return None;
            }
            Some(Weights {
                denominator,
                weight: w as i16,
                offset: o as i16,
            })
        })
    })
}
pub(super) fn p_skip(
    grid: &SyntaxGrid,
    address: usize,
    neighbour: &mut Neighbour,
    weights: [Option<Weights>; 3],
    parts: &mut [Partition; 16],
) {
    let left = grid.left(address, neighbour.slice);
    let top = grid.top(address, neighbour.slice);
    let zero = |n: Option<&Neighbour>, index: usize| {
        n.is_none_or(|n| n.motion[index].reference == Some(0) && n.motion[index].mv == [0, 0])
    };
    let mv = if zero(left, 3) || zero(top, 12) {
        [0, 0]
    } else {
        MotionCache::new(grid, address, neighbour.slice).predict(0, 0, 4, 0, 0)
    };
    let prediction = Prediction {
        reference: Some(0),
        mv,
    };
    neighbour.motion = [prediction; 16];
    parts[0] = Partition {
        x: 0,
        y: 0,
        width: 16,
        height: 16,
        prediction,
        weights,
    };
}
pub(super) fn p_parts<'a>(
    entropy: &mut Entropy<'_>,
    grid: &SyntaxGrid,
    address: usize,
    neighbour: &mut Neighbour,
    header: &SliceHeader,
    weights: &[[Option<Weights>; 3]; 32],
    kind: u32,
    parts: &'a mut [Partition; 16],
) -> Result<(Kind<'a>, bool)> {
    let mut cache = MotionCache::new(grid, address, neighbour.slice);
    let mut count = 0;
    let mut subtypes = [0; 4];
    let groups = if kind >= 3 {
        for v in &mut subtypes {
            *v = entropy.p_subtype()?;
        }
        4
    } else if kind == 0 {
        1
    } else {
        2
    };
    let mut indices = [0; 4];
    for group in 0..groups {
        let (x, y, w, h) = match kind {
            0 => (0, 0, 4, 4),
            1 => (0, group * 2, 4, 2),
            2 => (group * 2, 0, 2, 4),
            _ => (group % 2 * 2, group / 2 * 2, 2, 2),
        };
        let a = cache.at(x as i32 - 1, y as i32, false);
        let b = cache.at(x as i32, y as i32 - 1, false);
        let index = if kind == 4 {
            0
        } else {
            entropy.reference(
                header.num_ref_idx_l0_active_minus1 + 1,
                a.reference,
                b.reference,
            )?
        };
        indices[group] = index;
        cache.reference(x, y, w, h, index);
        for yy in y..y + h {
            for xx in x..x + w {
                neighbour.motion[yy * 4 + xx].reference = Some(index);
            }
        }
    }
    let mut allowed8 = true;
    for group in 0..groups {
        let (gx, gy, gw, gh) = match kind {
            0 => (0, 0, 4, 4),
            1 => (0, group * 2, 4, 2),
            2 => (group * 2, 0, 2, 4),
            _ => (group % 2 * 2, group / 2 * 2, 2, 2),
        };
        let (w, h) = if kind < 3 {
            (gw, gh)
        } else {
            match subtypes[group] {
                0 => (2, 2),
                1 => (2, 1),
                2 => (1, 2),
                _ => (1, 1),
            }
        };
        if w < 2 || h < 2 {
            allowed8 = false;
        }
        for dy in (0..gh).step_by(h) {
            for dx in (0..gw).step_by(w) {
                let (x, y) = (gx + dx, gy + dy);
                let reference = indices[group];
                let prediction = cache.predict(x, y, w, kind, reference);
                let a = cache.at(x as i32 - 1, y as i32, true);
                let b = cache.at(x as i32, y as i32 - 1, true);
                let mut mvd = [0; 2];
                let mut mv = [0; 2];
                for axis in 0..2 {
                    mvd[axis] = entropy.mvd(
                        axis,
                        (a.mvd[axis] as i32).abs() + (b.mvd[axis] as i32).abs(),
                    )?;
                    mv[axis] = i16::try_from(prediction[axis] as i32 + mvd[axis])
                        .map_err(|_| Error::Invalid(crate::Fault::MotionVectorRange))?;
                }
                let prediction = Prediction {
                    reference: Some(reference),
                    mv,
                };
                for yy in y..y + h {
                    for xx in x..x + w {
                        let at = yy * 4 + xx;
                        neighbour.motion[at] = prediction;
                        neighbour.mvd[at] = mvd.map(|v| v.clamp(-70, 70) as i8);
                    }
                }
                cache.decoded(x, y, w, h, prediction, mvd);
                parts[count] = Partition {
                    x: (x * 4) as u8,
                    y: (y * 4) as u8,
                    width: (w * 4) as u8,
                    height: (h * 4) as u8,
                    prediction,
                    weights: weights[reference as usize],
                };
                count += 1;
            }
        }
    }
    Ok((
        Kind::Inter {
            parts: &parts[..count],
        },
        allowed8,
    ))
}
