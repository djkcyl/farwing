// SPDX-License-Identifier: LGPL-2.1-or-later
//! Progressive H.264 DXVA picture preparation; Rust parser and DPB metadata only.
use super::dxva::{Codec, Failure, Lease, Picture, Pool};
use anyhow::{Result, ensure};
use farwing_codec::syntax::h264::{
    poc::*,
    pps::Pps,
    ref_list::{DpbEntry, MmcoOp, PicStructure, RefMarking, perform_marking},
    slice_header::{RefPicListModificationOp, SliceHeader, SliceType},
    sps::Sps,
    transform::{select_scaling_list_4x4, select_scaling_list_8x8},
};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use windows::Win32::Graphics::Direct3D11::ID3D11Device;
#[repr(C, packed)]
#[allow(non_snake_case)]
struct Params {
    wFrameWidthInMbsMinus1: u16,
    wFrameHeightInMbsMinus1: u16,
    CurrPic: u8,
    num_ref_frames: u8,
    wBitFields: u16,
    bit_depth_luma_minus8: u8,
    bit_depth_chroma_minus8: u8,
    Reserved16Bits: u16,
    StatusReportFeedbackNumber: u32,
    RefFrameList: [u8; 16],
    CurrFieldOrderCnt: [i32; 2],
    FieldOrderCntList: [[i32; 2]; 16],
    pic_init_qs_minus26: i8,
    chroma_qp_index_offset: i8,
    second_chroma_qp_index_offset: i8,
    ContinuationFlag: u8,
    pic_init_qp_minus26: i8,
    num_ref_idx_l0_active_minus1: u8,
    num_ref_idx_l1_active_minus1: u8,
    Reserved8BitsA: u8,
    FrameNumList: [u16; 16],
    UsedForReferenceFlags: u32,
    NonExistingFrameFlags: u16,
    frame_num: u16,
    log2_max_frame_num_minus4: u8,
    pic_order_cnt_type: u8,
    log2_max_pic_order_cnt_lsb_minus4: u8,
    delta_pic_order_always_zero_flag: u8,
    direct_8x8_inference_flag: u8,
    entropy_coding_mode_flag: u8,
    pic_order_present_flag: u8,
    num_slice_groups_minus1: u8,
    slice_group_map_type: u8,
    deblocking_filter_control_present_flag: u8,
    redundant_pic_cnt_present_flag: u8,
    Reserved8BitsB: u8,
    slice_group_change_rate_minus1: u16,
    SliceGroupMap: [u8; 810],
}
impl Default for Params {
    fn default() -> Self {
        unsafe { std::mem::zeroed() }
    }
}
pub struct Avc {
    parser: farwing_codec::syntax::h264::headers::Headers,
    device: ID3D11Device,
    pool: Option<Arc<Pool>>,
    poc: PocState,
    refs: Vec<DpbEntry>,
    leases: Vec<Arc<Lease>>,
    mmco5: bool,
    prev_top: i32,
    report: u32,
    need_idr: bool,
    prev_ref_frame_num: Option<u32>,
}
impl Avc {
    pub fn new(device: ID3D11Device) -> Self {
        Self {
            parser: farwing_codec::syntax::h264::headers::Headers::new(),
            device,
            pool: None,
            poc: PocState::default(),
            refs: vec![],
            leases: vec![],
            mmco5: false,
            prev_top: 0,
            report: 0,
            need_idr: true,
            prev_ref_frame_num: None,
        }
    }
    pub fn reset(&mut self) {
        self.refs.clear();
        self.leases.clear();
        self.poc = PocState::default();
        self.mmco5 = false;
        self.prev_top = 0;
        self.need_idr = true;
        self.prev_ref_frame_num = None;
    }
    pub fn decode(&mut self, data: &[u8], cancel: &AtomicBool) -> Result<Picture> {
        let result = self.decode_inner(data, cancel);
        if result.is_err() {
            self.reset();
        }
        result
    }
    fn decode_inner(&mut self, data: &[u8], cancel: &AtomicBool) -> Result<Picture> {
        ensure!(!cancel.load(Ordering::Acquire), "decode cancelled");
        let mut first: Option<(u8, u8, SliceHeader, Sps, Pps)> = None;
        let mut slices = vec![];
        let mut all_intra = true;
        for nal in farwing_codec::syntax::h264::nal::AnnexBSplitter::new(data) {
            if let Some(farwing_codec::syntax::h264::headers::Slice {
                nal_unit_type,
                nal_ref_idc,
                header,
                sps,
                pps,
                ..
            }) = self.parser.process(nal)?
            {
                ensure!(
                    sps.frame_mbs_only_flag
                        && !header.field_pic_flag
                        && matches!(nal_unit_type, 1 | 5)
                        && sps.chroma_format_idc == 1
                        && sps.bit_depth_luma_minus8 == 0
                        && sps.bit_depth_chroma_minus8 == 0
                        && pps.num_slice_groups_minus1 == 0,
                    Failure::Unsupported
                );
                all_intra &= matches!(header.slice_type, SliceType::I | SliceType::SI);
                if let Some((old_kind, old_ref, old, old_sps, old_pps)) = &first {
                    ensure!(
                        old_sps == &sps
                            && old_pps == &pps
                            && (*old_kind == 5) == (nal_unit_type == 5)
                            && (*old_ref == 0) == (nal_ref_idc == 0)
                            && old.frame_num == header.frame_num
                            && old.pic_parameter_set_id == header.pic_parameter_set_id
                            && old.idr_pic_id == header.idr_pic_id
                            && old.pic_order_cnt_lsb == header.pic_order_cnt_lsb
                            && old.delta_pic_order_cnt_bottom == header.delta_pic_order_cnt_bottom
                            && old.delta_pic_order_cnt == header.delta_pic_order_cnt,
                        "AU contains different primary pictures or parameter sets"
                    );
                }
                slices.push(nal);
                if first.is_none() {
                    ensure!(
                        header.first_mb_in_slice == 0,
                        "missing first AVC slice / ASO unsupported"
                    );
                    first = Some((nal_unit_type, nal_ref_idc, header, sps, pps));
                }
            }
        }
        let (kind, ref_idc, h, s, p) = first.ok_or_else(|| anyhow::anyhow!("AU has no VCL"))?;
        let idr = kind == 5;
        ensure!(
            !self.need_idr || idr,
            "waiting for AVC IDR after start/reset/failure"
        );
        if idr {
            self.reset();
        }
        if let Some(previous) = self.prev_ref_frame_num {
            let next = (previous + 1) % (1 << (s.log2_max_frame_num_minus4 + 4));
            // A recovery frame after a loss refers only to one long-term
            // reference this decoder holds: the missing frames do not matter
            // (`docs/protocol/rfi-ltr-brief.md` §3.1).
            let long_term_only = !idr
                && h.num_ref_idx_l0_active_minus1 == 0
                && matches!(
                    h.ref_pic_list_modification.modifications_l0.as_slice(),
                    [RefPicListModificationOp::LongTerm(index)]
                        if self.refs.iter().any(|r| r.marking == RefMarking::LongTerm
                            && r.long_term_frame_idx == *index)
                );
            ensure!(
                h.frame_num == previous || h.frame_num == next || long_term_only,
                "AVC reference frame_num gap requires random access recovery"
            );
        }
        ensure!(
            s.pic_width_in_mbs_minus1 < 1024 && s.pic_height_in_map_units_minus1 < 1024,
            "unsupported AVC dimensions"
        );
        let width = (s.pic_width_in_mbs_minus1 + 1) * 16;
        let height = (s.pic_height_in_map_units_minus1 + 1) * 16;
        let crop = s.frame_cropping.as_ref();
        let (left, right, top, bottom) =
            crop.map_or((0, 0, 0, 0), |c| (c.left, c.right, c.top, c.bottom));
        ensure!(
            left.checked_add(right).is_some_and(|v| v < width / 2)
                && top.checked_add(bottom).is_some_and(|v| v < height / 2),
            "invalid AVC crop"
        );
        let count = capacity(&s)?;
        // H.264 E.2.1: infer zero for the constrained
        // profiles, otherwise use MaxDpbFrames when VUI gives no restriction.
        let inferred = if s.constraint_set_flags & 8 != 0
            && matches!(s.profile_idc, 44 | 86 | 100 | 110 | 122 | 244)
        {
            0
        } else {
            count as u32
        };
        let reorder_limit = s
            .vui
            .as_ref()
            .and_then(|v| v.bitstream_restriction.as_ref())
            .map_or(inferred, |v| v.max_num_reorder_frames);
        ensure!(
            reorder_limit <= count as u32,
            "AVC reorder bound exceeds DPB capacity"
        );
        if self
            .pool
            .as_ref()
            .is_none_or(|pool| pool.width != width || pool.height != height || pool.dpb < count)
        {
            ensure!(self.refs.is_empty(), "pool resize requires reference reset");
            self.pool = Some(
                Pool::new(self.device.clone(), Codec::H264, width, height, 8, 1, count)
                    .map_err(|e| e.context(Failure::HardwareFailure))?,
            );
        }
        let pool = self.pool.as_ref().unwrap();
        let current = Arc::new(pool.lease(false)?);
        let mut poc_state = self.poc.clone();
        let poc = derive_poc(
            &PocSps {
                pic_order_cnt_type: s.pic_order_cnt_type,
                log2_max_pic_order_cnt_lsb_minus4: s.log2_max_pic_order_cnt_lsb_minus4,
                log2_max_frame_num_minus4: s.log2_max_frame_num_minus4,
                delta_pic_order_always_zero_flag: s.delta_pic_order_always_zero_flag,
                offset_for_non_ref_pic: s.offset_for_non_ref_pic,
                offset_for_top_to_bottom_field: s.offset_for_top_to_bottom_field,
                num_ref_frames_in_pic_order_cnt_cycle: s.num_ref_frames_in_pic_order_cnt_cycle,
                offset_for_ref_frame: s.offset_for_ref_frame.clone(),
                frame_mbs_only_flag: true,
            },
            &PocSlice {
                is_reference: ref_idc != 0,
                is_idr: idr,
                frame_num: h.frame_num,
                field_pic_flag: false,
                bottom_field_flag: false,
                pic_order_cnt_lsb: h.pic_order_cnt_lsb,
                delta_pic_order_cnt_bottom: h.delta_pic_order_cnt_bottom,
                delta_pic_order_cnt: h.delta_pic_order_cnt,
                prev_had_mmco5: self.mmco5,
                prev_reference_top_foc_for_mmco5: self.prev_top,
            },
            &mut poc_state,
        )?;
        self.report = self.report.wrapping_add(1).max(1);
        let mut params = parameters(
            &s,
            &p,
            &h,
            ref_idc,
            all_intra,
            poc,
            self.report,
            current.index,
        )?;
        for (i, r) in self.refs.iter().enumerate() {
            ensure!(i < 16, "too many AVC references");
            params.RefFrameList[i] = r.dpb_key as u8
                | if r.marking == RefMarking::LongTerm {
                    128
                } else {
                    0
                };
            params.FieldOrderCntList[i] = [r.top_field_order_cnt, r.bottom_field_order_cnt];
            params.FrameNumList[i] = if r.marking == RefMarking::LongTerm {
                r.long_term_frame_idx as u16
            } else {
                r.frame_num as u16
            };
            params.UsedForReferenceFlags |= 3 << (2 * i);
        }
        let matrix = matrix(&s, &p);
        let bytes = unsafe {
            std::slice::from_raw_parts(
                (&params as *const Params).cast(),
                std::mem::size_of::<Params>(),
            )
        };
        pool.submit(&current, bytes, &matrix, &slices, cancel)
            .map_err(|e| e.context(Failure::HardwareFailure))?;
        self.poc = poc_state;
        self.mmco5 = false;
        if ref_idc != 0 {
            let mut entry = DpbEntry {
                frame_num: h.frame_num,
                top_field_order_cnt: poc.top_field_order_cnt,
                bottom_field_order_cnt: poc.bottom_field_order_cnt,
                pic_order_cnt: poc.pic_order_cnt,
                structure: PicStructure::Frame,
                marking: RefMarking::ShortTerm,
                long_term_frame_idx: 0,
                dpb_key: current.index as u32,
                field_markings: [RefMarking::ShortTerm; 2],
            };
            let marking = h
                .dec_ref_pic_marking
                .as_ref()
                .ok_or_else(|| anyhow::anyhow!("missing reference marking"))?;
            let ops = marking
                .adaptive_marking
                .as_ref()
                .map(|ops| ops.iter().map(map_mmco).collect::<Vec<_>>());
            self.mmco5 = perform_marking(
                &mut self.refs,
                &mut entry,
                s.max_num_ref_frames,
                idr,
                marking.long_term_reference_flag,
                marking.no_output_of_prior_pics_flag,
                ops.as_deref(),
                h.frame_num,
                1 << (s.log2_max_frame_num_minus4 + 4),
            );
            if self.mmco5 {
                let origin = entry.pic_order_cnt;
                entry.frame_num = 0;
                entry.top_field_order_cnt -= origin;
                entry.bottom_field_order_cnt -= origin;
                entry.pic_order_cnt = entry.top_field_order_cnt.min(entry.bottom_field_order_cnt);
            }
            self.refs.retain(|r| r.marking != RefMarking::Unused);
            self.leases
                .retain(|l| self.refs.iter().any(|r| r.dpb_key as usize == l.index));
            self.prev_top = entry.top_field_order_cnt;
            self.prev_ref_frame_num = Some(entry.frame_num);
            self.refs.push(entry);
            self.leases.push(current.clone());
        }
        self.need_idr = false;
        Ok(Picture {
            surface: current,
            left: left * 2,
            top: top * 2,
            width: width - 2 * (left + right),
            height: height - 2 * (top + bottom),
            poc: if self.mmco5 { 0 } else { poc.pic_order_cnt },
            reorder_limit,
            sequence_start: if idr {
                Some(
                    h.dec_ref_pic_marking
                        .as_ref()
                        .is_some_and(|m| m.no_output_of_prior_pics_flag),
                )
            } else if self.mmco5 {
                Some(false)
            } else {
                None
            },
            needed_for_output: true,
        })
    }
}
fn capacity(s: &Sps) -> Result<usize> {
    let mbs = match s.level_idc {
        9 | 10 => 396,
        11 if s.constraint_set_flags & 8 != 0 => 396,
        11 => 900,
        12 | 13 | 20 => 2376,
        21 => 4752,
        22 | 30 => 8100,
        31 => 18000,
        32 => 20480,
        40 | 41 => 32768,
        42 => 34816,
        50 => 110400,
        51 | 52 => 184320,
        60..=62 => 696320,
        _ => anyhow::bail!("unknown AVC level"),
    };
    ensure!(s.max_num_ref_frames <= 16, "too many references");
    let picture = (s.pic_width_in_mbs_minus1 + 1) * (s.pic_height_in_map_units_minus1 + 1);
    Ok((mbs / picture).min(16).max(s.max_num_ref_frames).max(1) as usize)
}
// Build the SDK parameter record from a zeroed layout so all reserved fields
// remain zero; explicit assignments keep the packed flags auditable.
#[allow(clippy::too_many_arguments, clippy::field_reassign_with_default)]
fn parameters(
    s: &Sps,
    p: &Pps,
    h: &SliceHeader,
    ref_idc: u8,
    intra: bool,
    poc: PocResult,
    report: u32,
    index: usize,
) -> Result<Params> {
    let ext = p.extension.as_ref();
    let mut v = Params::default();
    v.wFrameWidthInMbsMinus1 = s.pic_width_in_mbs_minus1 as u16;
    v.wFrameHeightInMbsMinus1 = s.pic_height_in_map_units_minus1 as u16;
    v.CurrPic = index as u8;
    v.num_ref_frames = s.max_num_ref_frames as u8;
    v.wBitFields = (s.chroma_format_idc as u16) << 4
        | ((ref_idc != 0) as u16) << 6
        | (p.constrained_intra_pred_flag as u16) << 7
        | (p.weighted_pred_flag as u16) << 8
        | (p.weighted_bipred_idc as u16) << 9
        | 1 << 11
        | 1 << 12
        | (ext.is_some_and(|e| e.transform_8x8_mode_flag) as u16) << 13
        | ((s.level_idc >= 31) as u16) << 14
        | (intra as u16) << 15;
    v.Reserved16Bits = 3;
    v.StatusReportFeedbackNumber = report;
    v.RefFrameList = [255; 16];
    v.CurrFieldOrderCnt = [poc.top_field_order_cnt, poc.bottom_field_order_cnt];
    v.pic_init_qs_minus26 = p.pic_init_qs_minus26 as i8;
    v.chroma_qp_index_offset = p.chroma_qp_index_offset as i8;
    v.second_chroma_qp_index_offset = ext.map_or(p.chroma_qp_index_offset, |e| {
        e.second_chroma_qp_index_offset
    }) as i8;
    v.ContinuationFlag = 1;
    v.pic_init_qp_minus26 = p.pic_init_qp_minus26 as i8;
    v.num_ref_idx_l0_active_minus1 = h.num_ref_idx_l0_active_minus1 as u8;
    v.num_ref_idx_l1_active_minus1 = h.num_ref_idx_l1_active_minus1 as u8;
    v.frame_num = h.frame_num as u16;
    v.log2_max_frame_num_minus4 = s.log2_max_frame_num_minus4 as u8;
    v.pic_order_cnt_type = s.pic_order_cnt_type as u8;
    v.log2_max_pic_order_cnt_lsb_minus4 = s.log2_max_pic_order_cnt_lsb_minus4 as u8;
    v.delta_pic_order_always_zero_flag = s.delta_pic_order_always_zero_flag as u8;
    v.direct_8x8_inference_flag = s.direct_8x8_inference_flag as u8;
    v.entropy_coding_mode_flag = p.entropy_coding_mode_flag as u8;
    v.pic_order_present_flag = p.bottom_field_pic_order_in_frame_present_flag as u8;
    v.deblocking_filter_control_present_flag = p.deblocking_filter_control_present_flag as u8;
    v.redundant_pic_cnt_present_flag = p.redundant_pic_cnt_present_flag as u8;
    Ok(v)
}
fn map_mmco(op: &farwing_codec::syntax::h264::slice_header::MmcoOp) -> MmcoOp {
    use farwing_codec::syntax::h264::slice_header::MmcoOp as S;
    match *op {
        S::MarkShortTermUnused(x) => MmcoOp::MarkShortTermUnused(x),
        S::MarkLongTermUnused(x) => MmcoOp::MarkLongTermUnused(x),
        S::AssignLongTerm(a, b) => MmcoOp::AssignLongTerm(a, b),
        S::SetMaxLongTermIdx(x) => MmcoOp::SetMaxLongTermIdx(x),
        S::MarkAllUnused => MmcoOp::MarkAllUnused,
        S::AssignCurrentLongTerm(x) => MmcoOp::AssignCurrentLongTerm(x),
    }
}
fn matrix(s: &Sps, p: &Pps) -> Vec<u8> {
    let scan4 = [0, 1, 4, 8, 5, 2, 3, 6, 9, 12, 13, 10, 7, 11, 14, 15];
    let mut scan8 = vec![];
    for sum in 0usize..15 {
        let mut diagonal: Vec<_> = (0usize..8)
            .filter_map(|x| sum.checked_sub(x).filter(|y| *y < 8).map(|y| y * 8 + x))
            .collect();
        if sum % 2 == 1 {
            diagonal.reverse()
        }
        scan8.extend(diagonal);
    }
    let mut out = vec![];
    for i in 0..6 {
        let m = select_scaling_list_4x4(i, s, p);
        out.extend(scan4.iter().map(|j| m[*j] as u8));
    }
    for i in 0..2 {
        let m = select_scaling_list_8x8(i, s, p);
        out.extend(scan8.iter().map(|j| m[*j] as u8));
    }
    out
}
