// SPDX-License-Identifier: LGPL-2.1-or-later
// FFmpeg h264pred_template.c directional/plane prediction, using packed FIR,
// row loads and transposes. Copyright (c) 2003-2011 Michael Niedermayer.
use super::super::intra::Edges;
use super::*;

pub(crate) fn average3(src: &[u8; 18]) -> [u8; 16] {
    let mut output = [0; 16];
    unsafe {
        for x in [0, 8] {
            let p = src.as_ptr().add(x);
            let a = words::<8>(p);
            let b = words::<8>(p.add(1));
            let c = words::<8>(p.add(2));
            let sum = _mm_add_epi16(
                _mm_add_epi16(a, c),
                _mm_add_epi16(_mm_slli_epi16::<1>(b), _mm_set1_epi16(2)),
            );
            _mm_storel_epi64(
                output.as_mut_ptr().add(x).cast(),
                _mm_packus_epi16(_mm_srli_epi16::<2>(sum), _mm_setzero_si128()),
            );
        }
    }
    output
}
#[inline]
unsafe fn lines<const HALF: bool, const SMOOTH: bool>(
    src: &[u8; 34],
    count: usize,
) -> ([u8; 32], [u8; 32]) {
    unsafe {
        let mut half = [0; 32];
        let mut smooth = [0; 32];
        let z = _mm_setzero_si128();
        for x in (0..count).step_by(8) {
            let p = src.as_ptr().add(x);
            let a = pixels::<8>(p);
            let b = pixels::<8>(p.add(1));
            if HALF {
                _mm_storel_epi64(half.as_mut_ptr().add(x).cast(), _mm_avg_epu8(a, b));
            }
            if SMOOTH {
                let a = _mm_unpacklo_epi8(a, z);
                let b = _mm_unpacklo_epi8(b, z);
                let c = words::<8>(p.add(2));
                let sum = _mm_add_epi16(
                    _mm_add_epi16(a, c),
                    _mm_add_epi16(_mm_slli_epi16::<1>(b), _mm_set1_epi16(2)),
                );
                _mm_storel_epi64(
                    smooth.as_mut_ptr().add(x).cast(),
                    _mm_packus_epi16(_mm_srli_epi16::<2>(sum), z),
                );
            }
        }
        (half, smooth)
    }
}
pub(crate) fn intra_nxn(dst: &mut Block<'_>, e: &Edges, mode: u8) -> bool {
    if !matches!(dst.width, 4 | 8) || dst.width != dst.height || !(3..=8).contains(&mode) {
        return false;
    }
    macro_rules! mode {
        ($m:literal) => {
            if dst.width == 4 {
                nxn::<4, $m>(dst, e)
            } else {
                nxn::<8, $m>(dst, e)
            }
        };
    }
    unsafe {
        match mode {
            3 => mode!(3),
            4 => mode!(4),
            5 => mode!(5),
            6 => mode!(6),
            7 => mode!(7),
            8 => mode!(8),
            _ => unreachable!(),
        }
    }
    true
}
unsafe fn nxn<const N: usize, const MODE: u8>(dst: &mut Block<'_>, e: &Edges) {
    unsafe {
        let mut source = [0u8; 34];
        if MODE == 3 || MODE == 7 {
            for (i, v) in source.iter_mut().enumerate() {
                *v = e.top[i.min(2 * N - 1)];
            }
        } else if MODE == 8 {
            for (i, v) in source.iter_mut().enumerate() {
                *v = e.left[i.min(N - 1)];
            }
        } else {
            for i in 0..N {
                source[i] = if MODE == 6 {
                    e.top[N - 1 - i]
                } else {
                    e.left[N - 1 - i]
                };
            }
            source[N] = e.corner;
            for i in N + 1..34 {
                source[i] = if MODE == 6 {
                    e.left[(i - N - 1).min(N - 1)]
                } else {
                    e.top[(i - N - 1).min(N - 1)]
                };
            }
        }
        let (half, smooth) = if MODE == 3 || MODE == 4 {
            lines::<false, true>(&source, 2 * N)
        } else {
            lines::<true, true>(&source, 2 * N)
        };
        let mut combined = [0u8; 32];
        if MODE == 8 {
            for x in (0..2 * N).step_by(8) {
                _mm_storeu_si128(
                    combined.as_mut_ptr().add(2 * x).cast(),
                    _mm_unpacklo_epi8(
                        pixels::<8>(half.as_ptr().add(x)),
                        pixels::<8>(smooth.as_ptr().add(x)),
                    ),
                );
            }
        }
        let mut rows = [_mm_setzero_si128(); 8];
        for y in 0..N {
            rows[y] = if MODE == 3 {
                pixels::<N>(smooth.as_ptr().add(y))
            } else if MODE == 4 {
                pixels::<N>(smooth.as_ptr().add(N - y - 1))
            } else if MODE == 7 {
                pixels::<N>(if y % 2 == 0 {
                    half.as_ptr().add(y / 2)
                } else {
                    smooth.as_ptr().add(y / 2)
                })
            } else if MODE == 8 {
                pixels::<N>(combined.as_ptr().add(y * 2))
            } else {
                let mut value = pixels::<N>(if y % 2 == 0 {
                    half.as_ptr().add(N - y / 2)
                } else {
                    smooth.as_ptr().add(N - y / 2 - 1)
                });
                let prefix = y.div_ceil(2);
                let mut replacement = 0u32;
                for x in 0..prefix {
                    replacement |= (smooth[N - y + 2 * x] as u32) << (8 * x);
                }
                let mask = if prefix == 4 {
                    u32::MAX
                } else {
                    (1u32 << (8 * prefix)) - 1
                };
                let mask = _mm_cvtsi32_si128(mask as i32);
                value = _mm_or_si128(
                    _mm_andnot_si128(mask, value),
                    _mm_and_si128(mask, _mm_cvtsi32_si128(replacement as i32)),
                );
                value
            };
        }
        if MODE == 6 {
            let t = transpose8(rows);
            rows = std::array::from_fn(|i| {
                if i % 2 == 0 {
                    t[i / 2]
                } else {
                    _mm_srli_si128::<8>(t[i / 2])
                }
            });
        }
        for y in 0..N {
            let p = dst.data.as_mut_ptr().add(y * dst.stride);
            if N == 4 {
                p.cast::<i32>().write_unaligned(_mm_cvtsi128_si32(rows[y]));
            } else {
                _mm_storel_epi64(p.cast(), rows[y]);
            }
        }
    }
}
pub(crate) fn plane(dst: &mut Block<'_>, a: i32, b: i32, c: i32, center: i32) -> bool {
    if !matches!(dst.width, 8 | 16) || dst.width != dst.height {
        return false;
    }
    // Valid byte edges bound these gradients. Retain a checked scalar fallback
    // for any future caller outside that contract, rather than truncating it.
    if a.unsigned_abs() > 8160
        || b.unsigned_abs() > 1400
        || c.unsigned_abs() > 1400
        || !matches!((dst.width, center), (8, 3) | (16, 7))
    {
        return false;
    }
    unsafe {
        let xs = _mm_sub_epi16(
            _mm_set_epi16(7, 6, 5, 4, 3, 2, 1, 0),
            _mm_set1_epi16(center as i16),
        );
        for y in 0..dst.height {
            for part in 0..dst.width / 8 {
                let x = _mm_add_epi16(xs, _mm_set1_epi16((part * 8) as i16));
                let bias = (a + c * (y as i32 - center) + 16) as i16;
                let v = _mm_srai_epi16::<5>(_mm_add_epi16(
                    _mm_mullo_epi16(x, _mm_set1_epi16(b as i16)),
                    _mm_set1_epi16(bias),
                ));
                _mm_storel_epi64(
                    dst.data.as_mut_ptr().add(y * dst.stride + part * 8).cast(),
                    _mm_packus_epi16(v, _mm_setzero_si128()),
                );
            }
        }
    }
    true
}
