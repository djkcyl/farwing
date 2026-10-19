// SPDX-License-Identifier: LGPL-2.1-or-later
// FFmpeg's H.264 chroma interpolation, prediction weighting and chroma loop
// filter arithmetic. Copyright (c) 2003-2011 Michael Niedermayer; Rust OpenUUYC.
use super::*;
#[inline]
unsafe fn load<const W: usize>(p: *const u8) -> __m128i {
    unsafe {
        if W == 2 {
            _mm_cvtsi32_si128(p.cast::<u16>().read_unaligned() as i32)
        } else {
            pixels::<W>(p)
        }
    }
}
#[inline]
unsafe fn store<const W: usize>(p: *mut u8, v: __m128i) {
    unsafe {
        if W == 2 {
            p.cast::<u16>().write_unaligned(_mm_cvtsi128_si32(v) as u16);
        } else if W == 4 {
            p.cast::<i32>().write_unaligned(_mm_cvtsi128_si32(v));
        } else {
            _mm_storel_epi64(p.cast(), v);
        }
    }
}
pub(crate) fn chroma(src: &[u8], stride: usize, fx: i32, fy: i32, dst: &mut Block<'_>) -> bool {
    if !matches!(dst.width, 2 | 4 | 8)
        || !matches!(dst.height, 2 | 4 | 8)
        || !(0..=7).contains(&fx)
        || !(0..=7).contains(&fy)
        || stride < dst.width + 1
        || dst
            .height
            .checked_mul(stride)
            .and_then(|n| n.checked_add(dst.width + 1))
            .is_none_or(|n| n > src.len())
    {
        return false;
    }
    unsafe {
        match dst.width {
            2 => bilinear::<2>(src, stride, fx, fy, dst),
            4 => bilinear::<4>(src, stride, fx, fy, dst),
            8 => bilinear::<8>(src, stride, fx, fy, dst),
            _ => unreachable!(),
        }
    }
    true
}
/// Unit weights reduce exactly to clipping sample+offset, regardless of the
/// denominator's rounding term. Saturating byte instructions process a full
/// row without unpacking, multiplication or variable shifts.
unsafe fn translate(dst: &mut Block<'_>, offset: i16) {
    unsafe {
        let amount = _mm_set1_epi8(offset.unsigned_abs() as i8);
        macro_rules! rows {
            ($width:literal) => {
                for y in 0..dst.height {
                    let p = dst.data.as_mut_ptr().add(y * dst.stride);
                    let value = load::<$width>(p);
                    let value = if offset < 0 {
                        _mm_subs_epu8(value, amount)
                    } else {
                        _mm_adds_epu8(value, amount)
                    };
                    store::<$width>(p, value);
                }
            };
        }
        match dst.width {
            2 => rows!(2),
            4 => rows!(4),
            8 => rows!(8),
            16 => {
                for y in 0..dst.height {
                    let p = dst.data.as_mut_ptr().add(y * dst.stride);
                    let value = _mm_loadu_si128(p.cast());
                    let value = if offset < 0 {
                        _mm_subs_epu8(value, amount)
                    } else {
                        _mm_adds_epu8(value, amount)
                    };
                    _mm_storeu_si128(p.cast(), value);
                }
            }
            _ => unreachable!(),
        }
    }
}
#[target_feature(enable = "avx2")]
unsafe fn weighted16_avx2(dst: &mut Block<'_>, denom: u8, factor: i16, offset: i16) {
    unsafe {
        let weight = _mm256_set1_epi16(factor);
        let offset = _mm256_set1_epi16(offset);
        let round = _mm256_set1_epi16(if denom == 0 { 0 } else { 1 << (denom - 1) });
        let shift = _mm_cvtsi32_si128(denom as i32);
        for y in 0..dst.height {
            let p = dst.data.as_mut_ptr().add(y * dst.stride);
            let value = _mm256_cvtepu8_epi16(_mm_loadu_si128(p.cast()));
            let value = _mm256_add_epi16(
                _mm256_sra_epi16(
                    _mm256_add_epi16(_mm256_mullo_epi16(value, weight), round),
                    shift,
                ),
                offset,
            );
            _mm_storeu_si128(
                p.cast(),
                _mm_packus_epi16(
                    _mm256_castsi256_si128(value),
                    _mm256_extracti128_si256::<1>(value),
                ),
            );
        }
    }
}
unsafe fn bilinear<const W: usize>(
    src: &[u8],
    stride: usize,
    fx: i32,
    fy: i32,
    dst: &mut Block<'_>,
) {
    unsafe {
        let z = _mm_setzero_si128();
        let a = _mm_set1_epi16(((8 - fx) * (8 - fy)) as i16);
        let b = _mm_set1_epi16((fx * (8 - fy)) as i16);
        let c = _mm_set1_epi16(((8 - fx) * fy) as i16);
        let d = _mm_set1_epi16((fx * fy) as i16);
        for y in 0..dst.height {
            let p = src.as_ptr().add(y * stride);
            let row = |at| _mm_unpacklo_epi8(load::<W>(at), z);
            let upper = _mm_add_epi16(
                _mm_mullo_epi16(row(p), a),
                _mm_mullo_epi16(row(p.add(1)), b),
            );
            let lower = _mm_add_epi16(
                _mm_mullo_epi16(row(p.add(stride)), c),
                _mm_mullo_epi16(row(p.add(stride + 1)), d),
            );
            let v = _mm_srai_epi16::<6>(_mm_add_epi16(
                _mm_add_epi16(upper, lower),
                _mm_set1_epi16(32),
            ));
            store::<W>(
                dst.data.as_mut_ptr().add(y * dst.stride),
                _mm_packus_epi16(v, z),
            );
        }
    }
}
pub(crate) fn weight(dst: &mut Block<'_>, denom: u8, factor: i16, offset: i16) -> bool {
    if !matches!(dst.width, 2 | 4 | 8 | 16)
        || denom > 7
        || !(-128..=128).contains(&factor)
        || !(-128..=127).contains(&offset)
    {
        return false;
    }
    unsafe {
        if factor == 1 << denom {
            translate(dst, offset);
            return true;
        }
        if dst.width == 16 && has_avx2() {
            weighted16_avx2(dst, denom, factor, offset);
            return true;
        }
        match dst.width {
            2 => weighted::<2>(dst, denom, factor, offset),
            4 => weighted::<4>(dst, denom, factor, offset),
            8 => weighted::<8>(dst, denom, factor, offset),
            16 => weighted::<16>(dst, denom, factor, offset),
            _ => unreachable!(),
        }
    }
    true
}
pub(crate) fn weighted_copy(
    src: &[u8],
    stride: usize,
    dst: &mut Block<'_>,
    denom: u8,
    factor: i16,
    offset: i16,
) -> bool {
    if !matches!(dst.width, 4 | 8 | 16)
        || stride < dst.width
        || denom > 7
        || !(-128..=128).contains(&factor)
        || !(-128..=127).contains(&offset)
        || (dst.height - 1)
            .checked_mul(stride)
            .and_then(|v| v.checked_add(dst.width))
            .is_none_or(|end| end > src.len())
    {
        return false;
    }
    // Fixed-width source loads are fully covered above. Block owns a validated
    // destination footprint, and shared/exclusive slices cannot alias.
    unsafe {
        match dst.width {
            4 => weighted_copy_inner::<4>(src, stride, dst, denom, factor, offset),
            8 => weighted_copy_inner::<8>(src, stride, dst, denom, factor, offset),
            16 => weighted_copy_inner::<16>(src, stride, dst, denom, factor, offset),
            _ => unreachable!(),
        }
    }
    true
}
unsafe fn weighted_copy_inner<const W: usize>(
    src: &[u8],
    stride: usize,
    dst: &mut Block<'_>,
    denom: u8,
    factor: i16,
    offset: i16,
) {
    unsafe {
        let amount = _mm_set1_epi8(offset.unsigned_abs() as i8);
        let factor_v = _mm_set1_epi16(factor);
        let offset_v = _mm_set1_epi16(offset);
        let round = _mm_set1_epi16(if denom == 0 { 0 } else { 1 << (denom - 1) });
        let shift = _mm_cvtsi32_si128(denom as i32);
        for y in 0..dst.height {
            if factor == 1 << denom && W == 16 {
                let v = _mm_loadu_si128(src.as_ptr().add(y * stride).cast());
                let v = if offset < 0 {
                    _mm_subs_epu8(v, amount)
                } else {
                    _mm_adds_epu8(v, amount)
                };
                _mm_storeu_si128(dst.data.as_mut_ptr().add(y * dst.stride).cast(), v);
                continue;
            }
            for chunk in 0..if W == 16 { 2 } else { 1 } {
                let v = load::<W>(src.as_ptr().add(y * stride + chunk * 8));
                let v = if factor == 1 << denom {
                    if offset < 0 {
                        _mm_subs_epu8(v, amount)
                    } else {
                        _mm_adds_epu8(v, amount)
                    }
                } else {
                    let v = _mm_add_epi16(
                        _mm_sra_epi16(
                            _mm_add_epi16(
                                _mm_mullo_epi16(
                                    _mm_unpacklo_epi8(v, _mm_setzero_si128()),
                                    factor_v,
                                ),
                                round,
                            ),
                            shift,
                        ),
                        offset_v,
                    );
                    _mm_packus_epi16(v, _mm_setzero_si128())
                };
                store::<W>(dst.data.as_mut_ptr().add(y * dst.stride + chunk * 8), v);
            }
        }
    }
}
unsafe fn weighted<const W: usize>(dst: &mut Block<'_>, denom: u8, factor: i16, offset: i16) {
    unsafe {
        let z = _mm_setzero_si128();
        let weight = _mm_set1_epi16(factor);
        let offset = _mm_set1_epi16(offset);
        let round = _mm_set1_epi16(if denom == 0 { 0 } else { 1 << (denom - 1) });
        let shift = _mm_cvtsi32_si128(denom as i32);
        for y in 0..dst.height {
            for chunk in 0..if W == 16 { 2 } else { 1 } {
                let p = dst.data.as_mut_ptr().add(y * dst.stride + chunk * 8);
                let v = _mm_unpacklo_epi8(load::<W>(p), z);
                let v = _mm_add_epi16(
                    _mm_sra_epi16(_mm_add_epi16(_mm_mullo_epi16(v, weight), round), shift),
                    offset,
                );
                store::<W>(p, _mm_packus_epi16(v, z));
            }
        }
    }
}

#[cfg(test)]
mod weight_tests {
    use super::*;

    #[test]
    fn fused_and_in_place_weights_match_scalar_with_short_rows_and_clipping() {
        for width in [4, 8, 16] {
            let height = 8;
            let source: Vec<u8> = (0..width * height).map(|i| (i * 71 + 19) as u8).collect();
            for denom in 0..=7 {
                for factor in [-128, -1, 0, 1, 2, 127, 128, 1i16 << denom] {
                    for offset in [-128, -17, 0, 11, 127] {
                        let mut expected = [91u8; 23 * 10];
                        let mut copied = expected;
                        let mut in_place = expected;
                        let round = if denom == 0 { 0 } else { 1 << (denom - 1) };
                        for y in 0..height {
                            in_place[25 + y * 23..25 + y * 23 + width]
                                .copy_from_slice(&source[y * width..(y + 1) * width]);
                            for x in 0..width {
                                expected[25 + y * 23 + x] =
                                    (((source[y * width + x] as i32 * factor as i32 + round)
                                        >> denom)
                                        + offset as i32)
                                        .clamp(0, 255) as u8;
                            }
                        }
                        assert!(weighted_copy(
                            &source,
                            width,
                            &mut Block::new(&mut copied[25..], 23, width, height).unwrap(),
                            denom,
                            factor,
                            offset
                        ));
                        assert!(weight(
                            &mut Block::new(&mut in_place[25..], 23, width, height).unwrap(),
                            denom,
                            factor,
                            offset
                        ));
                        assert_eq!(copied, expected);
                        assert_eq!(in_place, expected);
                    }
                }
            }
            let mut guard = [73u8; 23 * 10];
            assert!(!weighted_copy(
                &source[..source.len() - 1],
                width,
                &mut Block::new(&mut guard[25..], 23, width, height).unwrap(),
                7,
                128,
                -1
            ));
            assert!(guard.iter().all(|&v| v == 73));
        }
    }
}
pub(crate) fn chroma_filter(
    view: crate::dsp::filter::ValidatedEdge<'_, true>,
    alpha: i32,
    beta: i32,
    bs: [u8; 4],
    tc: [i16; 4],
) {
    let (data, stride, edge) = view.into_parts();
    let (x, y, vertical) = (edge.x, edge.y, edge.vertical);
    unsafe {
        filter_chroma(
            data.as_mut_ptr().add(y * stride + x),
            stride,
            vertical,
            alpha as i16,
            beta as i16,
            bs,
            tc,
        )
    };
}
unsafe fn filter_chroma(
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
            q.sub(2)
        } else {
            q.sub(2 * stride)
        };
        let z = _mm_setzero_si128();
        let bytes: [__m128i; 4] = if vertical {
            let pairs = transpose8(std::array::from_fn(|y| load::<4>(start.add(y * stride))));
            [
                pairs[0],
                _mm_srli_si128::<8>(pairs[0]),
                pairs[1],
                _mm_srli_si128::<8>(pairs[1]),
            ]
        } else {
            std::array::from_fn(|i| load::<8>(start.add(i * stride)))
        };
        let [p1, p0, q0, q1] = bytes.map(|v| _mm_unpacklo_epi8(v, z));
        let abs = |v| {
            let s = _mm_srai_epi16::<15>(v);
            _mm_sub_epi16(_mm_xor_si128(v, s), s)
        };
        let select = |m, a, b| _mm_or_si128(_mm_and_si128(m, a), _mm_andnot_si128(m, b));
        let s = _mm_set_epi16(
            bs[3] as i16,
            bs[3] as i16,
            bs[2] as i16,
            bs[2] as i16,
            bs[1] as i16,
            bs[1] as i16,
            bs[0] as i16,
            bs[0] as i16,
        );
        let t = _mm_set_epi16(
            tc[3] + 1,
            tc[3] + 1,
            tc[2] + 1,
            tc[2] + 1,
            tc[1] + 1,
            tc[1] + 1,
            tc[0] + 1,
            tc[0] + 1,
        );
        let enabled = _mm_and_si128(
            _mm_cmpgt_epi16(s, z),
            _mm_and_si128(
                _mm_cmpgt_epi16(_mm_set1_epi16(alpha), abs(_mm_sub_epi16(p0, q0))),
                _mm_and_si128(
                    _mm_cmpgt_epi16(_mm_set1_epi16(beta), abs(_mm_sub_epi16(p1, p0))),
                    _mm_cmpgt_epi16(_mm_set1_epi16(beta), abs(_mm_sub_epi16(q1, q0))),
                ),
            ),
        );
        if _mm_movemask_epi8(enabled) == 0 {
            return;
        }
        let four = _mm_set1_epi16(4);
        let two = _mm_set1_epi16(2);
        let delta = _mm_srai_epi16::<3>(_mm_add_epi16(
            _mm_add_epi16(
                _mm_slli_epi16::<2>(_mm_sub_epi16(q0, p0)),
                _mm_sub_epi16(p1, q1),
            ),
            four,
        ));
        let delta = _mm_max_epi16(_mm_sub_epi16(z, t), _mm_min_epi16(t, delta));
        let strong = _mm_cmpeq_epi16(s, four);
        let p = select(
            enabled,
            select(
                strong,
                _mm_srai_epi16::<2>(_mm_add_epi16(
                    _mm_add_epi16(_mm_slli_epi16::<1>(p1), p0),
                    _mm_add_epi16(q1, two),
                )),
                _mm_add_epi16(p0, delta),
            ),
            p0,
        );
        let q = select(
            enabled,
            select(
                strong,
                _mm_srai_epi16::<2>(_mm_add_epi16(
                    _mm_add_epi16(_mm_slli_epi16::<1>(q1), q0),
                    _mm_add_epi16(p1, two),
                )),
                _mm_sub_epi16(q0, delta),
            ),
            q0,
        );
        let p = _mm_packus_epi16(p, z);
        let q = _mm_packus_epi16(q, z);
        if vertical {
            let rows = transpose8([bytes[0], p, q, bytes[3], z, z, z, z]);
            for y in 0..8 {
                store::<4>(
                    start.add(y * stride),
                    if y % 2 == 0 {
                        rows[y / 2]
                    } else {
                        _mm_srli_si128::<8>(rows[y / 2])
                    },
                );
            }
        } else {
            store::<8>(start.add(stride), p);
            store::<8>(start.add(2 * stride), q);
        }
    }
}
