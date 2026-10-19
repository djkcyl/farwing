// SPDX-License-Identifier: LGPL-2.1-or-later
use crate::{Error, Result};
use std::sync::Arc;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Chroma {
    Yuv420,
    Yuv444,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Crop {
    pub left: usize,
    pub top: usize,
    pub width: usize,
    pub height: usize,
}

/// Eight-bit storage shared by reconstruction, reference MC and output.
/// Padding belongs to the allocation; the visible crop never changes stride.
#[derive(Clone)]
pub struct Plane {
    data: Vec<u8>,
    pub width: usize,
    pub height: usize,
    pub stride: usize,
    border: usize,
}
impl Plane {
    pub fn new(width: usize, height: usize, border: usize) -> Result<Self> {
        if width == 0 || height == 0 || width > 16384 || height > 16384 || border > 64 {
            return Err(Error::Invalid(crate::Fault::PlaneGeometry));
        }
        let stride = (width + 2 * border).next_multiple_of(32);
        let size = stride
            .checked_mul(height + 2 * border)
            .ok_or(Error::Allocation)?;
        let mut data = Vec::new();
        data.try_reserve_exact(size)
            .map_err(|_| Error::Allocation)?;
        data.resize(size, 0);
        Ok(Self {
            data,
            width,
            height,
            stride,
            border,
        })
    }
    pub fn row(&self, y: usize) -> &[u8] {
        let at = self.origin() + y * self.stride;
        &self.data[at..at + self.width]
    }
    pub fn row_mut(&mut self, y: usize) -> &mut [u8] {
        let at = self.origin() + y * self.stride;
        &mut self.data[at..at + self.width]
    }
    pub fn origin(&self) -> usize {
        self.border * self.stride + self.border
    }
    pub fn storage(&self) -> &[u8] {
        &self.data
    }
    #[inline]
    pub fn filter(
        &mut self,
        mut edge: crate::dsp::filter::Edge,
        params: crate::dsp::filter::Filter,
    ) -> Result<()> {
        edge.x += self.border;
        edge.y += self.border;
        crate::dsp::filter::apply(&mut self.data, self.stride, edge, params)
    }
    pub fn block_mut(
        &mut self,
        x: usize,
        y: usize,
        w: usize,
        h: usize,
    ) -> Result<crate::dsp::Block<'_>> {
        if x.checked_add(w).is_none_or(|n| n > self.width)
            || y.checked_add(h).is_none_or(|n| n > self.height)
        {
            return Err(Error::Invalid(crate::Fault::BlockOutsidePicture));
        }
        let at = self.origin() + y * self.stride + x;
        crate::dsp::Block::new(&mut self.data[at..], self.stride, w, h)
    }
    /// Completed reference frames acquire replicated borders once. Internal MC
    /// can then borrow a contiguous footprint; far-out MVs use edge emulation.
    pub fn extend_edges(&mut self) {
        let b = self.border;
        let s = self.stride;
        for y in 0..self.height {
            let at = (y + b) * s;
            let l = self.data[at + b];
            let r = self.data[at + b + self.width - 1];
            self.data[at..at + b].fill(l);
            self.data[at + b + self.width..at + s].fill(r);
        }
        for y in 0..b {
            self.data.copy_within(b * s..(b + 1) * s, y * s);
            let last = (b + self.height - 1) * s;
            self.data
                .copy_within(last..last + s, (b + self.height + y) * s);
        }
    }
    pub fn sample(&self, x: i32, y: i32) -> u8 {
        self.row(y.clamp(0, self.height as i32 - 1) as usize)
            [x.clamp(0, self.width as i32 - 1) as usize]
    }
    /// Borrow an MC footprint including halo without changing its coordinates.
    pub fn footprint(&self, x: i32, y: i32, w: usize, h: usize) -> Option<(&[u8], usize)> {
        let bx = x as i64 + self.border as i64;
        let by = y as i64 + self.border as i64;
        if bx < 0
            || by < 0
            || bx + w as i64 > (self.width + 2 * self.border) as i64
            || by + h as i64 > (self.height + 2 * self.border) as i64
        {
            return None;
        }
        let start = by as usize * self.stride + bx as usize;
        let len = (h.checked_sub(1)?)
            .checked_mul(self.stride)?
            .checked_add(w)?;
        Some((self.data.get(start..start.checked_add(len)?)?, self.stride))
    }
}

pub struct Picture {
    pub planes: [Plane; 3],
    pub chroma: Chroma,
    pub crop: Crop,
}
impl Picture {
    pub fn new(width: usize, height: usize, chroma: Chroma, crop: Crop) -> Result<Self> {
        Self::validate(width, height, chroma, crop)?;
        let sub = if chroma == Chroma::Yuv420 { 2 } else { 1 };
        Ok(Self {
            planes: [
                Plane::new(width, height, 32)?,
                Plane::new(width / sub, height / sub, 32 / sub)?,
                Plane::new(width / sub, height / sub, 32 / sub)?,
            ],
            chroma,
            crop,
        })
    }
    pub(crate) fn validate(width: usize, height: usize, chroma: Chroma, crop: Crop) -> Result<()> {
        if width % 16 != 0
            || height % 16 != 0
            || crop.width == 0
            || crop.height == 0
            || crop.left.checked_add(crop.width).is_none_or(|n| n > width)
            || crop.top.checked_add(crop.height).is_none_or(|n| n > height)
        {
            return Err(Error::Invalid(crate::Fault::CodedSizeOrCrop));
        }
        let sub = if chroma == Chroma::Yuv420 { 2 } else { 1 };
        if sub == 2 && (crop.left | crop.top | crop.width | crop.height) & 1 != 0 {
            return Err(Error::Invalid(crate::Fault::Code420CropAlignment));
        }
        Ok(())
    }
    pub fn finish(mut self) -> Arc<Self> {
        for p in &mut self.planes {
            p.extend_edges();
        }
        Arc::new(self)
    }
    /// The existing renderer consumes NV12 or packed I444; only visible samples
    /// are copied. Reference allocation lifetime is independent of this output.
    pub fn pack_into(&self, out: &mut Vec<u8>) -> Result<()> {
        #[cfg(feature = "profile")]
        let _pack = crate::profile::Timer::new(6);
        let c = self.crop;
        let n = c.width.checked_mul(c.height).ok_or(Error::Allocation)?;
        let size = if self.chroma == Chroma::Yuv444 {
            n * 3
        } else {
            n + n / 2
        };
        out.clear();
        out.try_reserve(size).map_err(|_| Error::Allocation)?;
        for y in c.top..c.top + c.height {
            out.extend_from_slice(&self.planes[0].row(y)[c.left..c.left + c.width]);
        }
        if self.chroma == Chroma::Yuv444 {
            for p in &self.planes[1..] {
                for y in c.top..c.top + c.height {
                    out.extend_from_slice(&p.row(y)[c.left..c.left + c.width]);
                }
            }
        } else {
            for y in c.top / 2..(c.top + c.height) / 2 {
                let u = self.planes[1].row(y);
                let v = self.planes[2].row(y);
                let range = c.left / 2..(c.left + c.width) / 2;
                crate::dsp::append_chroma(&u[range.clone()], &v[range], out);
            }
        }
        Ok(())
    }
}

/// Unfiltered bottom borders survive row filtering and parameter changes only
/// for this in-progress picture. Intra prediction never reads filtered edges.
pub struct IntraBorders {
    rows: [Vec<u8>; 3],
    pub row: Option<usize>,
}
impl IntraBorders {
    pub fn new(p: &Picture) -> Self {
        Self {
            rows: std::array::from_fn(|i| vec![128; p.planes[i].width]),
            row: None,
        }
    }
    pub fn save_before_filter(&mut self, p: &Picture, mb_row: usize) -> Result<()> {
        for (i, plane) in p.planes.iter().enumerate() {
            let bh = if i != 0 && p.chroma == Chroma::Yuv420 {
                8
            } else {
                16
            };
            let y = (mb_row + 1) * bh - 1;
            if y >= plane.height || self.rows[i].len() != plane.width {
                return Err(Error::Invalid(crate::Fault::IntraBorderGeometry));
            }
            self.rows[i].copy_from_slice(plane.row(y));
        }
        self.row = Some(mb_row);
        Ok(())
    }
    pub fn top(&self, plane: usize) -> &[u8] {
        &self.rows[plane]
    }
}
