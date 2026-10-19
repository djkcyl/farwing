// SPDX-License-Identifier: LGPL-2.1-or-later
// FFmpeg h264idct_template.c arithmetic, including the first-pass i16 truncation.
// Copyright (c) 2004-2011 Michael Niedermayer. Rust SIMD port OpenUUYC.
use super::*;

#[inline]
unsafe fn widen4(p: *const i16) -> __m128i {
    unsafe {
        let v = _mm_loadl_epi64(p.cast());
        _mm_unpacklo_epi16(v, _mm_srai_epi16::<15>(v))
    }
}
#[inline]
unsafe fn truncate(v: __m128i) -> __m128i {
    unsafe { _mm_srai_epi32::<16>(_mm_slli_epi32::<16>(v)) }
}
#[inline]
unsafe fn transpose4(r: [__m128i; 4]) -> [__m128i; 4] {
    unsafe {
        let a = _mm_unpacklo_epi32(r[0], r[1]);
        let b = _mm_unpackhi_epi32(r[0], r[1]);
        let c = _mm_unpacklo_epi32(r[2], r[3]);
        let d = _mm_unpackhi_epi32(r[2], r[3]);
        [
            _mm_unpacklo_epi64(a, c),
            _mm_unpackhi_epi64(a, c),
            _mm_unpacklo_epi64(b, d),
            _mm_unpackhi_epi64(b, d),
        ]
    }
}
#[inline]
unsafe fn four(v: [__m128i; 4]) -> [__m128i; 4] {
    unsafe {
        let a = _mm_add_epi32(v[0], v[2]);
        let b = _mm_sub_epi32(v[0], v[2]);
        let c = _mm_sub_epi32(_mm_srai_epi32::<1>(v[1]), v[3]);
        let d = _mm_add_epi32(v[1], _mm_srai_epi32::<1>(v[3]));
        [
            _mm_add_epi32(a, d),
            _mm_add_epi32(b, c),
            _mm_sub_epi32(b, c),
            _mm_sub_epi32(a, d),
        ]
    }
}
#[inline]
unsafe fn eight(v: [__m128i; 8]) -> [__m128i; 8] {
    unsafe {
        let add = |a, b| _mm_add_epi32(a, b);
        let sub = |a, b| _mm_sub_epi32(a, b);
        let z = _mm_setzero_si128();
        let a0 = add(v[0], v[4]);
        let a2 = sub(v[0], v[4]);
        let a4 = sub(_mm_srai_epi32::<1>(v[2]), v[6]);
        let a6 = add(_mm_srai_epi32::<1>(v[6]), v[2]);
        let (b0, b2, b4, b6) = (add(a0, a6), add(a2, a4), sub(a2, a4), sub(a0, a6));
        let a1 = sub(sub(sub(v[5], v[3]), v[7]), _mm_srai_epi32::<1>(v[7]));
        let a3 = sub(sub(add(v[1], v[7]), v[3]), _mm_srai_epi32::<1>(v[3]));
        let a5 = add(add(sub(v[7], v[1]), v[5]), _mm_srai_epi32::<1>(v[5]));
        let a7 = add(add(add(v[3], v[5]), v[1]), _mm_srai_epi32::<1>(v[1]));
        let (b1, b3, b5, b7) = (
            add(_mm_srai_epi32::<2>(a7), a1),
            add(a3, _mm_srai_epi32::<2>(a5)),
            sub(_mm_srai_epi32::<2>(a3), a5),
            sub(a7, _mm_srai_epi32::<2>(a1)),
        );
        let _ = z;
        [
            add(b0, b7),
            add(b2, b5),
            add(b4, b3),
            add(b6, b1),
            sub(b6, b1),
            sub(b4, b3),
            sub(b2, b5),
            sub(b0, b7),
        ]
    }
}
#[inline]
unsafe fn add_store4(p: *mut u8, v: __m128i) {
    unsafe {
        let zero = _mm_setzero_si128();
        let old = _mm_unpacklo_epi16(words::<4>(p), zero);
        let value = _mm_add_epi32(old, _mm_srai_epi32::<6>(v));
        let bytes = _mm_packus_epi16(_mm_packs_epi32(value, zero), zero);
        p.cast::<i32>().write_unaligned(_mm_cvtsi128_si32(bytes));
    }
}
pub(crate) fn add4(dst: &mut Block<'_>, coeff: &mut [i16; 16]) -> bool {
    if dst.width != 4 || dst.height != 4 {
        return false;
    }
    // SAFETY: checked 4x4 destination and fixed coefficient storage.
    unsafe { add4_inner(dst, coeff) };
    true
}
#[inline]
unsafe fn add4_inner(dst: &mut Block<'_>, coeff: &mut [i16; 16]) {
    // The 4-point transform's absolute row sum is at most 3.5, hence
    // two passes on |coefficient|<=2047 (including the +32 rounding DC)
    // stay inside i16. Larger legal inputs retain the exact i32 fallback.
    unsafe {
        if bounded(coeff, 2047) {
            add4_words(dst, coeff);
            return;
        }
    }
    coeff[0] = coeff[0].wrapping_add(32);
    unsafe {
        let rows = std::array::from_fn(|i| widen4(coeff.as_ptr().add(i * 4)));
        let first = four(rows).map(|v| truncate(v));
        let second = four(transpose4(first));
        for y in 0..4 {
            add_store4(dst.data.as_mut_ptr().add(y * dst.stride), second[y]);
        }
    }
    coeff.fill(0);
}
pub(crate) fn add8(dst: &mut Block<'_>, coeff: &mut [i16; 64]) -> bool {
    if dst.width != 8 || dst.height != 8 {
        return false;
    }
    unsafe {
        if bounded(coeff, 511) {
            add8_words(dst, coeff);
            return true;
        }
    }
    coeff[0] = coeff[0].wrapping_add(32);
    if has_avx2() {
        // The cached CPU/OS capability check guards the target-feature routine.
        unsafe { add8_avx2(dst, coeff) };
    } else {
        unsafe { add8_sse2(dst, coeff) };
    }
    coeff.fill(0);
    true
}
#[inline]
unsafe fn bounded(coeff: &[i16], limit: i16) -> bool {
    unsafe {
        let high = _mm_set1_epi16(limit);
        let low = _mm_set1_epi16(-limit);
        let mut bad = _mm_setzero_si128();
        for chunk in coeff.chunks_exact(8) {
            let v = _mm_loadu_si128(chunk.as_ptr().cast());
            bad = _mm_or_si128(
                bad,
                _mm_or_si128(_mm_cmpgt_epi16(v, high), _mm_cmpgt_epi16(low, v)),
            );
        }
        _mm_movemask_epi8(bad) == 0
    }
}
#[inline]
unsafe fn four_words(v: [__m128i; 4]) -> [__m128i; 4] {
    unsafe {
        let a = _mm_add_epi16(v[0], v[2]);
        let b = _mm_sub_epi16(v[0], v[2]);
        let c = _mm_sub_epi16(_mm_srai_epi16::<1>(v[1]), v[3]);
        let d = _mm_add_epi16(v[1], _mm_srai_epi16::<1>(v[3]));
        [
            _mm_add_epi16(a, d),
            _mm_add_epi16(b, c),
            _mm_sub_epi16(b, c),
            _mm_sub_epi16(a, d),
        ]
    }
}
#[inline]
unsafe fn add4_words(dst: &mut Block<'_>, coeff: &mut [i16; 16]) {
    unsafe {
        coeff[0] += 32;
        let v = four_words(std::array::from_fn(|i| {
            _mm_loadl_epi64(coeff.as_ptr().add(i * 4).cast())
        }));
        let a = _mm_unpacklo_epi16(v[0], v[1]);
        let b = _mm_unpacklo_epi16(v[2], v[3]);
        let c = _mm_unpacklo_epi32(a, b);
        let d = _mm_unpackhi_epi32(a, b);
        let rows = four_words([c, _mm_srli_si128::<8>(c), d, _mm_srli_si128::<8>(d)]);
        for y in 0..4 {
            let p = dst.data.as_mut_ptr().add(y * dst.stride);
            let value = _mm_add_epi16(words::<4>(p), _mm_srai_epi16::<6>(rows[y]));
            p.cast::<i32>()
                .write_unaligned(_mm_cvtsi128_si32(_mm_packus_epi16(value, value)));
        }
        coeff.fill(0);
    }
}
#[inline]
unsafe fn eight_words(v: [__m128i; 8]) -> [__m128i; 8] {
    unsafe {
        let add = |a, b| _mm_add_epi16(a, b);
        let sub = |a, b| _mm_sub_epi16(a, b);
        let a0 = add(v[0], v[4]);
        let a2 = sub(v[0], v[4]);
        let a4 = sub(_mm_srai_epi16::<1>(v[2]), v[6]);
        let a6 = add(_mm_srai_epi16::<1>(v[6]), v[2]);
        let (b0, b2, b4, b6) = (add(a0, a6), add(a2, a4), sub(a2, a4), sub(a0, a6));
        let a1 = sub(sub(sub(v[5], v[3]), v[7]), _mm_srai_epi16::<1>(v[7]));
        let a3 = sub(sub(add(v[1], v[7]), v[3]), _mm_srai_epi16::<1>(v[3]));
        let a5 = add(add(sub(v[7], v[1]), v[5]), _mm_srai_epi16::<1>(v[5]));
        let a7 = add(add(add(v[3], v[5]), v[1]), _mm_srai_epi16::<1>(v[1]));
        let (b1, b3, b5, b7) = (
            add(_mm_srai_epi16::<2>(a7), a1),
            add(a3, _mm_srai_epi16::<2>(a5)),
            sub(_mm_srai_epi16::<2>(a3), a5),
            sub(a7, _mm_srai_epi16::<2>(a1)),
        );
        [
            add(b0, b7),
            add(b2, b5),
            add(b4, b3),
            add(b6, b1),
            sub(b6, b1),
            sub(b4, b3),
            sub(b2, b5),
            sub(b0, b7),
        ]
    }
}
#[inline]
unsafe fn transpose_words(v: [__m128i; 8]) -> [__m128i; 8] {
    unsafe {
        let a = [
            _mm_unpacklo_epi16(v[0], v[1]),
            _mm_unpackhi_epi16(v[0], v[1]),
            _mm_unpacklo_epi16(v[2], v[3]),
            _mm_unpackhi_epi16(v[2], v[3]),
            _mm_unpacklo_epi16(v[4], v[5]),
            _mm_unpackhi_epi16(v[4], v[5]),
            _mm_unpacklo_epi16(v[6], v[7]),
            _mm_unpackhi_epi16(v[6], v[7]),
        ];
        let b = [
            _mm_unpacklo_epi32(a[0], a[2]),
            _mm_unpackhi_epi32(a[0], a[2]),
            _mm_unpacklo_epi32(a[1], a[3]),
            _mm_unpackhi_epi32(a[1], a[3]),
            _mm_unpacklo_epi32(a[4], a[6]),
            _mm_unpackhi_epi32(a[4], a[6]),
            _mm_unpacklo_epi32(a[5], a[7]),
            _mm_unpackhi_epi32(a[5], a[7]),
        ];
        [
            _mm_unpacklo_epi64(b[0], b[4]),
            _mm_unpackhi_epi64(b[0], b[4]),
            _mm_unpacklo_epi64(b[1], b[5]),
            _mm_unpackhi_epi64(b[1], b[5]),
            _mm_unpacklo_epi64(b[2], b[6]),
            _mm_unpackhi_epi64(b[2], b[6]),
            _mm_unpacklo_epi64(b[3], b[7]),
            _mm_unpackhi_epi64(b[3], b[7]),
        ]
    }
}
/// Max absolute row sum is 7.875; with floor shifts and the rounding DC,
/// coefficients bounded by 511 keep both butterfly passes below i16 limits.
#[inline]
unsafe fn add8_words(dst: &mut Block<'_>, coeff: &mut [i16; 64]) {
    unsafe {
        coeff[0] += 32;
        let rows = std::array::from_fn(|i| _mm_loadu_si128(coeff.as_ptr().add(i * 8).cast()));
        let out = eight_words(transpose_words(eight_words(rows)));
        for y in 0..8 {
            let p = dst.data.as_mut_ptr().add(y * dst.stride);
            let value = _mm_add_epi16(words::<8>(p), _mm_srai_epi16::<6>(out[y]));
            _mm_storel_epi64(p.cast(), _mm_packus_epi16(value, value));
        }
        coeff.fill(0);
    }
}
unsafe fn add8_sse2(dst: &mut Block<'_>, coeff: &mut [i16; 64]) {
    unsafe {
        let lo =
            eight(std::array::from_fn(|y| widen4(coeff.as_ptr().add(y * 8)))).map(|v| truncate(v));
        let hi = eight(std::array::from_fn(|y| {
            widen4(coeff.as_ptr().add(y * 8 + 4))
        }))
        .map(|v| truncate(v));
        for x in [0, 4] {
            let a = transpose4(std::array::from_fn(|i| lo[x + i]));
            let b = transpose4(std::array::from_fn(|i| hi[x + i]));
            let out = eight([a[0], a[1], a[2], a[3], b[0], b[1], b[2], b[3]]);
            for y in 0..8 {
                add_store4(dst.data.as_mut_ptr().add(y * dst.stride + x), out[y]);
            }
        }
    }
}
/// Register-only 8x8 transpose. Each 128-bit lane first transposes a 4x4
/// quadrant; the final lane permutations join the upper/lower quadrants.
#[target_feature(enable = "avx2")]
unsafe fn transpose8_avx(v: [__m256i; 8]) -> [__m256i; 8] {
    let a: [__m256i; 8] = std::array::from_fn(|i| {
        if i & 1 == 0 {
            _mm256_unpacklo_epi32(v[i], v[i + 1])
        } else {
            _mm256_unpackhi_epi32(v[i - 1], v[i])
        }
    });
    let b = [
        _mm256_unpacklo_epi64(a[0], a[2]),
        _mm256_unpackhi_epi64(a[0], a[2]),
        _mm256_unpacklo_epi64(a[1], a[3]),
        _mm256_unpackhi_epi64(a[1], a[3]),
        _mm256_unpacklo_epi64(a[4], a[6]),
        _mm256_unpackhi_epi64(a[4], a[6]),
        _mm256_unpacklo_epi64(a[5], a[7]),
        _mm256_unpackhi_epi64(a[5], a[7]),
    ];
    [
        _mm256_permute2x128_si256::<0x20>(b[0], b[4]),
        _mm256_permute2x128_si256::<0x20>(b[1], b[5]),
        _mm256_permute2x128_si256::<0x20>(b[2], b[6]),
        _mm256_permute2x128_si256::<0x20>(b[3], b[7]),
        _mm256_permute2x128_si256::<0x31>(b[0], b[4]),
        _mm256_permute2x128_si256::<0x31>(b[1], b[5]),
        _mm256_permute2x128_si256::<0x31>(b[2], b[6]),
        _mm256_permute2x128_si256::<0x31>(b[3], b[7]),
    ]
}
#[target_feature(enable = "avx2")]
unsafe fn eight_avx(v: [__m256i; 8]) -> [__m256i; 8] {
    let add = |a, b| _mm256_add_epi32(a, b);
    let sub = |a, b| _mm256_sub_epi32(a, b);
    let a0 = add(v[0], v[4]);
    let a2 = sub(v[0], v[4]);
    let a4 = sub(_mm256_srai_epi32::<1>(v[2]), v[6]);
    let a6 = add(_mm256_srai_epi32::<1>(v[6]), v[2]);
    let (b0, b2, b4, b6) = (add(a0, a6), add(a2, a4), sub(a2, a4), sub(a0, a6));
    let a1 = sub(sub(sub(v[5], v[3]), v[7]), _mm256_srai_epi32::<1>(v[7]));
    let a3 = sub(sub(add(v[1], v[7]), v[3]), _mm256_srai_epi32::<1>(v[3]));
    let a5 = add(add(sub(v[7], v[1]), v[5]), _mm256_srai_epi32::<1>(v[5]));
    let a7 = add(add(add(v[3], v[5]), v[1]), _mm256_srai_epi32::<1>(v[1]));
    let (b1, b3, b5, b7) = (
        add(_mm256_srai_epi32::<2>(a7), a1),
        add(a3, _mm256_srai_epi32::<2>(a5)),
        sub(_mm256_srai_epi32::<2>(a3), a5),
        sub(a7, _mm256_srai_epi32::<2>(a1)),
    );
    [
        add(b0, b7),
        add(b2, b5),
        add(b4, b3),
        add(b6, b1),
        sub(b6, b1),
        sub(b4, b3),
        sub(b2, b5),
        sub(b0, b7),
    ]
}
#[target_feature(enable = "avx2")]
unsafe fn add8_avx2(dst: &mut Block<'_>, coeff: &mut [i16; 64]) {
    unsafe {
        let rows = std::array::from_fn(|y| {
            _mm256_cvtepi16_epi32(_mm_loadu_si128(coeff.as_ptr().add(y * 8).cast()))
        });
        let first = eight_avx(rows).map(|v| _mm256_srai_epi32::<16>(_mm256_slli_epi32::<16>(v)));
        let second = eight_avx(transpose8_avx(first));
        for y in 0..8 {
            let p = dst.data.as_mut_ptr().add(y * dst.stride);
            let old = _mm256_cvtepu8_epi32(_mm_loadl_epi64(p.cast()));
            let v = _mm256_add_epi32(old, _mm256_srai_epi32::<6>(second[y]));
            let v = _mm_packs_epi32(_mm256_castsi256_si128(v), _mm256_extracti128_si256::<1>(v));
            _mm_storel_epi64(p.cast(), _mm_packus_epi16(v, _mm_setzero_si128()));
        }
    }
}
pub(crate) fn add_dc(dst: &mut Block<'_>, dc: &mut i16) -> bool {
    if !matches!(dst.width, 4 | 8) || dst.width != dst.height {
        return false;
    }
    // SAFETY: checked 4x4 or 8x8 destination and exclusive DC reference.
    unsafe { add_dc_inner(dst, dc) };
    true
}
#[inline]
unsafe fn add_dc_inner(dst: &mut Block<'_>, dc: &mut i16) {
    unsafe {
        let value = _mm_set1_epi16(((*dc as i32 + 32) >> 6) as i16);
        let zero = _mm_setzero_si128();
        for y in 0..dst.height {
            let p = dst.data.as_mut_ptr().add(y * dst.stride);
            let old = if dst.width == 4 {
                words::<4>(p)
            } else {
                words::<8>(p)
            };
            let v = _mm_packus_epi16(_mm_add_epi16(old, value), zero);
            if dst.width == 4 {
                p.cast::<i32>().write_unaligned(_mm_cvtsi128_si32(v));
            } else {
                _mm_storel_epi64(p.cast(), v);
            }
        }
    }
    *dc = 0;
}

/// The public transform entry checks the macroblock shape. Block's private
/// geometry guarantees its complete allocation footprint. Every subblock below
/// is inside that rectangle; no unchecked source or caller-supplied offsets.
pub(crate) fn add_plane(
    dst: &mut Block<'_>,
    coeff: &mut [i16; 256],
    nnz: &[u8; 16],
    transform8: bool,
    separate_dc: bool,
) {
    if has_avx2() {
        unsafe { add_plane_inner::<true>(dst, coeff, nnz, transform8, separate_dc) };
    } else {
        unsafe { add_plane_inner::<false>(dst, coeff, nnz, transform8, separate_dc) };
    }
}
unsafe fn add_plane_inner<const AVX2: bool>(
    dst: &mut Block<'_>,
    coeff: &mut [i16; 256],
    nnz: &[u8; 16],
    transform8: bool,
    separate_dc: bool,
) {
    let size = dst.width;
    let n = if transform8 { 8 } else { 4 };
    let count = size * size / (n * n);
    let full4 =
        |i: usize, c: &[i16; 256]| nnz[i] != 0 && (separate_dc || nnz[i] != 1 || c[i * 16] == 0);
    let mut i = 0;
    while i < count {
        let offset = i * n * n;
        let nz: u16 = if transform8 {
            nnz[i * 4..i * 4 + 4].iter().map(|&v| v as u16).sum()
        } else {
            nnz[i] as u16
        };
        if nz == 0 && coeff[offset] == 0 {
            i += 1;
            continue;
        }
        let (x, y) = super::super::transform::plane_block_position(i, size, n);
        let at = y * dst.stride + x;
        // Adjacent Z-order entries at even indices form one horizontal pair.
        // Packing two independent 4x4 butterflies into AVX2 lanes avoids two
        // dispatches and shares the transform arithmetic without extra frames.
        if AVX2 && !transform8 && i & 1 == 0 && full4(i, coeff) && full4(i + 1, coeff) {
            unsafe {
                let c: &mut [i16; 32] = (&mut coeff[offset..offset + 32]).try_into().unwrap();
                if bounded(c, 2047) {
                    add4_pair_words(dst.data.as_mut_ptr().add(at), dst.stride, c);
                } else {
                    add4_pair_avx2(dst.data.as_mut_ptr().add(at), dst.stride, c);
                }
            };
            i += 2;
            continue;
        }
        let mut block = Block {
            data: &mut dst.data[at..],
            stride: dst.stride,
            width: n,
            height: n,
        };
        unsafe {
            if nz == 0 || (!separate_dc && nz == 1 && coeff[offset] != 0) {
                add_dc_inner(&mut block, &mut coeff[offset]);
            } else if transform8 {
                let c: &mut [i16; 64] = (&mut coeff[offset..offset + 64]).try_into().unwrap();
                if bounded(c, 511) {
                    add8_words(&mut block, c);
                    i += 1;
                    continue;
                }
                c[0] = c[0].wrapping_add(32);
                if AVX2 {
                    add8_avx2(&mut block, c);
                } else {
                    add8_sse2(&mut block, c);
                }
                c.fill(0);
            } else {
                add4_inner(
                    &mut block,
                    (&mut coeff[offset..offset + 16]).try_into().unwrap(),
                );
            }
        }
        i += 1;
    }
}
#[inline]
unsafe fn add4_pair_words(dst: *mut u8, stride: usize, coeff: &mut [i16; 32]) {
    unsafe {
        coeff[0] += 32;
        coeff[16] += 32;
        let rows = std::array::from_fn(|i| {
            _mm_unpacklo_epi64(
                _mm_loadl_epi64(coeff.as_ptr().add(i * 4).cast()),
                _mm_loadl_epi64(coeff.as_ptr().add(16 + i * 4).cast()),
            )
        });
        let v = four_words(rows);
        let a = _mm_unpacklo_epi16(v[0], v[1]);
        let b = _mm_unpacklo_epi16(v[2], v[3]);
        let c = _mm_unpackhi_epi16(v[0], v[1]);
        let d = _mm_unpackhi_epi16(v[2], v[3]);
        let a0 = _mm_unpacklo_epi32(a, b);
        let a1 = _mm_unpackhi_epi32(a, b);
        let b0 = _mm_unpacklo_epi32(c, d);
        let b1 = _mm_unpackhi_epi32(c, d);
        let out = four_words([
            _mm_unpacklo_epi64(a0, b0),
            _mm_unpackhi_epi64(a0, b0),
            _mm_unpacklo_epi64(a1, b1),
            _mm_unpackhi_epi64(a1, b1),
        ]);
        for y in 0..4 {
            let p = dst.add(y * stride);
            let value = _mm_add_epi16(words::<8>(p), _mm_srai_epi16::<6>(out[y]));
            _mm_storel_epi64(p.cast(), _mm_packus_epi16(value, value));
        }
        coeff.fill(0);
    }
}
#[target_feature(enable = "avx2")]
unsafe fn four_avx(v: [__m256i; 4]) -> [__m256i; 4] {
    let a = _mm256_add_epi32(v[0], v[2]);
    let b = _mm256_sub_epi32(v[0], v[2]);
    let c = _mm256_sub_epi32(_mm256_srai_epi32::<1>(v[1]), v[3]);
    let d = _mm256_add_epi32(v[1], _mm256_srai_epi32::<1>(v[3]));
    [
        _mm256_add_epi32(a, d),
        _mm256_add_epi32(b, c),
        _mm256_sub_epi32(b, c),
        _mm256_sub_epi32(a, d),
    ]
}
#[target_feature(enable = "avx2")]
unsafe fn add4_pair_avx2(dst: *mut u8, stride: usize, coeff: &mut [i16; 32]) {
    unsafe {
        coeff[0] = coeff[0].wrapping_add(32);
        coeff[16] = coeff[16].wrapping_add(32);
        let rows = std::array::from_fn(|i| {
            _mm256_inserti128_si256::<1>(
                _mm256_castsi128_si256(widen4(coeff.as_ptr().add(i * 4))),
                widen4(coeff.as_ptr().add(16 + i * 4)),
            )
        });
        let v = four_avx(rows).map(|v| _mm256_srai_epi32::<16>(_mm256_slli_epi32::<16>(v)));
        let a = _mm256_unpacklo_epi32(v[0], v[1]);
        let b = _mm256_unpackhi_epi32(v[0], v[1]);
        let c = _mm256_unpacklo_epi32(v[2], v[3]);
        let d = _mm256_unpackhi_epi32(v[2], v[3]);
        let out = four_avx([
            _mm256_unpacklo_epi64(a, c),
            _mm256_unpackhi_epi64(a, c),
            _mm256_unpacklo_epi64(b, d),
            _mm256_unpackhi_epi64(b, d),
        ]);
        for y in 0..4 {
            let p = dst.add(y * stride);
            let old = _mm256_cvtepu8_epi32(_mm_loadl_epi64(p.cast()));
            let v = _mm256_add_epi32(old, _mm256_srai_epi32::<6>(out[y]));
            let v = _mm_packs_epi32(_mm256_castsi256_si128(v), _mm256_extracti128_si256::<1>(v));
            _mm_storel_epi64(p.cast(), _mm_packus_epi16(v, _mm_setzero_si128()));
        }
        coeff.fill(0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn bounded_word_transforms_match_wide_arithmetic_at_limits_and_preserve_guards() {
        let mut seed = 0x441287a5u32;
        let mut random = || {
            seed = seed.wrapping_mul(1664525).wrapping_add(1013904223);
            seed
        };
        for size in [4, 8] {
            let limit = if size == 4 { 2047 } else { 511 };
            for sample in 0..4096 {
                let mut coeff = [0i16; 64];
                for (index, c) in coeff[..size * size].iter_mut().enumerate() {
                    *c = match sample % 4 {
                        0 => {
                            if (sample >> (index % 12)) & 1 == 0 {
                                limit
                            } else {
                                -limit
                            }
                        }
                        1 => (random() % (2 * limit as u32 + 1)) as i16 - limit,
                        2 => {
                            if index == sample % (size * size) {
                                limit
                            } else {
                                0
                            }
                        }
                        _ => {
                            if index == sample % (size * size) {
                                -limit
                            } else {
                                0
                            }
                        }
                    };
                }
                let mut expected: [u8; 19 * 11] = std::array::from_fn(|_| (random() >> 24) as u8);
                let mut actual = expected;
                let mut wide = coeff;
                wide[0] = wide[0].wrapping_add(32);
                // Checked block origins/strides leave guard rows and columns.
                unsafe {
                    let mut dst = Block::new(&mut expected[21..], 19, size, size).unwrap();
                    if size == 8 {
                        add8_sse2(&mut dst, &mut wide);
                    } else {
                        let rows = std::array::from_fn(|i| widen4(wide.as_ptr().add(i * 4)));
                        let second = four(transpose4(four(rows).map(|v| truncate(v))));
                        for y in 0..4 {
                            add_store4(dst.data.as_mut_ptr().add(y * dst.stride), second[y]);
                        }
                    }
                    let mut dst = Block::new(&mut actual[21..], 19, size, size).unwrap();
                    if size == 8 {
                        add8_words(&mut dst, &mut coeff);
                    } else {
                        add4_words(&mut dst, (&mut coeff[..16]).try_into().unwrap());
                    }
                }
                assert_eq!(actual, expected, "size={size},sample={sample}");
                assert!(coeff[..size * size].iter().all(|&v| v == 0));
            }
        }
        for sample in 0..1024 {
            let mut coeff: [i16; 32] = std::array::from_fn(|_| {
                if sample & 1 == 0 {
                    if random() & 1 == 0 { 2047 } else { -2047 }
                } else {
                    (random() % 4095) as i16 - 2047
                }
            });
            let mut expected = [43u8; 19 * 7];
            let mut actual = expected;
            let mut wide = coeff;
            unsafe {
                for index in 0..2 {
                    let mut dst = Block::new(&mut expected[21 + index * 4..], 19, 4, 4).unwrap();
                    add4_words(
                        &mut dst,
                        (&mut wide[index * 16..index * 16 + 16]).try_into().unwrap(),
                    );
                }
                add4_pair_words(actual.as_mut_ptr().add(21), 19, &mut coeff);
            }
            assert_eq!(actual, expected);
            assert_eq!(coeff, wide);
        }
    }
    #[test]
    fn macroblock_batches_preserve_sparse_dc_ac_and_guard_pixels() {
        use super::super::super::transform;
        let mut seed = 0x33abcde1u32;
        let mut random = || {
            seed = seed.wrapping_mul(1664525).wrapping_add(1013904223);
            seed
        };
        for (size, transform8, separate_dc) in [
            (8, false, true),
            (16, false, true),
            (16, false, false),
            (16, true, false),
        ] {
            let n = if transform8 { 8 } else { 4 };
            for _ in 0..256 {
                let mut coeff = [0i16; 256];
                let mut nnz = [0u8; 16];
                for i in 0..size * size / (n * n) {
                    let kind = random() % 4;
                    let offset = i * n * n;
                    let nc = i * (if transform8 { 4 } else { 1 });
                    if kind == 1 {
                        coeff[offset] = (random() >> 16) as i16;
                        nnz[nc] = u8::from(!separate_dc);
                    } else if kind >= 2 {
                        for c in &mut coeff[offset..offset + n * n] {
                            *c = (random() >> 16) as i16;
                        }
                        nnz[nc] = 16;
                    }
                }
                let original = coeff;
                let mut pixels: [u8; 32 * 18] = std::array::from_fn(|_| (random() >> 24) as u8);
                let initial = pixels;
                // A deliberately unaligned origin and larger stride leave
                // guard pixels around every row and at both allocation ends.
                for i in 0..size * size / (n * n) {
                    let offset = i * n * n;
                    let nc = i * (if transform8 { 4 } else { 1 });
                    let (x, y) = transform::plane_block_position(i, size, n);
                    let mut block = Block::new(&mut pixels[35 + y * 32 + x..], 32, n, n).unwrap();
                    if nnz[nc] == 0 || (!separate_dc && nnz[nc] == 1 && coeff[offset] != 0) {
                        if coeff[offset] != 0 {
                            transform::add_dc(&mut block, &mut coeff[offset]).unwrap();
                        }
                    } else if transform8 {
                        transform::add8(
                            &mut block,
                            (&mut coeff[offset..offset + 64]).try_into().unwrap(),
                        )
                        .unwrap();
                    } else {
                        transform::add4(
                            &mut block,
                            (&mut coeff[offset..offset + 16]).try_into().unwrap(),
                        )
                        .unwrap();
                    }
                }
                for avx in [false, true] {
                    if avx && !has_avx2() {
                        continue;
                    }
                    let mut actual = initial;
                    let mut c = original;
                    let mut block = Block::new(&mut actual[35..], 32, size, size).unwrap();
                    // Validated macroblock shape and CPU capability above.
                    unsafe {
                        if avx {
                            add_plane_inner::<true>(
                                &mut block,
                                &mut c,
                                &nnz,
                                transform8,
                                separate_dc,
                            );
                        } else {
                            add_plane_inner::<false>(
                                &mut block,
                                &mut c,
                                &nnz,
                                transform8,
                                separate_dc,
                            );
                        }
                    }
                    assert_eq!(
                        actual, pixels,
                        "size={size}, transform8={transform8}, separate_dc={separate_dc}, avx={avx}"
                    );
                    assert_eq!(c, coeff);
                }
            }
        }
    }
    #[test]
    fn sse2_and_avx2_keep_identical_signed_transform_arithmetic() {
        if !std::is_x86_feature_detected!("avx2") {
            return;
        }
        let mut seed = 0x5566ab32u32;
        for _ in 0..2048 {
            let mut a = [0i16; 64];
            for v in &mut a {
                seed = seed.wrapping_mul(1664525).wrapping_add(1013904223);
                *v = (seed >> 16) as i16;
            }
            a[0] = a[0].wrapping_add(32);
            let mut b = a;
            let mut pixels = [0u8; 16 * 8];
            for v in &mut pixels {
                seed = seed.wrapping_mul(1664525).wrapping_add(1013904223);
                *v = (seed >> 24) as u8;
            }
            let mut other = pixels;
            // Both inputs have the complete checked 8x8 footprint and CPU
            // detection above authorizes AVX2; coefficient arrays are fixed.
            unsafe {
                add8_sse2(&mut Block::new(&mut pixels, 16, 8, 8).unwrap(), &mut a);
                add8_avx2(&mut Block::new(&mut other, 16, 8, 8).unwrap(), &mut b);
            }
            assert_eq!(pixels, other);
        }
    }
}
