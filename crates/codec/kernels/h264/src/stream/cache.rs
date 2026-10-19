// SPDX-License-Identifier: LGPL-2.1-or-later
// FFmpeg-style 5x8 macroblock neighbourhoods. External neighbours are loaded
// once; coefficient and intra-mode syntax use fixed offsets in the hot loop.
use super::{Chroma, Neighbour};
pub(super) struct Cache {
    nnz: [[u8; 40]; 3],
    modes: [u8; 40],
    dc: [(bool, bool); 3],
    left8: [[bool; 4]; 3],
    top8: [[bool; 4]; 3],
}
impl Cache {
    #[inline]
    pub fn new(
        left: Option<&Neighbour>,
        top: Option<&Neighbour>,
        chroma: Chroma,
        intra: bool,
        constrained: bool,
        cabac: bool,
    ) -> Self {
        let missing = if cabac && !intra { 0 } else { 64 };
        let mut out = Self {
            nnz: [[missing; 40]; 3],
            modes: [255; 40],
            dc: [(intra, intra); 3],
            left8: [[intra; 4]; 3],
            top8: [[intra; 4]; 3],
        };
        for p in 0..3 {
            let side = if p != 0 && chroma == Chroma::Yuv420 {
                2
            } else {
                4
            };
            for y in 0..side {
                out.nnz[p][12 + y * 8..12 + y * 8 + side].fill(0);
            }
            if let Some(n) = left {
                out.dc[p].0 = n.dc[p];
                for y in 0..side {
                    let value = n.nnz[p][y * 4 + side - 1];
                    out.nnz[p][11 + y * 8] = value;
                    out.left8[p][y] = if n.transform8 { value != 0 } else { n.pcm };
                }
            }
            if let Some(n) = top {
                out.dc[p].1 = n.dc[p];
                for x in 0..side {
                    let value = n.nnz[p][(side - 1) * 4 + x];
                    out.nnz[p][4 + x] = value;
                    out.top8[p][x] = if n.transform8 { value != 0 } else { n.pcm };
                }
            }
        }
        if intra {
            for y in 0..4 {
                out.modes[12 + y * 8..16 + y * 8].fill(2);
            }
            if let Some(n) = left.filter(|n| !constrained || n.intra) {
                for y in 0..4 {
                    out.modes[11 + y * 8] = n.modes[y * 4 + 3];
                }
            }
            if let Some(n) = top.filter(|n| !constrained || n.intra) {
                out.modes[4..8].copy_from_slice(&n.modes[12..16]);
            }
        }
        out
    }
    #[inline]
    pub fn nc(&self, plane: usize, x: usize, y: usize) -> i32 {
        let i = 12 + y * 8 + x;
        let sum = self.nnz[plane][i - 1] as i32 + self.nnz[plane][i - 8] as i32;
        if sum < 64 { (sum + 1) >> 1 } else { sum & 31 }
    }
    #[inline]
    pub fn cbf(&self, plane: usize, x: usize, y: usize, dc: bool, block8: bool) -> (bool, bool) {
        if dc {
            return self.dc[plane];
        }
        let i = 12 + y * 8 + x;
        (
            if block8 && x == 0 {
                self.left8[plane][y]
            } else {
                self.nnz[plane][i - 1] != 0
            },
            if block8 && y == 0 {
                self.top8[plane][x]
            } else {
                self.nnz[plane][i - 8] != 0
            },
        )
    }
    #[inline]
    pub fn mode(&self, x: usize, y: usize) -> u8 {
        let i = 12 + y * 8 + x;
        let (a, b) = (self.modes[i - 1], self.modes[i - 8]);
        if a > 8 || b > 8 { 2 } else { a.min(b) }
    }
    #[inline]
    pub fn set_mode(&mut self, x: usize, y: usize, mode: u8) {
        self.modes[12 + y * 8 + x] = mode;
    }
    #[inline]
    pub fn set_nnz(&mut self, plane: usize, x: usize, y: usize, count: u8) {
        self.nnz[plane][12 + y * 8 + x] = count;
    }
}
