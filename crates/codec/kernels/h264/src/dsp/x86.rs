// SPDX-License-Identifier: LGPL-2.1-or-later
// Packed implementation of the FFmpeg h264dsp_template.c luma filter arithmetic.
// Original Copyright (c) 2003-2011 Michael Niedermayer; Rust port OpenUUYC.
// Only this module contains unsafe intrinsics. x86_64 guarantees SSE2.
use super::Block;
use std::arch::x86_64::*;
mod deblock;
mod idct;
mod pixel;
mod predict;
mod qpel16;
pub(super) use idct::{add_dc, add_plane, add4, add8};
pub(super) use pixel::{chroma, chroma_filter, weight, weighted_copy};
pub(super) use predict::{average3, intra_nxn, plane};

#[inline]
fn has_avx2() -> bool {
    static AVX2: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *AVX2.get_or_init(|| std::is_x86_feature_detected!("avx2"))
}

/// FFmpeg put_pixels scheduling: fixed 2/4/8/16-byte loads and stores, rather
/// than a variable-length memcpy call for every row of an integer partition.
pub(super) fn copy(src: &[u8], stride: usize, dst: &mut Block<'_>) -> bool {
    let w = dst.width;
    if !matches!(w, 2 | 4 | 8 | 16)
        || stride < w
        || (dst.height - 1)
            .checked_mul(stride)
            .and_then(|v| v.checked_add(w))
            .is_none_or(|end| end > src.len())
    {
        return false;
    }
    // Both allocations cover the complete fixed-width rectangle; no overread.
    unsafe {
        match w {
            2 => copy_width::<2>(src.as_ptr(), stride, dst),
            4 => copy_width::<4>(src.as_ptr(), stride, dst),
            8 => copy_width::<8>(src.as_ptr(), stride, dst),
            16 => copy_width::<16>(src.as_ptr(), stride, dst),
            _ => unreachable!(),
        }
    }
    true
}
#[inline]
unsafe fn copy_width<const W: usize>(src: *const u8, stride: usize, dst: &mut Block<'_>) {
    unsafe {
        for y in 0..dst.height {
            let s = src.add(y * stride);
            let d = dst.data.as_mut_ptr().add(y * dst.stride);
            match W {
                2 => d
                    .cast::<u16>()
                    .write_unaligned(s.cast::<u16>().read_unaligned()),
                4 => d
                    .cast::<u32>()
                    .write_unaligned(s.cast::<u32>().read_unaligned()),
                8 => _mm_storel_epi64(d.cast(), _mm_loadl_epi64(s.cast())),
                16 => _mm_storeu_si128(d.cast(), _mm_loadu_si128(s.cast())),
                _ => unreachable!(),
            }
        }
    }
}

#[cfg(test)]
mod motion_tests {
    use super::*;
    #[test]
    fn packed_motion_preserves_exact_short_rows_and_destination_guards() {
        let mut state = 0x47af3201u32;
        let mut random = || {
            state = state.wrapping_mul(1664525).wrapping_add(1013904223);
            (state >> 24) as u8
        };
        for w in [2, 4, 8, 16] {
            for h in [2, 4, 8, 16] {
                let src: Vec<u8> = (0..w * h).map(|_| random()).collect();
                let mut actual = [37u8; 32 * 18];
                let mut expected = actual;
                for y in 0..h {
                    expected[35 + y * 32..35 + y * 32 + w]
                        .copy_from_slice(&src[y * w..(y + 1) * w]);
                }
                assert!(copy(
                    &src,
                    w,
                    &mut Block::new(&mut actual[35..], 32, w, h).unwrap()
                ));
                assert_eq!(actual, expected);
            }
        }
        if !std::is_x86_feature_detected!("ssse3") {
            return;
        }
        for w in [4, 8, 16] {
            for h in [4, 8, 16] {
                // Minimum legal footprint: the final source row has precisely
                // W+5 bytes. The SSSE3 helper cannot rely on allocator padding.
                let stride = w + 5;
                let src: Vec<u8> = (0..stride * (h + 5)).map(|_| random()).collect();
                for fx in 0..4 {
                    for fy in 0..4 {
                        if fx | fy == 0 {
                            continue;
                        }
                        let mut actual = [37u8; 32 * 18];
                        let mut expected = actual;
                        // Dimensions, fraction and complete footprints above
                        // meet dispatch's contract; SSSE3 is checked above.
                        unsafe {
                            qpel_dispatch::<false>(
                                &src,
                                stride,
                                fx,
                                fy,
                                &mut Block::new(&mut expected[35..], 32, w, h).unwrap(),
                            );
                            qpel_dispatch::<true>(
                                &src,
                                stride,
                                fx,
                                fy,
                                &mut Block::new(&mut actual[35..], 32, w, h).unwrap(),
                            );
                        }
                        assert_eq!(actual, expected, "w={w} h={h} fx={fx} fy={fy}");
                        if w == 16 && has_avx2() {
                            let mut wide = [37u8; 32 * 18];
                            // Same minimum checked footprint; runtime AVX2
                            // guard above authorizes the sixteen-lane kernel.
                            unsafe {
                                qpel16::run(
                                    &src,
                                    stride,
                                    fx,
                                    fy,
                                    &mut Block::new(&mut wide[35..], 32, w, h).unwrap(),
                                );
                            }
                            assert_eq!(wide, expected, "AVX2 h={h} fx={fx} fy={fy}");
                        }
                    }
                }
            }
        }
    }
}

/// Borrowed source is the exact six-tap footprint, including halo. The 4-pixel
/// specialization uses 32-bit loads/stores, so short rows are never overread.
pub(super) fn qpel(src: &[u8], stride: usize, fx: u8, fy: u8, dst: &mut Block<'_>) -> bool {
    let (w, h) = (dst.width, dst.height);
    if !matches!(w, 4 | 8 | 16)
        || !matches!(h, 4 | 8 | 16)
        || fx > 3
        || fy > 3
        || fx | fy == 0
        || stride < w + 5
        || (h + 4)
            .checked_mul(stride)
            .and_then(|n| n.checked_add(w + 5))
            .is_none_or(|n| n > src.len())
    {
        return false;
    }
    static SSSE3: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    // The same checked footprint covers both ISA paths, including short rows.
    unsafe {
        if w == 16 && has_avx2() {
            qpel16::run(src, stride, fx, fy, dst);
        } else if *SSSE3.get_or_init(|| std::is_x86_feature_detected!("ssse3")) {
            qpel_ssse3(src, stride, fx, fy, dst);
        } else {
            qpel_dispatch::<false>(src, stride, fx, fy, dst);
        }
    }
    true
}
#[target_feature(enable = "ssse3")]
unsafe fn qpel_ssse3(src: &[u8], stride: usize, fx: u8, fy: u8, dst: &mut Block<'_>) {
    unsafe { qpel_dispatch::<true>(src, stride, fx, fy, dst) }
}
#[inline(always)]
unsafe fn qpel_dispatch<const FAST: bool>(
    src: &[u8],
    stride: usize,
    fx: u8,
    fy: u8,
    dst: &mut Block<'_>,
) {
    macro_rules! position {
        ($x:literal,$y:literal) => {
            match dst.width {
                4 => interpolate::<4, $x, $y, FAST>(src, stride, dst),
                8 => interpolate::<8, $x, $y, FAST>(src, stride, dst),
                16 => interpolate::<16, $x, $y, FAST>(src, stride, dst),
                _ => unreachable!(),
            }
        };
    }
    // Caller validates both allocation footprints and the CPU capability.
    unsafe {
        match (fx, fy) {
            (1, 0) => position!(1, 0),
            (2, 0) => position!(2, 0),
            (3, 0) => position!(3, 0),
            (0, 1) => position!(0, 1),
            (1, 1) => position!(1, 1),
            (2, 1) => position!(2, 1),
            (3, 1) => position!(3, 1),
            (0, 2) => position!(0, 2),
            (1, 2) => position!(1, 2),
            (2, 2) => position!(2, 2),
            (3, 2) => position!(3, 2),
            (0, 3) => position!(0, 3),
            (1, 3) => position!(1, 3),
            (2, 3) => position!(2, 3),
            (3, 3) => position!(3, 3),
            _ => unreachable!(),
        }
    }
}
#[inline]
unsafe fn pixels<const W: usize>(p: *const u8) -> __m128i {
    unsafe {
        if W == 4 {
            _mm_cvtsi32_si128(p.cast::<i32>().read_unaligned())
        } else {
            _mm_loadl_epi64(p.cast())
        }
    }
}
#[inline]
unsafe fn words<const W: usize>(p: *const u8) -> __m128i {
    unsafe { _mm_unpacklo_epi8(pixels::<W>(p), _mm_setzero_si128()) }
}
#[inline]
unsafe fn fir(a: __m128i, b: __m128i, c: __m128i, d: __m128i, e: __m128i, f: __m128i) -> __m128i {
    unsafe {
        let center = _mm_add_epi16(c, d);
        let middle = _mm_add_epi16(b, e);
        _mm_add_epi16(
            _mm_sub_epi16(
                _mm_add_epi16(_mm_slli_epi16::<4>(center), _mm_slli_epi16::<2>(center)),
                _mm_add_epi16(_mm_slli_epi16::<2>(middle), middle),
            ),
            _mm_add_epi16(a, f),
        )
    }
}
#[inline]
unsafe fn horizontal<const W: usize, const FAST: bool>(p: *const u8) -> __m128i {
    unsafe {
        if FAST && W != 4 {
            return horizontal_ssse3(p);
        }
        fir(
            words::<W>(p),
            words::<W>(p.add(1)),
            words::<W>(p.add(2)),
            words::<W>(p.add(3)),
            words::<W>(p.add(4)),
            words::<W>(p.add(5)),
        )
    }
}
/// FFmpeg h264_qpel_8bit.asm's SSSE3 horizontal lowpass (Michael Niedermayer,
/// Loren Merritt, Daniel Kang): unpack once, then PALIGNR the six tap vectors.
/// The original loads 16 bytes for eight outputs. Here two overlapping 8-byte
/// loads consume exactly the required 13 bytes, including at allocation ends.
#[target_feature(enable = "ssse3")]
#[inline]
unsafe fn horizontal_ssse3(p: *const u8) -> __m128i {
    unsafe {
        let bytes = _mm_unpacklo_epi64(
            _mm_loadl_epi64(p.cast()),
            _mm_srli_si128::<3>(_mm_loadl_epi64(p.add(5).cast())),
        );
        let lo = _mm_unpacklo_epi8(bytes, _mm_setzero_si128());
        let hi = _mm_unpackhi_epi8(bytes, _mm_setzero_si128());
        let outer = _mm_add_epi16(lo, _mm_alignr_epi8::<10>(hi, lo));
        let middle = _mm_add_epi16(_mm_alignr_epi8::<2>(hi, lo), _mm_alignr_epi8::<8>(hi, lo));
        let center = _mm_add_epi16(_mm_alignr_epi8::<4>(hi, lo), _mm_alignr_epi8::<6>(hi, lo));
        _mm_add_epi16(
            outer,
            _mm_mullo_epi16(
                _mm_sub_epi16(_mm_slli_epi16::<2>(center), middle),
                _mm_set1_epi16(5),
            ),
        )
    }
}
#[inline]
unsafe fn vertical<const W: usize>(p: *const u8, stride: usize) -> __m128i {
    unsafe {
        fir(
            words::<W>(p),
            words::<W>(p.add(stride)),
            words::<W>(p.add(2 * stride)),
            words::<W>(p.add(3 * stride)),
            words::<W>(p.add(4 * stride)),
            words::<W>(p.add(5 * stride)),
        )
    }
}
#[inline]
unsafe fn half_pixel(v: __m128i) -> __m128i {
    unsafe {
        _mm_packus_epi16(
            _mm_srai_epi16::<5>(_mm_add_epi16(v, _mm_set1_epi16(16))),
            _mm_setzero_si128(),
        )
    }
}
#[inline]
unsafe fn diagonal(r: [__m128i; 6]) -> __m128i {
    unsafe {
        let a = _mm_set1_epi32((-5i32 << 16) | 1);
        let b = _mm_set1_epi32((20 << 16) | 20);
        let c = _mm_set1_epi32((1 << 16) | 65531);
        let calc = |p: __m128i, q: __m128i, s: __m128i| {
            _mm_srai_epi32::<10>(_mm_add_epi32(
                _mm_add_epi32(_mm_madd_epi16(p, a), _mm_madd_epi16(q, b)),
                _mm_add_epi32(_mm_madd_epi16(s, c), _mm_set1_epi32(512)),
            ))
        };
        let lo = calc(
            _mm_unpacklo_epi16(r[0], r[1]),
            _mm_unpacklo_epi16(r[2], r[3]),
            _mm_unpacklo_epi16(r[4], r[5]),
        );
        let hi = calc(
            _mm_unpackhi_epi16(r[0], r[1]),
            _mm_unpackhi_epi16(r[2], r[3]),
            _mm_unpackhi_epi16(r[4], r[5]),
        );
        _mm_packus_epi16(_mm_packs_epi32(lo, hi), _mm_setzero_si128())
    }
}
#[inline(always)]
unsafe fn interpolate<const W: usize, const X: u8, const Y: u8, const FAST: bool>(
    src: &[u8],
    stride: usize,
    dst: &mut Block<'_>,
) {
    unsafe {
        let diagonal_needed = X != 0 && Y != 0 && (X == 2 || Y == 2);
        let chunks = if W == 16 { 2 } else { 1 };
        let mut strip = [[_mm_setzero_si128(); 2]; 21];
        if diagonal_needed {
            for y in 0..dst.height + 5 {
                for i in 0..chunks {
                    strip[y][i] = horizontal::<W, FAST>(src.as_ptr().add(y * stride + i * 8));
                }
            }
        }
        for y in 0..dst.height {
            for i in 0..chunks {
                let x = i * 8;
                let h = || {
                    half_pixel(horizontal::<W, FAST>(
                        src.as_ptr().add((y + 2 + usize::from(Y == 3)) * stride + x),
                    ))
                };
                let v = || {
                    half_pixel(vertical::<W>(
                        src.as_ptr().add(y * stride + x + 2 + usize::from(X == 3)),
                        stride,
                    ))
                };
                let j = || diagonal(std::array::from_fn(|r| strip[y + r][i]));
                let g =
                    || {
                        pixels::<W>(src.as_ptr().add(
                            (y + 2 + usize::from(Y == 3)) * stride + x + 2 + usize::from(X == 3),
                        ))
                    };
                let out = match (X, Y) {
                    (2, 0) => h(),
                    (0, 2) => v(),
                    (2, 2) => j(),
                    (1 | 3, 0) => _mm_avg_epu8(g(), h()),
                    (0, 1 | 3) => _mm_avg_epu8(g(), v()),
                    (2, 1 | 3) => _mm_avg_epu8(h(), j()),
                    (1 | 3, 2) => _mm_avg_epu8(v(), j()),
                    (1 | 3, 1 | 3) => _mm_avg_epu8(h(), v()),
                    _ => unreachable!(),
                };
                let p = dst.data.as_mut_ptr().add(y * dst.stride + x);
                if W == 4 {
                    p.cast::<i32>().write_unaligned(_mm_cvtsi128_si32(out));
                } else {
                    _mm_storel_epi64(p.cast(), out);
                }
            }
        }
    }
}

/// Validates the complete 8x16/16x8 footprint before any pointer operation.
pub(super) fn luma(
    view: crate::dsp::filter::ValidatedEdge<'_, false>,
    alpha: i32,
    beta: i32,
    bs: [u8; 4],
    tc: [i16; 4],
) {
    let (data, stride, edge) = view.into_parts();
    let (x, y, vertical) = (edge.x, edge.y, edge.vertical);
    // Safety: every load/store is inside the checked footprint, and no aliases
    // are created. All filter intermediates fit signed 16-bit lanes at 8 bit.
    unsafe {
        let p = data.as_mut_ptr().add(y * stride + x);
        if has_avx2() {
            deblock::run(p, stride, vertical, alpha as i16, beta as i16, bs, tc);
        } else {
            run(p, stride, vertical, alpha as i16, beta as i16, bs, tc);
        }
    };
}
#[inline]
pub(crate) unsafe fn transpose8(r: [__m128i; 8]) -> [__m128i; 4] {
    unsafe {
        let a = _mm_unpacklo_epi8(r[0], r[1]);
        let b = _mm_unpacklo_epi8(r[2], r[3]);
        let c = _mm_unpacklo_epi8(r[4], r[5]);
        let d = _mm_unpacklo_epi8(r[6], r[7]);
        let e = _mm_unpacklo_epi16(a, b);
        let f = _mm_unpackhi_epi16(a, b);
        let g = _mm_unpacklo_epi16(c, d);
        let h = _mm_unpackhi_epi16(c, d);
        [
            _mm_unpacklo_epi32(e, g),
            _mm_unpackhi_epi32(e, g),
            _mm_unpacklo_epi32(f, h),
            _mm_unpackhi_epi32(f, h),
        ]
    }
}
#[inline]
unsafe fn columns(p: *const u8, stride: usize) -> [__m128i; 8] {
    unsafe {
        let a = transpose8(std::array::from_fn(|i| {
            _mm_loadl_epi64(p.add(i * stride).cast())
        }));
        let b = transpose8(std::array::from_fn(|i| {
            _mm_loadl_epi64(p.add((i + 8) * stride).cast())
        }));
        std::array::from_fn(|i| {
            if i % 2 == 0 {
                _mm_unpacklo_epi64(a[i / 2], b[i / 2])
            } else {
                _mm_unpackhi_epi64(a[i / 2], b[i / 2])
            }
        })
    }
}
#[inline]
unsafe fn store_columns(p: *mut u8, stride: usize, c: [__m128i; 8]) {
    unsafe {
        for group in 0..2 {
            let rows = transpose8(if group == 0 {
                c
            } else {
                c.map(|v| _mm_srli_si128::<8>(v))
            });
            for i in 0..8 {
                let v = if i % 2 == 0 {
                    rows[i / 2]
                } else {
                    _mm_srli_si128::<8>(rows[i / 2])
                };
                _mm_storel_epi64(p.add((group * 8 + i) * stride).cast(), v);
            }
        }
    }
}
#[inline]
unsafe fn filter<const MODE: u8>(
    s: [__m128i; 8],
    alpha: i16,
    beta: i16,
    bs: __m128i,
    tc0: __m128i,
) -> [__m128i; 8] {
    unsafe {
        let [p3, p2, p1, p0, q0, q1, q2, q3] = s;
        let zero = _mm_setzero_si128();
        let one = _mm_set1_epi16(1);
        let two = _mm_set1_epi16(2);
        let four = _mm_set1_epi16(4);
        let add = |a, b| _mm_add_epi16(a, b);
        let sub = |a, b| _mm_sub_epi16(a, b);
        let select = |mask, a, b| _mm_or_si128(_mm_and_si128(mask, a), _mm_andnot_si128(mask, b));
        let abs = |v| {
            let sign = _mm_srai_epi16::<15>(v);
            sub(_mm_xor_si128(v, sign), sign)
        };
        let clamp = |v, lo, hi| _mm_max_epi16(lo, _mm_min_epi16(hi, v));
        let a = _mm_set1_epi16(alpha);
        let b = _mm_set1_epi16(beta);
        let enabled = _mm_and_si128(
            _mm_cmpgt_epi16(a, abs(sub(p0, q0))),
            _mm_and_si128(
                _mm_cmpgt_epi16(b, abs(sub(p1, p0))),
                _mm_cmpgt_epi16(b, abs(sub(q1, q0))),
            ),
        );
        // MODE 0: ordinary edges; MODE 1: all-strong intra edge;
        // MODE 2 retains mixed strengths for the public DSP contract.
        let normal = if MODE == 1 {
            zero
        } else {
            _mm_and_si128(
                enabled,
                _mm_and_si128(_mm_cmpgt_epi16(bs, zero), _mm_cmpgt_epi16(four, bs)),
            )
        };
        let strong = if MODE == 0 {
            zero
        } else if MODE == 1 {
            enabled
        } else {
            _mm_and_si128(enabled, _mm_cmpeq_epi16(bs, four))
        };
        if _mm_movemask_epi8(_mm_or_si128(normal, strong)) == 0 {
            return s;
        }
        let ap = _mm_cmpgt_epi16(b, abs(sub(p2, p0)));
        let aq = _mm_cmpgt_epi16(b, abs(sub(q2, q0)));
        let tc = add(tc0, add(_mm_and_si128(ap, one), _mm_and_si128(aq, one)));
        let delta = clamp(
            _mm_srai_epi16::<3>(add(
                add(_mm_slli_epi16::<2>(sub(q0, p0)), sub(p1, q1)),
                four,
            )),
            sub(zero, tc),
            tc,
        );
        let midpoint = _mm_srai_epi16::<1>(add(add(p0, q0), one));
        let np1 = add(
            p1,
            clamp(
                sub(_mm_srai_epi16::<1>(add(p2, midpoint)), p1),
                sub(zero, tc0),
                tc0,
            ),
        );
        let nq1 = add(
            q1,
            clamp(
                sub(_mm_srai_epi16::<1>(add(q2, midpoint)), q1),
                sub(zero, tc0),
                tc0,
            ),
        );
        let np0 = clamp(add(p0, delta), zero, _mm_set1_epi16(255));
        let nq0 = clamp(sub(q0, delta), zero, _mm_set1_epi16(255));
        // FFmpeg has separate normal/intra kernels. Ordinary inter edges
        // never execute the strong filter's p2/q2 and six-tap arithmetic.
        if _mm_movemask_epi8(strong) == 0 {
            return [
                p3,
                p2,
                select(_mm_and_si128(normal, ap), np1, p1),
                select(normal, np0, p0),
                select(normal, nq0, q0),
                select(_mm_and_si128(normal, aq), nq1, q1),
                q2,
                q3,
            ];
        }
        let small = _mm_cmpgt_epi16(_mm_set1_epi16((alpha >> 2) + 2), abs(sub(p0, q0)));
        let sp = _mm_and_si128(strong, _mm_and_si128(small, ap));
        let sq = _mm_and_si128(strong, _mm_and_si128(small, aq));
        let twice = |v| _mm_slli_epi16::<1>(v);
        let p0s = select(
            sp,
            _mm_srai_epi16::<3>(add(
                add(add(add(p2, twice(p1)), twice(p0)), twice(q0)),
                add(q1, four),
            )),
            _mm_srai_epi16::<2>(add(add(twice(p1), p0), add(q1, two))),
        );
        let q0s = select(
            sq,
            _mm_srai_epi16::<3>(add(
                add(add(add(p1, twice(p0)), twice(q0)), twice(q1)),
                add(q2, four),
            )),
            _mm_srai_epi16::<2>(add(add(twice(q1), q0), add(p1, two))),
        );
        let p1s = _mm_srai_epi16::<2>(add(add(p2, p1), add(add(p0, q0), two)));
        let q1s = _mm_srai_epi16::<2>(add(add(p0, q0), add(add(q1, q2), two)));
        let p2s = _mm_srai_epi16::<3>(add(
            add(twice(p3), add(twice(p2), p2)),
            add(add(p1, p0), add(q0, four)),
        ));
        let q2s = _mm_srai_epi16::<3>(add(
            add(twice(q3), add(twice(q2), q2)),
            add(add(q1, q0), add(p0, four)),
        ));
        [
            p3,
            select(sp, p2s, p2),
            select(sp, p1s, select(_mm_and_si128(normal, ap), np1, p1)),
            select(strong, p0s, select(normal, np0, p0)),
            select(strong, q0s, select(normal, nq0, q0)),
            select(sq, q1s, select(_mm_and_si128(normal, aq), nq1, q1)),
            select(sq, q2s, q2),
            q3,
        ]
    }
}
unsafe fn run(
    q: *mut u8,
    stride: usize,
    vertical: bool,
    alpha: i16,
    beta: i16,
    bs: [u8; 4],
    tc: [i16; 4],
) {
    unsafe {
        let start = if vertical {
            q.sub(4)
        } else {
            q.sub(4 * stride)
        };
        let c = if vertical {
            columns(start, stride)
        } else {
            std::array::from_fn(|i| _mm_loadu_si128(start.add(i * stride).cast()))
        };
        let zero = _mm_setzero_si128();
        let mut halves = [[_mm_setzero_si128(); 8]; 2];
        for h in 0..2 {
            let s = c.map(|v| {
                if h == 0 {
                    _mm_unpacklo_epi8(v, zero)
                } else {
                    _mm_unpackhi_epi8(v, zero)
                }
            });
            let a = bs[h * 2] as i16;
            let b = bs[h * 2 + 1] as i16;
            let ta = tc[h * 2];
            let tb = tc[h * 2 + 1];
            halves[h] = if bs == [4; 4] {
                filter::<1>(
                    s,
                    alpha,
                    beta,
                    _mm_set_epi16(b, b, b, b, a, a, a, a),
                    _mm_set_epi16(tb, tb, tb, tb, ta, ta, ta, ta),
                )
            } else if bs.iter().all(|&v| v < 4) {
                filter::<0>(
                    s,
                    alpha,
                    beta,
                    _mm_set_epi16(b, b, b, b, a, a, a, a),
                    _mm_set_epi16(tb, tb, tb, tb, ta, ta, ta, ta),
                )
            } else {
                filter::<2>(
                    s,
                    alpha,
                    beta,
                    _mm_set_epi16(b, b, b, b, a, a, a, a),
                    _mm_set_epi16(tb, tb, tb, tb, ta, ta, ta, ta),
                )
            };
        }
        let output = std::array::from_fn(|i| _mm_packus_epi16(halves[0][i], halves[1][i]));
        if vertical {
            store_columns(start, stride, output);
        } else {
            // p3/q3 are read-only. Store only potentially filtered rows.
            for i in 1..7 {
                _mm_storeu_si128(start.add(i * stride).cast(), output[i]);
            }
        }
    }
}
