// SPDX-License-Identifier: LGPL-2.1-or-later
// FFmpeg h264dsp_template.c/h264_deblock.asm arithmetic, adapted to sixteen
// simultaneous 16-bit AVX2 lanes. Copyright (c) 2003-2011 Michael Niedermayer;
// Rust adaptation Copyright (c) 2026 OpenUUYC contributors.
use super::*;
#[target_feature(enable = "avx2")]
#[inline]
unsafe fn filter<const MODE: u8>(
    s: [__m256i; 8],
    alpha: i16,
    beta: i16,
    bs: __m256i,
    tc0: __m256i,
) -> [__m256i; 8] {
    let [p3, p2, p1, p0, q0, q1, q2, q3] = s;
    let zero = _mm256_setzero_si256();
    let one = _mm256_set1_epi16(1);
    let two = _mm256_set1_epi16(2);
    let four = _mm256_set1_epi16(4);
    let add = |a, b| _mm256_add_epi16(a, b);
    let sub = |a, b| _mm256_sub_epi16(a, b);
    let select =
        |mask, a, b| _mm256_or_si256(_mm256_and_si256(mask, a), _mm256_andnot_si256(mask, b));
    let abs = |v| _mm256_abs_epi16(v);
    let clamp = |v, lo, hi| _mm256_max_epi16(lo, _mm256_min_epi16(hi, v));
    let a = _mm256_set1_epi16(alpha);
    let b = _mm256_set1_epi16(beta);
    let enabled = _mm256_and_si256(
        _mm256_cmpgt_epi16(a, abs(sub(p0, q0))),
        _mm256_and_si256(
            _mm256_cmpgt_epi16(b, abs(sub(p1, p0))),
            _mm256_cmpgt_epi16(b, abs(sub(q1, q0))),
        ),
    );
    // MODE 0: ordinary edges; MODE 1: all-strong intra edge;
    // MODE 2 retains mixed strengths for the public DSP contract.
    let normal = if MODE == 1 {
        zero
    } else {
        _mm256_and_si256(
            enabled,
            _mm256_and_si256(_mm256_cmpgt_epi16(bs, zero), _mm256_cmpgt_epi16(four, bs)),
        )
    };
    let strong = if MODE == 0 {
        zero
    } else if MODE == 1 {
        enabled
    } else {
        _mm256_and_si256(enabled, _mm256_cmpeq_epi16(bs, four))
    };
    if _mm256_movemask_epi8(_mm256_or_si256(normal, strong)) == 0 {
        return s;
    }
    let ap = _mm256_cmpgt_epi16(b, abs(sub(p2, p0)));
    let aq = _mm256_cmpgt_epi16(b, abs(sub(q2, q0)));
    let tc = add(
        tc0,
        add(_mm256_and_si256(ap, one), _mm256_and_si256(aq, one)),
    );
    let delta = clamp(
        _mm256_srai_epi16::<3>(add(
            add(_mm256_slli_epi16::<2>(sub(q0, p0)), sub(p1, q1)),
            four,
        )),
        sub(zero, tc),
        tc,
    );
    let midpoint = _mm256_srai_epi16::<1>(add(add(p0, q0), one));
    let np1 = add(
        p1,
        clamp(
            sub(_mm256_srai_epi16::<1>(add(p2, midpoint)), p1),
            sub(zero, tc0),
            tc0,
        ),
    );
    let nq1 = add(
        q1,
        clamp(
            sub(_mm256_srai_epi16::<1>(add(q2, midpoint)), q1),
            sub(zero, tc0),
            tc0,
        ),
    );
    let np0 = clamp(add(p0, delta), zero, _mm256_set1_epi16(255));
    let nq0 = clamp(sub(q0, delta), zero, _mm256_set1_epi16(255));
    // FFmpeg has separate normal/intra kernels. Ordinary inter edges
    // never execute the strong filter's p2/q2 and six-tap arithmetic.
    if _mm256_movemask_epi8(strong) == 0 {
        return [
            p3,
            p2,
            select(_mm256_and_si256(normal, ap), np1, p1),
            select(normal, np0, p0),
            select(normal, nq0, q0),
            select(_mm256_and_si256(normal, aq), nq1, q1),
            q2,
            q3,
        ];
    }
    let small = _mm256_cmpgt_epi16(_mm256_set1_epi16((alpha >> 2) + 2), abs(sub(p0, q0)));
    let sp = _mm256_and_si256(strong, _mm256_and_si256(small, ap));
    let sq = _mm256_and_si256(strong, _mm256_and_si256(small, aq));
    let twice = |v| _mm256_slli_epi16::<1>(v);
    let p0s = select(
        sp,
        _mm256_srai_epi16::<3>(add(
            add(add(add(p2, twice(p1)), twice(p0)), twice(q0)),
            add(q1, four),
        )),
        _mm256_srai_epi16::<2>(add(add(twice(p1), p0), add(q1, two))),
    );
    let q0s = select(
        sq,
        _mm256_srai_epi16::<3>(add(
            add(add(add(p1, twice(p0)), twice(q0)), twice(q1)),
            add(q2, four),
        )),
        _mm256_srai_epi16::<2>(add(add(twice(q1), q0), add(p1, two))),
    );
    let p1s = _mm256_srai_epi16::<2>(add(add(p2, p1), add(add(p0, q0), two)));
    let q1s = _mm256_srai_epi16::<2>(add(add(p0, q0), add(add(q1, q2), two)));
    let p2s = _mm256_srai_epi16::<3>(add(
        add(twice(p3), add(twice(p2), p2)),
        add(add(p1, p0), add(q0, four)),
    ));
    let q2s = _mm256_srai_epi16::<3>(add(
        add(twice(q3), add(twice(q2), q2)),
        add(add(q1, q0), add(p0, four)),
    ));
    [
        p3,
        select(sp, p2s, p2),
        select(sp, p1s, select(_mm256_and_si256(normal, ap), np1, p1)),
        select(strong, p0s, select(normal, np0, p0)),
        select(strong, q0s, select(normal, nq0, q0)),
        select(sq, q1s, select(_mm256_and_si256(normal, aq), nq1, q1)),
        select(sq, q2s, q2),
        q3,
    ]
}
/// The caller validates the complete 8x16/16x8 footprint and CPU/OS AVX2
/// capability. Transpose byte columns only once, filter all 16 samples together.
#[target_feature(enable = "avx2")]
pub(super) unsafe fn run(
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
        let samples = c.map(|v| _mm256_cvtepu8_epi16(v));
        let strengths = _mm256_setr_epi16(
            bs[0] as i16,
            bs[0] as i16,
            bs[0] as i16,
            bs[0] as i16,
            bs[1] as i16,
            bs[1] as i16,
            bs[1] as i16,
            bs[1] as i16,
            bs[2] as i16,
            bs[2] as i16,
            bs[2] as i16,
            bs[2] as i16,
            bs[3] as i16,
            bs[3] as i16,
            bs[3] as i16,
            bs[3] as i16,
        );
        let tc0 = _mm256_setr_epi16(
            tc[0], tc[0], tc[0], tc[0], tc[1], tc[1], tc[1], tc[1], tc[2], tc[2], tc[2], tc[2],
            tc[3], tc[3], tc[3], tc[3],
        );
        let filtered = if bs == [4; 4] {
            filter::<1>(samples, alpha, beta, strengths, tc0)
        } else if bs.iter().all(|&v| v < 4) {
            filter::<0>(samples, alpha, beta, strengths, tc0)
        } else {
            filter::<2>(samples, alpha, beta, strengths, tc0)
        };
        let output = filtered
            .map(|v| _mm_packus_epi16(_mm256_castsi256_si128(v), _mm256_extracti128_si256::<1>(v)));
        if vertical {
            store_columns(start, stride, output);
        } else {
            for i in 1..7 {
                _mm_storeu_si128(start.add(i * stride).cast(), output[i]);
            }
        }
    }
}
