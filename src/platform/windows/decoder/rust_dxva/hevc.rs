// SPDX-License-Identifier: LGPL-2.1-or-later
use super::{
    dxva::{Codec, Failure, Lease, Picture, Pool},
    params::{HevcParams, HevcRangeParams},
};
use anyhow::{Context, Result, ensure};
use farwing_codec::syntax::hevc::{
    bitreader::BitReader,
    dpb::{LongTermEntry, build_rps_poc_lists},
    nal::{NalHeader, strip_emulation_prevention},
    poc::{NalKind, PocState},
    pps::PicParameterSet,
    scaling_list::ScalingListData,
    slice::{SliceLongTermRefPicSource, SliceSegmentHeader, SliceType},
    sps::{MaterializedShortTermRefPicSet, SeqParameterSet, ShortTermRefPicSet},
};
use std::{
    collections::HashMap,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};
use windows::Win32::Graphics::Direct3D11::ID3D11Device;
struct Reference {
    poc: i32,
    long: bool,
    frame: Arc<Lease>,
}
pub struct Hevc {
    device: ID3D11Device,
    pool: Option<Arc<Pool>>,
    sps: HashMap<u8, SeqParameterSet>,
    pps: HashMap<u8, PicParameterSet>,
    refs: Vec<Reference>,
    poc: PocState,
    fresh: bool,
    no_rasl_output: bool,
    report: u32,
}
impl Hevc {
    pub fn new(device: ID3D11Device) -> Self {
        Self {
            device,
            pool: None,
            sps: HashMap::new(),
            pps: HashMap::new(),
            refs: vec![],
            poc: PocState::new(),
            fresh: true,
            no_rasl_output: true,
            report: 0,
        }
    }
    pub fn reset(&mut self) {
        self.refs.clear();
        self.poc = PocState::new();
        self.fresh = true;
        self.no_rasl_output = true;
    }
    pub fn decode(&mut self, data: &[u8], cancel: &AtomicBool) -> Result<Option<Picture>> {
        let result = self.decode_inner(data, cancel);
        if result.is_err() {
            self.reset();
        }
        result
    }
    fn decode_inner(&mut self, data: &[u8], cancel: &AtomicBool) -> Result<Option<Picture>> {
        ensure!(!cancel.load(Ordering::Acquire), "decode cancelled");
        let mut first: Option<(
            NalHeader,
            SliceSegmentHeader,
            SeqParameterSet,
            PicParameterSet,
            u16,
        )> = None;
        let mut slices = vec![];
        for nal in farwing_codec::syntax::h264::nal::AnnexBSplitter::new(data) {
            let header = NalHeader::parse(nal)?;
            ensure!(header.nuh_layer_id == 0, "layered HEVC unsupported");
            let rbsp = strip_emulation_prevention(&nal[2..]);
            match header.nal_unit_type {
                32 => {}
                33 => {
                    let s = SeqParameterSet::parse(&rbsp)?;
                    self.sps.insert(s.sps_id, s);
                }
                34 => {
                    let p = PicParameterSet::parse(&rbsp)?;
                    self.pps.insert(p.pps_id, p);
                }
                0..=31 => {
                    let mut bits = BitReader::new(&rbsp);
                    let is_first = bits.u1()?;
                    if (16..=23).contains(&header.nal_unit_type) {
                        bits.u1()?;
                    }
                    let id = bits.ue()?;
                    ensure!(id <= 63, "invalid HEVC PPS id");
                    let p = self.pps.get(&(id as u8)).context("missing HEVC PPS")?;
                    let s = self.sps.get(&p.sps_id).context("missing HEVC SPS")?;
                    let sh = SliceSegmentHeader::parse(&rbsp, header.nal_unit_type, s, p)?;
                    if let Some((old_nal, old, old_sps, old_pps, _)) = &first {
                        ensure!(
                            is_first == 0
                                && old_sps == s
                                && old_pps == p
                                && old_nal.nal_unit_type == header.nal_unit_type
                                && old_nal.temporal_id == header.temporal_id
                                && (sh.dependent_slice_segment_flag
                                    || old.slice_pic_order_cnt_lsb == sh.slice_pic_order_cnt_lsb),
                            "AU contains different HEVC pictures or parameter sets"
                        );
                    }
                    if first.is_none() {
                        ensure!(is_first != 0, "missing first HEVC slice");
                        let rps_bits = inline_rps_bits(&rbsp, header.nal_unit_type, s, p)?;
                        first = Some((header, sh, s.clone(), p.clone(), rps_bits));
                    }
                    slices.push(nal);
                }
                _ => {}
            }
        }
        let (nal, sh, s, p, rps_bits) = first.context("no HEVC slices")?;
        let kind = NalKind::new(nal.nal_unit_type);
        // Leading RASL pictures at a fresh CRA/BLA may reference pictures
        // before the entry point. Discard them without invalidating recovery.
        if kind.is_rasl() && self.no_rasl_output {
            return Ok(None);
        }
        ensure!(
            !self.fresh || kind.is_idr() || kind.is_bla() || kind.is_cra(),
            "waiting for HEVC random access after start/reset/failure"
        );
        let no_rasl = kind.is_idr() || kind.is_bla() || (kind.is_cra() && self.fresh);
        if kind.is_idr() || kind.is_bla() || kind.is_cra() {
            self.no_rasl_output = no_rasl;
        }
        if kind.is_idr() || kind.is_bla() {
            self.reset();
        }
        ensure!(
            matches!(s.chroma_format_idc, 1 | 3)
                && !s.separate_colour_plane_flag
                && s.bit_depth_luma_minus8 == s.bit_depth_chroma_minus8
                && matches!(s.bit_depth_luma_minus8, 0 | 2),
            Failure::Unsupported
        );
        let depth = s.bit_depth_luma_minus8 + 8;
        let width = s.pic_width_in_luma_samples;
        let height = s.pic_height_in_luma_samples;
        let crop = &s.conformance_window;
        let crop_unit = if s.chroma_format_idc == 1 { 2 } else { 1 };
        ensure!(
            crop.left_offset
                .checked_add(crop.right_offset)
                .is_some_and(|v| v < width / crop_unit)
                && crop
                    .top_offset
                    .checked_add(crop.bottom_offset)
                    .is_some_and(|v| v < height / crop_unit),
            "invalid HEVC crop"
        );
        let capacity = s.sub_layer_ordering_info[s.max_sub_layers_minus1 as usize]
            .max_dec_pic_buffering_minus1
            + 1;
        ensure!((1..=16).contains(&capacity), "invalid HEVC DPB size");
        if self.pool.as_ref().is_none_or(|pool| {
            pool.width != width
                || pool.height != height
                || pool.depth != depth
                || pool.chroma != s.chroma_format_idc as u8
                || pool.dpb < capacity as usize
        }) {
            ensure!(self.refs.is_empty(), "HEVC resize requires reference reset");
            self.pool = Some(
                Pool::new(
                    self.device.clone(),
                    Codec::Hevc,
                    width,
                    height,
                    depth,
                    s.chroma_format_idc as u8,
                    capacity as usize,
                )
                .map_err(|e| e.context(Failure::HardwareFailure))?,
            );
        }
        let pool = self.pool.as_ref().unwrap();
        let current = Arc::new(pool.lease(false)?);
        let max_lsb = 1u32 << (s.log2_max_pic_order_cnt_lsb_minus4 + 4);
        let poc = self.poc.derive(
            kind,
            no_rasl,
            sh.slice_pic_order_cnt_lsb.unwrap_or(0),
            max_lsb,
        );
        let sets = s.materialize_short_term_ref_pic_sets()?;
        let mut delta_count = 0;
        let rps = if kind.is_idr() {
            MaterializedShortTermRefPicSet::default()
        } else if let Some(inline) = &sh.inline_short_term_ref_pic_set {
            let source = if inline.inter_ref_pic_set_prediction_flag {
                let index = sets
                    .len()
                    .checked_sub(inline.delta_idx_minus1 as usize + 1)
                    .context("invalid predicted RPS index")?;
                delta_count = sets[index].num_delta_pocs();
                Some(&sets[index])
            } else {
                None
            };
            inline.materialize(source)?
        } else {
            sets.get(sh.short_term_ref_pic_set_idx.unwrap_or(0) as usize)
                .context("invalid SPS RPS index")?
                .clone()
        };
        let mut cycle = 0;
        let mut long = vec![];
        for (i, entry) in sh.long_term_ref_pics.iter().enumerate() {
            cycle = if i == 0 || i == sh.num_long_term_sps.unwrap_or(0) as usize {
                entry.delta_poc_msb_cycle_lt
            } else {
                cycle + entry.delta_poc_msb_cycle_lt
            };
            let (lsb, used) = match entry.source {
                SliceLongTermRefPicSource::Sps { lt_idx_sps } => {
                    let e = s
                        .long_term_ref_pics
                        .get(lt_idx_sps as usize)
                        .context("invalid SPS long term index")?;
                    (e.poc_lsb, e.used_by_curr_pic)
                }
                SliceLongTermRefPicSource::InSlice {
                    poc_lsb_lt,
                    used_by_curr_pic_lt_flag,
                } => (poc_lsb_lt, used_by_curr_pic_lt_flag),
            };
            long.push(LongTermEntry {
                poc_lsb_lt: lsb,
                used_by_curr_pic_lt: used,
                delta_poc_msb_present: entry.delta_poc_msb_present_flag,
                delta_poc_msb_cycle_lt: cycle,
            });
        }
        let lists = build_rps_poc_lists(kind.is_idr(), poc.val, max_lsb, &rps, &long);
        // Positions of RefPicListTemp0 (before, after, long-term) that this
        // P slice actually predicts from. A recovery frame after a loss keeps
        // the lost picture in its RPS but only selects a long-term reference
        // (`docs/protocol/rfi-ltr-brief.md` §3.1): its missing entry stays
        // in place as invalid, so the list indices do not move.
        let total = lists.st_curr_before.len() + lists.st_curr_after.len() + lists.lt_curr.len();
        let selected: Option<Vec<usize>> = (sh.slice_type == Some(SliceType::P) && total > 0).then(|| {
            let active = usize::from(
                sh.num_ref_idx_l0_active_minus1
                    .unwrap_or(p.num_ref_idx_l0_default_active_minus1),
            ) + 1;
            match sh
                .ref_pic_lists_modification
                .as_ref()
                .filter(|m| m.ref_pic_list_modification_flag_l0)
            {
                Some(modification) => modification
                    .list_entry_l0
                    .iter()
                    .take(active)
                    .map(|entry| *entry as usize)
                    .collect(),
                None => (0..active).map(|i| i % total).collect(),
            }
        });
        let mut keep = vec![];
        let mut before = vec![];
        let mut after = vec![];
        let mut lt = vec![];
        let mut position = 0;
        for (pocs, target, is_long, flags) in [
            (&lists.st_curr_before, &mut before, false, None),
            (&lists.st_curr_after, &mut after, false, None),
            (
                &lists.lt_curr,
                &mut lt,
                true,
                Some(&lists.curr_delta_poc_msb_present),
            ),
        ] {
            for (i, value) in pocs.iter().enumerate() {
                let found = self.refs.iter().find(|r| {
                    if is_long && flags.is_some_and(|f| !f[i]) {
                        (r.poc as u32 & (max_lsb - 1)) == (*value as u32 & (max_lsb - 1))
                    } else {
                        r.poc == *value
                    }
                });
                let unused = selected.as_ref().is_some_and(|s| !s.contains(&position));
                position += 1;
                match found {
                    Some(r) => {
                        target.push(Some(r.poc));
                        keep.push((r.poc, is_long));
                    }
                    None if unused => target.push(None),
                    None => anyhow::bail!("missing HEVC reference picture"),
                }
            }
        }
        for (value, is_long, flag) in lists.st_foll.iter().map(|p| (*p, false, true)).chain(
            lists
                .lt_foll
                .iter()
                .zip(&lists.foll_delta_poc_msb_present)
                .map(|(p, f)| (*p, true, *f)),
        ) {
            if let Some(r) = self.refs.iter().find(|r| {
                if is_long && !flag {
                    (r.poc as u32 & (max_lsb - 1)) == (value as u32 & (max_lsb - 1))
                } else {
                    r.poc == value
                }
            }) {
                keep.push((r.poc, is_long));
            }
        }
        self.refs.retain(|r| keep.iter().any(|p| p.0 == r.poc));
        for r in &mut self.refs {
            r.long = keep.iter().any(|p| p.0 == r.poc && p.1);
        }
        self.report = self.report.wrapping_add(1).max(1);
        let mut v = parameters(
            &s,
            &p,
            nal.nal_unit_type,
            poc.val,
            self.report,
            current.index,
        )?;
        v.wNumBitsForShortTermRPSInSlice = rps_bits;
        v.ucNumDeltaPocsOfRefRpsIdx = delta_count as u8;
        for (i, r) in self.refs.iter().enumerate() {
            ensure!(i < 15, "too many HEVC references");
            v.RefPicList[i] = r.frame.index as u8 | if r.long { 128 } else { 0 };
            v.PicOrderCntValList[i] = r.poc;
        }
        fn indices(pocs: &[Option<i32>], refs: &[Reference]) -> Result<[u8; 8]> {
            ensure!(pocs.len() <= 8, "too many active HEVC references");
            let mut out = [255; 8];
            for (i, p) in pocs.iter().enumerate() {
                // A missing, unselected picture stays an invalid entry.
                let Some(p) = p else { continue };
                out[i] = refs
                    .iter()
                    .position(|r| r.poc == *p)
                    .context("RPS not in DPB")? as u8;
            }
            Ok(out)
        }
        v.RefPicSetStCurrBefore = indices(&before, &self.refs)?;
        v.RefPicSetStCurrAfter = indices(&after, &self.refs)?;
        v.RefPicSetLtCurr = indices(&lt, &self.refs)?;
        let matrix = matrix(&s, &p);
        let range = range_parameters(v, &s, &p)?;
        let parameter_len = if s.chroma_format_idc == 3 {
            std::mem::size_of::<HevcRangeParams>()
        } else {
            std::mem::size_of::<HevcParams>()
        };
        let bytes = unsafe {
            std::slice::from_raw_parts((&range as *const HevcRangeParams).cast(), parameter_len)
        };
        pool.submit(&current, bytes, &matrix, &slices, cancel)
            .map_err(|e| e.context(Failure::HardwareFailure))?;
        if nal.temporal_id == 0 && !(kind.is_rasl() || kind.is_radl() || kind.is_slnr()) {
            self.poc.update_prev_tid0(poc);
        }
        self.fresh = false;
        self.refs.push(Reference {
            poc: poc.val,
            long: false,
            frame: current.clone(),
        });
        Ok(Some(Picture {
            surface: current,
            left: crop_unit * crop.left_offset,
            top: crop_unit * crop.top_offset,
            width: width - crop_unit * (crop.left_offset + crop.right_offset),
            height: height - crop_unit * (crop.top_offset + crop.bottom_offset),
            poc: poc.val,
            reorder_limit: s.sub_layer_ordering_info[s.max_sub_layers_minus1 as usize]
                .max_num_reorder_pics,
            sequence_start: if kind.is_idr() || kind.is_bla() {
                Some(sh.no_output_of_prior_pics_flag.unwrap_or(false))
            } else {
                None
            },
            needed_for_output: sh.pic_output_flag,
        }))
    }
}
fn inline_rps_bits(rbsp: &[u8], kind: u8, s: &SeqParameterSet, p: &PicParameterSet) -> Result<u16> {
    if kind == 19 || kind == 20 {
        return Ok(0);
    }
    let mut b = BitReader::new(rbsp);
    ensure!(b.u1()? != 0, "expected first slice");
    if (16..=23).contains(&kind) {
        b.u1()?;
    }
    b.ue()?;
    b.skip(p.num_extra_slice_header_bits as usize)?;
    b.ue()?;
    if p.output_flag_present_flag {
        b.u1()?;
    }
    if s.separate_colour_plane_flag {
        b.u(2)?;
    }
    b.u(s.log2_max_pic_order_cnt_lsb_minus4 + 4)?;
    if b.u1()? != 0 {
        return Ok(0);
    }
    let start = b.bit_pos();
    ShortTermRefPicSet::parse_slice_inline(&mut b, s)?;
    Ok((b.bit_pos() - start).try_into()?)
}
fn range_parameters(
    base: HevcParams,
    s: &SeqParameterSet,
    p: &PicParameterSet,
) -> Result<HevcRangeParams> {
    let mut out = HevcRangeParams {
        base,
        ..Default::default()
    };
    if let Some(s) = &s.sps_range_extension {
        out.flags = (s.transform_skip_rotation_enabled_flag as u16)
            | (s.transform_skip_context_enabled_flag as u16) << 1
            | (s.implicit_rdpcm_enabled_flag as u16) << 2
            | (s.explicit_rdpcm_enabled_flag as u16) << 3
            | (s.extended_precision_processing_flag as u16) << 4
            | (s.intra_smoothing_disabled_flag as u16) << 5
            | (s.persistent_rice_adaptation_enabled_flag as u16) << 6
            | (s.high_precision_offsets_enabled_flag as u16) << 7
            | (s.cabac_bypass_alignment_enabled_flag as u16) << 8;
    }
    if let Some(p) = &p.pps_range_extension {
        out.flags |= (p.cross_component_prediction_enabled_flag as u16) << 9
            | (p.chroma_qp_offset_list_enabled_flag as u16) << 10;
        out.diff_cu_chroma_qp_offset_depth = p.diff_cu_chroma_qp_offset_depth.try_into()?;
        out.log2_sao_offset_scale_luma = p.log2_sao_offset_scale_luma.try_into()?;
        out.log2_sao_offset_scale_chroma = p.log2_sao_offset_scale_chroma.try_into()?;
        out.log2_max_transform_skip_block_size_minus2 =
            p.log2_max_transform_skip_block_size_minus2.try_into()?;
        out.chroma_qp_offset_list_len_minus1 = p.chroma_qp_offset_list_len_minus1.try_into()?;
        ensure!(
            p.chroma_qp_offset_list.len() <= 6,
            "invalid HEVC chroma QP list"
        );
        for (index, entry) in p.chroma_qp_offset_list.iter().enumerate() {
            out.cb_qp_offset_list[index] = entry.cb_qp_offset;
            out.cr_qp_offset_list[index] = entry.cr_qp_offset;
        }
    }
    Ok(out)
}

// Reserved SDK fields stay zero while independently specified flags are packed.
#[allow(clippy::field_reassign_with_default)]
fn parameters(
    s: &SeqParameterSet,
    p: &PicParameterSet,
    kind: u8,
    poc: i32,
    report: u32,
    index: usize,
) -> Result<HevcParams> {
    let mut v = HevcParams::default();
    v.PicWidthInMinCbsY =
        (s.pic_width_in_luma_samples >> (s.log2_min_luma_coding_block_size_minus3 + 3)) as u16;
    v.PicHeightInMinCbsY =
        (s.pic_height_in_luma_samples >> (s.log2_min_luma_coding_block_size_minus3 + 3)) as u16;
    v.wFormatAndSequenceInfoFlags = s.chroma_format_idc as u16
        | (s.bit_depth_luma_minus8 as u16) << 3
        | (s.bit_depth_chroma_minus8 as u16) << 6
        | (s.log2_max_pic_order_cnt_lsb_minus4 as u16) << 9;
    v.CurrPic = index as u8;
    v.sps_max_dec_pic_buffering_minus1 = s.sub_layer_ordering_info[s.max_sub_layers_minus1 as usize]
        .max_dec_pic_buffering_minus1 as u8;
    v.log2_min_luma_coding_block_size_minus3 = s.log2_min_luma_coding_block_size_minus3;
    v.log2_diff_max_min_luma_coding_block_size = s.log2_diff_max_min_luma_coding_block_size;
    v.log2_min_transform_block_size_minus2 = s.log2_min_luma_transform_block_size_minus2;
    v.log2_diff_max_min_transform_block_size = s.log2_diff_max_min_luma_transform_block_size;
    v.max_transform_hierarchy_depth_inter = s.max_transform_hierarchy_depth_inter;
    v.max_transform_hierarchy_depth_intra = s.max_transform_hierarchy_depth_intra;
    v.num_short_term_ref_pic_sets = s.num_short_term_ref_pic_sets as u8;
    v.num_long_term_ref_pics_sps = s.num_long_term_ref_pics_sps as u8;
    v.num_ref_idx_l0_default_active_minus1 = p.num_ref_idx_l0_default_active_minus1;
    v.num_ref_idx_l1_default_active_minus1 = p.num_ref_idx_l1_default_active_minus1;
    v.init_qp_minus26 = p.init_qp_minus26 as i8;
    v.dwCodingParamToolFlags = s.scaling_list_enabled_flag as u32
        | (s.amp_enabled_flag as u32) << 1
        | (s.sample_adaptive_offset_enabled_flag as u32) << 2
        | (s.pcm_enabled_flag as u32) << 3
        | (s.long_term_ref_pics_present_flag as u32) << 17
        | (s.sps_temporal_mvp_enabled_flag as u32) << 18
        | (s.strong_intra_smoothing_enabled_flag as u32) << 19
        | (p.dependent_slice_segments_enabled_flag as u32) << 20
        | (p.output_flag_present_flag as u32) << 21
        | (p.num_extra_slice_header_bits as u32) << 22
        | (p.sign_data_hiding_enabled_flag as u32) << 25
        | (p.cabac_init_present_flag as u32) << 26;
    if let Some(pcm) = &s.pcm {
        v.dwCodingParamToolFlags |= (pcm.bit_depth_luma_minus1 as u32) << 4
            | (pcm.bit_depth_chroma_minus1 as u32) << 8
            | (pcm.log2_min_pcm_luma_coding_block_size_minus3 as u32) << 12
            | (pcm.log2_diff_max_min_pcm_luma_coding_block_size as u32) << 14
            | (pcm.loop_filter_disabled_flag as u32) << 16;
    }
    let flags = [
        p.constrained_intra_pred_flag,
        p.transform_skip_enabled_flag,
        p.cu_qp_delta_enabled_flag,
        p.pps_slice_chroma_qp_offsets_present_flag,
        p.weighted_pred_flag,
        p.weighted_bipred_flag,
        p.transquant_bypass_enabled_flag,
        p.tiles_enabled_flag,
        p.entropy_coding_sync_enabled_flag,
        p.tiles.uniform_spacing_flag,
        p.tiles_enabled_flag && p.loop_filter_across_tiles_enabled_flag,
        p.pps_loop_filter_across_slices_enabled_flag,
        p.deblocking.override_enabled_flag,
        p.deblocking.disabled_flag,
        p.lists_modification_present_flag,
        p.slice_segment_header_extension_present_flag,
        (16..=23).contains(&kind),
        kind == 19 || kind == 20,
        (16..=23).contains(&kind),
    ];
    for (i, f) in flags.iter().enumerate() {
        v.dwCodingSettingPicturePropertyFlags |= (*f as u32) << i;
    }
    v.pps_cb_qp_offset = p.pps_cb_qp_offset;
    v.pps_cr_qp_offset = p.pps_cr_qp_offset;
    if p.tiles_enabled_flag {
        ensure!(
            p.tiles.num_tile_columns_minus1 <= 19 && p.tiles.num_tile_rows_minus1 <= 21,
            "tile layout exceeds DXVA limits"
        );
        v.num_tile_columns_minus1 = p.tiles.num_tile_columns_minus1 as u8;
        v.num_tile_rows_minus1 = p.tiles.num_tile_rows_minus1 as u8;
        for (i, x) in p.tiles.column_width_minus1.iter().enumerate() {
            v.column_width_minus1[i] = (*x).try_into()?;
        }
        for (i, x) in p.tiles.row_height_minus1.iter().enumerate() {
            v.row_height_minus1[i] = (*x).try_into()?;
        }
    }
    v.diff_cu_qp_delta_depth = p.diff_cu_qp_delta_depth as u8;
    v.pps_beta_offset_div2 = p.deblocking.beta_offset_div2;
    v.pps_tc_offset_div2 = p.deblocking.tc_offset_div2;
    v.log2_parallel_merge_level_minus2 = p.log2_parallel_merge_level_minus2 as u8;
    v.CurrPicOrderCntVal = poc;
    v.RefPicList = [255; 15];
    v.StatusReportFeedbackNumber = report;
    Ok(v)
}
fn matrix(s: &SeqParameterSet, p: &PicParameterSet) -> Vec<u8> {
    if !s.scaling_list_enabled_flag {
        return vec![];
    }
    let fallback = ScalingListData::all_default();
    let m = p
        .scaling_list_data
        .as_ref()
        .or(s.scaling_list_data.as_ref())
        .unwrap_or(&fallback);
    let mut out = vec![];
    for size in 0..4 {
        for matrix in if size == 3 {
            vec![0, 3]
        } else {
            (0..6).collect()
        } {
            out.extend(m.lists[size][matrix].coef.iter().map(|x| *x as u8));
        }
    }
    for i in 0..6 {
        out.push(m.lists[2][i].dc_coef as u8);
    }
    for i in [0, 3] {
        out.push(m.lists[3][i].dc_coef as u8);
    }
    out
}
