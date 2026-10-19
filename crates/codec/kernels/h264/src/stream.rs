// SPDX-License-Identifier: LGPL-2.1-or-later
// Streaming syntax-to-macroblock path. Header grammar is supplied by the pinned
// pure-Rust syntax library; entropy, dequant, pixels and filtering are local.
use crate::headers::{Headers, Slice as ParsedSlice};
use crate::syntax::{
    nal::AnnexBSplitter,
    pps::Pps,
    slice_header::{SliceHeader, SliceType},
    sps::Sps,
};
use crate::{
    Error, Result,
    dsp::transform::{self, Dequant},
    entropy::Entropy,
    picture::{Chroma, Crop, Picture},
    reconstruct::{Kind, Macroblock, Prediction, Reconstruction, References, Slice},
    scan,
};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
mod cache;
mod inter;
pub(crate) const CBP420: [u8; 48] = [
    47, 31, 15, 0, 23, 27, 29, 30, 7, 11, 13, 14, 39, 43, 45, 46, 16, 3, 5, 10, 12, 19, 21, 26, 28,
    35, 37, 42, 44, 1, 2, 4, 8, 17, 18, 20, 24, 6, 9, 22, 25, 32, 33, 34, 36, 40, 38, 41,
];
pub(crate) const CBP444: [u8; 16] = [15, 0, 7, 11, 13, 14, 3, 5, 10, 12, 1, 2, 4, 8, 6, 9];
const CHROMA_QP: [u8; 52] = [
    0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20, 21, 22, 23, 24, 25,
    26, 27, 28, 29, 29, 30, 31, 32, 32, 33, 34, 34, 35, 35, 36, 36, 37, 37, 37, 38, 38, 38, 39, 39,
    39, 39,
];
fn xy(i: usize) -> (usize, usize) {
    ((i / 4 % 2) * 2 + i % 2, (i / 8) * 2 + (i % 4 / 2))
}

#[derive(Clone)]
struct Neighbour {
    address: usize,
    intra: bool,
    skip: bool,
    motion: [Prediction; 16],
    // CABAC neighbours use only the saturated absolute context magnitude.
    // The actual (full i16) motion vector is stored independently above.
    mvd: [[i8; 2]; 16],
    slice: u32,
    modes: [u8; 16],
    nnz: [[u8; 16]; 3],
    dc: [bool; 3],
    i16: bool,
    transform8: bool,
    pcm: bool,
    chroma_mode: u8,
    cbp: u8,
}
struct SyntaxGrid {
    total: usize,
    base: usize,
    width: usize,
    rows: Vec<Option<Neighbour>>,
    chroma: Chroma,
}
impl SyntaxGrid {
    fn at(&self, address: usize) -> Option<&Neighbour> {
        let slot = if address >= self.base {
            address - self.base
        } else {
            address
                .checked_add(self.rows.len())?
                .checked_sub(self.base)?
        };
        self.rows
            .get(slot)
            .and_then(Option::as_ref)
            .filter(|n| n.address == address)
    }
    fn put(&mut self, address: usize, value: Neighbour) {
        if address >= self.base + self.rows.len() {
            self.base += self.rows.len();
        }
        self.rows[address - self.base] = Some(value);
    }

    fn left(&self, address: usize, slice: u32) -> Option<&Neighbour> {
        if address % self.width == 0 {
            None
        } else {
            self.at(address - 1).filter(|n| n.slice == slice)
        }
    }
    fn top(&self, address: usize, slice: u32) -> Option<&Neighbour> {
        if address < self.width {
            None
        } else {
            self.at(address - self.width).filter(|n| n.slice == slice)
        }
    }
}

/// Decoded AU output. Unsupported syntax fails explicitly; the software core
/// never routes input to a native decoder or buffers another complete frame.
pub struct Output {
    pub picture: Arc<Picture>,
    pub token: u64,
    pub poc: i32,
}
pub struct Decoder {
    headers: Headers,
    order: crate::order::Order,
    dpb: crate::dpb::Dpb,
    need_idr: bool,
    dequant: Option<(Sps, Pps, Arc<Dequant>)>,
    last_poc: i32,
    ended: bool,
    closed: bool,
}
impl Default for Decoder {
    fn default() -> Self {
        Self::new()
    }
}
impl Decoder {
    pub fn new() -> Self {
        Self {
            headers: Headers::new(),
            order: crate::order::Order::default(),
            dpb: crate::dpb::Dpb::default(),
            need_idr: true,
            dequant: None,
            last_poc: 0,
            ended: false,
            closed: false,
        }
    }
    /// The sending contract is progressive I/P. Output is ready
    /// after this AU; there is no B-picture queue or added frame holdback.
    pub fn submit(&mut self, au: &[u8], token: u64) -> Result<Vec<Output>> {
        static RUNNING: AtomicBool = AtomicBool::new(false);
        self.submit_with_cancel(au, token, &RUNNING)
    }
    pub fn submit_with_cancel(
        &mut self,
        au: &[u8],
        token: u64,
        cancel: &AtomicBool,
    ) -> Result<Vec<Output>> {
        if self.closed || self.ended {
            return Err(Error::Closed);
        }
        let picture = self.decode(au, cancel)?;
        Ok(vec![Output {
            picture,
            token,
            poc: self.last_poc,
        }])
    }
    pub fn seed(&mut self, extra: &[u8]) -> Result<()> {
        if self.closed || self.ended {
            return Err(Error::Closed);
        }
        if extra.is_empty() {
            return Ok(());
        }
        let mut parameters = self.headers.clone();
        if extra.first() == Some(&1) {
            if extra.len() < 7 {
                return Err(Error::Truncated);
            }
            let mut at = 6;
            let mut n = (extra[5] & 31) as usize;
            for round in 0..2 {
                if round == 1 {
                    n = *extra.get(at).ok_or(Error::Truncated)? as usize;
                    at += 1;
                }
                for _ in 0..n {
                    let size = extra.get(at..at + 2).ok_or(Error::Truncated)?;
                    let len = u16::from_be_bytes(size.try_into().unwrap()) as usize;
                    at += 2;
                    let nal = extra.get(at..at + len).ok_or(Error::Truncated)?;
                    at += len;
                    if nal
                        .first()
                        .is_none_or(|b| b & 31 != if round == 0 { 7 } else { 8 })
                        || parameters.process(nal)?.is_some()
                    {
                        return Err(Error::Invalid(crate::Fault::AvccParameterEntry));
                    }
                }
            }
        } else {
            let mut count = 0;
            for nal in AnnexBSplitter::new(extra) {
                if nal.first().is_none_or(|b| !matches!(b & 31, 7 | 8))
                    || parameters.process(nal)?.is_some()
                {
                    return Err(Error::Invalid(crate::Fault::ExtradataIsNotAParameterSet));
                }
                count += 1;
            }
            if count == 0 {
                return Err(Error::Invalid(crate::Fault::MissingAnnexBParameterPrefix));
            }
        }
        self.headers = parameters;
        Ok(())
    }
    pub fn finish(&mut self) -> Result<Vec<Output>> {
        if self.closed {
            return Err(Error::Closed);
        }
        self.ended = true;
        Ok(vec![])
    }
    pub fn reset(&mut self) -> Result<()> {
        if self.closed {
            return Err(Error::Closed);
        }
        self.dpb.clear();
        self.order = crate::order::Order::default();
        self.need_idr = true;
        self.ended = false;
        Ok(())
    }
    pub fn close(&mut self) {
        self.dpb = crate::dpb::Dpb::default();
        self.dequant = None;
        self.headers = Headers::new();
        self.closed = true;
    }
    fn decode(&mut self, au: &[u8], cancel: &AtomicBool) -> Result<Arc<Picture>> {
        let result = self.decode_inner(au, cancel);
        if result.is_err() {
            self.dpb.clear();
            self.order = crate::order::Order::default();
            self.need_idr = true;
        }
        result
    }
    fn decode_inner(&mut self, au: &[u8], cancel: &AtomicBool) -> Result<Arc<Picture>> {
        #[cfg(feature = "profile")]
        let _total = crate::profile::Timer::new(0);
        if cancel.load(Ordering::Acquire) {
            return Err(Error::Cancelled);
        }
        let mut parameters = self.headers.clone();
        let mut reconstruction = None;
        let mut grid = None;
        let mut sets: Option<(Sps, Pps)> = None;
        let mut slice_id = 0;
        let mut identity: Option<(SliceHeader, bool, bool)> = None;
        let mut order_ticket = None;
        for nal in AnnexBSplitter::new(au) {
            if cancel.load(Ordering::Acquire) {
                return Err(Error::Cancelled);
            }
            if nal.first().is_some_and(|h| matches!(h&31,2..=4|19..=21)) {
                return Err(Error::Unsupported(crate::Fault::ContractExtendedVcl));
            }
            #[cfg(feature = "profile")]
            let header_timer = crate::profile::Timer::new(5);
            let event = parameters.process(nal)?;
            #[cfg(feature = "profile")]
            drop(header_timer);
            if let Some(ParsedSlice {
                header,
                sps,
                pps,
                rbsp,
                slice_data_cursor,
                nal_unit_type,
                nal_ref_idc,
            }) = event
            {
                let idr = nal_unit_type == 5;
                let is_reference = nal_ref_idc != 0;
                if let Some((first, old_idr, old_ref)) = &identity {
                    if *old_idr != idr
                        || *old_ref != is_reference
                        || first.frame_num != header.frame_num
                        || first.idr_pic_id != header.idr_pic_id
                        || first.pic_order_cnt_lsb != header.pic_order_cnt_lsb
                        || first.delta_pic_order_cnt != header.delta_pic_order_cnt
                        || first.delta_pic_order_cnt_bottom != header.delta_pic_order_cnt_bottom
                        || first.dec_ref_pic_marking != header.dec_ref_pic_marking
                    {
                        return Err(Error::Invalid(crate::Fault::MixedPictureIdentities));
                    }
                } else {
                    if self.need_idr && !idr {
                        return Err(Error::NeedKeyframe);
                    }
                    if idr {
                        self.dpb.clear();
                    }
                    order_ticket = Some(self.order.prepare(&sps, &header, idr, is_reference)?);
                    identity = Some((header.clone(), idr, is_reference));
                }
                if !sps.frame_mbs_only_flag
                    || header.field_pic_flag
                    || sps.bit_depth_luma_minus8 != 0
                    || sps.bit_depth_chroma_minus8 != 0
                    || !matches!(sps.chroma_array_type(), 1 | 3)
                    || pps.num_slice_groups_minus1 != 0
                {
                    return Err(Error::Unsupported(crate::Fault::StreamingFrameFormat));
                }
                if !matches!(header.slice_type, SliceType::I | SliceType::P) {
                    return Err(Error::Unsupported(crate::Fault::ContractOnlyIp));
                }
                if let Some((old_sps, old_pps)) = &sets {
                    if old_sps != &sps || old_pps != &pps {
                        return Err(Error::Invalid(crate::Fault::ParameterChangeWithinPicture));
                    }
                } else {
                    if header.first_mb_in_slice != 0 {
                        return Err(Error::Invalid(crate::Fault::MissingFirstMacroblock));
                    }
                    let chroma = if sps.chroma_array_type() == 3 {
                        Chroma::Yuv444
                    } else {
                        Chroma::Yuv420
                    };
                    let (width, height) = (
                        sps.pic_width_in_mbs() as usize * 16,
                        sps.frame_height_in_mbs() as usize * 16,
                    );
                    let unit = if chroma == Chroma::Yuv444 { 1 } else { 2 };
                    let c = sps.frame_cropping.as_ref();
                    let (left, right, top, bottom) = c.map_or((0, 0, 0, 0), |c| {
                        (
                            c.left as usize * unit,
                            c.right as usize * unit,
                            c.top as usize * unit,
                            c.bottom as usize * unit,
                        )
                    });
                    let crop = Crop {
                        left,
                        top,
                        width: width
                            .checked_sub(left + right)
                            .ok_or(Error::Invalid(crate::Fault::HorizontalCrop))?,
                        height: height
                            .checked_sub(top + bottom)
                            .ok_or(Error::Invalid(crate::Fault::VerticalCrop))?,
                    };
                    reconstruction = Some(Reconstruction::new(
                        self.dpb.picture(width, height, chroma, crop)?,
                    ));
                    grid = Some(SyntaxGrid {
                        width: width / 16,
                        rows: vec![None; width / 16 * (height / 16).min(2)],
                        total: width / 16 * (height / 16),
                        base: 0,
                        chroma,
                    });
                    sets = Some((sps.clone(), pps.clone()));
                }
                if self
                    .dequant
                    .as_ref()
                    .is_none_or(|(old_sps, old_pps, _)| old_sps != &sps || old_pps != &pps)
                {
                    let four = std::array::from_fn(|i| {
                        crate::syntax::transform::select_scaling_list_4x4(i, &sps, &pps)
                            .map(|v| v as u8)
                    });
                    let eight = std::array::from_fn(|i| {
                        crate::syntax::transform::select_scaling_list_8x8(i, &sps, &pps)
                            .map(|v| v as u8)
                    });
                    self.dequant = Some((
                        sps.clone(),
                        pps.clone(),
                        Arc::new(Dequant::new(&four, &eight)?),
                    ));
                }
                let dequant = self.dequant.as_ref().unwrap().2.clone();
                let references = if header.slice_type != SliceType::I {
                    self.dpb.p_list(&header, &sps)?
                } else {
                    vec![]
                };
                decode_slice(
                    &rbsp,
                    slice_data_cursor,
                    &header,
                    &sps,
                    &pps,
                    slice_id,
                    &dequant,
                    grid.as_mut().unwrap(),
                    reconstruction.as_mut().unwrap(),
                    &references,
                    cancel,
                )?;
                slice_id += 1;
            }
        }
        let picture = reconstruction
            .ok_or(Error::Invalid(crate::Fault::AuHasNoPicture))?
            .finish()?;
        if cancel.load(Ordering::Acquire) {
            return Err(Error::Cancelled);
        }
        let (header, idr, is_reference) = identity.unwrap();
        self.dpb.commit(
            picture.clone(),
            &header,
            &sets.as_ref().unwrap().0,
            idr,
            is_reference,
        )?;
        let ticket = order_ticket.unwrap();
        self.last_poc = ticket.poc;
        self.order.commit(ticket);
        self.headers = parameters;
        self.need_idr = false;
        Ok(picture)
    }
}

#[allow(clippy::too_many_arguments)]
fn decode_slice(
    rbsp: &[u8],
    cursor: (usize, u8),
    header: &SliceHeader,
    sps: &Sps,
    pps: &Pps,
    slice_id: u32,
    dq: &Dequant,
    grid: &mut SyntaxGrid,
    output: &mut Reconstruction,
    references: &[crate::reconstruct::Reference],
    cancel: &AtomicBool,
) -> Result<()> {
    // Format and entropy mode are fixed by the validated active SPS/PPS.
    // Match once per slice, not at every macroblock/residual dispatch.
    match (grid.chroma, pps.entropy_coding_mode_flag) {
        (Chroma::Yuv420, false) => decode_slice_sized::<false, false>(
            rbsp, cursor, header, sps, pps, slice_id, dq, grid, output, references, cancel,
        ),
        (Chroma::Yuv420, true) => decode_slice_sized::<false, true>(
            rbsp, cursor, header, sps, pps, slice_id, dq, grid, output, references, cancel,
        ),
        (Chroma::Yuv444, false) => decode_slice_sized::<true, false>(
            rbsp, cursor, header, sps, pps, slice_id, dq, grid, output, references, cancel,
        ),
        (Chroma::Yuv444, true) => decode_slice_sized::<true, true>(
            rbsp, cursor, header, sps, pps, slice_id, dq, grid, output, references, cancel,
        ),
    }
}
#[allow(clippy::too_many_arguments)]
fn decode_slice_sized<const FULL: bool, const CABAC: bool>(
    rbsp: &[u8],
    cursor: (usize, u8),
    header: &SliceHeader,
    sps: &Sps,
    pps: &Pps,
    slice_id: u32,
    dq: &Dequant,
    grid: &mut SyntaxGrid,
    output: &mut Reconstruction,
    references: &[crate::reconstruct::Reference],
    cancel: &AtomicBool,
) -> Result<()> {
    let weights = std::array::from_fn(|i| inter::weights(header, i as u8));
    let mut address = header.first_mb_in_slice as usize;
    let mut qp = 26 + pps.pic_init_qp_minus26 + header.slice_qp_delta;
    if !(0..=51).contains(&qp) {
        return Err(Error::Invalid(crate::Fault::SliceQuantizer));
    }
    let slice = Slice {
        id: slice_id,
        constrained_intra: pps.constrained_intra_pred_flag,
        deblock_idc: header.disable_deblocking_filter_idc as u8,
        alpha_offset: (header.slice_alpha_c0_offset_div2 * 2) as i8,
        beta_offset: (header.slice_beta_offset_div2 * 2) as i8,
    };
    let mut entropy = Entropy::new(
        rbsp,
        cursor,
        qp as u8,
        CABAC,
        if header.slice_type == SliceType::I {
            None
        } else {
            Some(header.cabac_init_idc as u8)
        },
    )?;
    let mut coefficients = [0i16; 768];
    let mut partitions = [crate::reconstruct::Partition::default(); 16];
    while entropy.more() {
        if cancel.load(Ordering::Acquire) {
            return Err(Error::Cancelled);
        }
        if address >= grid.total {
            return Err(Error::Invalid(crate::Fault::TooManyMacroblocks));
        }
        let left = grid.left(address, slice_id);
        let top = grid.top(address, slice_id);
        let skipped = header.slice_type != SliceType::I
            && entropy.skip_p(left.is_some_and(|n| !n.skip), top.is_some_and(|n| !n.skip))?;
        let (is_intra, raw) = if skipped {
            (false, 0)
        } else if header.slice_type == SliceType::P {
            entropy.p_type()?
        } else {
            (
                true,
                entropy.mb_type(left.is_some_and(|n| n.i16), top.is_some_and(|n| n.i16))?,
            )
        };
        if raw > 25 || !is_intra && raw > 4 {
            return Err(Error::Invalid(crate::Fault::MacroblockType));
        }
        let mut neighbour = Neighbour {
            address,
            intra: is_intra,
            skip: skipped,
            motion: [Prediction::default(); 16],
            mvd: [[0; 2]; 16],
            slice: slice_id,
            modes: [2; 16],
            nnz: [[0; 16]; 3],
            dc: [false; 3],
            i16: is_intra && raw != 0,
            transform8: false,
            pcm: false,
            chroma_mode: 0,
            cbp: 0,
        };
        if skipped {
            inter::p_skip(grid, address, &mut neighbour, weights[0], &mut partitions);
            entropy.no_delta();
            output.submit_skip(
                address,
                &partitions[0],
                [
                    qp as i8,
                    CHROMA_QP[(qp + pps.chroma_qp_index_offset).clamp(0, 51) as usize] as i8,
                    CHROMA_QP[(qp + pps.second_chroma_qp_index_offset()).clamp(0, 51) as usize]
                        as i8,
                ],
                slice,
                references,
            )?;
            grid.put(address, neighbour);
            address += 1;
            entropy.end_mb()?;
            continue;
        }
        if raw == 25 {
            let count = if FULL { 768 } else { 384 };
            let pcm = entropy.pcm(count)?;
            output.submit_pcm(address, &pcm, slice)?;
            neighbour.nnz = [[16; 16]; 3];
            neighbour.dc = [true; 3];
            neighbour.pcm = true;
            neighbour.cbp = 47;
            grid.put(address, neighbour);
            address += 1;
            entropy.end_mb()?;
            continue;
        }
        let mut cache = cache::Cache::new(
            left,
            top,
            if FULL { Chroma::Yuv444 } else { Chroma::Yuv420 },
            is_intra,
            pps.constrained_intra_pred_flag,
            CABAC,
        );
        let i16 = is_intra && raw != 0;
        let mut use8 = is_intra
            && !i16
            && pps.transform_8x8_mode_flag()
            && entropy.transform8(
                left.is_some_and(|n| n.transform8),
                top.is_some_and(|n| n.transform8),
            )?;
        neighbour.transform8 = use8;
        let mut inter8_allowed = true;
        let kind = if !is_intra {
            let (kind, allowed) = inter::p_parts(
                &mut entropy,
                grid,
                address,
                &mut neighbour,
                header,
                &weights,
                raw,
                &mut partitions,
            )?;
            inter8_allowed = allowed;
            kind
        } else if i16 {
            Kind::Intra16(((raw - 1) % 4) as u8)
        } else {
            let mut modes = [2; 16];
            let count = if use8 { 4 } else { 16 };
            for i in 0..count {
                let (bx, by) = xy(if use8 { i * 4 } else { i });
                let prediction = cache.mode(bx, by);
                let mode = entropy.mode(prediction)?;
                modes[i] = mode;
                let span = if use8 { 2 } else { 1 };
                for yy in by..by + span {
                    for xx in bx..bx + span {
                        neighbour.modes[yy * 4 + xx] = mode;
                        cache.set_mode(xx, yy, mode);
                    }
                }
            }
            if use8 {
                Kind::Intra8(modes[..4].try_into().unwrap())
            } else {
                Kind::Intra4(modes)
            }
        };
        let chroma_mode = if is_intra && !FULL {
            entropy.chroma_mode(
                left.map_or(0, |n| n.chroma_mode),
                top.map_or(0, |n| n.chroma_mode),
            )?
        } else {
            0
        };
        neighbour.chroma_mode = chroma_mode;
        let cbp = if i16 {
            (((raw - 1) / 12) * 15 + (((raw - 1) / 4) % 3) * 16) as u8
        } else {
            entropy.cbp(
                left.map_or(15, |n| n.cbp),
                top.map_or(15, |n| n.cbp),
                !FULL,
                is_intra,
            )?
        };
        neighbour.cbp = cbp;
        if !is_intra && cbp & 15 != 0 && inter8_allowed && pps.transform_8x8_mode_flag() {
            use8 = entropy.transform8(
                left.is_some_and(|n| n.transform8),
                top.is_some_and(|n| n.transform8),
            )?;
            neighbour.transform8 = use8;
        }
        if cbp != 0 || i16 {
            let delta = entropy.delta()?;
            if !(-26..=25).contains(&delta) {
                return Err(Error::Invalid(crate::Fault::MacroblockQpDelta));
            }
            qp = (qp + delta + 52) % 52;
        } else {
            entropy.no_delta();
        }
        let qc0 = CHROMA_QP[(qp + pps.chroma_qp_index_offset).clamp(0, 51) as usize];
        let qc1 = CHROMA_QP[(qp + pps.second_chroma_qp_index_offset()).clamp(0, 51) as usize];
        let bypass = sps.qpprime_y_zero_transform_bypass_flag && qp == 0;
        let mut mb = Macroblock {
            kind,
            cbp,
            coefficients: &mut coefficients,
            nonzero: [0; 48],
            transform8: use8,
            bypass,
            qp: [qp as i8, qc0 as i8, qc1 as i8],
            chroma_mode,
        };
        for plane in 0..if FULL { 3 } else { 1 } {
            let q = mb.qp[plane] as usize;
            let mut dc = [0i16; 16];
            if i16 {
                let count = entropy.residual(
                    if CABAC { 0 } else { cache.nc(plane, 0, 0) },
                    [0, 6, 10][plane],
                    Some(cache.cbf(plane, 0, 0, true, false)),
                    &scan::DC_LUMA,
                    None,
                    &mut dc,
                )?;
                neighbour.dc[plane] = count != 0;
                if !bypass {
                    dc = transform::luma_dc(&dc, dq.four[plane + 3 * usize::from(!is_intra)][q][0]);
                }
            }
            for group in 0..4 {
                if use8 && CABAC {
                    if cbp & (1 << group) != 0 {
                        let (bx, by) = xy(group * 4);
                        let cbf = if plane == 0 && !FULL {
                            None
                        } else {
                            Some(cache.cbf(plane, bx, by, false, true))
                        };
                        let count = entropy.residual(
                            0,
                            [5, 9, 13][plane],
                            cbf,
                            &scan::EIGHT,
                            if bypass {
                                None
                            } else {
                                Some(&dq.eight[plane * 2 + usize::from(!is_intra)][q])
                            },
                            &mut mb.coefficients
                                [plane * 256 + group * 64..plane * 256 + group * 64 + 64],
                        )?;
                        for sub in 0..4 {
                            let (xx, yy) = xy(group * 4 + sub);
                            // Reconstruction stores the total once per 8x8
                            // transform. CABAC's neighbour cache still needs
                            // that count replicated at every covered 4x4 cell.
                            mb.nonzero[plane * 16 + group * 4 + sub] =
                                if sub == 0 { count } else { 0 };
                            neighbour.nnz[plane][yy * 4 + xx] = count;
                            cache.set_nnz(plane, xx, yy, count);
                        }
                    }
                    continue;
                }
                for sub in 0..4 {
                    let index = group * 4 + sub;
                    let (bx, by) = xy(index);
                    let nc = if CABAC { 0 } else { cache.nc(plane, bx, by) };
                    if cbp & (1 << group) != 0 {
                        if use8 {
                            let scan = &scan::EIGHT_CAVLC[sub];
                            let count = entropy.residual(
                                nc,
                                0,
                                None,
                                scan,
                                if bypass {
                                    None
                                } else {
                                    Some(&dq.eight[plane * 2 + usize::from(!is_intra)][q])
                                },
                                &mut mb.coefficients
                                    [plane * 256 + group * 64..plane * 256 + group * 64 + 64],
                            )?;
                            mb.nonzero[plane * 16 + index] = count;
                            neighbour.nnz[plane][by * 4 + bx] = count;
                            cache.set_nnz(plane, bx, by, count);
                        } else {
                            let scan = if i16 { &scan::FOUR_AC } else { &scan::FOUR };
                            let count = entropy.residual(
                                nc,
                                if i16 {
                                    [1, 7, 11][plane]
                                } else {
                                    [2, 8, 12][plane]
                                },
                                Some(cache.cbf(plane, bx, by, false, false)),
                                scan,
                                if bypass {
                                    None
                                } else {
                                    Some(&dq.four[plane + 3 * usize::from(!is_intra)][q])
                                },
                                &mut mb.coefficients
                                    [plane * 256 + index * 16..plane * 256 + index * 16 + 16],
                            )?;
                            mb.nonzero[plane * 16 + index] = count;
                            neighbour.nnz[plane][by * 4 + bx] = count;
                            cache.set_nnz(plane, bx, by, count);
                        }
                    }
                    if i16 {
                        mb.coefficients[plane * 256 + index * 16] = dc[by * 4 + bx];
                    }
                }
            }
        }
        if !FULL {
            let coded_chroma = cbp >> 4;
            if coded_chroma != 0 {
                for plane in 1..3 {
                    let mut dc = [0; 4];
                    let count = entropy.residual(
                        -1,
                        3,
                        Some(cache.cbf(plane, 0, 0, true, false)),
                        &scan::DC_CHROMA,
                        None,
                        &mut dc,
                    )?;
                    neighbour.dc[plane] = count != 0;
                    let q = mb.qp[plane] as usize;
                    let dc = if bypass {
                        dc
                    } else {
                        transform::chroma_dc(dc, dq.four[plane + 3 * usize::from(!is_intra)][q][0])
                    };
                    for (index, &value) in dc.iter().enumerate() {
                        mb.coefficients[plane * 256 + index * 16] = value;
                    }
                }
            }
            if coded_chroma == 2 {
                for plane in 1..3 {
                    for index in 0..4 {
                        let (bx, by) = (index % 2, index / 2);
                        let q = mb.qp[plane] as usize;
                        let count = entropy.residual(
                            if CABAC { 0 } else { cache.nc(plane, bx, by) },
                            4,
                            Some(cache.cbf(plane, bx, by, false, false)),
                            &scan::FOUR_AC,
                            if bypass {
                                None
                            } else {
                                Some(&dq.four[plane + 3 * usize::from(!is_intra)][q])
                            },
                            &mut mb.coefficients
                                [plane * 256 + index * 16..plane * 256 + index * 16 + 16],
                        )?;
                        mb.nonzero[plane * 16 + index] = count;
                        neighbour.nnz[plane][by * 4 + bx] = count;
                        cache.set_nnz(plane, bx, by, count);
                    }
                }
            }
        }
        output.submit(
            address,
            &mut mb,
            slice,
            References {
                pictures: references,
            },
        )?;
        grid.put(address, neighbour);
        address += 1;
        entropy.end_mb()?;
    }
    entropy.finish()
}

#[cfg(feature = "profile")]
impl Drop for Decoder {
    fn drop(&mut self) {
        crate::profile::report();
    }
}
