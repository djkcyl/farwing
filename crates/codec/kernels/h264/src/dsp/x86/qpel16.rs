// SPDX-License-Identifier: LGPL-2.1-or-later
// FFmpeg six-tap H.264 qpel arithmetic, sixteen simultaneous i16 lanes.
// Copyright (c) 2003-2011 Michael Niedermayer; Rust adaptation OpenUUYC.
use super::*;

#[target_feature(enable = "avx2")]
#[inline]
unsafe fn words16(p: *const u8) -> __m256i {
    unsafe { _mm256_cvtepu8_epi16(_mm_loadu_si128(p.cast())) }
}
#[target_feature(enable = "avx2")]
#[inline]
unsafe fn fir(v: [__m256i; 6]) -> __m256i {
    let middle = _mm256_add_epi16(v[1], v[4]);
    let center = _mm256_add_epi16(v[2], v[3]);
    _mm256_add_epi16(
        _mm256_add_epi16(v[0], v[5]),
        _mm256_mullo_epi16(
            _mm256_sub_epi16(_mm256_slli_epi16::<2>(center), middle),
            _mm256_set1_epi16(5),
        ),
    )
}
#[target_feature(enable = "avx2")]
#[inline]
unsafe fn horizontal(p: *const u8) -> __m256i {
    unsafe { fir(std::array::from_fn(|i| words16(p.add(i)))) }
}
#[target_feature(enable = "avx2")]
#[inline]
unsafe fn vertical(p: *const u8, stride: usize) -> __m256i {
    unsafe { fir(std::array::from_fn(|i| words16(p.add(i * stride)))) }
}
#[target_feature(enable = "avx2")]
#[inline]
unsafe fn bytes(v: __m256i) -> __m128i {
    _mm_packus_epi16(_mm256_castsi256_si128(v), _mm256_extracti128_si256::<1>(v))
}
#[target_feature(enable = "avx2")]
#[inline]
unsafe fn half(v: __m256i) -> __m128i {
    unsafe {
        bytes(_mm256_srai_epi16::<5>(_mm256_add_epi16(
            v,
            _mm256_set1_epi16(16),
        )))
    }
}
#[target_feature(enable = "avx2")]
#[inline]
unsafe fn diagonal(v: [__m256i; 6]) -> __m128i {
    let a = _mm256_set1_epi32((-5i32 << 16) | 1);
    let b = _mm256_set1_epi32((20 << 16) | 20);
    let c = _mm256_set1_epi32((1 << 16) | 65531);
    let calc = |p, q, r| {
        _mm256_srai_epi32::<10>(_mm256_add_epi32(
            _mm256_add_epi32(_mm256_madd_epi16(p, a), _mm256_madd_epi16(q, b)),
            _mm256_add_epi32(_mm256_madd_epi16(r, c), _mm256_set1_epi32(512)),
        ))
    };
    let lo = calc(
        _mm256_unpacklo_epi16(v[0], v[1]),
        _mm256_unpacklo_epi16(v[2], v[3]),
        _mm256_unpacklo_epi16(v[4], v[5]),
    );
    let hi = calc(
        _mm256_unpackhi_epi16(v[0], v[1]),
        _mm256_unpackhi_epi16(v[2], v[3]),
        _mm256_unpackhi_epi16(v[4], v[5]),
    );
    unsafe { bytes(_mm256_packs_epi32(lo, hi)) }
}
/// Parent validates source 21*(height+5) strided footprint, destination 16*h,
/// and CPU/OS AVX2. Width-16 loads at offset 5 consume exactly 21 source bytes.
#[target_feature(enable = "avx2")]
pub(super) unsafe fn run(src: &[u8], stride: usize, fx: u8, fy: u8, dst: &mut Block<'_>) {
    macro_rules! position {
        ($x:literal,$y:literal) => {
            interpolate::<$x, $y>(src, stride, dst)
        };
    }
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
#[target_feature(enable = "avx2")]
#[inline]
unsafe fn interpolate<const X: u8, const Y: u8>(src: &[u8], stride: usize, dst: &mut Block<'_>) {
    unsafe {
        let mut strip = [_mm256_setzero_si256(); 21];
        if X != 0 && Y != 0 && (X == 2 || Y == 2) {
            for y in 0..dst.height + 5 {
                strip[y] = horizontal(src.as_ptr().add(y * stride));
            }
        }
        for y in 0..dst.height {
            let h = || {
                half(horizontal(
                    src.as_ptr().add((y + 2 + usize::from(Y == 3)) * stride),
                ))
            };
            let v = || {
                half(vertical(
                    src.as_ptr().add(y * stride + 2 + usize::from(X == 3)),
                    stride,
                ))
            };
            let j = || diagonal(std::array::from_fn(|i| strip[y + i]));
            let g = || {
                _mm_loadu_si128(
                    src.as_ptr()
                        .add((y + 2 + usize::from(Y == 3)) * stride + 2 + usize::from(X == 3))
                        .cast(),
                )
            };
            let value = match (X, Y) {
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
            _mm_storeu_si128(dst.data.as_mut_ptr().add(y * dst.stride).cast(), value);
        }
    }
}
