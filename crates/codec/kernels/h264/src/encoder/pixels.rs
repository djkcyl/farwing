// SPDX-License-Identifier: LGPL-2.1-or-later
//! Fixed 8-bit AVC encoder kernels. All wide DSP and format switches stay out
//! of this path. Fixed encoder reconstruction is checked against the shared DSP.
// Quantization/combined-mode strategies derive from Cisco OpenH264 (BSD-2-Clause);
// see licenses/openh264-algorithms.txt and THIRD_PARTY_NOTICES.
/// Fixed 8-bit, zero-offset chroma mapping (H.264 Table 8-15).
#[inline]
pub(super) fn chroma_qp(qp: i32) -> i32 {
    const MAP: [u8; 52] = [
        0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20, 21, 22, 23, 24,
        25, 26, 27, 28, 29, 29, 30, 31, 32, 32, 33, 34, 34, 35, 35, 36, 36, 37, 37, 37, 38, 38, 38,
        39, 39, 39, 39,
    ];
    i32::from(MAP[qp as usize])
}
#[inline]
pub(super) fn hadamard2(c: &[i32; 4]) -> [i32; 4] {
    let (a, b, c, d) = (c[0] + c[2], c[1] + c[3], c[0] - c[2], c[1] - c[3]);
    [a + b, a - b, c + d, c - d]
}
#[derive(Clone, Copy)]
pub(super) struct Quant {
    dequant16: [i16; 16],
    mf16: [i16; 16],
    intra: [i16; 16],
    inter: [i16; 16],
    zero_chroma_bound: u8,
}
impl Quant {
    #[inline]
    pub fn at(qp: i32) -> &'static Self {
        static TABLE: [Quant; 52] = {
            let mut table = [Quant::new(0); 52];
            let mut i = 0;
            while i < 52 {
                table[i] = Quant::new(i as i32);
                i += 1;
            }
            table
        };
        &TABLE[qp as usize]
    }
    const fn new(qp: i32) -> Self {
        let factors = super::quant_tables::MF[qp as usize];
        const DQ: [[i32; 3]; 6] = [
            [10, 13, 16],
            [11, 14, 18],
            [13, 16, 20],
            [14, 18, 23],
            [16, 20, 25],
            [18, 23, 29],
        ];
        let mut mf16 = [0; 16];
        let mut dequant16 = [0; 16];
        let mut intra = [0; 16];
        let mut inter = [0; 16];
        let mut i = 0;
        while i < 16 {
            let k = (i & 1) + ((i >> 2) & 1);
            mf16[i] = factors[k];
            dequant16[i] = (DQ[qp as usize % 6][k] << (qp / 6)) as i16;
            inter[i] = super::quant_tables::FF[qp as usize][k];
            intra[i] = super::quant_tables::FF[qp as usize + 6][k];
            i += 1;
        }
        // A sufficient bound under the actual integer pre-bias tables.
        // Normal transform basis row sums are 16/24/36; chroma DC uses 64.
        let mut bound = 255i32;
        let mut k = 0;
        while k < 3 {
            let norm = if k == 0 {
                16
            } else if k == 1 {
                24
            } else {
                36
            };
            let room = 65535 / factors[k] as i32 - super::quant_tables::FF[qp as usize][k] as i32;
            let b = room / norm;
            if b < bound {
                bound = b;
            }
            k += 1;
        }
        if bound < 0 {
            bound = 0;
        }
        let dc_room = 65535 / (factors[0] as i32 >> 1) - 2 * inter[0] as i32;
        let mut chroma = dc_room / 64;
        if chroma > bound {
            chroma = bound;
        }
        if chroma < 0 {
            chroma = 0;
        }
        Self {
            mf16,
            dequant16,
            intra,
            inter,
            zero_chroma_bound: chroma as u8,
        }
    }
    #[inline]
    #[cfg(not(all(target_arch = "x86_64", not(feature = "scalar-dsp"))))]
    pub fn block(&self, coeff: &[i32; 16], intra: bool) -> [i16; 16] {
        let bias = if intra { &self.intra } else { &self.inter };
        std::array::from_fn(|i| {
            let v = coeff[i];
            let l = ((v.abs() + i32::from(bias[i])) * i32::from(self.mf16[i])) >> 16;
            (if v < 0 { -l } else { l }) as i16
        })
    }
    #[inline]
    pub fn dc<const N: usize>(&self, coeff: &[i32; N], intra: bool) -> [i32; N] {
        let ff = 2 * i32::from(if intra { self.intra[0] } else { self.inter[0] });
        let mf = i32::from(self.mf16[0]) >> 1;
        coeff.map(|v| {
            let level = ((v.abs() + ff) * mf) >> 16;
            if v < 0 { -level } else { level }
        })
    }
    #[inline]
    pub fn inverse_chroma_dc(&self, c: &[i32; 4]) -> [i32; 4] {
        // Flat scaling list and 8-bit 420 are fixed by the encoder. The same
        // precomputed dequant factor gives Eq. 8-326 without generic dispatch.
        hadamard2(c).map(|v| v * i32::from(self.dequant16[0]) >> 1)
    }
    #[inline]
    pub fn forward_block(&self, a: &[u8], sa: usize, b: &[u8; 16], intra: bool) -> [i16; 16] {
        assert!(a.len() >= 4 && sa <= (a.len() - 4) / 3);
        #[cfg(all(target_arch = "x86_64", not(feature = "scalar-dsp")))]
        {
            return simd::forward_quant(
                a,
                sa,
                b,
                &self.mf16,
                if intra { &self.intra } else { &self.inter },
            );
        }
        #[cfg(not(all(target_arch = "x86_64", not(feature = "scalar-dsp"))))]
        {
            self.block(&forward(a, sa, b, 4), intra)
        }
    }
    /// Four independent 4x4 residuals in an 8x8 tile, matching OpenH264's
    /// grouped DCT/quantization layout. Intra4 never calls this before its
    /// neighbouring reconstructions are available.
    #[inline]
    pub fn forward_four(
        &self,
        a: &[u8],
        sa: usize,
        b: &[u8],
        sb: usize,
        intra: bool,
    ) -> ([[i16; 16]; 4], [i32; 4]) {
        assert!(a.len() >= 8 && sa <= (a.len() - 8) / 7);
        assert!(b.len() >= 8 && sb <= (b.len() - 8) / 7);
        #[cfg(all(target_arch = "x86_64", not(feature = "scalar-dsp")))]
        {
            return simd::forward_four(
                a,
                sa,
                b,
                sb,
                &self.mf16,
                if intra { &self.intra } else { &self.inter },
            );
        }
        #[cfg(not(all(target_arch = "x86_64", not(feature = "scalar-dsp"))))]
        {
            let mut out = [[0; 16]; 4];
            let mut dc = [0; 4];
            for i in 0..4 {
                let x = i % 2 * 4;
                let y = i / 2 * 4;
                let c = forward(&a[y * sa + x..], sa, &b[y * sb + x..], sb);
                dc[i] = c[0];
                out[i] = self.block(&c, intra);
            }
            (out, dc)
        }
    }
    pub fn zero_chroma(&self, src: &[u8], stride: usize, pred: &[u8; 64]) -> bool {
        let limit = self.zero_chroma_bound;
        if src[0].abs_diff(pred[0]) > limit {
            return false;
        }
        assert!(src.len() >= 8 && stride <= (src.len() - 8) / 7);
        #[cfg(all(target_arch = "x86_64", not(feature = "scalar-dsp")))]
        {
            return simd::zero_chroma(src, stride, pred, limit);
        }
        #[cfg(not(all(target_arch = "x86_64", not(feature = "scalar-dsp"))))]
        {
            (0..8).all(|y| {
                src[y * stride..][..8]
                    .iter()
                    .zip(&pred[y * 8..][..8])
                    .all(|(&a, &b)| a.abs_diff(b) <= limit)
            })
        }
    }
    #[inline]
    pub fn reconstruct_into(
        &self,
        c: &[i16; 16],
        pred: &[u8],
        ps: usize,
        dst: &mut [u8],
        ds: usize,
        dc: Option<i32>,
    ) -> super::Result<()> {
        assert!(pred.len() >= 4 && ps <= (pred.len() - 4) / 3);
        assert!(dst.len() >= 4 && ds <= (dst.len() - 4) / 3);
        #[cfg(all(target_arch = "x86_64", not(feature = "scalar-dsp")))]
        {
            simd::reconstruct_into(c, &self.dequant16, pred, ps, dst, ds, dc);
            return Ok(());
        }
        #[cfg(not(all(target_arch = "x86_64", not(feature = "scalar-dsp"))))]
        {
            let out = self.reconstruct(c, &gather4(pred, ps), dc)?;
            for y in 0..4 {
                dst[y * ds..][..4].copy_from_slice(&out[y * 4..][..4]);
            }
            Ok(())
        }
    }
    #[inline]
    #[cfg(not(all(target_arch = "x86_64", not(feature = "scalar-dsp"))))]
    fn reconstruct(&self, c: &[i16; 16], p: &[u8; 16], dc: Option<i32>) -> super::Result<[u8; 16]> {
        let mut block = std::array::from_fn(|i| {
            let k = i % 4 * 4 + i / 4;
            (if i == 0 {
                dc.unwrap_or(i32::from(c[k]) * i32::from(self.dequant16[k]))
            } else {
                i32::from(c[k]) * i32::from(self.dequant16[k])
            }) as i16
        });
        let mut out = *p;
        crate::dsp::transform::add4(&mut crate::dsp::Block::new(&mut out, 4, 4, 4)?, &mut block)?;
        Ok(out)
    }
}
/// OpenH264 WelsCalculateSingleCtr4x4 (JVT-O079). Large coefficients keep
/// their group; isolated +/-1 coefficients are scored by the preceding zero run.
#[inline]
pub(super) fn decimate_score(coeff: &[i16; 16], ac_only: bool) -> u32 {
    #[cfg(all(target_arch = "x86_64", not(feature = "scalar-dsp")))]
    let (mut mask, large) = simd::decimate_mask(coeff, ac_only);
    #[cfg(not(all(target_arch = "x86_64", not(feature = "scalar-dsp"))))]
    let (mut mask, large) = {
        let mut mask = 0;
        let mut large = false;
        for (i, &v) in coeff.iter().enumerate().skip(usize::from(ac_only)) {
            mask |= u32::from(v != 0) << i;
            large |= v.unsigned_abs() > 1;
        }
        (mask, large)
    };
    if large {
        return 9;
    }
    // Quantization already decides empty/large blocks. Only +/-1 blocks need
    // scan-order run scoring; never materialize a discarded scan.
    const ORDER: [u8; 16] = [0, 1, 5, 6, 2, 4, 7, 12, 3, 8, 11, 13, 9, 10, 14, 15];
    let mut scan = 0u32;
    while mask != 0 {
        let at = mask.trailing_zeros() as usize;
        mask &= mask - 1;
        scan |= 1 << (ORDER[at] - u8::from(ac_only));
    }
    let mut score = 0;
    while scan != 0 {
        let at = 31 - scan.leading_zeros();
        scan ^= 1 << at;
        let gap = if scan == 0 {
            at
        } else {
            at - (31 - scan.leading_zeros()) - 1
        };
        score += match gap {
            0 => 3,
            1 | 2 => 2,
            3..=5 => 1,
            _ => 0,
        };
    }
    score
}

#[inline]
pub(super) fn add_dc_into(pred: &[u8], ps: usize, dst: &mut [u8], ds: usize, dc: i32) {
    assert!(pred.len() >= 4 && ps <= (pred.len() - 4) / 3);
    assert!(dst.len() >= 4 && ds <= (dst.len() - 4) / 3);
    let delta = (dc as i16).wrapping_add(32) >> 6;
    #[cfg(all(target_arch = "x86_64", not(feature = "scalar-dsp")))]
    {
        simd::add_dc_into(pred, ps, dst, ds, delta);
    }
    #[cfg(not(all(target_arch = "x86_64", not(feature = "scalar-dsp"))))]
    for y in 0..4 {
        for x in 0..4 {
            dst[y * ds + x] = (i32::from(pred[y * ps + x]) + i32::from(delta)).clamp(0, 255) as u8;
        }
    }
}
#[inline]
#[cfg(not(all(target_arch = "x86_64", not(feature = "scalar-dsp"))))]
fn forward(a: &[u8], sa: usize, b: &[u8], sb: usize) -> [i32; 16] {
    let residual = std::array::from_fn(|i| {
        i32::from(a[i / 4 * sa + i % 4]) - i32::from(b[i / 4 * sb + i % 4])
    });
    crate::syntax::forward::forward_core_4x4(&residual)
}
#[inline]
pub(super) fn satd4(a: &[u8], sa: usize, b: &[u8], sb: usize) -> u32 {
    #[cfg(all(target_arch = "x86_64", not(feature = "scalar-dsp")))]
    {
        return simd::satd(a, sa, b, sb);
    }
    #[cfg(not(all(target_arch = "x86_64", not(feature = "scalar-dsp"))))]
    {
        let mut t = [0i32; 16];
        for y in 0..4 {
            let d = std::array::from_fn::<_, 4, _>(|x| {
                i32::from(a[y * sa + x]) - i32::from(b[y * sb + x])
            });
            let p = d[0] + d[3];
            let q = d[1] + d[2];
            let r = d[1] - d[2];
            let s = d[0] - d[3];
            t[y * 4] = p + q;
            t[y * 4 + 1] = s + r;
            t[y * 4 + 2] = p - q;
            t[y * 4 + 3] = s - r;
        }
        let mut sum = 0;
        for x in 0..4 {
            let p = t[x] + t[12 + x];
            let q = t[4 + x] + t[8 + x];
            let r = t[4 + x] - t[8 + x];
            let s = t[x] - t[12 + x];
            sum += (p + q).unsigned_abs()
                + (s + r).unsigned_abs()
                + (p - q).unsigned_abs()
                + (s - r).unsigned_abs();
        }
        sum / 2
    }
}

#[inline]
fn q(a: u8, b: u8, c: u8) -> u8 {
    ((u16::from(a) + 2 * u16::from(b) + u16::from(c) + 2) >> 2) as u8
}
#[inline]
fn half(a: u8, b: u8) -> u8 {
    ((u16::from(a) + u16::from(b) + 1) >> 1) as u8
}
#[inline]
fn vr(t: &[u8], l: &[u8], corner: u8) -> [u8; 16] {
    let a = half(corner, t[0]);
    let b = half(t[0], t[1]);
    let c = half(t[1], t[2]);
    let d = half(t[2], t[3]);
    let e = q(l[0], corner, t[0]);
    let f = q(corner, t[0], t[1]);
    let g = q(t[0], t[1], t[2]);
    let h = q(t[1], t[2], t[3]);
    let i = q(corner, l[0], l[1]);
    let j = q(l[0], l[1], l[2]);
    [a, b, c, d, e, f, g, h, i, a, b, c, j, e, f, g]
}
#[inline]
fn vl(t: &[u8]) -> [u8; 16] {
    [
        half(t[0], t[1]),
        half(t[1], t[2]),
        half(t[2], t[3]),
        half(t[3], t[4]),
        q(t[0], t[1], t[2]),
        q(t[1], t[2], t[3]),
        q(t[2], t[3], t[4]),
        q(t[3], t[4], t[5]),
        half(t[1], t[2]),
        half(t[2], t[3]),
        half(t[3], t[4]),
        half(t[4], t[5]),
        q(t[1], t[2], t[3]),
        q(t[2], t[3], t[4]),
        q(t[3], t[4], t[5]),
        q(t[4], t[5], t[6]),
    ]
}
pub(super) struct Edges4 {
    top: [u8; 8],
    left: [u8; 8],
    corner: u8,
}
impl Edges4 {
    #[inline]
    pub fn gather(data: &[u8], stride: usize, x: usize, y: usize, right: bool) -> Self {
        let mut e = Self {
            top: [128; 8],
            left: [128; 8],
            corner: 128,
        };
        if y > 0 {
            let row = &data[(y - 1) * stride + x..];
            e.top[..4].copy_from_slice(&row[..4]);
            if right {
                e.top[4..].copy_from_slice(&row[4..8]);
            } else {
                e.top[4..].fill(row[3]);
            }
        }
        if x > 0 {
            for r in 0..4 {
                e.left[r] = data[(y + r) * stride + x - 1];
            }
            let last = e.left[3];
            e.left[4..].fill(last);
            if y > 0 {
                e.corner = data[(y - 1) * stride + x - 1];
            }
        }
        e
    }
}
#[inline]
fn predict4(mode: u8, e: &Edges4, available: u8) -> [u8; 16] {
    #[cfg(all(target_arch = "x86_64", not(feature = "scalar-dsp")))]
    if mode < 3 {
        return simd::basic_prediction(mode, e, available);
    } else if let Some(p) = simd::directional(mode, e) {
        return p;
    }
    let t = &e.top;
    let l = &e.left;
    let c = e.corner;
    match mode {
        0 => std::array::from_fn(|i| t[i % 4]),
        1 => std::array::from_fn(|i| l[i / 4]),
        2 => {
            let a = t[..4].iter().map(|&x| u16::from(x)).sum::<u16>();
            let b = l[..4].iter().map(|&x| u16::from(x)).sum::<u16>();
            let dc = match available {
                3 => (a + b + 4) >> 3,
                1 => (a + 2) >> 2,
                2 => (b + 2) >> 2,
                _ => 128,
            };
            [dc as u8; 16]
        }
        3 => {
            let values = std::array::from_fn::<_, 7, _>(|i| q(t[i], t[i + 1], t[(i + 2).min(7)]));
            std::array::from_fn(|i| values[i % 4 + i / 4])
        }
        4 => {
            let m = q(l[0], c, t[0]);
            let a = q(c, t[0], t[1]);
            let b = q(t[0], t[1], t[2]);
            let d = q(t[1], t[2], t[3]);
            let f = q(c, l[0], l[1]);
            let g = q(l[0], l[1], l[2]);
            let h = q(l[1], l[2], l[3]);
            [m, a, b, d, f, m, a, b, g, f, m, a, h, g, f, m]
        }
        5 => vr(t, l, c),
        6 => {
            let p = vr(l, t, c);
            std::array::from_fn(|i| p[(i % 4) * 4 + i / 4])
        }
        7 => vl(t),
        8 => {
            let p = vl(l);
            std::array::from_fn(|i| p[(i % 4) * 4 + i / 4])
        }
        _ => unreachable!(),
    }
}
#[inline]
fn gather4(src: &[u8], stride: usize) -> [u8; 16] {
    assert!(src.len() >= 4 && stride <= (src.len() - 4) / 3);
    #[cfg(all(target_arch = "x86_64", not(feature = "scalar-dsp")))]
    {
        return simd::gather4(src, stride);
    }
    #[cfg(not(all(target_arch = "x86_64", not(feature = "scalar-dsp"))))]
    {
        std::array::from_fn(|i| src[i / 4 * stride + i % 4])
    }
}
/// Combined V/H/DC SAD for the P-frame I16 decision. Reads each source
/// row once and materializes only the winning prediction in the caller.
pub(super) fn intra16_sad(src: &[u8], stride: usize, e: &crate::dsp::intra::Edges) -> [u32; 3] {
    assert!(src.len() >= 16 && stride <= (src.len() - 16) / 15);
    let dc = ((e.top[..16]
        .iter()
        .chain(e.left.iter())
        .map(|&v| u32::from(v))
        .sum::<u32>()
        + 16)
        >> 5) as u8;
    #[cfg(all(target_arch = "x86_64", not(feature = "scalar-dsp")))]
    {
        return simd::intra16_sad(src, stride, &e.top, &e.left, dc);
    }
    #[cfg(not(all(target_arch = "x86_64", not(feature = "scalar-dsp"))))]
    {
        let mut out = [0u32; 3];
        for y in 0..16 {
            for x in 0..16 {
                let v = src[y * stride + x];
                out[0] += u32::from(v.abs_diff(e.top[x]));
                out[1] += u32::from(v.abs_diff(e.left[y]));
                out[2] += u32::from(v.abs_diff(dc));
            }
        }
        out
    }
}
/// Variance of the sixteen truncated 4x4 source averages.
/// Only the screen P-frame fast-intra path applies the threshold of 150.
pub(super) fn fine_intra(src: &[u8], stride: usize) -> bool {
    let (mut sum, mut squares) = (0u32, 0u32);
    for y in (0..16).step_by(4) {
        for x in (0..16).step_by(4) {
            let v = (0..4)
                .map(|r| {
                    src[(y + r) * stride + x..][..4]
                        .iter()
                        .map(|&v| u32::from(v))
                        .sum::<u32>()
                })
                .sum::<u32>()
                >> 4;
            sum += v;
            squares += v * v;
        }
    }
    squares - (sum * sum >> 4) >= 150
}
/// Current LOW screen P frames use directional decisions, not an exhaustive
/// nine-mode pass. DC/H/V costs select which neighbouring directions to try.
pub(super) fn intra4_fast(
    src: &[u8],
    stride: usize,
    e: &Edges4,
    available: u8,
    right: bool,
    mpm: u8,
    lambda: u32,
) -> (u8, [u8; 16], u32) {
    let source = gather4(src, stride);
    let (mut best, mut chosen, mut pixels) = (u32::MAX, 2, [0; 16]);
    let mut trial = |mode| {
        let p = predict4(mode, e, available);
        #[cfg(all(target_arch = "x86_64", not(feature = "scalar-dsp")))]
        let d = simd::sad4(&source, &p);
        #[cfg(not(all(target_arch = "x86_64", not(feature = "scalar-dsp"))))]
        let d = source
            .iter()
            .zip(p)
            .map(|(&a, b)| u32::from(a.abs_diff(b)))
            .sum::<u32>();
        let cost = d + if mode == mpm { lambda } else { 4 * lambda };
        if cost < best {
            best = cost;
            chosen = mode;
            pixels = p;
        }
        cost
    };
    trial(2);
    if available == 3 {
        let h = trial(1);
        let v = trial(0);
        if v < h {
            if right {
                let vr = trial(5);
                let vl = trial(7);
                if vr < v || vl < v {
                    trial(if vr < vl { 4 } else { 3 });
                }
            } else {
                trial(4);
                trial(5);
            }
        } else {
            let hd = trial(6);
            let hu = trial(8);
            if hd < h || hu < h {
                if hd < hu {
                    trial(4);
                } else if right {
                    trial(3);
                }
            }
        }
    } else {
        if available & 2 != 0 {
            trial(1);
            trial(8);
        }
        if available & 1 != 0 {
            trial(0);
            if right {
                trial(3);
                trial(7);
            }
        }
    }
    (chosen, pixels, best)
}
/// Fixed 4x4 candidates share one source gather and one validated edge set.
/// No 8x8/444 dispatch or repeated destination construction in this hot loop.
#[inline(always)]
pub(super) fn intra4(
    src: &[u8],
    stride: usize,
    e: &Edges4,
    available: u8,
    right: bool,
    mpm: u8,
    lambda: u32,
) -> (u8, [u8; 16], u32) {
    let source = gather4(src, stride);
    let valid = |mode| match mode {
        0 => available & 1 != 0,
        3 | 7 => available & 1 != 0 && right,
        1 | 8 => available & 2 != 0,
        4..=6 => available == 3,
        _ => true,
    };
    #[cfg(all(target_arch = "x86_64", not(feature = "scalar-dsp")))]
    let basic = if available == 3 {
        Some(simd::intra_three(&source, e))
    } else {
        None
    };
    #[cfg(not(all(target_arch = "x86_64", not(feature = "scalar-dsp"))))]
    let basic: Option<[u32; 3]> = None;
    // MPM pays lambda; every other candidate pays 4*lambda and has nonnegative
    // distortion. Once MPM's total <=4*lambda, no remaining mode can win (ties
    // keep the first mode). This is an exact lower bound, not a quality heuristic.
    let mut best = u32::MAX;
    let mut chosen = 2;
    let mut pixels = [0; 16];
    if valid(mpm) {
        let p = predict4(mpm, e, available);
        let d = if mpm < 3 {
            basic.map_or_else(|| satd4(&source, 4, &p, 4), |v| v[mpm as usize])
        } else {
            satd4(&source, 4, &p, 4)
        };
        best = d + lambda;
        chosen = mpm;
        pixels = p;
        if best <= 4 * lambda {
            return (chosen, pixels, best);
        }
    }
    let mut order = [0u8; 9];
    let mut count = 0;
    for mode in 0..9 {
        if mode != mpm && valid(mode) {
            order[count] = mode;
            count += 1;
        }
    }
    let mut i = 0;
    while i < count {
        if let Some(costs) = basic {
            let mode = order[i];
            if mode < 3 {
                let d = costs[mode as usize];
                let score = d + 4 * lambda;
                if score < best {
                    best = score;
                    chosen = mode;
                    pixels = predict4(mode, e, available);
                }
                if d == 0 {
                    return (chosen, pixels, best);
                }
                i += 1;
                continue;
            }
        }
        // Keep the preferred mode's zero-distortion early exit. The remaining
        // modes share SIMD lanes in pairs; winner order and ties are unchanged.
        let pair = i + 1 < count && (basic.is_none() || order[i] >= 3 && order[i + 1] >= 3);
        let p = predict4(order[i], e, available);
        let q = if pair {
            predict4(order[i + 1], e, available)
        } else {
            p
        };
        let distortions = {
            #[cfg(all(target_arch = "x86_64", not(feature = "scalar-dsp")))]
            {
                if pair {
                    simd::satd_pair(&source, &p, &q)
                } else {
                    [satd4(&source, 4, &p, 4), 0]
                }
            }
            #[cfg(not(all(target_arch = "x86_64", not(feature = "scalar-dsp"))))]
            {
                [
                    satd4(&source, 4, &p, 4),
                    if pair { satd4(&source, 4, &q, 4) } else { 0 },
                ]
            }
        };
        for j in 0..if pair { 2 } else { 1 } {
            let mode = order[i + j];
            let d = distortions[j];
            let score = d + if mode == mpm { lambda } else { 4 * lambda };
            if score < best {
                best = score;
                chosen = mode;
                pixels = if j == 0 { p } else { q };
            }
            if d == 0 {
                return (chosen, pixels, best);
            }
        }
        i += if pair { 2 } else { 1 };
    }
    (chosen, pixels, best)
}
#[inline]
pub(super) fn average_block(a: &[u8; 256], b: &[u8; 256], len: usize) -> [u8; 256] {
    assert!(matches!(len, 64 | 128 | 256));
    #[cfg(all(target_arch = "x86_64", not(feature = "scalar-dsp")))]
    {
        return simd::average_block(a, b, len);
    }
    #[cfg(not(all(target_arch = "x86_64", not(feature = "scalar-dsp"))))]
    {
        let mut out = [0; 256];
        for i in 0..len {
            out[i] = ((u16::from(a[i]) + u16::from(b[i]) + 1) >> 1) as u8;
        }
        out
    }
}
/// Two adjacent 4x4 Hadamards per SSE2 vector. Full footprint checked once.
pub(super) fn satd_block(a: &[u8], sa: usize, b: &[u8], sb: usize, n: usize) -> u32 {
    assert!(matches!(n, 8 | 16));
    assert!(a.len() >= n && sa <= (a.len() - n) / (n - 1));
    assert!(b.len() >= n && sb <= (b.len() - n) / (n - 1));
    #[cfg(all(target_arch = "x86_64", not(feature = "scalar-dsp")))]
    {
        return simd::satd_block(a, sa, b, sb, n);
    }
    #[cfg(not(all(target_arch = "x86_64", not(feature = "scalar-dsp"))))]
    {
        let mut total = 0;
        for y in (0..n).step_by(4) {
            for x in (0..n).step_by(4) {
                total += satd4(&a[y * sa + x..], sa, &b[y * sb + x..], sb);
            }
        }
        total
    }
}

#[inline]
pub(super) fn nonzero_mask(c: &[i16]) -> u32 {
    assert!(c.len() <= 16);
    #[cfg(all(target_arch = "x86_64", not(feature = "scalar-dsp")))]
    {
        return simd::nonzero_mask(c);
    }
    #[cfg(not(all(target_arch = "x86_64", not(feature = "scalar-dsp"))))]
    {
        c.iter()
            .enumerate()
            .fold(0, |m, (i, &v)| m | ((v != 0) as u32) << i)
    }
}
#[cfg(all(target_arch = "x86_64", not(feature = "scalar-dsp")))]
#[allow(unsafe_code)]
mod simd {
    use std::arch::x86_64::*;
    #[inline]
    pub(super) fn average_block(a: &[u8; 256], b: &[u8; 256], len: usize) -> [u8; 256] {
        unsafe {
            let mut out = [0; 256];
            for i in (0..len).step_by(16) {
                let av = _mm_loadu_si128(a.as_ptr().add(i).cast());
                let bv = _mm_loadu_si128(b.as_ptr().add(i).cast());
                _mm_storeu_si128(out.as_mut_ptr().add(i).cast(), _mm_avg_epu8(av, bv));
            }
            out
        }
    }

    #[inline]
    pub(super) fn decimate_mask(c: &[i16; 16], ac_only: bool) -> (u32, bool) {
        unsafe {
            let lo = _mm_loadu_si128(c.as_ptr().cast());
            let hi = _mm_loadu_si128(c.as_ptr().add(8).cast());
            // Signed saturation preserves zero, +/-1 and the >1 class even
            // at i16::MIN/MAX, so both tests share one packed vector.
            let v = _mm_packs_epi16(lo, hi);
            let keep = 65535u32 ^ u32::from(ac_only);
            let large = _mm_movemask_epi8(_mm_or_si128(
                _mm_cmpgt_epi8(v, _mm_set1_epi8(1)),
                _mm_cmpgt_epi8(_mm_set1_epi8(-1), v),
            )) as u32
                & keep;
            let zero = _mm_cmpeq_epi8(v, _mm_setzero_si128());
            ((!_mm_movemask_epi8(zero) as u32) & keep, large != 0)
        }
    }
    #[inline]
    pub fn nonzero_mask(c: &[i16]) -> u32 {
        unsafe {
            let z = _mm_setzero_si128();
            let (lo, hi) = match c.len() {
                16 => (
                    _mm_loadu_si128(c.as_ptr().cast()),
                    _mm_loadu_si128(c.as_ptr().add(8).cast()),
                ),
                15 => (
                    _mm_loadu_si128(c.as_ptr().cast()),
                    _mm_srli_si128::<2>(_mm_loadu_si128(c.as_ptr().add(7).cast())),
                ),
                4 => (_mm_loadl_epi64(c.as_ptr().cast()), z),
                _ => {
                    return c
                        .iter()
                        .enumerate()
                        .fold(0, |m, (i, &v)| m | ((v != 0) as u32) << i);
                }
            };
            let eq = _mm_packs_epi16(_mm_cmpeq_epi16(lo, z), _mm_cmpeq_epi16(hi, z));
            (!_mm_movemask_epi8(eq) as u32) & ((1 << c.len()) - 1)
        }
    }
    #[inline]
    unsafe fn transpose(v: [__m128i; 4]) -> [__m128i; 4] {
        unsafe {
            let a = _mm_unpacklo_epi16(v[0], v[1]);
            let b = _mm_unpacklo_epi16(v[2], v[3]);
            let c = _mm_unpacklo_epi32(a, b);
            let d = _mm_unpackhi_epi32(a, b);
            let z = _mm_setzero_si128();
            [
                _mm_unpacklo_epi64(c, z),
                _mm_srli_si128(c, 8),
                _mm_unpacklo_epi64(d, z),
                _mm_srli_si128(d, 8),
            ]
        }
    }
    #[inline]
    unsafe fn transpose_pair(v: [__m128i; 4]) -> [__m128i; 4] {
        unsafe {
            let a = _mm_unpacklo_epi16(v[0], v[1]);
            let b = _mm_unpacklo_epi16(v[2], v[3]);
            let c = _mm_unpackhi_epi16(v[0], v[1]);
            let d = _mm_unpackhi_epi16(v[2], v[3]);
            let ab0 = _mm_unpacklo_epi32(a, b);
            let ab1 = _mm_unpackhi_epi32(a, b);
            let cd0 = _mm_unpacklo_epi32(c, d);
            let cd1 = _mm_unpackhi_epi32(c, d);
            [
                _mm_unpacklo_epi64(ab0, cd0),
                _mm_unpackhi_epi64(ab0, cd0),
                _mm_unpacklo_epi64(ab1, cd1),
                _mm_unpackhi_epi64(ab1, cd1),
            ]
        }
    }
    #[inline]
    unsafe fn pair_sums(rows: [__m128i; 4]) -> [u32; 2] {
        unsafe {
            let rows = butterfly::<false>(transpose_pair(butterfly::<false>(rows)));
            let mut sum = _mm_setzero_si128();
            let ones = _mm_set1_epi16(1);
            for row in rows {
                let sign = _mm_srai_epi16(row, 15);
                let abs = _mm_sub_epi16(_mm_xor_si128(row, sign), sign);
                sum = _mm_add_epi32(sum, _mm_madd_epi16(abs, ones));
            }
            sum = _mm_add_epi32(sum, _mm_srli_si128(sum, 4));
            [
                _mm_cvtsi128_si32(sum) as u32 / 2,
                _mm_cvtsi128_si32(_mm_srli_si128(sum, 8)) as u32 / 2,
            ]
        }
    }
    #[inline]
    pub(super) fn sad4(a: &[u8; 16], b: &[u8; 16]) -> u32 {
        unsafe {
            let v = _mm_sad_epu8(
                _mm_loadu_si128(a.as_ptr().cast()),
                _mm_loadu_si128(b.as_ptr().cast()),
            );
            (_mm_cvtsi128_si64(v) + _mm_cvtsi128_si64(_mm_srli_si128::<8>(v))) as u32
        }
    }
    #[inline]
    pub(super) fn intra16_sad(
        src: &[u8],
        stride: usize,
        top: &[u8; 32],
        left: &[u8; 16],
        dc: u8,
    ) -> [u32; 3] {
        unsafe {
            let v = _mm_loadu_si128(top.as_ptr().cast());
            let d = _mm_set1_epi8(dc as i8);
            let mut sums = [_mm_setzero_si128(); 3];
            for y in 0..16 {
                let a = _mm_loadu_si128(src.as_ptr().add(y * stride).cast());
                let h = _mm_set1_epi8(left[y] as i8);
                sums[0] = _mm_add_epi64(sums[0], _mm_sad_epu8(a, v));
                sums[1] = _mm_add_epi64(sums[1], _mm_sad_epu8(a, h));
                sums[2] = _mm_add_epi64(sums[2], _mm_sad_epu8(a, d));
            }
            sums.map(|s| (_mm_cvtsi128_si64(s) + _mm_cvtsi128_si64(_mm_srli_si128::<8>(s))) as u32)
        }
    }
    #[inline]
    pub(super) fn zero_chroma(src: &[u8], stride: usize, pred: &[u8; 64], limit: u8) -> bool {
        unsafe {
            let mut diff = _mm_setzero_si128();
            for y in 0..8 {
                let a = _mm_loadl_epi64(src.as_ptr().add(y * stride).cast());
                let b = _mm_loadl_epi64(pred.as_ptr().add(y * 8).cast());
                diff = _mm_max_epu8(diff, _mm_or_si128(_mm_subs_epu8(a, b), _mm_subs_epu8(b, a)));
            }
            _mm_movemask_epi8(_mm_cmpeq_epi8(
                _mm_subs_epu8(diff, _mm_set1_epi8(limit as i8)),
                _mm_setzero_si128(),
            )) == 65535
        }
    }
    #[inline]
    pub fn satd_pair(a: &[u8; 16], b: &[u8; 16], c: &[u8; 16]) -> [u32; 2] {
        unsafe {
            let z = _mm_setzero_si128();
            let rows = std::array::from_fn(|i| {
                let av = _mm_cvtsi32_si128(a.as_ptr().add(i * 4).cast::<i32>().read_unaligned());
                let bv = _mm_cvtsi32_si128(b.as_ptr().add(i * 4).cast::<i32>().read_unaligned());
                let cv = _mm_cvtsi32_si128(c.as_ptr().add(i * 4).cast::<i32>().read_unaligned());
                _mm_sub_epi16(
                    _mm_unpacklo_epi8(_mm_unpacklo_epi32(av, av), z),
                    _mm_unpacklo_epi8(_mm_unpacklo_epi32(bv, cv), z),
                )
            });
            pair_sums(rows)
        }
    }
    pub(super) fn satd_block(a: &[u8], sa: usize, b: &[u8], sb: usize, n: usize) -> u32 {
        unsafe {
            if std::is_x86_feature_detected!("avx2") {
                return satd_avx2(a, sa, b, sb, n);
            }
            let z = _mm_setzero_si128();
            let mut total = 0;
            for y in (0..n).step_by(4) {
                for x in (0..n).step_by(8) {
                    let rows = std::array::from_fn(|i| {
                        let av = _mm_loadl_epi64(a.as_ptr().add((y + i) * sa + x).cast());
                        let bv = _mm_loadl_epi64(b.as_ptr().add((y + i) * sb + x).cast());
                        _mm_sub_epi16(_mm_unpacklo_epi8(av, z), _mm_unpacklo_epi8(bv, z))
                    });
                    let s = pair_sums(rows);
                    total += s[0] + s[1];
                }
            }
            total
        }
    }
    #[target_feature(enable = "avx2")]
    #[inline]
    unsafe fn had_avx2(v: [__m256i; 4]) -> [__m256i; 4] {
        let p = _mm256_add_epi16(v[0], v[3]);
        let q = _mm256_add_epi16(v[1], v[2]);
        let r = _mm256_sub_epi16(v[1], v[2]);
        let s = _mm256_sub_epi16(v[0], v[3]);
        [
            _mm256_add_epi16(p, q),
            _mm256_add_epi16(s, r),
            _mm256_sub_epi16(p, q),
            _mm256_sub_epi16(s, r),
        ]
    }
    #[target_feature(enable = "avx2")]
    #[inline]
    unsafe fn transpose_avx2(v: [__m256i; 4]) -> [__m256i; 4] {
        let a = _mm256_unpacklo_epi16(v[0], v[1]);
        let b = _mm256_unpacklo_epi16(v[2], v[3]);
        let c = _mm256_unpackhi_epi16(v[0], v[1]);
        let d = _mm256_unpackhi_epi16(v[2], v[3]);
        let ab0 = _mm256_unpacklo_epi32(a, b);
        let ab1 = _mm256_unpackhi_epi32(a, b);
        let cd0 = _mm256_unpacklo_epi32(c, d);
        let cd1 = _mm256_unpackhi_epi32(c, d);
        [
            _mm256_unpacklo_epi64(ab0, cd0),
            _mm256_unpackhi_epi64(ab0, cd0),
            _mm256_unpacklo_epi64(ab1, cd1),
            _mm256_unpackhi_epi64(ab1, cd1),
        ]
    }
    #[target_feature(enable = "avx2")]
    #[inline]
    unsafe fn satd_avx2(a: &[u8], sa: usize, b: &[u8], sb: usize, n: usize) -> u32 {
        unsafe {
            let mut total = _mm256_setzero_si256();
            let ones = _mm256_set1_epi16(1);
            for group in 0..if n == 16 { 4 } else { 1 } {
                let rows = std::array::from_fn(|r| {
                    let (av, bv) = if n == 16 {
                        (
                            _mm_loadu_si128(a.as_ptr().add((group * 4 + r) * sa).cast()),
                            _mm_loadu_si128(b.as_ptr().add((group * 4 + r) * sb).cast()),
                        )
                    } else {
                        (
                            _mm_unpacklo_epi64(
                                _mm_loadl_epi64(a.as_ptr().add(r * sa).cast()),
                                _mm_loadl_epi64(a.as_ptr().add((r + 4) * sa).cast()),
                            ),
                            _mm_unpacklo_epi64(
                                _mm_loadl_epi64(b.as_ptr().add(r * sb).cast()),
                                _mm_loadl_epi64(b.as_ptr().add((r + 4) * sb).cast()),
                            ),
                        )
                    };
                    _mm256_sub_epi16(_mm256_cvtepu8_epi16(av), _mm256_cvtepu8_epi16(bv))
                });
                let rows = had_avx2(transpose_avx2(had_avx2(rows)));
                for v in rows {
                    total = _mm256_add_epi32(total, _mm256_madd_epi16(_mm256_abs_epi16(v), ones));
                }
            }
            let sum = _mm_add_epi32(
                _mm256_castsi256_si128(total),
                _mm256_extracti128_si256::<1>(total),
            );
            let sum = _mm_add_epi32(sum, _mm_srli_si128::<8>(sum));
            _mm_cvtsi128_si32(_mm_add_epi32(sum, _mm_srli_si128::<4>(sum))) as u32 / 2
        }
    }
    #[inline]
    unsafe fn butterfly<const TRANSFORM: bool>(v: [__m128i; 4]) -> [__m128i; 4] {
        unsafe {
            let p = _mm_add_epi16(v[0], v[3]);
            let q = _mm_add_epi16(v[1], v[2]);
            let r = _mm_sub_epi16(v[1], v[2]);
            let s = _mm_sub_epi16(v[0], v[3]);
            [
                _mm_add_epi16(p, q),
                if TRANSFORM {
                    _mm_add_epi16(_mm_slli_epi16(s, 1), r)
                } else {
                    _mm_add_epi16(s, r)
                },
                _mm_sub_epi16(p, q),
                if TRANSFORM {
                    _mm_sub_epi16(s, _mm_slli_epi16(r, 1))
                } else {
                    _mm_sub_epi16(s, r)
                },
            ]
        }
    }

    #[inline]
    unsafe fn inverse_words(v: [__m128i; 4]) -> [__m128i; 4] {
        unsafe {
            let a = _mm_add_epi16(v[0], v[2]);
            let b = _mm_sub_epi16(v[0], v[2]);
            let c = _mm_sub_epi16(_mm_srai_epi16(v[1], 1), v[3]);
            let d = _mm_add_epi16(v[1], _mm_srai_epi16(v[3], 1));
            [
                _mm_add_epi16(a, d),
                _mm_add_epi16(b, c),
                _mm_sub_epi16(b, c),
                _mm_sub_epi16(a, d),
            ]
        }
    }

    #[inline]
    pub(super) fn gather4(src: &[u8], stride: usize) -> [u8; 16] {
        unsafe {
            let row = |i| src.as_ptr().add(i * stride).cast::<i32>().read_unaligned();
            let v = _mm_set_epi32(row(3), row(2), row(1), row(0));
            let mut out = [0; 16];
            _mm_storeu_si128(out.as_mut_ptr().cast(), v);
            out
        }
    }
    #[inline]
    pub(super) fn basic_prediction(mode: u8, e: &super::Edges4, available: u8) -> [u8; 16] {
        unsafe {
            let top = e.top.as_ptr().cast::<i32>().read_unaligned();
            let left = e.left.as_ptr().cast::<i32>().read_unaligned();
            let v = match mode {
                0 => _mm_set1_epi32(top),
                1 => {
                    let l = _mm_cvtsi32_si128(left);
                    let l = _mm_unpacklo_epi8(l, l);
                    _mm_unpacklo_epi16(l, l)
                }
                2 => {
                    let t = _mm_cvtsi32_si128(top);
                    let l = _mm_cvtsi32_si128(left);
                    let z = _mm_setzero_si128();
                    let (v, round, shift) = match available {
                        3 => (_mm_unpacklo_epi32(t, l), 4, 3),
                        1 => (t, 2, 2),
                        2 => (l, 2, 2),
                        _ => (z, 128, 0),
                    };
                    let sum = _mm_cvtsi128_si32(_mm_sad_epu8(v, z));
                    _mm_set1_epi8(((sum + round) >> shift) as i8)
                }
                _ => unreachable!(),
            };
            let mut out = [0; 16];
            _mm_storeu_si128(out.as_mut_ptr().cast(), v);
            out
        }
    }
    #[inline]
    pub(super) fn directional(mode: u8, e: &super::Edges4) -> Option<[u8; 16]> {
        if std::is_x86_feature_detected!("ssse3") {
            Some(unsafe { directional_ssse3(mode, e) })
        } else {
            None
        }
    }
    #[inline]
    unsafe fn quarter_bytes(a: __m128i, b: __m128i, c: __m128i) -> __m128i {
        unsafe {
            // q(a,b,c) = ceil((b + floor((a+c)/2))/2), exactly.
            let odd = _mm_and_si128(_mm_xor_si128(a, c), _mm_set1_epi8(1));
            _mm_avg_epu8(_mm_sub_epi8(_mm_avg_epu8(a, c), odd), b)
        }
    }
    #[target_feature(enable = "ssse3")]
    #[inline]
    unsafe fn directional_ssse3(mode: u8, e: &super::Edges4) -> [u8; 16] {
        unsafe {
            let v = match mode {
                3 | 7 | 8 => {
                    let edge = if mode == 8 { &e.left } else { &e.top };
                    let line = _mm_shuffle_epi8(
                        _mm_loadl_epi64(edge.as_ptr().cast()),
                        _mm_setr_epi8(0, 1, 2, 3, 4, 5, 6, 7, 7, 7, 7, 7, 7, 7, 7, 7),
                    );
                    let q =
                        quarter_bytes(line, _mm_srli_si128::<1>(line), _mm_srli_si128::<2>(line));
                    if mode == 3 {
                        _mm_shuffle_epi8(
                            q,
                            _mm_setr_epi8(0, 1, 2, 3, 1, 2, 3, 4, 2, 3, 4, 5, 3, 4, 5, 6),
                        )
                    } else {
                        let h = _mm_avg_epu8(line, _mm_srli_si128::<1>(line));
                        let packed = _mm_unpacklo_epi64(h, q);
                        let mask = if mode == 7 {
                            _mm_setr_epi8(0, 1, 2, 3, 8, 9, 10, 11, 1, 2, 3, 4, 9, 10, 11, 12)
                        } else {
                            _mm_setr_epi8(0, 8, 1, 9, 1, 9, 2, 10, 2, 10, 3, 11, 3, 11, 4, 12)
                        };
                        _mm_shuffle_epi8(packed, mask)
                    }
                }
                4 | 5 | 6 => {
                    let (t, l) = if mode == 6 {
                        (&e.left, &e.top)
                    } else {
                        (&e.top, &e.left)
                    };
                    let top = _mm_cvtsi32_si128(t.as_ptr().cast::<i32>().read_unaligned());
                    let left = _mm_cvtsi32_si128(l.as_ptr().cast::<i32>().read_unaligned());
                    let base =
                        _mm_insert_epi16::<2>(_mm_unpacklo_epi64(left, top), e.corner as i32);
                    if mode == 4 {
                        let line = _mm_shuffle_epi8(
                            base,
                            _mm_setr_epi8(3, 2, 1, 0, 4, 8, 9, 10, 11, 11, 11, 11, 11, 11, 11, 11),
                        );
                        let q = quarter_bytes(
                            line,
                            _mm_srli_si128::<1>(line),
                            _mm_srli_si128::<2>(line),
                        );
                        _mm_shuffle_epi8(
                            q,
                            _mm_setr_epi8(3, 4, 5, 6, 2, 3, 4, 5, 1, 2, 3, 4, 0, 1, 2, 3),
                        )
                    } else {
                        let line = _mm_shuffle_epi8(
                            base,
                            _mm_setr_epi8(2, 1, 0, 4, 8, 9, 10, 11, 11, 11, 11, 11, 11, 11, 11, 11),
                        );
                        let q = quarter_bytes(
                            line,
                            _mm_srli_si128::<1>(line),
                            _mm_srli_si128::<2>(line),
                        );
                        let h = _mm_avg_epu8(line, _mm_srli_si128::<1>(line));
                        let packed = _mm_unpacklo_epi64(q, h);
                        let mask = if mode == 5 {
                            _mm_setr_epi8(11, 12, 13, 14, 2, 3, 4, 5, 1, 11, 12, 13, 0, 2, 3, 4)
                        } else {
                            _mm_setr_epi8(11, 2, 1, 0, 12, 3, 11, 2, 13, 4, 12, 3, 14, 5, 13, 4)
                        };
                        _mm_shuffle_epi8(packed, mask)
                    }
                }
                _ => unreachable!(),
            };
            let mut out = [0; 16];
            _mm_storeu_si128(out.as_mut_ptr().cast(), v);
            out
        }
    }
    #[inline]
    pub(super) fn add_dc_into(p: &[u8], ps: usize, d: &mut [u8], ds: usize, delta: i16) {
        unsafe {
            let z = _mm_setzero_si128();
            let add = _mm_set1_epi16(delta);
            for y in 0..4 {
                let old = p.as_ptr().add(y * ps).cast::<i32>().read_unaligned();
                let out = if delta == 0 {
                    old
                } else {
                    _mm_cvtsi128_si32(_mm_packus_epi16(
                        _mm_add_epi16(_mm_unpacklo_epi8(_mm_cvtsi32_si128(old), z), add),
                        z,
                    ))
                };
                d.as_mut_ptr()
                    .add(y * ds)
                    .cast::<i32>()
                    .write_unaligned(out);
            }
        }
    }
    #[inline]
    unsafe fn inverse_wide(v: [__m128i; 4]) -> [__m128i; 4] {
        unsafe {
            let a = _mm_add_epi32(v[0], v[2]);
            let b = _mm_sub_epi32(v[0], v[2]);
            let c = _mm_sub_epi32(_mm_srai_epi32(v[1], 1), v[3]);
            let d = _mm_add_epi32(v[1], _mm_srai_epi32(v[3], 1));
            [
                _mm_add_epi32(a, d),
                _mm_add_epi32(b, c),
                _mm_sub_epi32(b, c),
                _mm_sub_epi32(a, d),
            ]
        }
    }
    #[inline]
    unsafe fn transpose_wide(v: [__m128i; 4]) -> [__m128i; 4] {
        unsafe {
            let a = _mm_unpacklo_epi32(v[0], v[1]);
            let b = _mm_unpackhi_epi32(v[0], v[1]);
            let c = _mm_unpacklo_epi32(v[2], v[3]);
            let d = _mm_unpackhi_epi32(v[2], v[3]);
            [
                _mm_unpacklo_epi64(a, c),
                _mm_unpackhi_epi64(a, c),
                _mm_unpacklo_epi64(b, d),
                _mm_unpackhi_epi64(b, d),
            ]
        }
    }
    #[inline]
    pub(super) fn reconstruct_into(
        c: &[i16; 16],
        dq: &[i16; 16],
        p: &[u8],
        ps: usize,
        d: &mut [u8],
        ds: usize,
        dc: Option<i32>,
    ) {
        unsafe {
            let mut lo = _mm_mullo_epi16(
                _mm_loadu_si128(c.as_ptr().cast()),
                _mm_loadu_si128(dq.as_ptr().cast()),
            );
            let hi = _mm_mullo_epi16(
                _mm_loadu_si128(c.as_ptr().add(8).cast()),
                _mm_loadu_si128(dq.as_ptr().add(8).cast()),
            );
            if let Some(dc) = dc {
                lo = _mm_insert_epi16::<0>(lo, dc);
            }
            let mut rows = transpose([lo, _mm_srli_si128::<8>(lo), hi, _mm_srli_si128::<8>(hi)]);
            rows[0] = _mm_add_epi16(rows[0], _mm_cvtsi32_si128(32));
            let z = _mm_setzero_si128();
            if dc.is_none() && dq[5] <= 928 {
                let rows = inverse_words(transpose(inverse_words(rows)));
                for y in 0..4 {
                    let old = _mm_unpacklo_epi8(
                        _mm_cvtsi32_si128(p.as_ptr().add(y * ps).cast::<i32>().read_unaligned()),
                        z,
                    );
                    let v = _mm_add_epi16(old, _mm_srai_epi16(rows[y], 6));
                    d.as_mut_ptr()
                        .add(y * ds)
                        .cast::<i32>()
                        .write_unaligned(_mm_cvtsi128_si32(_mm_packus_epi16(v, z)));
                }
            } else {
                let rows = rows.map(|v| _mm_unpacklo_epi16(v, _mm_srai_epi16(v, 15)));
                let first =
                    inverse_wide(rows).map(|v| _mm_srai_epi32::<16>(_mm_slli_epi32::<16>(v)));
                let rows = inverse_wide(transpose_wide(first));
                for y in 0..4 {
                    let old = _mm_unpacklo_epi16(
                        _mm_unpacklo_epi8(
                            _mm_cvtsi32_si128(
                                p.as_ptr().add(y * ps).cast::<i32>().read_unaligned(),
                            ),
                            z,
                        ),
                        z,
                    );
                    let v = _mm_add_epi32(old, _mm_srai_epi32(rows[y], 6));
                    d.as_mut_ptr()
                        .add(y * ds)
                        .cast::<i32>()
                        .write_unaligned(_mm_cvtsi128_si32(_mm_packus_epi16(
                            _mm_packs_epi32(v, z),
                            z,
                        )));
                }
            }
        }
    }
    // OpenH264's 16-bit quantizer: pre-bias then unsigned multiply-high.
    // 8-bit forward levels plus FF fit u16; quantized signed levels fit i16.
    #[inline]
    unsafe fn quant8(v: __m128i, mf: __m128i, ff: __m128i) -> __m128i {
        unsafe {
            let sign = _mm_srai_epi16(v, 15);
            let abs = _mm_sub_epi16(_mm_xor_si128(v, sign), sign);
            let level = _mm_mulhi_epu16(_mm_add_epi16(abs, ff), mf);
            _mm_sub_epi16(_mm_xor_si128(level, sign), sign)
        }
    }

    // OpenH264's combined-3 strategy: transform the source once; vertical,
    // horizontal and DC prediction only alter one row/column/DC coefficient.
    #[inline]
    pub fn intra_three(a: &[u8; 16], e: &super::Edges4) -> [u32; 3] {
        let dc = (e.top[..4].iter().map(|&v| v as i32).sum::<i32>()
            + e.left[..4].iter().map(|&v| v as i32).sum::<i32>()
            + 4)
            >> 3;
        unsafe { basic_three(a, 4, &e.top, &e.left, dc) }
    }
    #[inline]
    unsafe fn basic_three(a: &[u8], stride: usize, top: &[u8], left: &[u8], dc: i32) -> [u32; 3] {
        unsafe {
            let z = _mm_setzero_si128();
            let rows = std::array::from_fn(|i| {
                _mm_unpacklo_epi8(
                    _mm_cvtsi32_si128(a.as_ptr().add(i * stride).cast::<i32>().read_unaligned()),
                    z,
                )
            });
            let rows = butterfly::<false>(transpose(butterfly::<false>(rows)));
            let ones = _mm_set1_epi16(1);
            let mut sums = z;
            for row in rows {
                let sign = _mm_srai_epi16(row, 15);
                sums = _mm_add_epi32(
                    sums,
                    _mm_madd_epi16(_mm_sub_epi16(_mm_xor_si128(row, sign), sign), ones),
                );
            }
            let total = _mm_cvtsi128_si32(_mm_add_epi32(sums, _mm_srli_si128(sums, 4)));
            let top = had_line(top);
            let left = had_line(left);
            // V/H predictions change only one Hadamard column/row. Correct
            // both costs together; 8-bit coefficients/deltas fit signed i16.
            let column = _mm_unpacklo_epi32(
                _mm_unpacklo_epi16(rows[0], rows[1]),
                _mm_unpacklo_epi16(rows[2], rows[3]),
            );
            let source = _mm_unpacklo_epi64(column, rows[0]);
            let predicted = _mm_setr_epi16(
                (4 * top[0]) as i16,
                (4 * top[1]) as i16,
                (4 * top[2]) as i16,
                (4 * top[3]) as i16,
                (4 * left[0]) as i16,
                (4 * left[1]) as i16,
                (4 * left[2]) as i16,
                (4 * left[3]) as i16,
            );
            let abs = |v| {
                let sign = _mm_srai_epi16::<15>(v);
                _mm_sub_epi16(_mm_xor_si128(v, sign), sign)
            };
            let delta = _mm_madd_epi16(
                _mm_sub_epi16(abs(_mm_sub_epi16(source, predicted)), abs(source)),
                ones,
            );
            let delta = _mm_add_epi32(delta, _mm_srli_si128::<4>(delta));
            let v = total + _mm_cvtsi128_si32(delta);
            let h = total + _mm_cvtsi128_si32(_mm_srli_si128::<8>(delta));
            let first = _mm_cvtsi128_si32(rows[0]) as i16 as i32;
            let d = total - first.abs() + (first - 16 * dc).abs();
            [v as u32 / 2, h as u32 / 2, d as u32 / 2]
        }
    }
    #[inline]
    fn had_line(v: &[u8]) -> [i32; 4] {
        let a = v[0] as i32;
        let b = v[1] as i32;
        let c = v[2] as i32;
        let d = v[3] as i32;
        [a + b + c + d, a + b - c - d, a - b - c + d, a - b + c - d]
    }
    #[inline]
    pub(super) fn forward_four(
        a: &[u8],
        sa: usize,
        b: &[u8],
        sb: usize,
        mf: &[i16; 16],
        ff: &[i16; 16],
    ) -> ([[i16; 16]; 4], [i32; 4]) {
        if std::arch::is_x86_feature_detected!("avx2") {
            return unsafe { forward_four_avx2(a, sa, b, sb, mf, ff) };
        }
        unsafe {
            let z = _mm_setzero_si128();
            let mf = _mm_loadu_si128(mf.as_ptr().cast());
            let ff = _mm_loadu_si128(ff.as_ptr().cast());
            let mut out = [[0i16; 16]; 4];
            let mut dc = [0i32; 4];
            for group in 0..2 {
                let rows = std::array::from_fn(|r| {
                    let av = _mm_loadl_epi64(a.as_ptr().add((group * 4 + r) * sa).cast());
                    let bv = _mm_loadl_epi64(b.as_ptr().add((group * 4 + r) * sb).cast());
                    _mm_sub_epi16(_mm_unpacklo_epi8(av, z), _mm_unpacklo_epi8(bv, z))
                });
                let rows =
                    transpose_pair(butterfly::<true>(transpose_pair(butterfly::<true>(rows))));
                dc[group * 2] = _mm_extract_epi16::<0>(rows[0]) as i16 as i32;
                dc[group * 2 + 1] = _mm_extract_epi16::<4>(rows[0]) as i16 as i32;
                _mm_storeu_si128(
                    out[group * 2].as_mut_ptr().cast(),
                    quant8(_mm_unpacklo_epi64(rows[0], rows[1]), mf, ff),
                );
                _mm_storeu_si128(
                    out[group * 2].as_mut_ptr().add(8).cast(),
                    quant8(_mm_unpacklo_epi64(rows[2], rows[3]), mf, ff),
                );
                _mm_storeu_si128(
                    out[group * 2 + 1].as_mut_ptr().cast(),
                    quant8(_mm_unpackhi_epi64(rows[0], rows[1]), mf, ff),
                );
                _mm_storeu_si128(
                    out[group * 2 + 1].as_mut_ptr().add(8).cast(),
                    quant8(_mm_unpackhi_epi64(rows[2], rows[3]), mf, ff),
                );
            }
            (out, dc)
        }
    }
    #[target_feature(enable = "avx2")]
    #[inline]
    unsafe fn forward_four_avx2(
        a: &[u8],
        sa: usize,
        b: &[u8],
        sb: usize,
        mf: &[i16; 16],
        ff: &[i16; 16],
    ) -> ([[i16; 16]; 4], [i32; 4]) {
        unsafe {
            let dct = |v: [__m256i; 4]| {
                let p = _mm256_add_epi16(v[0], v[3]);
                let q = _mm256_add_epi16(v[1], v[2]);
                let r = _mm256_sub_epi16(v[1], v[2]);
                let s = _mm256_sub_epi16(v[0], v[3]);
                [
                    _mm256_add_epi16(p, q),
                    _mm256_add_epi16(_mm256_slli_epi16::<1>(s), r),
                    _mm256_sub_epi16(p, q),
                    _mm256_sub_epi16(s, _mm256_slli_epi16::<1>(r)),
                ]
            };
            let rows = std::array::from_fn(|r| {
                let av = _mm_unpacklo_epi64(
                    _mm_loadl_epi64(a.as_ptr().add(r * sa).cast()),
                    _mm_loadl_epi64(a.as_ptr().add((r + 4) * sa).cast()),
                );
                let bv = _mm_unpacklo_epi64(
                    _mm_loadl_epi64(b.as_ptr().add(r * sb).cast()),
                    _mm_loadl_epi64(b.as_ptr().add((r + 4) * sb).cast()),
                );
                _mm256_sub_epi16(_mm256_cvtepu8_epi16(av), _mm256_cvtepu8_epi16(bv))
            });
            let rows = transpose_avx2(dct(transpose_avx2(dct(rows))));
            let mf = _mm256_broadcastsi128_si256(_mm_loadu_si128(mf.as_ptr().cast()));
            let ff = _mm256_broadcastsi128_si256(_mm_loadu_si128(ff.as_ptr().cast()));
            let quant = |v| {
                _mm256_sign_epi16(
                    _mm256_mulhi_epu16(_mm256_add_epi16(_mm256_abs_epi16(v), ff), mf),
                    v,
                )
            };
            let packed = [
                quant(_mm256_unpacklo_epi64(rows[0], rows[1])),
                quant(_mm256_unpacklo_epi64(rows[2], rows[3])),
                quant(_mm256_unpackhi_epi64(rows[0], rows[1])),
                quant(_mm256_unpackhi_epi64(rows[2], rows[3])),
            ];
            let mut out = [[0i16; 16]; 4];
            for i in 0..2 {
                _mm_storeu_si128(
                    out[0].as_mut_ptr().add(i * 8).cast(),
                    _mm256_castsi256_si128(packed[i]),
                );
                _mm_storeu_si128(
                    out[1].as_mut_ptr().add(i * 8).cast(),
                    _mm256_castsi256_si128(packed[i + 2]),
                );
                _mm_storeu_si128(
                    out[2].as_mut_ptr().add(i * 8).cast(),
                    _mm256_extracti128_si256::<1>(packed[i]),
                );
                _mm_storeu_si128(
                    out[3].as_mut_ptr().add(i * 8).cast(),
                    _mm256_extracti128_si256::<1>(packed[i + 2]),
                );
            }
            let dc = [
                _mm256_extract_epi16::<0>(rows[0]) as i16 as i32,
                _mm256_extract_epi16::<4>(rows[0]) as i16 as i32,
                _mm256_extract_epi16::<8>(rows[0]) as i16 as i32,
                _mm256_extract_epi16::<12>(rows[0]) as i16 as i32,
            ];
            (out, dc)
        }
    }
    #[inline]
    pub(super) fn forward_quant(
        a: &[u8],
        sa: usize,
        b: &[u8; 16],
        mf: &[i16; 16],
        bias: &[i16; 16],
    ) -> [i16; 16] {
        unsafe {
            let z = _mm_setzero_si128();
            let rows = std::array::from_fn(|i| {
                let av = a.as_ptr().add(i * sa).cast::<i32>().read_unaligned();
                let bv = b.as_ptr().add(i * 4).cast::<i32>().read_unaligned();
                _mm_sub_epi16(
                    _mm_unpacklo_epi8(_mm_cvtsi32_si128(av), z),
                    _mm_unpacklo_epi8(_mm_cvtsi32_si128(bv), z),
                )
            });
            let rows = transpose(butterfly::<true>(transpose(butterfly::<true>(rows))));
            let mut out = [0; 16];
            for i in 0..2 {
                let v = _mm_unpacklo_epi64(rows[i * 2], rows[i * 2 + 1]);
                let q = quant8(
                    v,
                    _mm_loadu_si128(mf.as_ptr().add(i * 8).cast()),
                    _mm_loadu_si128(bias.as_ptr().add(i * 8).cast()),
                );
                _mm_storeu_si128(out.as_mut_ptr().add(i * 8).cast(), q);
            }
            out
        }
    }

    #[inline]
    pub fn satd(a: &[u8], sa: usize, b: &[u8], sb: usize) -> u32 {
        assert!(a.len() >= 4 && sa <= (a.len() - 4) / 3);
        assert!(b.len() >= 4 && sb <= (b.len() - 4) / 3);
        unsafe {
            let z = _mm_setzero_si128();
            let rows = std::array::from_fn(|i| {
                let av = a.as_ptr().add(i * sa).cast::<i32>().read_unaligned();
                let bv = b.as_ptr().add(i * sb).cast::<i32>().read_unaligned();
                _mm_sub_epi16(
                    _mm_unpacklo_epi8(_mm_cvtsi32_si128(av), z),
                    _mm_unpacklo_epi8(_mm_cvtsi32_si128(bv), z),
                )
            });
            let rows = butterfly::<false>(transpose(butterfly::<false>(rows)));
            let ones = _mm_set1_epi16(1);
            let mut sum = z;
            for v in rows {
                let sign = _mm_srai_epi16(v, 15);
                let abs = _mm_sub_epi16(_mm_xor_si128(v, sign), sign);
                sum = _mm_add_epi32(sum, _mm_madd_epi16(abs, ones));
            }
            _mm_cvtsi128_si32(_mm_add_epi32(sum, _mm_srli_si128(sum, 4))) as u32 / 2
        }
    }
}
