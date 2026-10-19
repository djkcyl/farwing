//! Concrete streaming pixel output. Kept in the optimized codec crate so
//! application size optimization cannot scalarize the per-pixel conversions.
use crate::decoder::{Picture, PixelLayout, PlanarImageComponent as Plane};
use std::sync::atomic::{AtomicBool, Ordering};
#[derive(Debug)]
pub enum PackError {
    Cancelled,
    Unsupported,
    Allocation,
}
#[derive(Clone, Copy, Debug)]
pub enum PixelFormat {
    Nv12,
    P010,
    Ayuv,
    Y410,
}
pub struct PackedPicture {
    pub width: u32,
    pub height: u32,
    pub coded_width: u32,
    pub coded_height: u32,
    pub format: PixelFormat,
    pub data: Vec<u8>,
}
#[inline(never)]
pub fn pack_picture(p: &Picture, cancel: &AtomicBool) -> Result<PackedPicture, PackError> {
    #[cfg(target_arch = "x86_64")]
    if p.pixel_layout() == PixelLayout::I444 && std::is_x86_feature_detected!("avx2") {
        // SAFETY: detected AVX2. The common body uses checked Rust slices;
        // the wide entry vectorizes packed four-component output.
        return unsafe { pack_avx2(p, cancel) };
    }
    pack_inner(p, cancel)
}
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2")]
unsafe fn pack_avx2(p: &Picture, cancel: &AtomicBool) -> Result<PackedPicture, PackError> {
    pack_inner(p, cancel)
}
#[inline(always)]
fn pack_inner(p: &Picture, cancel: &AtomicBool) -> Result<PackedPicture, PackError> {
    let depth = p.bits_per_component().ok_or(PackError::Unsupported)?.0;
    let half = match p.pixel_layout() {
        PixelLayout::I420 => true,
        PixelLayout::I444 => false,
        _ => return Err(PackError::Unsupported),
    };
    if !matches!(depth, 8 | 10) {
        return Err(PackError::Unsupported);
    }
    let (w, h) = (p.width() as usize, p.height() as usize);
    if w == 0 || h == 0 {
        return Err(PackError::Unsupported);
    }
    let (cw, ch) = if half {
        (w.div_ceil(2) * 2, h.div_ceil(2) * 2)
    } else {
        (w, h)
    };
    let bytes = if depth == 8 { 1 } else { 2 };
    let len = if half {
        cw * ch * 3 / 2 * bytes
    } else {
        cw * ch * 4
    };
    let mut data = Vec::new();
    data.try_reserve_exact(len)
        .map_err(|_| PackError::Allocation)?;
    data.resize(len, 0);
    let planes = [p.plane(Plane::Y), p.plane(Plane::U), p.plane(Plane::V)];
    let strides = [
        p.stride(Plane::Y) as usize,
        p.stride(Plane::U) as usize,
        p.stride(Plane::V) as usize,
    ];
    let row = |plane: usize, y: usize, width: usize| {
        let at = y * strides[plane];
        &planes[plane][at..at + width * bytes]
    };
    match (half, depth) {
        (true, 8) => {
            for y in 0..ch {
                if cancel.load(Ordering::Acquire) {
                    return Err(PackError::Cancelled);
                }
                let source = row(0, y.min(h - 1), w);
                let output = &mut data[y * cw..(y + 1) * cw];
                output[..w].copy_from_slice(source);
                output[w..].fill(source[w - 1]);
            }
            for y in 0..ch / 2 {
                if cancel.load(Ordering::Acquire) {
                    return Err(PackError::Cancelled);
                }
                let u = row(1, y, cw / 2);
                let v = row(2, y, cw / 2);
                let output = &mut data[cw * ch + y * cw..cw * ch + (y + 1) * cw];
                for ((pixel, u), v) in output.chunks_exact_mut(2).zip(u).zip(v) {
                    pixel[0] = *u;
                    pixel[1] = *v;
                }
            }
        }
        (true, 10) => {
            for y in 0..ch {
                if cancel.load(Ordering::Acquire) {
                    return Err(PackError::Cancelled);
                }
                let source = row(0, y.min(h - 1), w);
                let output = &mut data[y * cw * 2..(y + 1) * cw * 2];
                for (dst, src) in output[..w * 2]
                    .chunks_exact_mut(2)
                    .zip(source.chunks_exact(2))
                {
                    dst.copy_from_slice(&(u16::from_le_bytes([src[0], src[1]]) << 6).to_le_bytes());
                }
                if cw != w {
                    let last = u16::from_le_bytes([source[w * 2 - 2], source[w * 2 - 1]]) << 6;
                    output[w * 2..].copy_from_slice(&last.to_le_bytes());
                }
            }
            for y in 0..ch / 2 {
                if cancel.load(Ordering::Acquire) {
                    return Err(PackError::Cancelled);
                }
                let u = row(1, y, cw / 2);
                let v = row(2, y, cw / 2);
                let output = &mut data[(cw * ch + y * cw) * 2..(cw * ch + (y + 1) * cw) * 2];
                for ((dst, u), v) in output
                    .chunks_exact_mut(4)
                    .zip(u.chunks_exact(2))
                    .zip(v.chunks_exact(2))
                {
                    dst[..2]
                        .copy_from_slice(&(u16::from_le_bytes([u[0], u[1]]) << 6).to_le_bytes());
                    dst[2..]
                        .copy_from_slice(&(u16::from_le_bytes([v[0], v[1]]) << 6).to_le_bytes());
                }
            }
        }
        (false, 8) => {
            for y in 0..h {
                if cancel.load(Ordering::Acquire) {
                    return Err(PackError::Cancelled);
                }
                let yy = row(0, y, w);
                let u = row(1, y, w);
                let v = row(2, y, w);
                for (((dst, yy), u), v) in data[y * w * 4..(y + 1) * w * 4]
                    .chunks_exact_mut(4)
                    .zip(yy)
                    .zip(u)
                    .zip(v)
                {
                    dst.copy_from_slice(&[*v, *u, *yy, 255]);
                }
            }
        }
        (false, 10) => {
            for y in 0..h {
                if cancel.load(Ordering::Acquire) {
                    return Err(PackError::Cancelled);
                }
                let yy = row(0, y, w);
                let u = row(1, y, w);
                let v = row(2, y, w);
                for (((dst, yy), u), v) in data[y * w * 4..(y + 1) * w * 4]
                    .chunks_exact_mut(4)
                    .zip(yy.chunks_exact(2))
                    .zip(u.chunks_exact(2))
                    .zip(v.chunks_exact(2))
                {
                    let yy = u32::from(u16::from_le_bytes([yy[0], yy[1]]));
                    let u = u32::from(u16::from_le_bytes([u[0], u[1]]));
                    let v = u32::from(u16::from_le_bytes([v[0], v[1]]));
                    dst.copy_from_slice(&(u | (yy << 10) | (v << 20) | 0xc0000000).to_le_bytes());
                }
            }
        }
        _ => return Err(PackError::Unsupported),
    }
    Ok(PackedPicture {
        width: w as u32,
        height: h as u32,
        coded_width: cw as u32,
        coded_height: ch as u32,
        format: match (half, depth) {
            (true, 8) => PixelFormat::Nv12,
            (true, _) => PixelFormat::P010,
            (false, 8) => PixelFormat::Ayuv,
            (false, _) => PixelFormat::Y410,
        },
        data,
    })
}
