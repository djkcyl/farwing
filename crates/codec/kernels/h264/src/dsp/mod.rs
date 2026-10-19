// SPDX-License-Identifier: LGPL-2.1-or-later
//! Byte-plane DSP interface. All kernels share checked geometry and operate
//! directly on coded planes; 4:4:4 uses the same luma kernels on three planes.
use crate::{Error, Result};
pub mod filter;
pub mod intra;
pub mod motion;
pub mod transform;
#[cfg(all(target_arch = "x86_64", not(feature = "scalar-dsp")))]
#[allow(unsafe_code)]
mod x86;
#[cfg(all(target_arch = "x86_64", not(feature = "scalar-dsp")))]
pub(crate) use x86::transpose8;

pub struct Block<'a> {
    data: &'a mut [u8],
    stride: usize,
    width: usize,
    height: usize,
}

/// Checked once per partition, before any output or boundary metadata changes.
/// Private fields let both subsampled and full-size targets derive contained
/// views without rechecking the same rectangle for every color plane.
pub(crate) struct PartitionGeometry {
    x: usize,
    y: usize,
    width: usize,
    height: usize,
}
impl PartitionGeometry {
    pub(crate) fn new(x: usize, y: usize, width: usize, height: usize) -> Result<Self> {
        if x > 12
            || y > 12
            || x % 4 != 0
            || y % 4 != 0
            || !matches!(width, 4 | 8 | 16)
            || !matches!(height, 4 | 8 | 16)
            || x + width > 16
            || y + height > 16
        {
            return Err(Error::Invalid(crate::Fault::PartitionGeometry));
        }
        Ok(Self {
            x,
            y,
            width,
            height,
        })
    }
    pub(crate) fn coordinates(&self) -> (usize, usize, usize, usize) {
        (self.x, self.y, self.width, self.height)
    }
    pub(crate) fn coverage(&self) -> u16 {
        let row = ((1u16 << (self.width / 4)) - 1) << (self.x / 4);
        let mut mask = 0;
        for y in self.y / 4..(self.y + self.height) / 4 {
            mask |= row << (4 * y);
        }
        mask
    }
}

/// An already bounded 16x16 (luma/444) or 8x8 (420 chroma) macroblock plane.
pub(crate) struct MacroblockPlane<'a>(Block<'a>);
impl<'a> MacroblockPlane<'a> {
    pub(crate) fn new(block: Block<'a>) -> Result<Self> {
        if !matches!(block.width, 8 | 16) || block.height != block.width {
            return Err(Error::Invalid(crate::Fault::DspBlockGeometry));
        }
        Ok(Self(block))
    }
    pub(crate) fn partition(&mut self, geometry: &PartitionGeometry) -> Block<'_> {
        let shift = usize::from(self.0.width == 8);
        let x = geometry.x >> shift;
        let y = geometry.y >> shift;
        let offset = y * self.0.stride + x;
        // Geometry is contained in 16x16; halving gives a contained 8x8
        // rectangle. The parent constructor checked the full allocation.
        Block {
            data: &mut self.0.data[offset..],
            stride: self.0.stride,
            width: geometry.width >> shift,
            height: geometry.height >> shift,
        }
    }
}
impl<'a> Block<'a> {
    pub fn new(data: &'a mut [u8], stride: usize, width: usize, height: usize) -> Result<Self> {
        if width == 0
            || height == 0
            || width > 16
            || height > 16
            || stride < width
            || (height - 1)
                .checked_mul(stride)
                .and_then(|n| n.checked_add(width))
                .is_none_or(|n| n > data.len())
        {
            return Err(Error::Invalid(crate::Fault::DspBlockGeometry));
        }
        Ok(Self {
            data,
            stride,
            width,
            height,
        })
    }
    pub fn row_mut(&mut self, y: usize) -> &mut [u8] {
        &mut self.data[y * self.stride..y * self.stride + self.width]
    }
    pub fn row(&self, y: usize) -> &[u8] {
        &self.data[y * self.stride..y * self.stride + self.width]
    }
}
#[inline]
pub(crate) fn clip(v: i32) -> u8 {
    v.clamp(0, 255) as u8
}
#[inline]
#[cfg(not(all(target_arch = "x86_64", not(feature = "scalar-dsp"))))]
pub(crate) fn avg(a: u8, b: u8) -> u8 {
    ((a as u16 + b as u16 + 1) >> 1) as u8
}

/// Append interleaved chroma directly to reserved output storage. A trusted
/// fixed-size iterator lets LLVM vectorize without pre-zeroing that storage.
#[inline]
pub(crate) fn append_chroma(u: &[u8], v: &[u8], dst: &mut Vec<u8>) {
    assert_eq!(u.len(), v.len());
    dst.extend(u.iter().zip(v).flat_map(|(&u, &v)| [u, v]));
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn interleave_matches_scalar_with_short_rows_and_guards() {
        for length in [0, 1, 2, 7, 15, 16, 17, 31, 33, 316] {
            let u: Vec<_> = (0..length).map(|x| (x * 3) as u8).collect();
            let v: Vec<_> = (0..length).map(|x| (x * 7 + 11) as u8).collect();
            let mut dst = vec![91; 3];
            append_chroma(&u, &v, &mut dst);
            assert_eq!(dst.len(), length * 2 + 3);
            dst.extend_from_slice(&[91; 3]);
            assert_eq!(&dst[..3], &[91; 3]);
            assert_eq!(&dst[length * 2 + 3..], &[91; 3]);
            for i in 0..length {
                assert_eq!(&dst[3 + i * 2..5 + i * 2], &[u[i], v[i]]);
            }
        }
    }
}
