// SPDX-License-Identifier: LGPL-2.1-or-later
//! Low-delay software encoder for the desktop sender. Reference interpolation,
//! prediction and deblocking share the decoder's checked byte-plane DSP.
//! Selected transform primitives are retained in the local syntax module;
//! frame decisions, motion search, rate control and ownership live here.
mod analysis;
mod pixels;
mod quant_tables;
mod rate;
mod search;
mod syntax;
use crate::syntax::{
    forward::forward_hadamard_4x4,
    transform::{FLAT_4X4_16, inverse_hadamard_luma_dc_16x16},
};
use crate::{
    dsp::{
        self, Block,
        filter::{Edge, Filter},
        intra::Edges,
    },
    picture::Plane,
};
use search::Mv;
use syntax::{BLOCKS, BitWriter, SCAN};

#[derive(Debug)]
pub struct EncodeError(pub &'static str);
impl std::fmt::Display for EncodeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.0)
    }
}
impl std::error::Error for EncodeError {}
impl From<crate::Error> for EncodeError {
    fn from(_: crate::Error) -> Self {
        Self("H264 encoder pixel geometry")
    }
}
pub type Result<T> = std::result::Result<T, EncodeError>;

/// The Annex B SPS and PPS this encoder's configuration implies.
///
/// A hardware encoder driven through VA-API has to supply these itself: the
/// i965 driver writes only the slice, and its packed-header support expects the
/// parameter sets to arrive as bytes rather than generated on request. Emitting
/// them from the same code that builds the software encoder's headers keeps the
/// two paths bit-identical in syntax, which matters because the driver's slice
/// has to match whatever this SPS declares.
pub fn parameter_sets(cfg: Config) -> Result<Vec<u8>> {
    cfg.validate()?;
    let mut out = Vec::new();
    syntax::parameters(cfg, &mut out);
    Ok(out)
}

#[derive(Clone, Copy, Debug)]
pub struct Config {
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub bitrate: u32,
}
impl Config {
    fn validate(self) -> Result<()> {
        if self.width == 0
            || self.height == 0
            || self.width > 3840
            || self.height > 2160
            || self.width % 2 != 0
            || self.height % 2 != 0
            || !(1..=144).contains(&self.fps)
            || !(1..=500_000_000).contains(&self.bitrate)
            || self.bitrate < self.fps.min(60)
        {
            return Err(EncodeError("invalid H264 encoder configuration"));
        }
        Ok(())
    }
}
pub struct AccessUnit {
    pub data: Vec<u8>,
    pub keyframe: bool,
    pub timestamp_100ns: i64,
}
#[derive(Clone, Copy, Default)]
struct Mb {
    intra: bool,
    mvs: [Mv; 4],
    integer_mv: Mv,
    qp: i32,
    nz: u16,
    quality_qp: i32,
    coded: bool,
    sad_cost: u32,
    skip: bool,
    skip_sad: u32,
}
#[derive(Clone, Copy, Default)]
struct ReferenceMb {
    integer_mv: Mv,
    skip_sad: Option<u32>,
    quality_qp: i32,
}
type Prediction = ([u8; 256], [[u8; 64]; 2], u32, u32);
struct Counts {
    width: usize,
    values: Vec<i8>,
}
impl Counts {
    fn new(w: usize, h: usize) -> Self {
        Self {
            width: w,
            values: vec![-1; w * h],
        }
    }
    #[inline]
    fn put(&mut self, x: usize, y: usize, count: usize) -> i32 {
        let a = if x > 0 {
            self.values[y * self.width + x - 1]
        } else {
            -1
        };
        let b = if y > 0 {
            self.values[(y - 1) * self.width + x]
        } else {
            -1
        };
        self.values[y * self.width + x] = count as i8;
        match (a >= 0, b >= 0) {
            (true, true) => (a as i32 + b as i32 + 1) / 2,
            (true, false) => a as i32,
            (false, true) => b as i32,
            _ => 0,
        }
    }
}
pub struct Encoder {
    cfg: Config,
    w: usize,
    h: usize,
    source: [Vec<u8>; 3],
    previous_luma: Vec<u8>,
    recon: [Vec<u8>; 3],
    reference: [Plane; 3],
    counts: [Counts; 3],
    modes: Vec<i8>,
    mbs: Vec<Mb>,
    previous: Vec<ReferenceMb>,
    reference_idr: bool,
    analysis: analysis::Analysis,
    rate: rate::Rate,
    frame: u16,
    id: u16,
    ready: bool,
    headers: Vec<u8>,
    since_idr: u32,
}
impl Encoder {
    pub fn new(cfg: Config) -> Result<Self> {
        cfg.validate()?;
        if cfg.bitrate > rate::MAXIMUM_BITRATE {
            return Err(EncodeError("H264 initial bitrate exceeds level limit"));
        }
        let w = (cfg.width as usize).next_multiple_of(16);
        let h = (cfg.height as usize).next_multiple_of(16);
        let buffers = || [vec![0; w * h], vec![0; w * h / 4], vec![0; w * h / 4]];
        let mut headers = Vec::new();
        syntax::parameters(cfg, &mut headers);
        Ok(Self {
            cfg,
            w,
            h,
            source: buffers(),
            previous_luma: vec![0; w * h],
            recon: buffers(),
            reference: [
                Plane::new(w, h, 32)?,
                Plane::new(w / 2, h / 2, 16)?,
                Plane::new(w / 2, h / 2, 16)?,
            ],
            counts: [
                Counts::new(w / 4, h / 4),
                Counts::new(w / 8, h / 8),
                Counts::new(w / 8, h / 8),
            ],
            modes: vec![-1; w * h / 16],
            mbs: vec![Mb::default(); w * h / 256],
            previous: vec![ReferenceMb::default(); w * h / 256],
            reference_idr: true,
            analysis: analysis::Analysis::new(w, h),
            rate: rate::Rate::new(cfg),
            frame: 0,
            id: 0,
            ready: false,
            headers,
            since_idr: 0,
        })
    }
    pub fn configure(&mut self, fps: u32, bitrate: u32) -> Result<()> {
        if self.cfg.fps == fps && self.cfg.bitrate == bitrate {
            return Ok(());
        }
        let c = Config {
            fps,
            bitrate,
            ..self.cfg
        };
        c.validate()?;
        self.cfg = c;
        self.rate.configure(c);
        Ok(())
    }
    pub fn encode(
        &mut self,
        y: &[u8],
        u: &[u8],
        v: &[u8],
        timestamp: i64,
        force_idr: bool,
    ) -> Result<Option<AccessUnit>> {
        #[cfg(all(target_arch = "x86_64", not(feature = "scalar-dsp")))]
        if let Some(result) = accelerated::encode(self, y, u, v, timestamp, force_idr) {
            return result;
        }
        self.encode_inner(y, u, v, timestamp, force_idr)
    }
    #[inline(always)]
    fn encode_inner(
        &mut self,
        y: &[u8],
        u: &[u8],
        v: &[u8],
        timestamp: i64,
        force_idr: bool,
    ) -> Result<Option<AccessUnit>> {
        let n = self.cfg.width as usize * self.cfg.height as usize;
        if y.len() != n || u.len() != n / 4 || v.len() != n / 4 {
            return Err(EncodeError("H264 input plane length"));
        }
        for (p, input) in [y, u, v].into_iter().enumerate() {
            let div = if p == 0 { 1 } else { 2 };
            let iw = self.cfg.width as usize / div;
            let ih = self.cfg.height as usize / div;
            let w = self.w / div;
            let h = self.h / div;
            for row in 0..h {
                let dst = &mut self.source[p][row * w..][..w];
                // T 3CFEDA -> 3D0448 pads source geometry with black luma
                // and neutral chroma. Edge replication belongs to references.
                let padding = if p == 0 { 0 } else { 128 };
                if row < ih {
                    dst[..iw].copy_from_slice(&input[row * iw..][..iw]);
                    dst[iw..].fill(padding);
                } else {
                    dst.fill(padding);
                }
            }
        }
        if !self.ready || force_idr {
            self.rate.request_keyframe();
        }
        let forced = !self.ready || force_idr || self.since_idr >= i32::MAX as u32 - 1;
        self.analysis
            .analyze(&self.source[0], &self.previous_luma, !forced);
        // T C2CB60 overrides the generic 3000-frame GOP with INT_MAX.
        // T 3D1BE3: explicit/period IDR overrides RC skip, scene IDR does not.
        if !self.rate.admit(forced, timestamp) {
            return Ok(None);
        }
        let idr = forced || self.analysis.large_change;
        if idr && !forced {
            self.analysis
                .analyze(&self.source[0], &self.previous_luma, false);
        }

        self.analysis
            .rate_complexity(&self.source[0], &self.reference[0], idr);
        self.analysis
            .prepare_features(idr, self.rate.reference_qp());

        // Mark references unavailable until the whole AU has succeeded. A
        // partial error must not produce a P picture against untransmitted data.
        self.ready = false;
        if idr {
            self.frame = 0;
            self.id = self.id.wrapping_add(1);
        }
        for c in &mut self.counts {
            c.values.fill(-1);
        }
        self.modes.fill(-1);
        for (old, mb) in self.previous.iter_mut().zip(&self.mbs) {
            *old = ReferenceMb {
                integer_mv: mb.integer_mv,
                skip_sad: mb.skip.then_some(mb.skip_sad),
                quality_qp: mb.quality_qp,
            };
        }
        self.mbs.fill(Mb::default());
        let qp = self
            .rate
            .begin(idr, self.analysis.complexity, &self.analysis.mb_cost);
        let mut qp_state = qp;
        let mut sw = syntax::slice(idr, self.frame, self.id, qp);
        let slice_header_bits = sw.position();
        let mut skips = 0;
        for my in 0..self.h / 16 {
            for mx in 0..self.w / 16 {
                let index = my * (self.w / 16) + mx;
                let before = sw.position();
                let qp = self.rate.mb_qp(index, before - slice_header_bits);
                let motion_lambda = search::lambda(qp);
                if idr {
                    self.macroblock::<true>(
                        mx,
                        my,
                        qp,
                        motion_lambda,
                        &mut qp_state,
                        &mut skips,
                        &mut sw,
                    )?;
                } else {
                    self.macroblock::<false>(
                        mx,
                        my,
                        qp,
                        motion_lambda,
                        &mut qp_state,
                        &mut skips,
                        &mut sw,
                    )?;
                }
                let mb = &mut self.mbs[index];
                mb.quality_qp =
                    if self.reference_idr || mb.intra || mb.coded || mb.mvs != [Mv::ZERO; 4] {
                        mb.qp
                    } else {
                        self.previous[index].quality_qp
                    };
                self.rate.mb_done(sw.position() - before, mb.qp);
            }
        }
        if !idr && skips > 0 {
            sw.ue(skips);
        }
        sw.rbsp_trailing_bits();
        // SetRates retains the initialized SPS; only a new encoder rebuilds it.
        let mut data = if idr {
            self.headers.clone()
        } else {
            Vec::new()
        };
        let header_bytes = data.len();
        syntax::nal(&mut data, if idr { 0x65 } else { 0x41 }, sw);
        self.filter()?;
        for p in 0..3 {
            let w = if p == 0 { self.w } else { self.w / 2 };
            for row in 0..self.reference[p].height {
                self.reference[p]
                    .row_mut(row)
                    .copy_from_slice(&self.recon[p][row * w..][..w]);
            }
            self.reference[p].extend_edges();
        }
        std::mem::swap(&mut self.source[0], &mut self.previous_luma);
        self.analysis.finish();
        // T 296DE8 gives RC the slice bytes, excluding preceding SPS/PPS.
        self.rate.finish((data.len() - header_bytes) * 8);
        self.frame = self.frame.wrapping_add(1) & 0x7fff;
        self.since_idr = if idr { 0 } else { self.since_idr + 1 };
        self.reference_idr = idr;
        self.ready = true;
        Ok(Some(AccessUnit {
            data,
            keyframe: idr,
            timestamp_100ns: timestamp,
        }))
    }
    fn partition_neighbours(
        &self,
        mx: usize,
        my: usize,
        part: usize,
        width: usize,
        trial: &[Mv; 4],
        ready: usize,
    ) -> (Option<Mv>, Option<Mv>, Option<Mv>) {
        let gx = (mx * 2 + part % 2) as i32;
        let gy = (my * 2 + part / 2) as i32;
        let mw = self.w / 16;
        let current = my * mw + mx;
        let get = |x: i32, y: i32| -> (bool, Option<Mv>) {
            if x < 0 || y < 0 || x as usize >= self.w / 8 || y as usize >= self.h / 8 {
                return (false, None);
            }
            let mb = (y as usize / 2) * mw + x as usize / 2;
            let p = (y as usize % 2) * 2 + x as usize % 2;
            if mb > current {
                return (false, None);
            }
            if mb == current {
                return if p < ready {
                    (true, Some(trial[p]))
                } else {
                    (false, None)
                };
            }
            (
                true,
                if self.mbs[mb].intra {
                    None
                } else {
                    Some(self.mbs[mb].mvs[p])
                },
            )
        };
        let a = get(gx - 1, gy).1;
        let b = get(gx, gy - 1).1;
        let mut c = get(gx + width as i32, gy - 1);
        if !c.0 {
            c = get(gx - 1, gy - 1);
        }
        (a, b, c.1)
    }
    fn predicted_sad(&self, mx: usize, my: usize) -> u32 {
        let width = self.w / 16;
        let at = my * width + mx;
        let a = if mx > 0 { Some(self.mbs[at - 1]) } else { None };
        let b = if my > 0 {
            Some(self.mbs[at - width])
        } else {
            None
        };
        let c = if my > 0 && mx + 1 < width {
            Some(self.mbs[at - width + 1])
        } else if my > 0 && mx > 0 {
            Some(self.mbs[at - width - 1])
        } else {
            None
        };
        let values = [a, b, c].map(|m| m.map_or(0, |m| m.sad_cost));
        let matching = [a, b, c].map(|m| m.is_some_and(|m| !m.intra));
        let v = if b.is_none() && c.is_none() && a.is_some() {
            values[0]
        } else if matching.iter().filter(|&&v| v).count() == 1 {
            values[matching.iter().position(|&v| v).unwrap()]
        } else {
            let [a, b, c] = values;
            a.min(b).max(a.max(b).min(c))
        };
        // T 3CE696 / PredictSad: 0.90625, rounded in the integer domain.
        (58 * v + 32) >> 6
    }
    fn prediction<const CHROMA_COST: bool>(
        &self,
        x: usize,
        y: usize,
        mv: Mv,
    ) -> Result<Prediction> {
        let mut py = [0u8; 256];
        let mut pc = [[0u8; 64]; 2];
        dsp::motion::luma(
            &self.reference[0],
            x as i32 * 4 + i32::from(mv.x),
            y as i32 * 4 + i32::from(mv.y),
            &mut Block::new(&mut py, 16, 16, 16)?,
        )?;
        let sad_y = search::sad(&self.source[0][y * self.w + x..], self.w, &py, 16, 16);
        let mut sad = sad_y;
        for p in 0..2 {
            dsp::motion::chroma(
                &self.reference[p + 1],
                x as i32 * 4 + i32::from(mv.x),
                y as i32 * 4 + i32::from(mv.y),
                &mut Block::new(&mut pc[p], 8, 8, 8)?,
            )?;
            if CHROMA_COST {
                sad += search::sad(
                    &self.source[p + 1][y / 2 * (self.w / 2) + x / 2..],
                    self.w / 2,
                    &pc[p],
                    8,
                    8,
                );
            }
        }
        Ok((py, pc, sad_y, sad))
    }
    fn static_motion(&self, x: usize, y: usize, index: usize) -> Option<Mv> {
        let mv = match self.analysis.static_kind[index] {
            1 => Mv::ZERO,
            2 => self.analysis.scroll,
            _ => return None,
        };
        let rx = x as i32 + (i32::from(mv.x) >> 2);
        let ry = y as i32 + (i32::from(mv.y) >> 2);
        if rx < 0 || ry < 0 || rx as usize + 16 > self.w || ry as usize + 16 > self.h {
            return None;
        }
        let stride = self.w / 2;
        let rx = rx as usize / 2;
        let ry = ry as usize / 2;
        for p in 1..3 {
            for row in 0..8 {
                if self.source[p][(y / 2 + row) * stride + x / 2..][..8]
                    != self.reference[p].row(ry + row)[rx..rx + 8]
                {
                    return None;
                }
            }
        }
        Some(mv)
    }
    fn probe_skip(
        &self,
        x: usize,
        y: usize,
        mv: Mv,
        luma: &pixels::Quant,
        chroma: &pixels::Quant,
        threshold: u32,
        previous: Option<u32>,
    ) -> Result<Option<Prediction>> {
        let (py, pc, sad_y, sad) = self.prediction::<true>(x, y, mv)?;
        // T 3D4A65: use the predicted/previous skip error before transforming.
        if sad == 0 || sad < threshold || previous.is_some_and(|limit| sad < limit) {
            return Ok(Some((py, pc, sad_y, sad)));
        }
        let mut total = 0;
        for group in 0..4 {
            let bx = group % 2 * 8;
            let by = group / 2 * 8;
            let (q, _) = luma.forward_four(
                &self.source[0][(y + by) * self.w + x + bx..],
                self.w,
                &py[by * 16 + bx..],
                16,
                false,
            );
            let mut score = 0;
            for block in q {
                score += pixels::decimate_score(&block, false);
                if score >= 6 {
                    return Ok(None);
                }
            }
            total += score;
            if total >= 6 {
                return Ok(None);
            }
        }
        for p in 0..2 {
            let (q, dc) = chroma.forward_four(
                &self.source[p + 1][y / 2 * (self.w / 2) + x / 2..],
                self.w / 2,
                &pc[p],
                8,
                false,
            );
            if chroma
                .dc(&pixels::hadamard2(&dc), false)
                .iter()
                .any(|&v| v != 0)
            {
                return Ok(None);
            }
            let mut score = 0;
            for block in q {
                score += pixels::decimate_score(&block, true);
                if score >= 7 {
                    return Ok(None);
                }
            }
        }
        Ok(Some((py, pc, sad_y, sad)))
    }
    fn commit_skip(
        &mut self,
        mx: usize,
        my: usize,
        mv: Mv,
        qp: i32,
        py: &[u8; 256],
        pc: &[[u8; 64]; 2],
        sad_y: u32,
        sad: u32,
    ) {
        let x = mx * 16;
        let y = my * 16;
        for row in 0..16 {
            self.recon[0][(y + row) * self.w + x..][..16].copy_from_slice(&py[row * 16..][..16]);
        }
        for p in 0..2 {
            for row in 0..8 {
                self.recon[p + 1][(y / 2 + row) * (self.w / 2) + x / 2..][..8]
                    .copy_from_slice(&pc[p][row * 8..][..8]);
            }
        }
        self.zero_counts(mx, my);
        self.mbs[my * (self.w / 16) + mx] = Mb {
            mvs: [mv; 4],
            integer_mv: mv,
            qp,
            sad_cost: sad_y,
            skip: true,
            skip_sad: sad,
            ..Mb::default()
        };
    }
    fn first_intra(
        &self,
        x: usize,
        y: usize,
        lambda: u32,
        mut best: u32,
    ) -> Result<Option<(u8, [u8; 256], u32)>> {
        // Every I16 mode costs at least one lambda, even at zero distortion.
        if best <= lambda {
            return Ok(None);
        }
        let e = edges(&self.recon[0], self.w, x, y, 16, 16)?;
        let source = &self.source[0][y * self.w + x..];
        let basic = if x > 0 && y > 0 {
            Some(pixels::intra16_sad(source, self.w, &e))
        } else {
            None
        };
        let mut chosen = None;
        let mut prediction = [0; 256];
        let mut buffer_mode = 4;
        for mode in 0..4 {
            let bits = if mode == 0 {
                1
            } else if mode < 3 {
                3
            } else {
                5
            };
            if bits * lambda >= best {
                continue;
            }
            let distortion = if mode < 3 && basic.is_some() {
                basic.unwrap()[mode as usize]
            } else {
                if dsp::intra::luma16(&mut Block::new(&mut prediction, 16, 16, 16)?, &e, mode)
                    .is_err()
                {
                    continue;
                }
                buffer_mode = mode;
                search::sad(source, self.w, &prediction, 16, 16)
            };
            let cost = distortion + bits * lambda;
            if cost < best {
                best = cost;
                chosen = Some(mode);
            }
        }
        if let Some(mode) = chosen {
            if buffer_mode != mode {
                dsp::intra::luma16(&mut Block::new(&mut prediction, 16, 16, 16)?, &e, mode)?;
            }
            Ok(Some((mode, prediction, best)))
        } else {
            Ok(None)
        }
    }
    #[inline(always)]
    fn macroblock<const IDR: bool>(
        &mut self,
        mx: usize,
        my: usize,
        qp: i32,
        motion_lambda: u32,
        qp_state: &mut i32,
        skips: &mut u32,
        w: &mut BitWriter,
    ) -> Result<()> {
        let idr = IDR;
        let quantizer = pixels::Quant::at(qp);
        let chroma_quantizer = pixels::Quant::at(pixels::chroma_qp(qp));
        let x = mx * 16;
        let y = my * 16;
        let index = my * (self.w / 16) + mx;
        // IDR never consumes an inter MV predictor or P-skip state.
        let (pred, skip_mv) = if IDR {
            (Mv::ZERO, Mv::ZERO)
        } else {
            let (a, b, c) = self.partition_neighbours(mx, my, 0, 2, &[Mv::ZERO; 4], 0);
            let pred = search::predictor(a, b, c);
            let skip = if mx == 0 || my == 0 || a == Some(Mv::ZERO) || b == Some(Mv::ZERO) {
                Mv::ZERO
            } else {
                pred
            };
            (pred, skip)
        };
        // Current screen SCD path precedes ordinary Skip/ME. Original-plane
        // equality alone is insufficient: retain quality-QP and MV conditions.
        let static_mv = if IDR {
            None
        } else {
            self.static_motion(x, y, index)
        };
        let static_prediction = if let Some(mv) = static_mv {
            Some(self.prediction::<false>(x, y, mv)?)
        } else {
            None
        };
        if let (Some(mv), Some((py, pc, sad_y, _))) = (static_mv, static_prediction.as_ref()) {
            let quality = self.previous[index].quality_qp;
            if mv == skip_mv && (quality - qp <= 5 || quality <= 26) {
                self.commit_skip(mx, my, mv, *qp_state, py, pc, *sad_y, *sad_y);
                *skips += 1;
                return Ok(());
            }
        }
        let mw = self.w / 16;
        let a = (mx > 0).then(|| self.mbs[index - 1]);
        let b = (my > 0).then(|| self.mbs[index - mw]);
        let d = (my > 0 && mx > 0).then(|| self.mbs[index - mw - 1]);
        let c = (my > 0 && mx + 1 < mw).then(|| self.mbs[index - mw + 1]);
        let keep = [a, b, c].into_iter().all(|mb| mb.is_some_and(|mb| mb.skip));
        let mut first_intra = None;
        if !IDR
            && static_mv.is_none()
            && (self.previous[index].skip_sad.is_some()
                || [a, b, c, d]
                    .into_iter()
                    .any(|mb| mb.is_some_and(|mb| mb.skip)))
        {
            let neighbours = [a, b, c.or(d)];
            let values = neighbours.map(|mb| mb.filter(|mb| mb.skip).map_or(0, |mb| mb.skip_sad));
            let matching = neighbours.map(|mb| mb.is_some_and(|mb| mb.skip));
            let threshold = if b.is_none() && c.or(d).is_none() {
                values[0]
            } else if matching.into_iter().filter(|v| *v).count() == 1 {
                values[matching.into_iter().position(|v| v).unwrap()]
            } else {
                let [a, b, c] = values;
                a.min(b).max(a.max(b).min(c))
            };
            if let Some((py, pc, sad_y, sad)) = self.probe_skip(
                x,
                y,
                skip_mv,
                quantizer,
                chroma_quantizer,
                threshold,
                self.previous[index].skip_sad,
            )? {
                if !keep {
                    first_intra = self.first_intra(x, y, motion_lambda, sad_y)?;
                }
                if first_intra.is_none() {
                    self.commit_skip(mx, my, skip_mv, *qp_state, &py, &pc, sad_y, sad);
                    *skips += 1;
                    return Ok(());
                }
            }
        }
        let mut intra = idr || first_intra.is_some();
        let mut mv = Mv::ZERO;
        let mut mvs = [Mv::ZERO; 4];
        let mut partition = 0u8; // AVC P16 / P16x8 / P8x16 / P8(ref0), values 0/1/2/4.
        let mut p8_predictions = [Mv::ZERO; 4];
        let mut py = [0u8; 256];
        let mut pc = [[0u8; 64]; 2];
        let mut integer_sad = 0;
        let mut integer_mv = Mv::ZERO;
        let sad_prediction = if IDR { 0 } else { self.predicted_sad(mx, my) };
        let cross_threshold = self.analysis.features.threshold;
        if let (Some(chosen), Some((luma, chroma, sad_y, _))) = (static_mv, static_prediction) {
            mv = chosen;
            mvs = [chosen; 4];
            integer_mv = chosen;
            integer_sad = sad_y;
            py = luma;
            pc = chroma;
        } else if !intra {
            // T 3D3DF6 keeps integer P16 candidates separate from final coded
            // partition vectors. The prior IDR field is all zero, so the first
            // P picture naturally has no temporal candidate besides zero.
            let mut extra = [Mv::ZERO; 4];
            let mut extra_count = 0;
            for candidate in [
                (mx > 0).then(|| self.mbs[index - 1].integer_mv),
                (my > 0).then(|| self.mbs[index - mw].integer_mv),
                (mx + 1 < mw).then(|| self.previous[index + 1].integer_mv),
                (my + 1 < self.h / 16).then(|| self.previous[index + mw].integer_mv),
            ]
            .into_iter()
            .flatten()
            {
                extra[extra_count] = candidate;
                extra_count += 1;
            }
            let (chosen, inter_cost) = if self.analysis.static_kind[index] != 0 {
                let mv = if self.analysis.static_kind[index] == 1 {
                    Mv::ZERO
                } else {
                    self.analysis.scroll
                };
                let (reference, stride) = self.reference[0]
                    .footprint(
                        x as i32 + i32::from(mv.x) / 4,
                        y as i32 + i32::from(mv.y) / 4,
                        16,
                        16,
                    )
                    .ok_or(EncodeError("static reference"))?;
                let measured = search::sad(
                    &self.source[0][y * self.w + x..],
                    self.w,
                    reference,
                    stride,
                    16,
                ) + search::motion_cost(mv, pred, motion_lambda);
                (mv, measured)
            } else {
                search::integer::<16>(
                    &self.source[0][y * self.w + x..],
                    self.w,
                    &self.reference[0],
                    x,
                    y,
                    pred,
                    motion_lambda,
                    &extra[..extra_count],
                    sad_prediction,
                    cross_threshold,
                    &mut self.analysis.features,
                )
            };
            integer_sad = inter_cost;
            integer_mv = chosen;
            mv = chosen;
            mvs = [mv; 4];
            // T 3D607C / WelsMdFirstIntraMode: compare I16 with the full
            // P16 cost before attempting subpartitions and fractional motion.
            // Reuse the winning prediction instead of testing I16 a second time.
            first_intra = self.first_intra(x, y, motion_lambda, inter_cost)?;
            intra = first_intra.is_some();
            if !intra && self.analysis.split[index] {
                let mut trial_mvs = [Mv::ZERO; 4];
                let mut sum = 0;
                for part in 0..4 {
                    let bx = (part % 2) * 8;
                    let by = (part / 2) * 8;
                    let (a, b, c) = self.partition_neighbours(mx, my, part, 1, &trial_mvs, part);
                    let predicted = search::predictor(a, b, c);
                    let kind = self.analysis.static_parts[index][part];
                    let (motion, cost) = if kind != 0 {
                        let motion = if kind == 1 {
                            Mv::ZERO
                        } else {
                            self.analysis.scroll
                        };
                        let (pixels, stride) = self.reference[0]
                            .footprint(
                                (x + bx) as i32 + i32::from(motion.x) / 4,
                                (y + by) as i32 + i32::from(motion.y) / 4,
                                8,
                                8,
                            )
                            .ok_or(EncodeError("static partition reference"))?;
                        let d = search::sad(
                            &self.source[0][(y + by) * self.w + x + bx..],
                            self.w,
                            pixels,
                            stride,
                            8,
                        );
                        (
                            motion,
                            d + search::motion_cost(motion, predicted, motion_lambda),
                        )
                    } else {
                        search::integer::<8>(
                            &self.source[0][(y + by) * self.w + x + bx..],
                            self.w,
                            &self.reference[0],
                            x + bx,
                            y + by,
                            predicted,
                            motion_lambda,
                            &[],
                            sad_prediction >> 2,
                            cross_threshold >> 2,
                            &mut self.analysis.features,
                        )
                    };
                    trial_mvs[part] = motion;
                    sum += cost;
                }
                // T 3D7703 compares the summed per-partition costs directly.
                // No extra heuristic penalty; 3D750E merges equal pairs only.
                if sum < inter_cost {
                    let vertical = trial_mvs[0] == trial_mvs[2] && trial_mvs[1] == trial_mvs[3];
                    let horizontal = trial_mvs[0] == trial_mvs[1] && trial_mvs[2] == trial_mvs[3];
                    partition = match (horizontal, vertical) {
                        (true, false) => 1,
                        (false, true) => 2,
                        _ => 4,
                    };
                    integer_sad = sum;
                    mvs = trial_mvs;
                }
            }
            if !intra {
                if partition != 0 {
                    let mut refined = [Mv::ZERO; 4];
                    let parts: &[usize] = match partition {
                        1 => &[0, 2],
                        2 => &[0, 1],
                        _ => &[0, 1, 2, 3],
                    };
                    let width = if partition == 1 { 16 } else { 8 };
                    let height = if partition == 2 { 16 } else { 8 };
                    for &part in parts {
                        let bx = part % 2 * 8;
                        let by = part / 2 * 8;
                        let (a, b, c) =
                            self.partition_neighbours(mx, my, part, width / 8, &refined, part);
                        let preferred = match (partition, part) {
                            (1, 0) => b,
                            (1, 2) => a,
                            (2, 0) => a,
                            (2, 1) => c,
                            _ => None,
                        };
                        let predicted = preferred.unwrap_or_else(|| search::predictor(a, b, c));
                        p8_predictions[part] = predicted;
                        let (motion, pixels) = search::refine(
                            &self.source[0][(y + by) * self.w + x + bx..],
                            self.w,
                            &self.reference[0],
                            x + bx,
                            y + by,
                            predicted,
                            motion_lambda,
                            width,
                            height,
                            mvs[part],
                        )?;
                        for sy in 0..height / 8 {
                            for sx in 0..width / 8 {
                                refined[part + sy * 2 + sx] = motion;
                            }
                        }
                        for row in 0..height {
                            py[(by + row) * 16 + bx..][..width]
                                .copy_from_slice(&pixels[row * width..][..width]);
                        }
                    }
                    mvs = refined;
                } else {
                    let (motion, pixels) = search::refine(
                        &self.source[0][y * self.w + x..],
                        self.w,
                        &self.reference[0],
                        x,
                        y,
                        pred,
                        motion_lambda,
                        16,
                        16,
                        mv,
                    )?;
                    mv = motion;
                    mvs = [mv; 4];
                    py = pixels;
                }
                for p in 0..2 {
                    if partition != 0 {
                        for part in 0..4 {
                            let bx = (part % 2) * 4;
                            let by = (part / 2) * 4;
                            dsp::motion::chroma(
                                &self.reference[p + 1],
                                (x / 2 + bx) as i32 * 8 + i32::from(mvs[part].x),
                                (y / 2 + by) as i32 * 8 + i32::from(mvs[part].y),
                                &mut Block::new(&mut pc[p][by * 8 + bx..], 8, 4, 4)?,
                            )?;
                        }
                    } else {
                        dsp::motion::chroma(
                            &self.reference[p + 1],
                            x as i32 * 4 + i32::from(mv.x),
                            y as i32 * 4 + i32::from(mv.y),
                            &mut Block::new(&mut pc[p], 8, 8, 8)?,
                        )?;
                    }
                }
            }
        }
        let mut i16_mode = 0u8;
        let mut i16_cost = u32::MAX;
        let mut i16_prediction = [0u8; 256];
        if let Some((mode, prediction, cost)) = first_intra {
            i16_mode = mode;
            i16_prediction = prediction;
            i16_cost = cost;
        } else if intra {
            let e = edges(&self.recon[0], self.w, x, y, 16, 16)?;
            for mode in 0..4 {
                let bits = if mode == 0 {
                    1
                } else if mode < 3 {
                    3
                } else {
                    5
                };
                if bits * motion_lambda >= i16_cost {
                    continue;
                }
                let mut trial = [0u8; 256];
                if dsp::intra::luma16(&mut Block::new(&mut trial, 16, 16, 16)?, &e, mode).is_err() {
                    continue;
                }
                let score = search::cost(
                    &self.source[0][y * self.w + x..],
                    self.w,
                    &trial,
                    16,
                    16,
                    idr,
                ) + bits * motion_lambda;
                if score < i16_cost {
                    i16_cost = score;
                    i16_mode = mode;
                    i16_prediction = trial;
                }
            }
        }
        let luma_dc_nc = self.counts[0].put(x / 4, y / 4, 0);
        let mut i4_cost = 24 * motion_lambda;
        let mut i16 = intra
            && (40 * motion_lambda >= i16_cost
                || !IDR && !pixels::fine_intra(&self.source[0][y * self.w + x..], self.w));
        let mut quantized = [[0i16; 16]; 16];
        let mut masks = [0u16; 16];
        let mut modes = [(2u8, 2u8); 16];
        let mut luma_dc = [0i32; 16];
        let mut scaled_dc = None;
        if intra && !i16 {
            // Intra4 needs sequential reconstructed pixels and mode predictors,
            // but discarded trials do not need CAVLC scans or count updates.
            for (i, &(bx, by)) in BLOCKS.iter().enumerate() {
                let gx = (x + bx) / 4;
                let gy = (y + by) / 4;
                let stride = self.w / 4;
                let left = if gx > 0 {
                    self.modes[gy * stride + gx - 1]
                } else {
                    -1
                };
                let top = if gy > 0 {
                    self.modes[(gy - 1) * stride + gx]
                } else {
                    -1
                };
                let mpm = if left < 0 || top < 0 {
                    2
                } else {
                    left.min(top) as u8
                };
                let right = !matches!(i, 3 | 7 | 11 | 13 | 15) && x + bx + 8 <= self.w;
                let e = pixels::Edges4::gather(&self.recon[0], self.w, x + bx, y + by, right);
                let available = u8::from(y + by > 0) | (u8::from(x + bx > 0) << 1);
                let source = &self.source[0][(y + by) * self.w + x + bx..];
                let (mode, prediction, cost) = if IDR {
                    pixels::intra4(source, self.w, &e, available, right, mpm, motion_lambda)
                } else {
                    pixels::intra4_fast(source, self.w, &e, available, right, mpm, motion_lambda)
                };
                i4_cost += cost;
                if i4_cost + (15 - i) as u32 * motion_lambda >= i16_cost {
                    i16 = true;
                    break;
                }
                modes[i] = (mode, mpm);
                self.modes[gy * stride + gx] = mode as i8;
                let q = quantizer.forward_block(source, self.w, &prediction, true);
                masks[i] = pixels::nonzero_mask(&q) as u16;
                quantized[i] = q;
                let dst = &mut self.recon[0][(y + by) * self.w + x + bx..];
                if masks[i] == 0 {
                    pixels::add_dc_into(&prediction, 4, dst, self.w, 0);
                } else {
                    quantizer.reconstruct_into(&q, &prediction, 4, dst, self.w, None)?;
                }
            }
        }
        if i16 || !intra {
            let prediction = if i16 { &i16_prediction } else { &py };
            let mut dc = [0i32; 16];
            let mut scores = [0u32; 4];
            for group in 0..4 {
                let bx = group % 2 * 8;
                let by = group / 2 * 8;
                let (q, d) = quantizer.forward_four(
                    &self.source[0][(y + by) * self.w + x + bx..],
                    self.w,
                    &prediction[by * 16 + bx..],
                    16,
                    i16,
                );
                quantized[group * 4..group * 4 + 4].copy_from_slice(&q);
                if i16 {
                    for k in 0..4 {
                        dc[(by / 4 + k / 2) * 4 + bx / 4 + k % 2] = d[k];
                        quantized[group * 4 + k][0] = 0;
                    }
                } else {
                    for block in q {
                        if scores[group] < 6 {
                            scores[group] += pixels::decimate_score(&block, false);
                        }
                    }
                }
            }
            if i16 {
                let q = quantizer.dc(&forward_hadamard_4x4(&dc).map(|v| (v + 1) >> 1), true);
                luma_dc = SCAN.map(|k| q[k]);
                scaled_dc = Some(
                    inverse_hadamard_luma_dc_16x16(&q, qp, &FLAT_4X4_16, 8)
                        .map_err(|_| EncodeError("intra16 DC"))?,
                );
            } else {
                let whole = scores.iter().sum::<u32>();
                for group in 0..4 {
                    if whole < 6 || scores[group] < 4 {
                        quantized[group * 4..group * 4 + 4].fill([0; 16]);
                    }
                }
            }
            for (i, &(bx, by)) in BLOCKS.iter().enumerate() {
                masks[i] = pixels::nonzero_mask(&quantized[i]) as u16;
                self.modes[((y + by) / 4) * (self.w / 4) + (x + bx) / 4] = 2;
                let dc = scaled_dc.as_ref().map(|d| d[by / 4 * 4 + bx / 4]);
                let pred = &prediction[by * 16 + bx..];
                let dst = &mut self.recon[0][(y + by) * self.w + x + bx..];
                if masks[i] == 0 {
                    pixels::add_dc_into(pred, 16, dst, self.w, dc.unwrap_or(0));
                } else {
                    quantizer.reconstruct_into(&quantized[i], pred, 16, dst, self.w, dc)?;
                }
            }
        }
        let mut ncs = [0i32; 16];
        let mut nz = 0u16;
        let mut cbpl = 0u8;
        for (i, &(bx, by)) in BLOCKS.iter().enumerate() {
            let count = masks[i].count_ones() as usize;
            if count > 0 {
                cbpl |= 1 << (i / 4);
                nz |= 1 << ((by / 4) * 4 + bx / 4);
            }
            ncs[i] = self.counts[0].put((x + bx) / 4, (y + by) / 4, count);
        }
        if i16 && cbpl != 0 {
            cbpl = 15;
        }
        let mut chroma_mode = 0;
        if intra {
            let es = [
                edges(&self.recon[1], self.w / 2, x / 2, y / 2, 8, 8)?,
                edges(&self.recon[2], self.w / 2, x / 2, y / 2, 8, 8)?,
            ];
            let mut best = u32::MAX;
            for mode in 0..4 {
                let mut trial = [[0u8; 64]; 2];
                // T 3D388E includes chroma mode ue(v) cost as well as distortion.
                let mut score = motion_lambda
                    * if mode == 0 {
                        1
                    } else if mode < 3 {
                        3
                    } else {
                        5
                    };
                if score >= best {
                    continue;
                }
                let mut valid = true;
                for p in 0..2 {
                    if dsp::intra::chroma8(&mut Block::new(&mut trial[p], 8, 8, 8)?, &es[p], mode)
                        .is_err()
                    {
                        valid = false;
                        break;
                    }
                    score += search::cost(
                        &self.source[p + 1][y / 2 * (self.w / 2) + x / 2..],
                        self.w / 2,
                        &trial[p],
                        8,
                        8,
                        idr,
                    );
                    if score >= best {
                        valid = false;
                        break;
                    }
                }
                if valid && score < best {
                    best = score;
                    chroma_mode = mode;
                    pc = trial;
                }
            }
        }
        let mut dc = [[0i32; 4]; 2];
        let mut ac = [[[0i16; 15]; 4]; 2];
        let mut cn = [[0i32; 4]; 2];
        let mut cbpc = 0u8;
        for p in 0..2 {
            let stride = self.w / 2;
            // The chroma DC/AC sufficient bound proves all inter coefficients
            // zero, including rounding. Bypass no perceptual/heuristic test.
            if !intra
                && chroma_quantizer.zero_chroma(
                    &self.source[p + 1][y / 2 * stride + x / 2..],
                    stride,
                    &pc[p],
                )
            {
                for i in 0..4 {
                    cn[p][i] = self.counts[p + 1].put(mx * 2 + i % 2, my * 2 + i / 2, 0);
                }
                for row in 0..8 {
                    self.recon[p + 1][(y / 2 + row) * stride + x / 2..][..8]
                        .copy_from_slice(&pc[p][row * 8..][..8]);
                }
                continue;
            }
            let (mut quants, dc_input) = chroma_quantizer.forward_four(
                &self.source[p + 1][y / 2 * stride + x / 2..],
                stride,
                &pc[p],
                8,
                intra,
            );
            for q in &mut quants {
                q[0] = 0;
            }
            dc[p] = chroma_quantizer.dc(&pixels::hadamard2(&dc_input), intra);
            if dc[p].iter().any(|&v| v != 0) {
                cbpc = cbpc.max(1);
            }
            let scaled = chroma_quantizer.inverse_chroma_dc(&dc[p]);
            // T 55A253: chroma inter AC uses the separate <7 decimation
            // threshold; transformed DC is retained independently.
            if !intra {
                let mut score = 0;
                for q in &quants {
                    if score < 7 {
                        score += pixels::decimate_score(q, true);
                    }
                }
                if score < 7 {
                    quants.fill([0; 16]);
                }
            }
            for i in 0..4 {
                let bx = (i % 2) * 4;
                let by = (i / 2) * 4;
                let quant = quants[i];
                ac[p][i] = std::array::from_fn(|j| quant[SCAN[j + 1]]);
                let count = ac[p][i].iter().filter(|&&v| v != 0).count();
                if count > 0 {
                    cbpc = 2;
                }
                cn[p][i] = self.counts[p + 1].put(mx * 2 + i % 2, my * 2 + i / 2, count);
                let prediction = &pc[p][by * 8 + bx..];
                let dst = &mut self.recon[p + 1][(y / 2 + by) * stride + x / 2 + bx..];
                if count == 0 {
                    pixels::add_dc_into(prediction, 8, dst, stride, scaled[i]);
                } else {
                    chroma_quantizer.reconstruct_into(
                        &quant,
                        prediction,
                        8,
                        dst,
                        stride,
                        Some(scaled[i]),
                    )?;
                }
            }
        }
        if !idr && !intra && partition == 0 && cbpl == 0 && cbpc == 0 && mv == skip_mv {
            *skips += 1;
            self.mbs[index] = Mb {
                intra: false,
                mvs,
                integer_mv,
                sad_cost: integer_sad,
                skip: true,
                skip_sad: search::sad(&self.source[0][y * self.w + x..], self.w, &py, 16, 16)
                    + (0..2)
                        .map(|p| {
                            search::sad(
                                &self.source[p + 1][y / 2 * (self.w / 2) + x / 2..],
                                self.w / 2,
                                &pc[p],
                                8,
                                8,
                            )
                        })
                        .sum::<u32>(),
                qp: *qp_state,
                nz: 0,
                quality_qp: 0,
                coded: false,
            };
            return Ok(());
        }
        if !idr {
            w.ue(*skips);
            *skips = 0;
        }
        w.ue(if i16 {
            1 + u32::from(i16_mode)
                + 4 * u32::from(cbpc)
                + 12 * u32::from(cbpl != 0)
                + if idr { 0 } else { 5 }
        } else if intra {
            if idr { 0 } else { 5 }
        } else {
            u32::from(partition)
        });
        if intra {
            if !i16 {
                for (mode, mpm) in modes {
                    w.u(1, (mode == mpm) as u32);
                    if mode != mpm {
                        w.u(
                            3,
                            if mode < mpm {
                                mode as u32
                            } else {
                                mode as u32 - 1
                            },
                        );
                    }
                }
            }
            w.ue(chroma_mode as u32);
        } else if partition != 0 {
            if partition == 4 {
                for _ in 0..4 {
                    w.ue(0);
                }
            }
            let parts: &[usize] = match partition {
                1 => &[0, 2],
                2 => &[0, 1],
                _ => &[0, 1, 2, 3],
            };
            for &part in parts {
                w.se(i32::from(mvs[part].x) - i32::from(p8_predictions[part].x));
                w.se(i32::from(mvs[part].y) - i32::from(p8_predictions[part].y));
            }
        } else {
            w.se(i32::from(mv.x) - i32::from(pred.x));
            w.se(i32::from(mv.y) - i32::from(pred.y));
        }
        if !i16 {
            syntax::cbp(w, intra, cbpl | (cbpc << 4));
        }
        if i16 || cbpl != 0 || cbpc != 0 {
            w.se(qp - *qp_state);
            *qp_state = qp;
        }
        if i16 {
            syntax::residual(w, &luma_dc.map(|v| v as i16), luma_dc_nc)?;
        }
        for i in 0..16 {
            if cbpl & (1 << (i / 4)) != 0 {
                let levels = SCAN.map(|k| quantized[i][k]);
                syntax::residual(w, if i16 { &levels[1..] } else { &levels }, ncs[i])?;
            }
        }
        if cbpc > 0 {
            for d in &dc {
                syntax::residual(w, &d.map(|v| v as i16), -1)?;
            }
        }
        if cbpc == 2 {
            for p in 0..2 {
                for i in 0..4 {
                    syntax::residual(w, &ac[p][i], cn[p][i])?;
                }
            }
        }
        self.mbs[index] = Mb {
            intra,
            mvs,
            integer_mv,
            sad_cost: if intra { 0 } else { integer_sad },
            skip: false,
            skip_sad: 0,
            qp: *qp_state,
            nz,
            quality_qp: 0,
            coded: cbpl != 0 || cbpc != 0,
        };
        Ok(())
    }
    fn zero_counts(&mut self, mx: usize, my: usize) {
        for p in 0..3 {
            let n = if p == 0 { 4 } else { 2 };
            for y in 0..n {
                let at = (my * n + y) * self.counts[p].width + mx * n;
                self.counts[p].values[at..at + n].fill(0);
                if p == 0 {
                    self.modes[at..at + n].fill(2);
                }
            }
        }
    }
    fn filter(&mut self) -> Result<()> {
        let mw = self.w / 16;
        for my in 0..self.h / 16 {
            for mx in 0..mw {
                let index = my * mw + mx;
                let q = self.mbs[index];
                // Within a non-intra block with no luma residual and one MV,
                // every internal boundary strength is zero; only outer edges
                // can depend on neighbouring macroblocks.
                let edges = if !q.intra && q.nz == 0 && q.mvs == [q.mvs[0]; 4] {
                    1
                } else {
                    4
                };
                for vertical in [true, false] {
                    for edge in 0..edges {
                        if edge == 0 && (if vertical { mx == 0 } else { my == 0 }) {
                            continue;
                        }
                        let p = if edge == 0 {
                            self.mbs[index - if vertical { 1 } else { mw }]
                        } else {
                            q
                        };
                        let mut bs = [0u8; 4];
                        for (k, s) in bs.iter_mut().enumerate() {
                            let qi = if vertical { k * 4 + edge } else { edge * 4 + k };
                            let pi = if edge == 0 {
                                if vertical { k * 4 + 3 } else { 12 + k }
                            } else {
                                if vertical { qi - 1 } else { qi - 4 }
                            };
                            *s = if p.intra || q.intra {
                                if edge == 0 { 4 } else { 3 }
                            } else if (q.nz >> qi) & 1 != 0 || (p.nz >> pi) & 1 != 0 {
                                2
                            } else {
                                u8::from(
                                    (p.mvs[(pi / 8) * 2 + (pi % 4) / 2].x as i32
                                        - q.mvs[(qi / 8) * 2 + (qi % 4) / 2].x as i32)
                                        .abs()
                                        >= 4
                                        || (p.mvs[(pi / 8) * 2 + (pi % 4) / 2].y as i32
                                            - q.mvs[(qi / 8) * 2 + (qi % 4) / 2].y as i32)
                                            .abs()
                                            >= 4,
                                )
                            };
                        }
                        if bs == [0; 4] {
                            continue;
                        }
                        for plane in 0..3 {
                            if plane > 0 && edge % 2 != 0 {
                                continue;
                            }
                            let div = if plane == 0 { 1 } else { 2 };
                            let n = 16 / div;
                            let qp = if plane == 0 {
                                (p.qp + q.qp + 1) / 2
                            } else {
                                (pixels::chroma_qp(p.qp) + pixels::chroma_qp(q.qp) + 1) / 2
                            };
                            dsp::filter::apply(
                                &mut self.recon[plane],
                                self.w / div,
                                Edge {
                                    x: mx * n + if vertical { edge * 4 / div } else { 0 },
                                    y: my * n + if vertical { 0 } else { edge * 4 / div },
                                    vertical,
                                    segment_len: 4 / div,
                                },
                                Filter {
                                    strength: bs,
                                    qp,
                                    alpha_offset: 0,
                                    beta_offset: 0,
                                    subsampled_chroma: plane > 0,
                                },
                            )?;
                        }
                    }
                }
            }
        }
        Ok(())
    }
}
fn edges(data: &[u8], stride: usize, x: usize, y: usize, n: usize, top_n: usize) -> Result<Edges> {
    let mut left = [0u8; 16];
    if x > 0 {
        for i in 0..n {
            left[i] = data[(y + i) * stride + x - 1];
        }
    }
    let top = if y > 0 {
        &data[(y - 1) * stride + x..][..top_n]
    } else {
        &[]
    };
    Ok(Edges::new(
        n,
        top,
        if x > 0 { &left[..n] } else { &[] },
        if x > 0 && y > 0 {
            Some(data[(y - 1) * stride + x - 1])
        } else {
            None
        },
    )?)
}

#[cfg(all(target_arch = "x86_64", not(feature = "scalar-dsp")))]
#[allow(unsafe_code)]
mod accelerated {
    use super::*;
    pub(super) fn encode(
        e: &mut Encoder,
        y: &[u8],
        u: &[u8],
        v: &[u8],
        timestamp: i64,
        idr: bool,
    ) -> Option<Result<Option<AccessUnit>>> {
        if std::is_x86_feature_detected!("avx2")
            && std::is_x86_feature_detected!("popcnt")
            && std::is_x86_feature_detected!("lzcnt")
        {
            // CPU and OS extended-state support are checked before this target.
            Some(unsafe { avx2(e, y, u, v, timestamp, idr) })
        } else {
            None
        }
    }
    #[target_feature(enable = "avx2,popcnt,lzcnt")]
    unsafe fn avx2(
        e: &mut Encoder,
        y: &[u8],
        u: &[u8],
        v: &[u8],
        timestamp: i64,
        idr: bool,
    ) -> Result<Option<AccessUnit>> {
        e.encode_inner(y, u, v, timestamp, idr)
    }
}
