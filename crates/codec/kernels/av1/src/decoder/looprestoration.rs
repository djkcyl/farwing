#![deny(unsafe_op_in_unsafe_fn)]

use std::ffi::{c_int, c_uint};
use std::ops::Add;
use std::{cmp, iter, mem, slice};

use bitflags::bitflags;
use libc::ptrdiff_t;
use to_method::To;
use zerocopy::{AsBytes, FromBytes, FromZeroes};

use crate::decoder::align::AlignedVec64;
use crate::decoder::cpu::CpuFlags;
use crate::decoder::cursor::CursorMut;
use crate::decoder::disjoint_mut::DisjointMut;
use crate::decoder::ffi_safe::FFISafe;
#[cfg(all(feature = "asm", any(target_arch = "x86", target_arch = "x86_64")))]
use crate::decoder::include::common::bitdepth::bd_fn;
#[cfg(feature = "asm")]
use crate::decoder::include::common::bitdepth::bpc_fn;
use crate::decoder::include::common::bitdepth::{
    AsPrimitive, BitDepth, DynPixel, LeftPixelRow, ToPrimitive, BPC,
};
use crate::decoder::include::common::intops::iclip;
use crate::decoder::include::dav1d::picture::{
    FFISafeRav1dPictureDataComponentOffset, Rav1dPictureDataComponentOffset,
};
use crate::decoder::strided::Strided as _;
use crate::decoder::tables::dav1d_sgr_x_by_x;
use crate::decoder::wrap_fn_ptr::wrap_fn_ptr;

bitflags! {
    #[derive(Clone, Copy)]
    #[repr(transparent)]
    pub struct LrEdgeFlags: u8 {
        const LEFT = 1 << 0;
        const RIGHT = 1 << 1;
        const TOP = 1 << 2;
        const BOTTOM = 1 << 3;
    }
}

impl LrEdgeFlags {
    pub const fn select(&self, select: bool) -> Self {
        if select {
            *self
        } else {
            Self::empty()
        }
    }
}

#[derive(FromZeroes, FromBytes, AsBytes)]
#[repr(C)]
pub struct LooprestorationParamsSgr {
    pub s0: u32,
    pub s1: u32,
    pub w0: i16,
    pub w1: i16,
}

/// This [`zerocopy`]-based "`union`" has the same layout
/// as an actual `union` would, so it's safe to continue passing to asm,
/// but it's otherwise safe to use from Rust.
///
/// [`zerocopy`]: ::zerocopy
#[derive(Default)]
#[repr(C)]
#[repr(align(16))]
pub struct LooprestorationParams {
    /// [`Align16`] moved to [`Self`] because we can't `#[derive(`[`AsBytes`]`)]` on it due to generics.
    ///
    /// [`Align16`]: crate::decoder::align::Align16
    pub filter: [[i16; 8]; 2],
}

impl LooprestorationParams {
    pub fn sgr(&self) -> &LooprestorationParamsSgr {
        // These asserts ensure this is a no-op.
        const _: () = assert!(
            mem::size_of::<LooprestorationParams>() >= mem::size_of::<LooprestorationParamsSgr>()
        );
        let _: () = assert!(
            mem::align_of::<LooprestorationParams>() >= mem::align_of::<LooprestorationParamsSgr>()
        );
        FromBytes::ref_from_prefix(AsBytes::as_bytes(&self.filter)).unwrap()
    }

    pub fn sgr_mut(&mut self) -> &mut LooprestorationParamsSgr {
        // These asserts ensure this is a no-op.
        const _: () = assert!(
            mem::size_of::<LooprestorationParams>() >= mem::size_of::<LooprestorationParamsSgr>()
        );
        const _: () = assert!(
            mem::align_of::<LooprestorationParams>() >= mem::align_of::<LooprestorationParamsSgr>()
        );
        FromBytes::mut_from_prefix(AsBytes::as_bytes_mut(&mut self.filter)).unwrap()
    }
}

wrap_fn_ptr!(pub unsafe extern "C" fn loop_restoration_filter(
    dst_ptr: *mut DynPixel,
    dst_stride: ptrdiff_t,
    left: *const LeftPixelRow<DynPixel>,
    lpf_ptr: *const DynPixel,
    w: c_int,
    h: c_int,
    params: &LooprestorationParams,
    edges: LrEdgeFlags,
    bitdepth_max: c_int,
    _dst: FFISafeRav1dPictureDataComponentOffset,
    _lpf: *const FFISafe<DisjointMut<AlignedVec64<u8>>>,
) -> ());

impl loop_restoration_filter::Fn {
    /// Although the spec applies restoration filters over 4x4 blocks,
    /// they can be applied to a bigger surface.
    ///
    /// * `w` is constrained by the restoration unit size (`w <= 256`).
    /// * `h` is constrained by the stripe height (`h <= 64`).
    ///
    /// The filter functions are allowed to do
    /// aligned writes past the right edge of the buffer,
    /// aligned up to the minimum loop restoration unit size
    /// (which is 32 pixels for subsampled chroma and 64 pixels for luma).
    pub fn call<BD: BitDepth>(
        &self,
        dst: Rav1dPictureDataComponentOffset,
        left: &[LeftPixelRow<BD::Pixel>],
        lpf: &DisjointMut<AlignedVec64<u8>>,
        lpf_off: isize,
        w: c_int,
        h: c_int,
        params: &LooprestorationParams,
        edges: LrEdgeFlags,
        bd: BD,
    ) {
        let dst_ptr = dst.as_mut_ptr::<BD>().cast();
        let dst_stride = dst.stride();
        let left = left[..h as usize].as_ptr().cast();
        // NOTE: The calculated pointer may point to before the beginning of
        // `lpf`, so we must use `.wrapping_offset` here. `.wrapping_offset` is
        // needed since `.offset` requires the pointer to be in bounds, which
        // `.wrapping_offset` does not, and delays that requirement to when the
        // pointer is dereferenced.
        let lpf_ptr = lpf
            .as_mut_ptr()
            .cast::<BD::Pixel>()
            .wrapping_offset(lpf_off)
            .cast();
        let bd = bd.into_c();
        let dst = dst.into_ffi_safe();
        let lpf = FFISafe::new(lpf);
        // SAFETY: Fallbacks `fn wiener_rust`, `fn sgr_{3x3,5x5,mix}_rust` are safe; asm is supposed to do the same.
        unsafe {
            self.get()(
                dst_ptr, dst_stride, left, lpf_ptr, w, h, params, edges, bd, dst, lpf,
            )
        }
    }
}

pub struct Rav1dLoopRestorationDSPContext {
    pub wiener: [loop_restoration_filter::Fn; 2],
    pub sgr: [loop_restoration_filter::Fn; 3],
}

const REST_UNIT_STRIDE: usize = 256 * 3 / 2 + 3 + 3;

// TODO Reuse p when no padding is needed (add and remove lpf pixels in p)
// TODO Chroma only requires 2 rows of padding.
#[inline(never)]
fn padding<BD: BitDepth>(
    dst: &mut [BD::Pixel; (64 + 3 + 3) * REST_UNIT_STRIDE],
    p: Rav1dPictureDataComponentOffset,
    left: &[LeftPixelRow<BD::Pixel>],
    lpf: &DisjointMut<AlignedVec64<u8>>,
    lpf_off: isize,
    unit_w: usize,
    stripe_h: usize,
    edges: LrEdgeFlags,
) {
    let left = &left[..stripe_h];
    assert!(stripe_h > 0);
    let stride = p.pixel_stride::<BD>();

    let [have_left, have_right, have_top, have_bottom] = [
        LrEdgeFlags::LEFT,
        LrEdgeFlags::RIGHT,
        LrEdgeFlags::TOP,
        LrEdgeFlags::BOTTOM,
    ]
    .map(|lr_have| edges.contains(lr_have));
    let [have_left_3, have_right_3] = [have_left, have_right].map(|have| 3 * have as usize);

    // Copy more pixels if we don't have to pad them
    let unit_w = unit_w + have_left_3 + have_right_3;
    let dst_l = &mut dst[3 - have_left_3..];
    let p = p - have_left_3;
    let lpf_off = lpf_off - (have_left_3 as isize);
    let abs_stride = stride.unsigned_abs();

    if have_top {
        // Copy previous loop filtered rows
        let lpf_guard;
        let (above_1, above_2) = if stride < 0 {
            lpf_guard = lpf
                .slice_as::<_, BD::Pixel>(((lpf_off + stride) as usize.., ..abs_stride + unit_w));
            let above_2 = &*lpf_guard;
            let above_1 = &above_2[abs_stride..];
            (above_1, above_2)
        } else {
            lpf_guard = lpf.slice_as((lpf_off as usize.., ..abs_stride + unit_w));
            let above_1 = &*lpf_guard;
            let above_2 = &above_1[abs_stride..];
            (above_1, above_2)
        };
        BD::pixel_copy(dst_l, above_1, unit_w);
        BD::pixel_copy(&mut dst_l[REST_UNIT_STRIDE..], above_1, unit_w);
        BD::pixel_copy(&mut dst_l[2 * REST_UNIT_STRIDE..], above_2, unit_w);
    } else {
        // Pad with first row
        let p = &*p.slice::<BD>(unit_w);
        BD::pixel_copy(dst_l, p, unit_w);
        BD::pixel_copy(&mut dst_l[REST_UNIT_STRIDE..], p, unit_w);
        BD::pixel_copy(&mut dst_l[2 * REST_UNIT_STRIDE..], p, unit_w);
        if have_left {
            let left = &left[0][1..];
            BD::pixel_copy(dst_l, left, 3);
            BD::pixel_copy(&mut dst_l[REST_UNIT_STRIDE..], left, left.len());
            BD::pixel_copy(&mut dst_l[2 * REST_UNIT_STRIDE..], left, left.len());
        }
    }

    let dst_tl = &mut dst_l[3 * REST_UNIT_STRIDE..];
    if have_bottom {
        // Copy next loop filtered rows
        let offset = lpf_off + (6 + if stride < 0 { 1 } else { 0 }) * stride;
        let lpf = &*lpf.slice_as((offset as usize.., ..abs_stride + unit_w));
        let (below_1, below_2) = if stride < 0 {
            (&lpf[abs_stride..], lpf)
        } else {
            (lpf, &lpf[abs_stride..])
        };
        BD::pixel_copy(&mut dst_tl[stripe_h * REST_UNIT_STRIDE..], below_1, unit_w);
        BD::pixel_copy(
            &mut dst_tl[(stripe_h + 1) * REST_UNIT_STRIDE..],
            below_2,
            unit_w,
        );
        BD::pixel_copy(
            &mut dst_tl[(stripe_h + 2) * REST_UNIT_STRIDE..],
            below_2,
            unit_w,
        );
    } else {
        // Pad with last row
        let src = p + ((stripe_h - 1) as isize * stride);
        let src = &*src.slice::<BD>(unit_w);
        BD::pixel_copy(&mut dst_tl[stripe_h * REST_UNIT_STRIDE..], src, unit_w);
        BD::pixel_copy(
            &mut dst_tl[(stripe_h + 1) * REST_UNIT_STRIDE..],
            src,
            unit_w,
        );
        BD::pixel_copy(
            &mut dst_tl[(stripe_h + 2) * REST_UNIT_STRIDE..],
            src,
            unit_w,
        );
        if have_left {
            let left = &left[stripe_h - 1][1..];
            BD::pixel_copy(&mut dst_tl[stripe_h * REST_UNIT_STRIDE..], left, left.len());
            BD::pixel_copy(
                &mut dst_tl[(stripe_h + 1) * REST_UNIT_STRIDE..],
                left,
                left.len(),
            );
            BD::pixel_copy(
                &mut dst_tl[(stripe_h + 2) * REST_UNIT_STRIDE..],
                left,
                left.len(),
            );
        }
    }

    // Inner UNIT_WxSTRIPE_H
    let len = unit_w - have_left_3;
    for j in 0..stripe_h {
        let p = p + have_left_3 + (j as isize * stride);
        BD::pixel_copy(
            &mut dst_tl[j * REST_UNIT_STRIDE + have_left_3..],
            &p.slice::<BD>(len),
            len,
        );
    }

    if !have_right {
        // Pad 3x(STRIPE_H+6) with last column
        for j in 0..stripe_h + 6 {
            let row_last = dst_l[(unit_w - 1) + j * REST_UNIT_STRIDE];
            let pad = &mut dst_l[unit_w + j * REST_UNIT_STRIDE..];
            BD::pixel_set(pad, row_last, 3);
        }
    }

    if !have_left {
        // Pad 3x(STRIPE_H+6) with first column
        for j in 0..stripe_h + 6 {
            let offset = j * REST_UNIT_STRIDE;
            // This would be `dst_l[offset]` in C,
            // but that results in multiple mutable borrows of `dst`,
            // so we recalculate `dst_l` here.
            // `3 * (have_left == 0) as c_int` simplifies to `3 * 1` and then `3`.
            let val = dst[3 + offset];
            BD::pixel_set(&mut dst[offset..], val, 3);
        }
    } else {
        let dst = &mut dst[3 * REST_UNIT_STRIDE..];
        for j in 0..stripe_h {
            BD::pixel_copy(&mut dst[j * REST_UNIT_STRIDE..], &left[j][1..], 3);
        }
    };
}

/// Calculates the offset between `lpf` and `ptr`.
///
/// This behaves like [`offset_from`], but allows for `ptr` to point to outside
/// the allocation of `lpf`. This is necessary because `ptr` may point to before
/// the beginning of `lpf`, which violates the safety conditions of
/// [`offset_from`].
///
/// [`offset_from`]: https://doc.rust-lang.org/stable/std/primitive.pointer.html#method.offset_from
fn reconstruct_lpf_offset<BD: BitDepth>(
    lpf: &DisjointMut<AlignedVec64<u8>>,
    ptr: *const BD::Pixel,
) -> isize {
    let base = lpf.as_mut_ptr().cast::<BD::Pixel>();
    (ptr as isize - base as isize) / (mem::size_of::<BD::Pixel>() as isize)
}

/// # Safety
///
/// Must be called by [`loop_restoration_filter::Fn::call`].
#[deny(unsafe_op_in_unsafe_fn)]
unsafe extern "C" fn wiener_c_erased<BD: BitDepth>(
    _p_ptr: *mut DynPixel,
    _stride: ptrdiff_t,
    left: *const LeftPixelRow<DynPixel>,
    lpf_ptr: *const DynPixel,
    w: c_int,
    h: c_int,
    params: &LooprestorationParams,
    edges: LrEdgeFlags,
    bitdepth_max: c_int,
    p: FFISafeRav1dPictureDataComponentOffset,
    lpf: *const FFISafe<DisjointMut<AlignedVec64<u8>>>,
) {
    // SAFETY: Was passed as `WithOffset::into_ffi_safe(_)` in `loop_restoration_filter::Fn::call`.
    let p = unsafe { FFISafe::from_with_offset(p) };
    let left = left.cast();
    // SAFETY: Was passed as `FFISafe::new(_)` in `loop_restoration_filter::Fn::call`.
    let lpf = unsafe { FFISafe::get(lpf) };
    let lpf_ptr = lpf_ptr.cast();
    let lpf_off = reconstruct_lpf_offset::<BD>(lpf, lpf_ptr);
    let bd = BD::from_c(bitdepth_max);
    let w = w as usize;
    let h = h as usize;
    // SAFETY: Length sliced in `loop_restoration_filter::Fn::call`.
    let left = unsafe { slice::from_raw_parts(left, h) };
    wiener_rust(p, left, lpf, lpf_off, w, h, params, edges, bd)
}

// FIXME Could split into luma and chroma specific functions,
// (since first and last tops are always 0 for chroma)
// FIXME Could implement a version that requires less temporary memory
// (should be possible to implement with only 6 rows of temp storage)
fn wiener_rust<BD: BitDepth>(
    p: Rav1dPictureDataComponentOffset,
    left: &[LeftPixelRow<BD::Pixel>],
    lpf: &DisjointMut<AlignedVec64<u8>>,
    lpf_off: isize,
    w: usize,
    h: usize,
    params: &LooprestorationParams,
    edges: LrEdgeFlags,
    bd: BD,
) {
    // Wiener filtering is applied to a maximum stripe height of 64 + 3 pixels
    // of padding above and below
    let mut tmp = [0.into(); (64 + 3 + 3) * REST_UNIT_STRIDE];

    padding::<BD>(&mut tmp, p, left, lpf, lpf_off, w, h, edges);

    // Values stored between horizontal and vertical filtering don't
    // fit in a u8.
    let mut hor = [0; (64 + 3 + 3) * REST_UNIT_STRIDE];

    let filter = &params.filter;
    let bitdepth = bd.bitdepth().as_::<c_int>();
    let round_bits_h = 3 + (bitdepth == 12) as c_int * 2;
    let rounding_off_h = 1 << round_bits_h - 1;
    let clip_limit = 1 << bitdepth + 1 + 7 - round_bits_h;
    for (tmp, hor) in tmp
        .chunks_exact(REST_UNIT_STRIDE)
        .zip(hor.chunks_exact_mut(REST_UNIT_STRIDE))
        .take(h + 6)
    {
        for i in 0..w {
            let mut sum = 1 << bitdepth + 6;

            if BD::BPC == BPC::BPC8 {
                sum += tmp[i + 3].to::<i32>() * 128;
            }

            for (&tmp, &filter) in iter::zip(&tmp[i..i + 7], &filter[0][..7]) {
                sum += tmp.to::<i32>() * filter as c_int;
            }

            hor[i] = iclip(sum + rounding_off_h >> round_bits_h, 0, clip_limit - 1) as u16;
        }
    }

    let round_bits_v = 11 - (bitdepth == 12) as c_int * 2;
    let rounding_off_v = 1 << round_bits_v - 1;
    let round_offset = 1 << bitdepth + (round_bits_v - 1);
    for j in 0..h {
        for i in 0..w {
            let mut sum = -round_offset;
            let z = &hor[j * REST_UNIT_STRIDE + i..(j + 7) * REST_UNIT_STRIDE];

            for k in 0..7 {
                sum += z[k * REST_UNIT_STRIDE] as c_int * filter[1][k] as c_int;
            }

            let p = p + (j as isize * p.pixel_stride::<BD>()) + i;
            *p.index_mut::<BD>() =
                iclip(sum + rounding_off_v >> round_bits_v, 0, bd.into_c()).as_();
        }
    }
}

/// Sum over a 3x3 area
///
/// The `dst` and `src` pointers are positioned 3 pixels above and 3 pixels to the
/// left of the top left corner. However, the self guided filter only needs 1
/// pixel above and one pixel to the left. As for the pixels below and to the
/// right they must be computed in the sums, but don't need to be stored.
///
/// Example for a 4x4 block:
///
/// ```text
/// x x x x x x x x x x
/// x c c c c c c c c x
/// x i s s s s s s i x
/// x i s s s s s s i x
/// x i s s s s s s i x
/// x i s s s s s s i x
/// x i s s s s s s i x
/// x i s s s s s s i x
/// x c c c c c c c c x
/// x x x x x x x x x x
/// ```
///
/// * s: Pixel summed and stored
/// * i: Pixel summed and stored (between loops)
/// * c: Pixel summed not stored
/// * x: Pixel not summed not stored
fn boxsum3<BD: BitDepth>(
    sumsq: &mut [i32; (64 + 2 + 2) * REST_UNIT_STRIDE],
    sum: &mut [BD::Coef; (64 + 2 + 2) * REST_UNIT_STRIDE],
    src: &[BD::Pixel; (64 + 3 + 3) * REST_UNIT_STRIDE],
    w: usize,
    h: usize,
) {
    // We skip the first row, as it is never used
    let src = &src[REST_UNIT_STRIDE..];

    // We skip the first and last columns, as they are never used
    for x in 1..w - 1 {
        let mut sum_v = &mut sum[x..];
        let mut sumsq_v = &mut sumsq[x..];
        let mut s = &src[x..];
        let mut a: c_int = s[0].as_();
        let mut a2 = a * a;
        let mut b: c_int = s[REST_UNIT_STRIDE].as_();
        let mut b2 = b * b;

        // We skip the first 2 rows, as they are skipped in the next loop and
        // we don't need the last 2 row as it is skipped in the next loop
        for _ in 2..h - 2 {
            s = &s[REST_UNIT_STRIDE..];
            let c: c_int = s[REST_UNIT_STRIDE].as_();
            let c2 = c * c;
            sum_v = &mut sum_v[REST_UNIT_STRIDE..];
            sumsq_v = &mut sumsq_v[REST_UNIT_STRIDE..];
            sum_v[0] = (a + b + c).as_();
            sumsq_v[0] = a2 + b2 + c2;
            a = b;
            a2 = b2;
            b = c;
            b2 = c2;
        }
    }

    // We skip the first row as it is never read
    let mut sum = &mut sum[REST_UNIT_STRIDE..];
    let mut sumsq = &mut sumsq[REST_UNIT_STRIDE..];

    // We skip the last 2 rows as it is never read
    for _ in 2..h - 2 {
        let mut a = sum[1];
        let mut a2 = sumsq[1];
        let mut b = sum[2];
        let mut b2 = sumsq[2];

        // We don't store the first column as it is never read and
        // we don't store the last 2 columns as they are never read
        for x in 2..w - 2 {
            let c = sum[x + 1];
            let c2 = sumsq[x + 1];
            sum[x] = a + b + c;
            sumsq[x] = a2 + b2 + c2;
            a = b;
            a2 = b2;
            b = c;
            b2 = c2;
        }

        sum = &mut sum[REST_UNIT_STRIDE..];
        sumsq = &mut sumsq[REST_UNIT_STRIDE..];
    }
}

/// Sum over a 5x5 area
///
/// The `dst` and `src` pointers are positioned 3 pixels above and 3 pixels to the
/// left of the top left corner. However, the self guided filter only needs 1
/// pixel above and one pixel to the left. As for the pixels below and to the
/// right they must be computed in the sums, but don't need to be stored.
///
/// Example for a 4x4 block:
///
/// ```text
/// c c c c c c c c c c
/// c c c c c c c c c c
/// i i s s s s s s i i
/// i i s s s s s s i i
/// i i s s s s s s i i
/// i i s s s s s s i i
/// i i s s s s s s i i
/// i i s s s s s s i i
/// c c c c c c c c c c
/// c c c c c c c c c c
/// ```
///
/// * s: Pixel summed and stored
/// * i: Pixel summed and stored (between loops)
/// * c: Pixel summed not stored
/// * x: Pixel not summed not stored
fn boxsum5<BD: BitDepth>(
    sumsq: &mut [i32; (64 + 2 + 2) * REST_UNIT_STRIDE],
    sum: &mut [BD::Coef; (64 + 2 + 2) * REST_UNIT_STRIDE],
    src: &[BD::Pixel; (64 + 3 + 3) * REST_UNIT_STRIDE],
    w: usize,
    h: usize,
) {
    for x in 0..w {
        let mut sum_v = &mut sum[x..];
        let mut sumsq_v = &mut sumsq[x..];
        let s = &src[x..];
        let mut a: c_int = (s[0]).as_();
        let mut a2 = a * a;
        let mut b: c_int = (s[1 * REST_UNIT_STRIDE]).as_();
        let mut b2 = b * b;
        let mut c: c_int = (s[2 * REST_UNIT_STRIDE]).as_();
        let mut c2 = c * c;
        let mut d: c_int = (s[3 * REST_UNIT_STRIDE]).as_();
        let mut d2 = d * d;

        let mut s = &src[3 * REST_UNIT_STRIDE + x..];

        // We skip the first 2 rows, as they are skipped in the next loop and
        // we don't need the last 2 row as it is skipped in the next loop
        for _ in 2..h - 2 {
            s = &s[REST_UNIT_STRIDE..];
            let e: c_int = s[0].as_();
            let e2 = e * e;
            sum_v = &mut sum_v[REST_UNIT_STRIDE..];
            sumsq_v = &mut sumsq_v[REST_UNIT_STRIDE..];
            sum_v[0] = (a + b + c + d + e).as_();
            sumsq_v[0] = a2 + b2 + c2 + d2 + e2;
            a = b;
            b = c;
            c = d;
            d = e;
            a2 = b2;
            b2 = c2;
            c2 = d2;
            d2 = e2;
        }
    }

    // We skip the first row as it is never read
    let mut sum = &mut sum[REST_UNIT_STRIDE..];
    let mut sumsq = &mut sumsq[REST_UNIT_STRIDE..];
    for _ in 2..h - 2 {
        let mut a = sum[0];
        let mut a2 = sumsq[0];
        let mut b = sum[1];
        let mut b2 = sumsq[1];
        let mut c = sum[2];
        let mut c2 = sumsq[2];
        let mut d = sum[3];
        let mut d2 = sumsq[3];

        for x in 2..w - 2 {
            let e = sum[x + 2];
            let e2 = sumsq[x + 2];
            sum[x] = a + b + c + d + e;
            sumsq[x] = a2 + b2 + c2 + d2 + e2;
            a = b;
            b = c;
            c = d;
            d = e;
            a2 = b2;
            b2 = c2;
            c2 = d2;
            d2 = e2;
        }
        sum = &mut sum[REST_UNIT_STRIDE..];
        sumsq = &mut sumsq[REST_UNIT_STRIDE..];
    }
}

#[inline(never)]
fn selfguided_filter<BD: BitDepth>(
    dst: &mut [BD::Coef; 64 * 384],
    src: &[BD::Pixel; (64 + 3 + 3) * REST_UNIT_STRIDE],
    w: usize,
    h: usize,
    n: c_int,
    s: c_uint,
    bd: BD,
) {
    let sgr_one_by_x = if n == 25 { 164 } else { 455 };

    // Selfguided filter is applied to a maximum stripe height of 64 + 3 pixels
    // of padding above and below
    let mut sumsq = [0; (64 + 2 + 2) * REST_UNIT_STRIDE];
    // By inverting `a` and `b` after the boxsums, `b` can be of `BD::Coef` instead of `i32`.
    let mut sum = [0.as_::<BD::Coef>(); (64 + 2 + 2) * REST_UNIT_STRIDE];

    let step = (n == 25) as usize + 1;
    if n == 25 {
        boxsum5::<BD>(&mut sumsq, &mut sum, src, w + 6, h + 6);
    } else {
        boxsum3::<BD>(&mut sumsq, &mut sum, src, w + 6, h + 6);
    }
    let bitdepth_min_8 = bd.bitdepth() - 8;

    let mut a = CursorMut::new(&mut sumsq) + 2 * REST_UNIT_STRIDE + 3;
    let mut b = CursorMut::new(&mut sum) + 2 * REST_UNIT_STRIDE + 3;

    let mut aa = a.clone() - REST_UNIT_STRIDE;
    let mut bb = b.clone() - REST_UNIT_STRIDE;
    for _ in (-1..h as isize + 1).step_by(step) {
        for i in -1..w as isize + 1 {
            let a = aa[i] + (1 << 2 * bitdepth_min_8 >> 1) >> 2 * bitdepth_min_8;
            let b = bb[i].as_::<c_int>() + (1 << bitdepth_min_8 >> 1) >> bitdepth_min_8;

            let p = cmp::max(a * n - b * b, 0) as c_uint;
            let z = (p * s + (1 << 19)) >> 20;
            let x = dav1d_sgr_x_by_x[cmp::min(z, 255) as usize] as c_uint;

            // This is where we invert A and B, so that B is of size coef.
            aa[i] = ((x * bb[i].as_::<c_uint>() * sgr_one_by_x + (1 << 11)) >> 12) as c_int;
            bb[i] = x.as_::<BD::Coef>();
        }
        aa += step as usize * REST_UNIT_STRIDE;
        bb += step as usize * REST_UNIT_STRIDE;
    }

    fn six_neighbors<P>(p: &CursorMut<P>, i: isize) -> c_int
    where
        P: Add<Output = P> + ToPrimitive<c_int> + Copy,
    {
        let stride = REST_UNIT_STRIDE as isize;
        (p[i - stride] + p[i + stride]).as_::<c_int>() * 6
            + (p[i - 1 - stride] + p[i - 1 + stride] + p[i + 1 - stride] + p[i + 1 + stride])
                .as_::<c_int>()
                * 5
    }

    fn eight_neighbors<P>(p: &CursorMut<P>, i: isize) -> c_int
    where
        P: Add<Output = P> + ToPrimitive<c_int> + Copy,
    {
        let stride = REST_UNIT_STRIDE as isize;
        (p[i] + p[i - 1] + p[i + 1] + p[i - stride] + p[i + stride]).as_::<c_int>() * 4
            + (p[i - 1 - stride] + p[i - 1 + stride] + p[i + 1 - stride] + p[i + 1 + stride])
                .as_::<c_int>()
                * 3
    }

    const MAX_RESTORATION_WIDTH: usize = 256 * 3 / 2;

    let mut src = &src[3 * REST_UNIT_STRIDE + 3..];
    let mut dst = dst.as_mut_slice();
    if n == 25 {
        let mut j = 0;
        while j < h - 1 {
            for i in 0..w {
                let (a, b) = (six_neighbors(&b, i as isize), six_neighbors(&a, i as isize));
                dst[i] = ((b - a * src[i].as_::<c_int>() + (1 << 8)) >> 9).as_();
            }
            dst = &mut dst[MAX_RESTORATION_WIDTH..];
            src = &src[REST_UNIT_STRIDE..];
            b += REST_UNIT_STRIDE;
            a += REST_UNIT_STRIDE;
            for i in 0..w {
                let (a, b) = (
                    b[i].as_::<c_int>() * 6 + (b[i as isize - 1] + b[i + 1]).as_::<c_int>() * 5,
                    a[i] * 6 + (a[i as isize - 1] + a[i + 1]) * 5,
                );
                dst[i] = (b - a * src[i].as_::<c_int>() + (1 << 7) >> 8).as_();
            }
            dst = &mut dst[MAX_RESTORATION_WIDTH..];
            src = &src[REST_UNIT_STRIDE..];
            b += REST_UNIT_STRIDE;
            a += REST_UNIT_STRIDE;
            j += 2;
        }
        // Last row, when number of rows is odd
        if j + 1 == h {
            for i in 0..w {
                let (a, b) = (six_neighbors(&b, i as isize), six_neighbors(&a, i as isize));
                dst[i] = (b - a * src[i].as_::<c_int>() + (1 << 8) >> 9).as_();
            }
        }
    } else {
        for _ in 0..h {
            for i in 0..w {
                let (a, b) = (
                    eight_neighbors(&b, i as isize),
                    eight_neighbors(&a, i as isize),
                );
                dst[i] = (b - a * src[i].as_::<c_int>() + (1 << 8) >> 9).as_();
            }
            dst = &mut dst[384..];
            src = &src[REST_UNIT_STRIDE..];
            b += REST_UNIT_STRIDE;
            a += REST_UNIT_STRIDE;
        }
    };
}

/// # Safety
///
/// Must be called by [`loop_restoration_filter::Fn::call`].
#[deny(unsafe_op_in_unsafe_fn)]
unsafe extern "C" fn sgr_5x5_c_erased<BD: BitDepth>(
    _p_ptr: *mut DynPixel,
    _stride: ptrdiff_t,
    left: *const LeftPixelRow<DynPixel>,
    lpf_ptr: *const DynPixel,
    w: c_int,
    h: c_int,
    params: &LooprestorationParams,
    edges: LrEdgeFlags,
    bitdepth_max: c_int,
    p: FFISafeRav1dPictureDataComponentOffset,
    lpf: *const FFISafe<DisjointMut<AlignedVec64<u8>>>,
) {
    // SAFETY: Was passed as `WithOffset::into_ffi_safe(_)` in `loop_restoration_filter::Fn::call`.
    let p = unsafe { FFISafe::from_with_offset(p) };
    let left = left.cast();
    // SAFETY: Was passed as `FFISafe::new(_)` in `loop_restoration_filter::Fn::call`.
    let lpf = unsafe { FFISafe::get(lpf) };
    let lpf_ptr = lpf_ptr.cast();
    let lpf_off = reconstruct_lpf_offset::<BD>(lpf, lpf_ptr);
    let w = w as usize;
    let h = h as usize;
    let bd = BD::from_c(bitdepth_max);
    // SAFETY: Length sliced in `loop_restoration_filter::Fn::call`.
    let left = unsafe { slice::from_raw_parts(left, h) };
    sgr_5x5_rust(p, left, lpf, lpf_off, w, h, params, edges, bd)
}

fn sgr_5x5_rust<BD: BitDepth>(
    p: Rav1dPictureDataComponentOffset,
    left: &[LeftPixelRow<BD::Pixel>],
    lpf: &DisjointMut<AlignedVec64<u8>>,
    lpf_off: isize,
    w: usize,
    h: usize,
    params: &LooprestorationParams,
    edges: LrEdgeFlags,
    bd: BD,
) {
    // Selfguided filter is applied to a maximum stripe height of 64 + 3 pixels
    // of padding above and below
    let mut tmp = [0.as_(); (64 + 3 + 3) * REST_UNIT_STRIDE];

    // Selfguided filter outputs to a maximum stripe height of 64 and a
    // maximum restoration width of 384 (256 * 1.5)
    let mut dst = [0.as_(); 64 * 384];

    padding::<BD>(&mut tmp, p, left, lpf, lpf_off, w, h, edges);
    let sgr = params.sgr();
    selfguided_filter(&mut dst, &mut tmp, w, h, 25, sgr.s0, bd);

    let w0 = sgr.w0 as c_int;
    for j in 0..h {
        let p = p + (j as isize * p.pixel_stride::<BD>());
        let p = &mut *p.slice_mut::<BD>(w);
        for i in 0..w {
            let v = w0 * dst[j * 384 + i].as_::<c_int>();
            p[i] = bd.iclip_pixel(p[i].as_::<c_int>() + (v + (1 << 10) >> 11));
        }
    }
}

/// # Safety
///
/// Must be called by [`loop_restoration_filter::Fn::call`].
#[deny(unsafe_op_in_unsafe_fn)]
unsafe extern "C" fn sgr_3x3_c_erased<BD: BitDepth>(
    _p_ptr: *mut DynPixel,
    _stride: ptrdiff_t,
    left: *const LeftPixelRow<DynPixel>,
    lpf_ptr: *const DynPixel,
    w: c_int,
    h: c_int,
    params: &LooprestorationParams,
    edges: LrEdgeFlags,
    bitdepth_max: c_int,
    p: FFISafeRav1dPictureDataComponentOffset,
    lpf: *const FFISafe<DisjointMut<AlignedVec64<u8>>>,
) {
    // SAFETY: Was passed as `WithOffset::into_ffi_safe(_)` in `loop_restoration_filter::Fn::call`.
    let p = unsafe { FFISafe::from_with_offset(p) };
    let left = left.cast();
    // SAFETY: Was passed as `FFISafe::new(_)` in `loop_restoration_filter::Fn::call`.
    let lpf = unsafe { FFISafe::get(lpf) };
    let lpf_ptr = lpf_ptr.cast();
    let lpf_off = reconstruct_lpf_offset::<BD>(lpf, lpf_ptr);
    let w = w as usize;
    let h = h as usize;
    let bd = BD::from_c(bitdepth_max);
    // SAFETY: Length sliced in `loop_restoration_filter::Fn::call`.
    let left = unsafe { slice::from_raw_parts(left, h) };
    sgr_3x3_rust(p, left, lpf, lpf_off, w, h, params, edges, bd)
}

fn sgr_3x3_rust<BD: BitDepth>(
    p: Rav1dPictureDataComponentOffset,
    left: &[LeftPixelRow<BD::Pixel>],
    lpf: &DisjointMut<AlignedVec64<u8>>,
    lpf_off: isize,
    w: usize,
    h: usize,
    params: &LooprestorationParams,
    edges: LrEdgeFlags,
    bd: BD,
) {
    let mut tmp = [0.as_(); (64 + 3 + 3) * REST_UNIT_STRIDE];
    let mut dst = [0.as_(); 64 * 384];

    padding::<BD>(&mut tmp, p, left, lpf, lpf_off, w, h, edges);
    let sgr = params.sgr();
    selfguided_filter(&mut dst, &mut tmp, w, h, 9, sgr.s1, bd);

    let w1 = sgr.w1 as c_int;
    for j in 0..h {
        let p = p + (j as isize * p.pixel_stride::<BD>());
        let p = &mut *p.slice_mut::<BD>(w);
        for i in 0..w {
            let v = w1 * dst[j * 384 + i].as_::<c_int>();
            p[i] = bd.iclip_pixel(p[i].as_::<c_int>() + (v + (1 << 10) >> 11));
        }
    }
}

/// # Safety
///
/// Must be called by [`loop_restoration_filter::Fn::call`].
#[deny(unsafe_op_in_unsafe_fn)]
unsafe extern "C" fn sgr_mix_c_erased<BD: BitDepth>(
    _p_ptr: *mut DynPixel,
    _stride: ptrdiff_t,
    left: *const LeftPixelRow<DynPixel>,
    lpf_ptr: *const DynPixel,
    w: c_int,
    h: c_int,
    params: &LooprestorationParams,
    edges: LrEdgeFlags,
    bitdepth_max: c_int,
    p: FFISafeRav1dPictureDataComponentOffset,
    lpf: *const FFISafe<DisjointMut<AlignedVec64<u8>>>,
) {
    // SAFETY: Was passed as `WithOffset::into_ffi_safe(_)` in `loop_restoration_filter::Fn::call`.
    let p = unsafe { FFISafe::from_with_offset(p) };
    let left = left.cast();
    // SAFETY: Was passed as `FFISafe::new(_)` in `loop_restoration_filter::Fn::call`.
    let lpf = unsafe { FFISafe::get(lpf) };
    let lpf_ptr = lpf_ptr.cast();
    let lpf_off = reconstruct_lpf_offset::<BD>(lpf, lpf_ptr);
    let w = w as usize;
    let h = h as usize;
    let bd = BD::from_c(bitdepth_max);
    // SAFETY: Length sliced in `loop_restoration_filter::Fn::call`.
    let left = unsafe { slice::from_raw_parts(left, h) };
    sgr_mix_rust(p, left, lpf, lpf_off, w, h, params, edges, bd)
}

fn sgr_mix_rust<BD: BitDepth>(
    p: Rav1dPictureDataComponentOffset,
    left: &[LeftPixelRow<BD::Pixel>],
    lpf: &DisjointMut<AlignedVec64<u8>>,
    lpf_off: isize,
    w: usize,
    h: usize,
    params: &LooprestorationParams,
    edges: LrEdgeFlags,
    bd: BD,
) {
    let mut tmp = [0.as_(); (64 + 3 + 3) * REST_UNIT_STRIDE];
    let mut dst0 = [0.as_(); 64 * 384];
    let mut dst1 = [0.as_(); 64 * 384];

    padding::<BD>(&mut tmp, p, left, lpf, lpf_off, w, h, edges);
    let sgr = params.sgr();
    selfguided_filter(&mut dst0, &mut tmp, w, h, 25, sgr.s0, bd);
    selfguided_filter(&mut dst1, &mut tmp, w, h, 9, sgr.s1, bd);

    let w0 = sgr.w0 as c_int;
    let w1 = sgr.w1 as c_int;
    for j in 0..h {
        let p = p + (j as isize * p.pixel_stride::<BD>());
        let p = &mut *p.slice_mut::<BD>(w);
        for i in 0..w {
            let v = w0 * dst0[j * 384 + i].as_::<c_int>() + w1 * dst1[j * 384 + i].as_::<c_int>();
            p[i] = bd.iclip_pixel(p[i].as_::<c_int>() + (v + (1 << 10) >> 11));
        }
    }
}

impl Rav1dLoopRestorationDSPContext {
    pub const fn default<BD: BitDepth>() -> Self {
        Self {
            wiener: [loop_restoration_filter::Fn::new(wiener_c_erased::<BD>); 2],
            sgr: [
                loop_restoration_filter::Fn::new(sgr_5x5_c_erased::<BD>),
                loop_restoration_filter::Fn::new(sgr_3x3_c_erased::<BD>),
                loop_restoration_filter::Fn::new(sgr_mix_c_erased::<BD>),
            ],
        }
    }

    #[cfg(feature = "asm")]
    #[inline(always)]
    const fn init_x86<BD: BitDepth>(mut self, flags: CpuFlags, bpc: u8) -> Self {
        if !flags.contains(CpuFlags::SSE2) {
            return self;
        }

        if let BPC::BPC8 = BD::BPC {
            self.wiener[0] = bpc_fn!(loop_restoration_filter::decl_fn, 8 bpc, wiener_filter7, sse2);
            self.wiener[1] = bpc_fn!(loop_restoration_filter::decl_fn, 8 bpc, wiener_filter5, sse2);
        };

        if !flags.contains(CpuFlags::SSSE3) {
            return self;
        }

        self.wiener[0] = bd_fn!(loop_restoration_filter::decl_fn, BD, wiener_filter7, ssse3);
        self.wiener[1] = bd_fn!(loop_restoration_filter::decl_fn, BD, wiener_filter5, ssse3);

        if matches!(BD::BPC, BPC::BPC8) || bpc == 10 {
            self.sgr[0] = bd_fn!(loop_restoration_filter::decl_fn, BD, sgr_filter_5x5, ssse3);
            self.sgr[1] = bd_fn!(loop_restoration_filter::decl_fn, BD, sgr_filter_3x3, ssse3);
            self.sgr[2] = bd_fn!(loop_restoration_filter::decl_fn, BD, sgr_filter_mix, ssse3);
        }

        #[cfg(target_arch = "x86_64")]
        {
            if !flags.contains(CpuFlags::AVX2) {
                return self;
            }

            self.wiener[0] = bd_fn!(loop_restoration_filter::decl_fn, BD, wiener_filter7, avx2);
            self.wiener[1] = bd_fn!(loop_restoration_filter::decl_fn, BD, wiener_filter5, avx2);

            if matches!(BD::BPC, BPC::BPC8) || bpc == 10 {
                self.sgr[0] = bd_fn!(loop_restoration_filter::decl_fn, BD, sgr_filter_5x5, avx2);
                self.sgr[1] = bd_fn!(loop_restoration_filter::decl_fn, BD, sgr_filter_3x3, avx2);
                self.sgr[2] = bd_fn!(loop_restoration_filter::decl_fn, BD, sgr_filter_mix, avx2);
            }

            if !flags.contains(CpuFlags::AVX512ICL) {
                return self;
            }

            self.wiener[0] = bd_fn!(
                loop_restoration_filter::decl_fn,
                BD,
                wiener_filter7,
                avx512icl
            );
            self.wiener[1] = match BD::BPC {
                // With VNNI we don't need a 5-tap version.
                BPC::BPC8 => self.wiener[0],
                BPC::BPC16 => {
                    bpc_fn!(loop_restoration_filter::decl_fn, 16 bpc, wiener_filter5, avx512icl)
                }
            };

            if matches!(BD::BPC, BPC::BPC8) || bpc == 10 {
                self.sgr[0] = bd_fn!(
                    loop_restoration_filter::decl_fn,
                    BD,
                    sgr_filter_5x5,
                    avx512icl
                );
                self.sgr[1] = bd_fn!(
                    loop_restoration_filter::decl_fn,
                    BD,
                    sgr_filter_3x3,
                    avx512icl
                );
                self.sgr[2] = bd_fn!(
                    loop_restoration_filter::decl_fn,
                    BD,
                    sgr_filter_mix,
                    avx512icl
                );
            }
        }

        self
    }

    #[inline(always)]
    const fn init<BD: BitDepth>(self, flags: CpuFlags, bpc: u8) -> Self {
        #[cfg(feature = "asm")]
        {
            {
                return self.init_x86::<BD>(flags, bpc);
            }
        }

        #[allow(unreachable_code)] // Reachable on some #[cfg]s.
        {
            let _ = flags;
            let _ = bpc;
            self
        }
    }

    pub const fn new<BD: BitDepth>(flags: CpuFlags, bpc: u8) -> Self {
        Self::default::<BD>().init::<BD>(flags, bpc)
    }
}
