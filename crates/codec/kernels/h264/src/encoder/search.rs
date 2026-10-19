// SPDX-License-Identifier: LGPL-2.1-or-later
use super::Result;
use crate::{
    dsp::{self, Block},
    picture::Plane,
};

#[derive(Clone, Copy, Default, Debug, PartialEq, Eq)]
pub(super) struct Mv {
    pub x: i16,
    pub y: i16,
}
impl Mv {
    pub const ZERO: Self = Self { x: 0, y: 0 };
}
pub(super) fn predictor(a: Option<Mv>, b: Option<Mv>, c: Option<Mv>) -> Mv {
    if b.is_none() && c.is_none() {
        return a.unwrap_or_default();
    }
    let available = [a, b, c];
    if available.iter().filter(|x| x.is_some()).count() == 1 {
        return available.into_iter().flatten().next().unwrap();
    }
    let [a, b, c] = available.map(|x| x.unwrap_or_default());
    fn med(a: i16, b: i16, c: i16) -> i16 {
        a.min(b).max(a.max(b).min(c))
    }
    Mv {
        x: med(a.x, b.x, c.x),
        y: med(a.y, b.y, c.y),
    }
}
pub(super) fn sad(a: &[u8], sa: usize, b: &[u8], sb: usize, n: usize) -> u32 {
    debug_assert!(a.len() >= (n - 1) * sa + n && b.len() >= (n - 1) * sb + n);
    #[cfg(all(target_arch = "x86_64", not(feature = "scalar-dsp")))]
    if n == 16 {
        return simd::sad16(a, sa, b, sb);
    }
    #[cfg(all(target_arch = "x86_64", not(feature = "scalar-dsp")))]
    if n == 8 {
        return simd::sad8(a, sa, b, sb);
    }
    let mut sum = 0;
    for y in 0..n {
        for x in 0..n {
            sum += a[y * sa + x].abs_diff(b[y * sb + x]) as u32;
        }
    }
    sum
}
/// Four raster-ordered 8x8 SADs from one 16x16 load pass. Source analysis
/// consumes the individual quadrants, not only their aggregate.
pub(super) fn sad_quadrants(a: &[u8], b: &[u8], stride: usize) -> [u32; 4] {
    assert!(a.len() >= 16 && stride <= (a.len() - 16) / 15);
    assert!(b.len() >= 16 && stride <= (b.len() - 16) / 15);
    #[cfg(all(target_arch = "x86_64", not(feature = "scalar-dsp")))]
    {
        simd::sad_quadrants(a, b, stride)
    }
    #[cfg(not(all(target_arch = "x86_64", not(feature = "scalar-dsp"))))]
    {
        std::array::from_fn(|i| {
            let at = i / 2 * 8 * stride + i % 2 * 8;
            sad(&a[at..], stride, &b[at..], stride, 8)
        })
    }
}
/// Exact min of the two source-edge predictors, bounded by the inter cost.
/// The screen RC producer needs only this minimum, not prediction buffers.
pub(super) fn source_intra_sad(src: &[u8], stride: usize, x: usize, y: usize, best: u32) -> u32 {
    assert!(x + 16 <= stride && src.len() >= 16 && (y + 15) * stride + x <= src.len() - 16);
    #[cfg(all(target_arch = "x86_64", not(feature = "scalar-dsp")))]
    {
        return simd::source_intra_sad(src, stride, x, y, best);
    }
    #[cfg(not(all(target_arch = "x86_64", not(feature = "scalar-dsp"))))]
    {
        let mut best = best;
        for vertical in [true, false] {
            if best == 0 || (vertical && y == 0) || (!vertical && x == 0) {
                continue;
            }
            let mut cost = 0;
            for row in 0..16 {
                for col in 0..16 {
                    let predicted = if vertical {
                        src[(y - 1) * stride + x + col]
                    } else {
                        src[(y + row) * stride + x - 1]
                    };
                    cost += u32::from(src[(y + row) * stride + x + col].abs_diff(predicted));
                }
                if cost >= best {
                    break;
                }
            }
            best = best.min(cost);
        }
        best
    }
}
/// A partial SAD is sufficient once it reaches the remaining candidate budget.
/// Only a winning candidate consumes the returned distortion, which is exact.
#[inline]
fn sad_until(a: &[u8], sa: usize, b: &[u8], sb: usize, n: usize, limit: u32) -> u32 {
    #[cfg(all(target_arch = "x86_64", not(feature = "scalar-dsp")))]
    {
        if n == 16 {
            return simd::sad_until::<16>(a, sa, b, sb, limit);
        }
        return simd::sad_until::<8>(a, sa, b, sb, limit);
    }
    #[cfg(not(all(target_arch = "x86_64", not(feature = "scalar-dsp"))))]
    {
        let mut sum = 0;
        for y in 0..n {
            for x in 0..n {
                sum += u32::from(a[y * sa + x].abs_diff(b[y * sb + x]));
            }
            if y % 4 == 3 && sum >= limit {
                break;
            }
        }
        sum
    }
}
#[inline]
pub(super) fn motion_cost(mv: Mv, pred: Mv, lambda: u32) -> u32 {
    lambda
        * (mv_bits(i32::from(mv.x) - i32::from(pred.x))
            + mv_bits(i32::from(mv.y) - i32::from(pred.y)))
}
#[cfg(all(target_arch = "x86_64", not(feature = "scalar-dsp")))]
#[allow(unsafe_code)]
mod simd {
    use std::arch::x86_64::*;
    pub(super) fn sad_quadrants(a: &[u8], b: &[u8], stride: usize) -> [u32; 4] {
        // The parent checked both complete 16x16 footprints. PSADBW keeps
        // left/right eight-pixel sums in separate lanes, each <=16320.
        unsafe {
            let mut out = [0; 4];
            for half in 0..2 {
                let mut sum = _mm_setzero_si128();
                for y in half * 8..half * 8 + 8 {
                    sum = _mm_add_epi64(
                        sum,
                        _mm_sad_epu8(
                            _mm_loadu_si128(a.as_ptr().add(y * stride).cast()),
                            _mm_loadu_si128(b.as_ptr().add(y * stride).cast()),
                        ),
                    );
                }
                out[half * 2] = _mm_cvtsi128_si64(sum) as u32;
                out[half * 2 + 1] = _mm_cvtsi128_si64(_mm_srli_si128::<8>(sum)) as u32;
            }
            out
        }
    }
    pub(super) fn source_intra_sad(
        src: &[u8],
        stride: usize,
        x: usize,
        y: usize,
        mut best: u32,
    ) -> u32 {
        // Parent validated the full source block. Top/left loads are guarded
        // by their actual availability and cannot precede the allocation.
        unsafe {
            let source = src.as_ptr().add(y * stride + x);
            for vertical in [true, false] {
                if best == 0 || (vertical && y == 0) || (!vertical && x == 0) {
                    continue;
                }
                let top = if vertical {
                    _mm_loadu_si128(source.sub(stride).cast())
                } else {
                    _mm_setzero_si128()
                };
                let mut sum = _mm_setzero_si128();
                let mut cost = 0;
                for block in 0..4 {
                    for row in block * 4..block * 4 + 4 {
                        let a = _mm_loadu_si128(source.add(row * stride).cast());
                        let p = if vertical {
                            top
                        } else {
                            _mm_set1_epi8(*source.add(row * stride).sub(1) as i8)
                        };
                        sum = _mm_add_epi64(sum, _mm_sad_epu8(a, p));
                    }
                    cost = (_mm_cvtsi128_si64(sum) + _mm_cvtsi128_si64(_mm_srli_si128::<8>(sum)))
                        as u32;
                    if cost >= best {
                        break;
                    }
                }
                best = best.min(cost);
            }
            best
        }
    }
    pub fn line_search(
        src: &[u8],
        ss: usize,
        reference: &super::Plane,
        x: usize,
        y: usize,
        n: usize,
        pred: super::Mv,
        lambda: u32,
        vertical: bool,
        lo: i32,
        hi: i32,
        limit: u32,
    ) -> Option<(super::Mv, u32)> {
        if !std::arch::is_x86_feature_detected!("sse4.1") {
            return None;
        }
        let count = (hi - lo) as usize;
        if count == 0 {
            return None;
        }
        assert!(count <= 1008 && matches!(n, 8 | 16));
        assert!(src.len() >= (n - 1) * ss + n);
        let (input, rs) = if vertical {
            reference.footprint(x as i32, y as i32 + lo, n, (count + n).next_multiple_of(8))?
        } else {
            reference.footprint(x as i32 + lo, y as i32, count + n, n)?
        };
        // One footprint check precedes all SIMD loads and transpose tiles.
        Some(unsafe {
            if std::is_x86_feature_detected!("avx2") {
                line_avx2(
                    src, ss, input, rs, n, pred, lambda, vertical, lo, count, limit,
                )
            } else {
                line_sse41(
                    src, ss, input, rs, n, pred, lambda, vertical, lo, count, limit,
                )
            }
        })
    }
    #[target_feature(enable = "sse4.1")]
    unsafe fn line_sse41(
        src: &[u8],
        ss: usize,
        input: &[u8],
        rs: usize,
        n: usize,
        pred: super::Mv,
        lambda: u32,
        vertical: bool,
        lo: i32,
        count: usize,
        limit: u32,
    ) -> (super::Mv, u32) {
        unsafe {
            if n == 16 {
                line::<false, 16>(src, ss, input, rs, pred, lambda, vertical, lo, count, limit)
            } else {
                line::<false, 8>(src, ss, input, rs, pred, lambda, vertical, lo, count, limit)
            }
        }
    }
    #[target_feature(enable = "avx2")]
    unsafe fn line_avx2(
        src: &[u8],
        ss: usize,
        input: &[u8],
        rs: usize,
        n: usize,
        pred: super::Mv,
        lambda: u32,
        vertical: bool,
        lo: i32,
        count: usize,
        limit: u32,
    ) -> (super::Mv, u32) {
        unsafe {
            if n == 16 {
                line::<true, 16>(src, ss, input, rs, pred, lambda, vertical, lo, count, limit)
            } else {
                line::<true, 8>(src, ss, input, rs, pred, lambda, vertical, lo, count, limit)
            }
        }
    }
    #[inline(always)]
    unsafe fn line<const WIDE: bool, const N: usize>(
        src: &[u8],
        ss: usize,
        input: &[u8],
        rs: usize,
        pred: super::Mv,
        lambda: u32,
        vertical: bool,
        lo: i32,
        count: usize,
        limit: u32,
    ) -> (super::Mv, u32) {
        let n = N;
        // One component is fixed throughout an axis. Its cost plus a one-bit
        // varying component is a lower bound for every candidate on the axis.
        let (axis_pred, other_pred) = if vertical {
            (pred.y, pred.x)
        } else {
            (pred.x, pred.y)
        };
        let fixed = lambda * super::mv_bits(-i32::from(other_pred));
        let floor = fixed + lambda;
        if limit <= floor {
            return (super::Mv::ZERO, limit);
        }
        unsafe {
            let mut source = [0u8; 256];
            let mut transposed = [0u8; 1040 * 16];
            let (a, sa, b, sb) = if vertical {
                // Transpose initializes exactly n rows of n source bytes and
                // rounded(count+n) reference bytes. Every SIMD load below is
                // inside those rows; unused stride padding is never read.
                transpose(src.as_ptr(), ss, source.as_mut_ptr().cast(), 16, n, n);
                transpose(
                    input.as_ptr(),
                    rs,
                    transposed.as_mut_ptr().cast(),
                    1040,
                    n,
                    (count + n).next_multiple_of(8),
                );
                (source.as_ptr().cast(), 16, transposed.as_ptr().cast(), 1040)
            } else {
                (src.as_ptr(), ss, input.as_ptr(), rs)
            };
            let mut best = super::Mv::ZERO;
            let mut cost = limit;
            let mut at = 0;
            'groups: while at + 8 <= count {
                let batch = if WIDE && at + 16 <= count { 16 } else { 8 };
                // MV bit length is monotone in absolute distance. Clamping
                // to the continuous batch interval is conservative even when
                // the predictor lies between its integer-pixel candidates.
                let first_delta = (lo + at as i32) * 4;
                let last_delta = first_delta + (batch as i32 - 1) * 4;
                let nearest = i32::from(axis_pred).clamp(first_delta, last_delta);
                let group_floor = fixed + lambda * super::mv_bits(nearest - i32::from(axis_pred));
                if group_floor >= cost {
                    at += batch;
                    continue;
                }
                let mut values = [0u16; 16];
                if WIDE && batch == 16 {
                    let mut sad = _mm256_setzero_si256();
                    for row in 0..n {
                        let av = if n == 16 {
                            _mm_loadu_si128(a.add(row * sa).cast())
                        } else {
                            _mm_loadl_epi64(a.add(row * sa).cast())
                        };
                        let av = _mm256_broadcastsi128_si256(av);
                        let load = |offset| {
                            _mm256_inserti128_si256::<1>(
                                _mm256_castsi128_si256(_mm_loadu_si128(
                                    b.add(row * sb + at + offset).cast(),
                                )),
                                _mm_loadu_si128(b.add(row * sb + at + offset + 8).cast()),
                            )
                        };
                        // AVX2 encodes the MPSADBW control for each 128-bit
                        // lane: 45=5|(5<<3), 18=2|(2<<3), 63=7|(7<<3).
                        let bv = load(0);
                        sad = _mm256_add_epi16(
                            sad,
                            _mm256_add_epi16(
                                _mm256_mpsadbw_epu8::<0>(bv, av),
                                _mm256_mpsadbw_epu8::<45>(bv, av),
                            ),
                        );
                        if n == 16 {
                            let bv = load(8);
                            sad = _mm256_add_epi16(
                                sad,
                                _mm256_add_epi16(
                                    _mm256_mpsadbw_epu8::<18>(bv, av),
                                    _mm256_mpsadbw_epu8::<63>(bv, av),
                                ),
                            );
                        }
                        if row % 4 == 3 {
                            let lo = _mm_minpos_epu16(_mm256_castsi256_si128(sad));
                            let hi = _mm_minpos_epu16(_mm256_extracti128_si256::<1>(sad));
                            if (_mm_extract_epi16::<0>(_mm_min_epu16(lo, hi)) as u32)
                                >= cost - group_floor
                            {
                                at += batch;
                                continue 'groups;
                            }
                        }
                    }
                    _mm256_storeu_si256(values.as_mut_ptr().cast(), sad);
                } else {
                    let mut sad = _mm_setzero_si128();
                    for row in 0..n {
                        let av = if n == 16 {
                            _mm_loadu_si128(a.add(row * sa).cast())
                        } else {
                            _mm_loadl_epi64(a.add(row * sa).cast())
                        };
                        let bv = _mm_loadu_si128(b.add(row * sb + at).cast());
                        sad = _mm_add_epi16(
                            sad,
                            _mm_add_epi16(
                                _mm_mpsadbw_epu8::<0>(bv, av),
                                _mm_mpsadbw_epu8::<5>(bv, av),
                            ),
                        );
                        if n == 16 {
                            let bv = _mm_loadu_si128(b.add(row * sb + at + 8).cast());
                            sad = _mm_add_epi16(
                                sad,
                                _mm_add_epi16(
                                    _mm_mpsadbw_epu8::<2>(bv, av),
                                    _mm_mpsadbw_epu8::<7>(bv, av),
                                ),
                            );
                        }
                        // Partial SAD plus the batch MV lower bound already
                        // loses; the remaining pixels cannot rescue a candidate.
                        if row % 4 == 3
                            && (_mm_extract_epi16::<0>(_mm_minpos_epu16(sad)) as u32)
                                >= cost - group_floor
                        {
                            at += batch;
                            continue 'groups;
                        }
                    }
                    _mm_storeu_si128(values.as_mut_ptr().cast(), sad);
                }
                for (i, &value) in values[..batch].iter().enumerate() {
                    let delta = (lo + at as i32 + i as i32) as i16 * 4;
                    let mv = if vertical {
                        super::Mv { x: 0, y: delta }
                    } else {
                        super::Mv { x: delta, y: 0 }
                    };
                    let trial = u32::from(value)
                        + fixed
                        + lambda * super::mv_bits(i32::from(delta) - i32::from(axis_pred));
                    if trial < cost {
                        best = mv;
                        cost = trial;
                        if cost == floor {
                            return (best, cost);
                        }
                    }
                }
                at += batch;
            }
            while at < count {
                let mut distortion = 0;
                for row in 0..n {
                    for col in 0..n {
                        distortion += u32::from(
                            (*a.add(row * sa + col)).abs_diff(*b.add(row * sb + at + col)),
                        );
                    }
                }
                let delta = (lo + at as i32) as i16 * 4;
                let mv = if vertical {
                    super::Mv { x: 0, y: delta }
                } else {
                    super::Mv { x: delta, y: 0 }
                };
                let trial = distortion
                    + fixed
                    + lambda * super::mv_bits(i32::from(delta) - i32::from(axis_pred));
                if trial < cost {
                    best = mv;
                    cost = trial;
                    if cost == floor {
                        return (best, cost);
                    }
                }
                at += 1;
            }
            (best, cost)
        }
    }
    unsafe fn transpose(
        src: *const u8,
        ss: usize,
        dst: *mut u8,
        ds: usize,
        width: usize,
        height: usize,
    ) {
        unsafe {
            for y in (0..height).step_by(8) {
                for x in (0..width).step_by(8) {
                    let rows =
                        std::array::from_fn(|r| _mm_loadl_epi64(src.add((y + r) * ss + x).cast()));
                    for (i, v) in crate::dsp::transpose8(rows).into_iter().enumerate() {
                        _mm_storel_epi64(dst.add((x + 2 * i) * ds + y).cast(), v);
                        _mm_storel_epi64(
                            dst.add((x + 2 * i + 1) * ds + y).cast(),
                            _mm_srli_si128::<8>(v),
                        );
                    }
                }
            }
        }
    }
    pub fn sad_until<const N: usize>(a: &[u8], sa: usize, b: &[u8], sb: usize, limit: u32) -> u32 {
        assert!(a.len() >= N && sa <= (a.len() - N) / (N - 1));
        assert!(b.len() >= N && sb <= (b.len() - N) / (N - 1));
        unsafe {
            let mut sum = _mm_setzero_si128();
            let mut total = 0;
            for base in (0..N).step_by(4) {
                for r in 0..4 {
                    let ap = a.as_ptr().add((base + r) * sa);
                    let bp = b.as_ptr().add((base + r) * sb);
                    let (av, bv) = if N == 16 {
                        (_mm_loadu_si128(ap.cast()), _mm_loadu_si128(bp.cast()))
                    } else {
                        (_mm_loadl_epi64(ap.cast()), _mm_loadl_epi64(bp.cast()))
                    };
                    sum = _mm_add_epi64(sum, _mm_sad_epu8(av, bv));
                }
                total = (_mm_cvtsi128_si64(sum) + _mm_cvtsi128_si64(_mm_srli_si128(sum, 8))) as u32;
                if total >= limit {
                    break;
                }
            }
            total
        }
    }

    pub fn sad8(a: &[u8], sa: usize, b: &[u8], sb: usize) -> u32 {
        assert!(a.len() >= 7 * sa + 8 && b.len() >= 7 * sb + 8);
        unsafe {
            let mut sum = _mm_setzero_si128();
            for y in 0..8 {
                sum = _mm_add_epi64(
                    sum,
                    _mm_sad_epu8(
                        _mm_loadl_epi64(a.as_ptr().add(y * sa).cast()),
                        _mm_loadl_epi64(b.as_ptr().add(y * sb).cast()),
                    ),
                );
            }
            _mm_cvtsi128_si64(sum) as u32
        }
    }
    pub fn sad16(a: &[u8], sa: usize, b: &[u8], sb: usize) -> u32 {
        assert!(a.len() >= 15 * sa + 16 && b.len() >= 15 * sb + 16);
        // SSE2 is guaranteed on x86-64; validated full rows precede all loads.
        unsafe {
            let mut s = _mm_setzero_si128();
            for y in 0..16 {
                s = _mm_add_epi64(
                    s,
                    _mm_sad_epu8(
                        _mm_loadu_si128(a.as_ptr().add(y * sa).cast()),
                        _mm_loadu_si128(b.as_ptr().add(y * sb).cast()),
                    ),
                );
            }
            (_mm_cvtsi128_si64(s) + _mm_cvtsi128_si64(_mm_srli_si128(s, 8))) as u32
        }
    }
}
fn mv_bits(v: i32) -> u32 {
    let code = if v <= 0 {
        (-v as u32) * 2
    } else {
        v as u32 * 2 - 1
    };
    2 * (32 - (code + 1).leading_zeros()) - 1
}
pub(super) fn lambda(qp: i32) -> u32 {
    const COST: [u32; 52] = [
        1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 2, 2, 2, 2, 3, 3, 3, 4, 4, 4, 5, 6, 6, 7,
        8, 9, 10, 11, 13, 14, 16, 18, 20, 23, 25, 29, 32, 36, 40, 45, 51, 57, 64, 72, 81, 91,
    ];
    COST[qp as usize]
}
pub(super) fn integer<const N: usize>(
    src: &[u8],
    stride: usize,
    reference: &Plane,
    x: usize,
    y: usize,
    pred: Mv,
    lambda: u32,
    extra: &[Mv],
    expected_sad: u32,
    threshold: u32,
    features: &mut super::analysis::Features,
) -> (Mv, u32) {
    let mx = (x & !15) as i32;
    let my = (y & !15) as i32;
    let min_x = (-mx - 13).max(-504);
    let min_y = (-my - 13).max(-504);
    let max_x = (reference.width as i32 - mx - 3).min(504);
    let max_y = (reference.height as i32 - my - 3).min(504);
    // All seeds, diamond steps and feature candidates are bounded below.
    // Validate their shared reference rectangle once, outside the hot loop.
    let (area, rs) = reference
        .footprint(
            x as i32 + min_x,
            y as i32 + min_y,
            (max_x - min_x) as usize + N,
            (max_y - min_y) as usize + N,
        )
        .expect("bounded integer search footprint");
    let mut best = Mv::ZERO;
    let mut cost = u32::MAX;
    let evaluate = |mv: Mv, best: &mut Mv, cost: &mut u32| {
        let penalty = motion_cost(mv, pred, lambda);
        if penalty >= *cost {
            return;
        }
        let ix = (i32::from(mv.x) / 4 - min_x) as usize;
        let iy = (i32::from(mv.y) / 4 - min_y) as usize;
        let p = &area[iy * rs + ix..];
        let trial = sad_until(src, stride, p, rs, N, *cost - penalty) + penalty;
        if trial < *cost {
            *best = mv;
            *cost = trial;
        }
    };
    // Product seeds: normative MVP, base zero, then integer spatial/temporal
    // fields for P16. Feature search belongs after diamond/cross, only for P8.
    let mut tried = [Mv::ZERO; 6];
    let mut count = 0;
    for mv in [pred, Mv::ZERO].into_iter().chain(extra.iter().copied()) {
        let mv = Mv {
            x: ((i32::from(mv.x) + 2) >> 2).clamp(min_x, max_x) as i16 * 4,
            y: ((i32::from(mv.y) + 2) >> 2).clamp(min_y, max_y) as i16 * 4,
        };
        if tried[..count].contains(&mv) {
            continue;
        }
        tried[count] = mv;
        count += 1;
        evaluate(mv, &mut best, &mut cost);
    }
    if cost >= expected_sad && cost > 2 * lambda {
        let mut previous = best;
        for _ in 0..16 {
            let old = best;
            for (dx, dy) in [(0, -4), (0, 4), (-4, 0), (4, 0)] {
                let mv = Mv {
                    x: old.x + dx,
                    y: old.y + dy,
                };
                // The previous centre already lost to the current winner.
                // Its exact cost cannot improve as the best budget shrinks.
                if mv == previous {
                    continue;
                }
                if i32::from(mv.x) < min_x * 4
                    || i32::from(mv.x) > max_x * 4
                    || i32::from(mv.y) < min_y * 4
                    || i32::from(mv.y) > max_y * 4
                {
                    continue;
                }
                evaluate(mv, &mut best, &mut cost);
            }
            if best == old {
                break;
            }
            previous = old;
        }
        // Scan each axis at the colocated origin, not sparse offsets
        // around the diamond winner. Horizontal is threshold-gated.
        // Ordinary P8 stays diamond-only while FME is disabled;
        // P16 still consumes the reference's retained cross-search threshold.
        if cost >= threshold && (N == 16 || features.enabled) {
            for (vertical, lo, hi) in [(true, min_y, max_y), (false, min_x, max_x)] {
                if !vertical && cost < threshold {
                    break;
                }
                #[cfg(all(target_arch = "x86_64", not(feature = "scalar-dsp")))]
                if let Some((mv, trial)) = simd::line_search(
                    src, stride, reference, x, y, N, pred, lambda, vertical, lo, hi, cost,
                ) {
                    if trial < cost {
                        best = mv;
                        cost = trial;
                    }
                    continue;
                }
                for delta in lo..hi {
                    let mv = if vertical {
                        Mv {
                            x: 0,
                            y: delta as i16 * 4,
                        }
                    } else {
                        Mv {
                            x: delta as i16 * 4,
                            y: 0,
                        }
                    };
                    evaluate(mv, &mut best, &mut cost);
                }
            }
        }
        if N == 8 && cost >= threshold && features.enabled {
            let before = cost;
            features.prepare(reference);
            let candidates = features.candidates(src, stride);
            // Feature buckets retain raster order. Bound the candidate rows
            // before scanning; no point outside this interval is reachable by
            // the existing MV limits, and the original tie order is retained.
            let first = ((y as i32 + min_y).max(0) as u32) << 16;
            let last = (((y as i32 + max_y) as u32) << 16) | 65535;
            let start = candidates.partition_point(|&p| p < first);
            let candidates = &candidates[start..];
            let end = candidates.partition_point(|&p| p <= last);
            for &position in &candidates[..end] {
                let dx = (position & 65535) as i32 - x as i32;
                let dy = (position >> 16) as i32 - y as i32;
                if dx == 0 || dy == 0 || dx < min_x || dx > max_x || dy < min_y || dy > max_y {
                    continue;
                }
                evaluate(
                    Mv {
                        x: dx as i16 * 4,
                        y: dy as i16 * 4,
                    },
                    &mut best,
                    &mut cost,
                );
                if cost < threshold {
                    break;
                }
            }
            features.saving += u64::from(before - cost);
        }
    }
    (best, cost)
}

/// A partition is selected before it is refined. The quarter-pel stage uses SATD and axial half/quarter candidates, not an eight-
/// direction refinement for every discarded partition candidate.
pub(super) fn refine(
    src: &[u8],
    stride: usize,
    reference: &Plane,
    x: usize,
    y: usize,
    pred: Mv,
    lambda: u32,
    width: usize,
    height: usize,
    initial: Mv,
) -> Result<(Mv, [u8; 256])> {
    let mut best = initial;
    let mut pixels = [0u8; 256];
    dsp::motion::luma(
        reference,
        x as i32 * 4 + i32::from(best.x),
        y as i32 * 4 + i32::from(best.y),
        &mut Block::new(&mut pixels, width, width, height)?,
    )?;
    let mut cost =
        satd_rect(src, stride, &pixels, width, width, height) + motion_cost(best, pred, lambda);
    // Two one-bit MV components are the absolute cost floor. No later
    // fractional candidate can beat an exact match at that cost.
    if cost == 2 * lambda {
        return Ok((best, pixels));
    }
    let integer_pixels = pixels;
    let directions = [(0, -1), (0, 1), (-1, 0), (1, 0)];
    let mut half = [[0u8; 256]; 4];
    let mut valid = [false; 4];
    let mut half_choice = None;
    for (i, (dx, dy)) in directions.into_iter().enumerate() {
        let mv = Mv {
            x: initial.x + dx * 2,
            y: initial.y + dy * 2,
        };
        if mv.x.abs() > 2048 || mv.y.abs() > 2048 {
            continue;
        }
        let penalty = motion_cost(mv, pred, lambda);
        if penalty >= cost {
            continue;
        }
        dsp::motion::luma(
            reference,
            x as i32 * 4 + i32::from(mv.x),
            y as i32 * 4 + i32::from(mv.y),
            &mut Block::new(&mut half[i], width, width, height)?,
        )?;
        valid[i] = true;
        let trial = satd_rect(src, stride, &half[i], width, width, height) + penalty;
        if trial < cost {
            best = mv;
            cost = trial;
            pixels = half[i];
            half_choice = Some(i);
            if cost == 2 * lambda {
                return Ok((best, pixels));
            }
        }
    }
    let centre = best;
    for (i, (dx, dy)) in directions.into_iter().enumerate() {
        let mv = Mv {
            x: centre.x + dx,
            y: centre.y + dy,
        };
        if mv.x.abs() > 2048 || mv.y.abs() > 2048 {
            continue;
        }
        let penalty = motion_cost(mv, pred, lambda);
        if penalty >= cost {
            continue;
        }
        let candidate = if half_choice.is_none() {
            // OpenH264 reuses the half-pel planes for quarter-pel averages.
            // A skipped half candidate may still feed a cheaper quarter MV.
            if !valid[i] {
                let half_mv = Mv {
                    x: initial.x + dx * 2,
                    y: initial.y + dy * 2,
                };
                dsp::motion::luma(
                    reference,
                    x as i32 * 4 + i32::from(half_mv.x),
                    y as i32 * 4 + i32::from(half_mv.y),
                    &mut Block::new(&mut half[i], width, width, height)?,
                )?;
                valid[i] = true;
            }
            super::pixels::average_block(&integer_pixels, &half[i], width * height)
        } else if i == half_choice.unwrap() ^ 1 {
            super::pixels::average_block(
                &integer_pixels,
                &half[half_choice.unwrap()],
                width * height,
            )
        } else {
            let mut candidate = [0u8; 256];
            dsp::motion::luma(
                reference,
                x as i32 * 4 + i32::from(mv.x),
                y as i32 * 4 + i32::from(mv.y),
                &mut Block::new(&mut candidate, width, width, height)?,
            )?;
            candidate
        };
        let trial = satd_rect(src, stride, &candidate, width, width, height) + penalty;
        if trial < cost {
            best = mv;
            cost = trial;
            pixels = candidate;
        }
    }
    Ok((best, pixels))
}

/// Four-by-four Hadamard distortion, summed over a fixed AVC block.
#[inline]
pub(super) fn satd(a: &[u8], sa: usize, b: &[u8], sb: usize, n: usize) -> u32 {
    super::pixels::satd_block(a, sa, b, sb, n)
}

#[inline]
pub(super) fn cost(a: &[u8], sa: usize, b: &[u8], sb: usize, n: usize, idr: bool) -> u32 {
    if idr {
        satd(a, sa, b, sb, n)
    } else {
        sad(a, sa, b, sb, n)
    }
}

fn satd_rect(a: &[u8], sa: usize, b: &[u8], sb: usize, width: usize, height: usize) -> u32 {
    if width == height {
        return satd(a, sa, b, sb, width);
    }
    if width == 16 {
        satd(a, sa, b, sb, 8) + satd(&a[8..], sa, &b[8..], sb, 8)
    } else {
        satd(a, sa, b, sb, 8) + satd(&a[8 * sa..], sa, &b[8 * sb..], sb, 8)
    }
}
