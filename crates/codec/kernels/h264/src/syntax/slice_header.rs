// SPDX-License-Identifier: MIT
// Derived from oxideav-h264 0.1.8, Copyright (c) 2026 Karpelès Lab Inc.
// Retained syntax/metadata primitives used by this client; see COPYING.OxideAV in this crate.

use crate::syntax::bitstream::{BitError, BitReader};

use crate::syntax::nal::{NalHeader, NalUnitType};

use crate::syntax::pps::Pps;

use crate::syntax::sps::Sps;

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum SliceHeaderError {
    #[error("bitstream read failed: {0}")]
    Bitstream(#[from] BitError),
    #[error("slice_type out of range (got {0}, max 9)")]
    SliceTypeOutOfRange(u32),
    #[error("pic_parameter_set_id out of range (got {0}, max 255)")]
    PpsIdOutOfRange(u32),
    #[error("IDR slice must have slice_type I or SI (got raw {0})")]
    IdrSliceTypeNotI(u32),
    #[error("colour_plane_id out of range (got {0}, max 2)")]
    ColourPlaneIdOutOfRange(u32),
    #[error("idr_pic_id out of range (got {0}, max 65535)")]
    IdrPicIdOutOfRange(u32),
    #[error("redundant_pic_cnt out of range (got {0}, max 127)")]
    RedundantPicCntOutOfRange(u32),
    #[error("cabac_init_idc out of range (got {0}, max 2)")]
    CabacInitIdcOutOfRange(u32),
    #[error("disable_deblocking_filter_idc out of range (got {0}, max 2)")]
    DisableDeblockingFilterIdcOutOfRange(u32),
    #[error("slice_alpha_c0_offset_div2 out of range (got {0}, must be -6..=6)")]
    SliceAlphaC0OffsetDiv2OutOfRange(i32),
    #[error("slice_beta_offset_div2 out of range (got {0}, must be -6..=6)")]
    SliceBetaOffsetDiv2OutOfRange(i32),
    #[error("luma_log2_weight_denom out of range (got {0}, max 7)")]
    LumaLog2WeightDenomOutOfRange(u32),
    #[error("chroma_log2_weight_denom out of range (got {0}, max 7)")]
    ChromaLog2WeightDenomOutOfRange(u32),
    #[error("memory_management_control_operation out of range (got {0}, max 6)")]
    MmcoOutOfRange(u32),
    #[error("modification_of_pic_nums_idc out of range (got {0}, max 3)")]
    ModOfPicNumsIdcOutOfRange(u32),
    #[error("referenced PPS id {0} not available")]
    PpsLookupFailed(u32),
    #[error("slice layer extension (nal_unit_type {0}) not supported")]
    SliceExtensionNotSupported(u8),
    #[error("num_ref_idx_lX_active_minus1 out of range (got {0}, max 63)")]
    NumRefIdxActiveMinus1OutOfRange(u32),
    #[error(
        "first_mb_in_slice out of range (got {got}, max {max} for PicSizeInMbs={pic_size_in_mbs}, mbaff={mbaff})"
    )]
    FirstMbInSliceOutOfRange {
        got: u32,
        max: u32,
        pic_size_in_mbs: u32,
        mbaff: bool,
    },
}

#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum SliceType {
    P,
    B,
    I,
    SP,
    SI,
}

impl SliceType {
    pub fn all_in_picture(slice_type_raw: u32) -> bool {
        slice_type_raw >= 5
    }

    pub fn from_raw(slice_type_raw: u32) -> Result<Self, SliceHeaderError> {
        Ok(match slice_type_raw % 5 {
            0 => SliceType::P,
            1 => SliceType::B,
            2 => SliceType::I,
            3 => SliceType::SP,
            4 => SliceType::SI,
            _ => return Err(SliceHeaderError::SliceTypeOutOfRange(slice_type_raw)),
        })
    }

    pub fn has_list_0(self) -> bool {
        matches!(self, SliceType::P | SliceType::SP | SliceType::B)
    }

    pub fn has_list_1(self) -> bool {
        matches!(self, SliceType::B)
    }

    pub fn is_intra(self) -> bool {
        matches!(self, SliceType::I | SliceType::SI)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RefPicListModificationOp {
    Subtract(u32),
    Add(u32),
    LongTerm(u32),
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct RefPicListModification {
    pub modifications_l0: Vec<RefPicListModificationOp>,
    pub modifications_l1: Vec<RefPicListModificationOp>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PredWeightTable {
    pub luma_log2_weight_denom: u32,
    pub chroma_log2_weight_denom: u32,
    pub luma_weights_l0: Vec<Option<(i32, i32)>>,
    pub chroma_weights_l0: Vec<Option<[(i32, i32); 2]>>,
    pub luma_weights_l1: Vec<Option<(i32, i32)>>,
    pub chroma_weights_l1: Vec<Option<[(i32, i32); 2]>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MmcoOp {
    MarkShortTermUnused(u32),
    MarkLongTermUnused(u32),
    AssignLongTerm(u32, u32),
    SetMaxLongTermIdx(u32),
    MarkAllUnused,
    AssignCurrentLongTerm(u32),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DecRefPicMarking {
    pub no_output_of_prior_pics_flag: bool,
    pub long_term_reference_flag: bool,
    pub adaptive_marking: Option<Vec<MmcoOp>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SliceHeader {
    pub first_mb_in_slice: u32,
    pub slice_type_raw: u32,
    pub slice_type: SliceType,
    pub all_slices_same_type: bool,
    pub pic_parameter_set_id: u32,
    pub colour_plane_id: u8,
    pub frame_num: u32,
    pub field_pic_flag: bool,
    pub bottom_field_flag: bool,
    pub idr_pic_id: u32,
    pub pic_order_cnt_lsb: u32,
    pub delta_pic_order_cnt_bottom: i32,
    pub delta_pic_order_cnt: [i32; 2],
    pub redundant_pic_cnt: u32,
    pub direct_spatial_mv_pred_flag: bool,
    pub num_ref_idx_active_override_flag: bool,
    pub num_ref_idx_l0_active_minus1: u32,
    pub num_ref_idx_l1_active_minus1: u32,
    pub ref_pic_list_modification: RefPicListModification,
    pub pred_weight_table: Option<PredWeightTable>,
    pub dec_ref_pic_marking: Option<DecRefPicMarking>,
    pub cabac_init_idc: u32,
    pub slice_qp_delta: i32,
    pub sp_for_switch_flag: bool,
    pub slice_qs_delta: i32,
    pub disable_deblocking_filter_idc: u32,
    pub slice_alpha_c0_offset_div2: i32,
    pub slice_beta_offset_div2: i32,
    pub slice_group_change_cycle: u32,
}

impl SliceHeader {
    pub fn parse(
        rbsp: &[u8],
        sps: &Sps,
        pps: &Pps,
        nal_header: &NalHeader,
    ) -> Result<Self, SliceHeaderError> {
        Self::parse_and_tell(rbsp, sps, pps, nal_header).map(|(h, _)| h)
    }

    pub fn parse_and_tell(
        rbsp: &[u8],
        sps: &Sps,
        pps: &Pps,
        nal_header: &NalHeader,
    ) -> Result<(Self, (usize, u8)), SliceHeaderError> {
        // Annex F/G/J — MVC/SVC/3D-AVC slice extensions use a
        // different sub-header prefix and derive IdrPicFlag from the
        // NAL-unit-header extension's `idr_flag` / `non_idr_flag`
        // rather than from the NAL unit type. Those are routed through
        // [`Self::parse_mvc_extension_and_tell`]; the base entry point
        // rejects them so a caller can't accidentally parse a NAL 20/21
        // body as a §7.3.3 `slice_header()`.
        let nut = nal_header.nal_unit_type;
        if matches!(
            nut,
            NalUnitType::SliceExtension | NalUnitType::SliceExtensionDepth
        ) {
            return Err(SliceHeaderError::SliceExtensionNotSupported(nut.as_u8()));
        }
        // §7.4.3 — for NAL unit types 1..=5, IdrPicFlag = (type == 5).
        Self::parse_and_tell_inner(rbsp, sps, pps, nal_header, nut.is_idr())
    }

    fn parse_and_tell_inner(
        rbsp: &[u8],
        sps: &Sps,
        pps: &Pps,
        nal_header: &NalHeader,
        idr_pic_flag: bool,
    ) -> Result<(Self, (usize, u8)), SliceHeaderError> {
        let mut r = BitReader::new(rbsp);

        // §7.3.3 — first_mb_in_slice ue(v), slice_type ue(v), pic_parameter_set_id ue(v).
        let first_mb_in_slice = r.ue()?;
        let slice_type_raw = r.ue()?;
        if slice_type_raw > 9 {
            return Err(SliceHeaderError::SliceTypeOutOfRange(slice_type_raw));
        }
        let slice_type = SliceType::from_raw(slice_type_raw)?;
        let all_slices_same_type = SliceType::all_in_picture(slice_type_raw);

        // §7.4.3 — on IDR (nal_unit_type == 5) slice_type must be in
        // {2,4,7,9} (I / SI).
        if idr_pic_flag && !slice_type.is_intra() {
            return Err(SliceHeaderError::IdrSliceTypeNotI(slice_type_raw));
        }

        let pic_parameter_set_id = r.ue()?;
        if pic_parameter_set_id > 255 {
            return Err(SliceHeaderError::PpsIdOutOfRange(pic_parameter_set_id));
        }

        // §7.3.3 — colour_plane_id u(2) when separate_colour_plane_flag == 1.
        let colour_plane_id = if sps.separate_colour_plane_flag {
            let v = r.u(2)?;
            if v > 2 {
                return Err(SliceHeaderError::ColourPlaneIdOutOfRange(v));
            }
            v as u8
        } else {
            0
        };

        // §7.4.3 — frame_num u(v), v = log2_max_frame_num_minus4 + 4.
        let frame_num = r.u(sps.log2_max_frame_num_minus4 + 4)?;

        // §7.3.3 — field_pic_flag / bottom_field_flag only when
        // !frame_mbs_only_flag.
        let (field_pic_flag, bottom_field_flag) = if !sps.frame_mbs_only_flag {
            let fpf = r.u(1)? == 1;
            let bff = if fpf { r.u(1)? == 1 } else { false };
            (fpf, bff)
        } else {
            (false, false)
        };

        // §7.4.3 — `first_mb_in_slice` shall be in:
        //   * `0 .. PicSizeInMbs - 1`        when MbaffFrameFlag == 0
        //   * `0 .. PicSizeInMbs/2 - 1`      when MbaffFrameFlag == 1
        // (MBAFF: the parsed value is in macroblock-pair units; the
        // underlying raw MB address is `first_mb_in_slice * 2`.)
        // Without this check, downstream `mb_sample_origin` in §6.4.1
        // computes `(mb_addr / PicWidthInMbs) * 16` as i32 and an
        // attacker-supplied multi-million MB address overflows the
        // multiply. PicSizeInMbs derived per eq. (7-31) = PicWidthInMbs
        // * PicHeightInMbs, with PicHeightInMbs = FrameHeightInMbs /
        // (1 + field_pic_flag) per eq. (7-28).
        let mbaff_frame_flag = sps.mb_adaptive_frame_field_flag && !field_pic_flag;
        let pic_height_in_mbs = sps.frame_height_in_mbs() / (1 + u32::from(field_pic_flag));
        let pic_size_in_mbs = sps.pic_width_in_mbs() * pic_height_in_mbs;
        let max_first_mb = if mbaff_frame_flag {
            pic_size_in_mbs / 2
        } else {
            pic_size_in_mbs
        }
        .saturating_sub(1);
        if first_mb_in_slice > max_first_mb {
            return Err(SliceHeaderError::FirstMbInSliceOutOfRange {
                got: first_mb_in_slice,
                max: max_first_mb,
                pic_size_in_mbs,
                mbaff: mbaff_frame_flag,
            });
        }

        // §7.3.3 — idr_pic_id ue(v) when IdrPicFlag.
        let idr_pic_id = if idr_pic_flag {
            let v = r.ue()?;
            if v > 65535 {
                return Err(SliceHeaderError::IdrPicIdOutOfRange(v));
            }
            v
        } else {
            0
        };

        // §7.3.3 — POC type 0 fields.
        let mut pic_order_cnt_lsb: u32 = 0;
        let mut delta_pic_order_cnt_bottom: i32 = 0;
        // §7.3.3 — POC type 1 fields (present only when !delta_pic_order_always_zero_flag).
        let mut delta_pic_order_cnt = [0i32; 2];

        if sps.pic_order_cnt_type == 0 {
            // u(v), v = log2_max_pic_order_cnt_lsb_minus4 + 4.
            pic_order_cnt_lsb = r.u(sps.log2_max_pic_order_cnt_lsb_minus4 + 4)?;
            if pps.bottom_field_pic_order_in_frame_present_flag && !field_pic_flag {
                delta_pic_order_cnt_bottom = r.se()?;
            }
        } else if sps.pic_order_cnt_type == 1 && !sps.delta_pic_order_always_zero_flag {
            delta_pic_order_cnt[0] = r.se()?;
            if pps.bottom_field_pic_order_in_frame_present_flag && !field_pic_flag {
                delta_pic_order_cnt[1] = r.se()?;
            }
        }
        // §7.3.3 — POC type 2 has no per-slice POC fields.

        // §7.3.3 — redundant_pic_cnt under redundant_pic_cnt_present_flag.
        let redundant_pic_cnt = if pps.redundant_pic_cnt_present_flag {
            let v = r.ue()?;
            if v > 127 {
                return Err(SliceHeaderError::RedundantPicCntOutOfRange(v));
            }
            v
        } else {
            0
        };

        // §7.3.3 — direct_spatial_mv_pred_flag for B slices.
        let direct_spatial_mv_pred_flag = if matches!(slice_type, SliceType::B) {
            r.u(1)? == 1
        } else {
            false
        };

        // §7.3.3 — num_ref_idx_active_override_flag group for P/SP/B slices.
        let mut num_ref_idx_active_override_flag = false;
        let mut num_ref_idx_l0_active_minus1 = pps.num_ref_idx_l0_default_active_minus1;
        let mut num_ref_idx_l1_active_minus1 = pps.num_ref_idx_l1_default_active_minus1;
        if matches!(slice_type, SliceType::P | SliceType::SP | SliceType::B) {
            num_ref_idx_active_override_flag = r.u(1)? == 1;
            if num_ref_idx_active_override_flag {
                num_ref_idx_l0_active_minus1 = r.ue()?;
                if num_ref_idx_l0_active_minus1 > 63 {
                    return Err(SliceHeaderError::NumRefIdxActiveMinus1OutOfRange(
                        num_ref_idx_l0_active_minus1,
                    ));
                }
                if matches!(slice_type, SliceType::B) {
                    num_ref_idx_l1_active_minus1 = r.ue()?;
                    if num_ref_idx_l1_active_minus1 > 63 {
                        return Err(SliceHeaderError::NumRefIdxActiveMinus1OutOfRange(
                            num_ref_idx_l1_active_minus1,
                        ));
                    }
                }
            }
        }
        // Defensive: if the override flag wasn't set, the values come
        // from the active PPS — but the PPS parser doesn't bound them
        // either (§7.4.2.2 only requires 0..=31). A malformed PPS
        // followed by a slice that inherits its defaults could feed
        // huge values into the same allocation paths. Re-check here so
        // every slice header that reaches the weight-table / ref-list
        // code is provably bounded.
        if num_ref_idx_l0_active_minus1 > 63 {
            return Err(SliceHeaderError::NumRefIdxActiveMinus1OutOfRange(
                num_ref_idx_l0_active_minus1,
            ));
        }
        if num_ref_idx_l1_active_minus1 > 63 {
            return Err(SliceHeaderError::NumRefIdxActiveMinus1OutOfRange(
                num_ref_idx_l1_active_minus1,
            ));
        }

        // §7.3.3 — ref_pic_list_modification() (or ref_pic_list_mvc_modification()
        // for nal_unit_type 20/21, which we rejected above).
        let ref_pic_list_modification = parse_ref_pic_list_modification(&mut r, &slice_type)?;

        // §7.3.3 — pred_weight_table() when explicit weighted pred is active.
        let use_pred_weight = (pps.weighted_pred_flag
            && matches!(slice_type, SliceType::P | SliceType::SP))
            || (pps.weighted_bipred_idc == 1 && matches!(slice_type, SliceType::B));
        let pred_weight_table = if use_pred_weight {
            Some(parse_pred_weight_table(
                &mut r,
                &slice_type,
                num_ref_idx_l0_active_minus1,
                num_ref_idx_l1_active_minus1,
                sps.chroma_array_type(),
            )?)
        } else {
            None
        };

        // §7.3.3 — dec_ref_pic_marking() when nal_ref_idc != 0.
        let dec_ref_pic_marking = if nal_header.nal_ref_idc != 0 {
            Some(parse_dec_ref_pic_marking(&mut r, idr_pic_flag)?)
        } else {
            None
        };

        // §7.3.3 — cabac_init_idc when CABAC and slice is not I/SI.
        let cabac_init_idc = if pps.entropy_coding_mode_flag && !slice_type.is_intra() {
            let v = r.ue()?;
            if v > 2 {
                return Err(SliceHeaderError::CabacInitIdcOutOfRange(v));
            }
            v
        } else {
            0
        };

        // §7.3.3 — slice_qp_delta se(v).
        let slice_qp_delta = r.se()?;

        // §7.3.3 — SP / SI fields.
        let mut sp_for_switch_flag = false;
        let mut slice_qs_delta = 0i32;
        if matches!(slice_type, SliceType::SP | SliceType::SI) {
            if matches!(slice_type, SliceType::SP) {
                sp_for_switch_flag = r.u(1)? == 1;
            }
            slice_qs_delta = r.se()?;
        }

        // §7.3.3 — deblocking filter group under
        // deblocking_filter_control_present_flag.
        let mut disable_deblocking_filter_idc: u32 = 0;
        let mut slice_alpha_c0_offset_div2: i32 = 0;
        let mut slice_beta_offset_div2: i32 = 0;
        if pps.deblocking_filter_control_present_flag {
            disable_deblocking_filter_idc = r.ue()?;
            if disable_deblocking_filter_idc > 2 {
                return Err(SliceHeaderError::DisableDeblockingFilterIdcOutOfRange(
                    disable_deblocking_filter_idc,
                ));
            }
            if disable_deblocking_filter_idc != 1 {
                slice_alpha_c0_offset_div2 = r.se()?;
                if !(-6..=6).contains(&slice_alpha_c0_offset_div2) {
                    return Err(SliceHeaderError::SliceAlphaC0OffsetDiv2OutOfRange(
                        slice_alpha_c0_offset_div2,
                    ));
                }
                slice_beta_offset_div2 = r.se()?;
                if !(-6..=6).contains(&slice_beta_offset_div2) {
                    return Err(SliceHeaderError::SliceBetaOffsetDiv2OutOfRange(
                        slice_beta_offset_div2,
                    ));
                }
            }
        }

        // §7.3.3 — slice_group_change_cycle u(v) when FMO map types 3..=5.
        let mut slice_group_change_cycle: u32 = 0;
        if pps.num_slice_groups_minus1 > 0 && is_changing_map_type(pps) {
            // §7.4.3 eq. (7-37): bits = Ceil(Log2(PicSizeInMapUnits /
            // SliceGroupChangeRate + 1)).
            let pic_size_in_map_units = sps.pic_width_in_mbs() * sps.pic_height_in_map_units();
            let change_rate = slice_group_change_rate(pps).max(1);
            // Division is integer division, matching the spec's "/".
            let bits = ceil_log2(pic_size_in_map_units / change_rate + 1);
            slice_group_change_cycle = if bits == 0 { 0 } else { r.u(bits)? };
        }

        let cursor = r.position();
        Ok((
            SliceHeader {
                first_mb_in_slice,
                slice_type_raw,
                slice_type,
                all_slices_same_type,
                pic_parameter_set_id,
                colour_plane_id,
                frame_num,
                field_pic_flag,
                bottom_field_flag,
                idr_pic_id,
                pic_order_cnt_lsb,
                delta_pic_order_cnt_bottom,
                delta_pic_order_cnt,
                redundant_pic_cnt,
                direct_spatial_mv_pred_flag,
                num_ref_idx_active_override_flag,
                num_ref_idx_l0_active_minus1,
                num_ref_idx_l1_active_minus1,
                ref_pic_list_modification,
                pred_weight_table,
                dec_ref_pic_marking,
                cabac_init_idc,
                slice_qp_delta,
                sp_for_switch_flag,
                slice_qs_delta,
                disable_deblocking_filter_idc,
                slice_alpha_c0_offset_div2,
                slice_beta_offset_div2,
                slice_group_change_cycle,
            },
            cursor,
        ))
    }

    pub fn mbaff_frame_flag(&self, sps: &Sps) -> bool {
        sps.mb_adaptive_frame_field_flag && !self.field_pic_flag
    }
}

fn parse_ref_pic_list_modification(
    r: &mut BitReader<'_>,
    slice_type: &SliceType,
) -> Result<RefPicListModification, SliceHeaderError> {
    let mut result = RefPicListModification::default();
    // §7.3.3.1 — list 0 loop when slice_type % 5 != 2 && != 4
    // (i.e. slice has list 0 prediction: P, SP, B).
    if slice_type.has_list_0() {
        let flag_l0 = r.u(1)? == 1;
        if flag_l0 {
            result.modifications_l0 = parse_mod_loop(r)?;
        }
    }
    // §7.3.3.1 — list 1 loop for B slices (slice_type % 5 == 1).
    if slice_type.has_list_1() {
        let flag_l1 = r.u(1)? == 1;
        if flag_l1 {
            result.modifications_l1 = parse_mod_loop(r)?;
        }
    }
    Ok(result)
}

fn parse_mod_loop(
    r: &mut BitReader<'_>,
) -> Result<Vec<RefPicListModificationOp>, SliceHeaderError> {
    let mut ops = Vec::new();
    loop {
        let idc = r.ue()?;
        match idc {
            0 => {
                let abs_diff = r.ue()?;
                ops.push(RefPicListModificationOp::Subtract(abs_diff));
            }
            1 => {
                let abs_diff = r.ue()?;
                ops.push(RefPicListModificationOp::Add(abs_diff));
            }
            2 => {
                let long_term_pic_num = r.ue()?;
                ops.push(RefPicListModificationOp::LongTerm(long_term_pic_num));
            }
            3 => break,
            _ => return Err(SliceHeaderError::ModOfPicNumsIdcOutOfRange(idc)),
        }
    }
    Ok(ops)
}

fn parse_pred_weight_table(
    r: &mut BitReader<'_>,
    slice_type: &SliceType,
    num_ref_idx_l0_active_minus1: u32,
    num_ref_idx_l1_active_minus1: u32,
    chroma_array_type: u32,
) -> Result<PredWeightTable, SliceHeaderError> {
    let luma_log2_weight_denom = r.ue()?;
    if luma_log2_weight_denom > 7 {
        return Err(SliceHeaderError::LumaLog2WeightDenomOutOfRange(
            luma_log2_weight_denom,
        ));
    }
    let chroma_log2_weight_denom = if chroma_array_type != 0 {
        let v = r.ue()?;
        if v > 7 {
            return Err(SliceHeaderError::ChromaLog2WeightDenomOutOfRange(v));
        }
        v
    } else {
        0
    };

    let (luma_weights_l0, chroma_weights_l0) =
        parse_weight_list(r, num_ref_idx_l0_active_minus1, chroma_array_type)?;

    // §7.3.3.2 — list 1 branch only for B slices (slice_type % 5 == 1).
    let (luma_weights_l1, chroma_weights_l1) = if slice_type.has_list_1() {
        parse_weight_list(r, num_ref_idx_l1_active_minus1, chroma_array_type)?
    } else {
        (Vec::new(), Vec::new())
    };

    Ok(PredWeightTable {
        luma_log2_weight_denom,
        chroma_log2_weight_denom,
        luma_weights_l0,
        chroma_weights_l0,
        luma_weights_l1,
        chroma_weights_l1,
    })
}

#[allow(clippy::type_complexity)]
fn parse_weight_list(
    r: &mut BitReader<'_>,
    num_ref_idx_minus1: u32,
    chroma_array_type: u32,
) -> Result<(Vec<Option<(i32, i32)>>, Vec<Option<[(i32, i32); 2]>>), SliceHeaderError> {
    let count = num_ref_idx_minus1 as usize + 1;
    let mut luma = Vec::with_capacity(count);
    let mut chroma = Vec::with_capacity(if chroma_array_type != 0 { count } else { 0 });
    for _ in 0..count {
        let luma_flag = r.u(1)? == 1;
        luma.push(if luma_flag {
            let w = r.se()?;
            let o = r.se()?;
            Some((w, o))
        } else {
            None
        });
        if chroma_array_type != 0 {
            let chroma_flag = r.u(1)? == 1;
            if chroma_flag {
                let wcb = r.se()?;
                let ocb = r.se()?;
                let wcr = r.se()?;
                let ocr = r.se()?;
                chroma.push(Some([(wcb, ocb), (wcr, ocr)]));
            } else {
                chroma.push(None);
            }
        }
    }
    Ok((luma, chroma))
}

fn parse_dec_ref_pic_marking(
    r: &mut BitReader<'_>,
    idr_pic_flag: bool,
) -> Result<DecRefPicMarking, SliceHeaderError> {
    if idr_pic_flag {
        let no_output_of_prior_pics_flag = r.u(1)? == 1;
        let long_term_reference_flag = r.u(1)? == 1;
        Ok(DecRefPicMarking {
            no_output_of_prior_pics_flag,
            long_term_reference_flag,
            adaptive_marking: None,
        })
    } else {
        let adaptive = r.u(1)? == 1;
        let adaptive_marking = if adaptive {
            let mut ops = Vec::new();
            loop {
                // §7.3.3.3 — memory_management_control_operation ue(v).
                let mmco = r.ue()?;
                match mmco {
                    0 => break,
                    1 => {
                        let diff = r.ue()?;
                        ops.push(MmcoOp::MarkShortTermUnused(diff));
                    }
                    2 => {
                        let ltpn = r.ue()?;
                        ops.push(MmcoOp::MarkLongTermUnused(ltpn));
                    }
                    3 => {
                        let diff = r.ue()?;
                        let ltfi = r.ue()?;
                        ops.push(MmcoOp::AssignLongTerm(diff, ltfi));
                    }
                    4 => {
                        let max = r.ue()?;
                        ops.push(MmcoOp::SetMaxLongTermIdx(max));
                    }
                    5 => {
                        ops.push(MmcoOp::MarkAllUnused);
                    }
                    6 => {
                        let ltfi = r.ue()?;
                        ops.push(MmcoOp::AssignCurrentLongTerm(ltfi));
                    }
                    _ => return Err(SliceHeaderError::MmcoOutOfRange(mmco)),
                }
            }
            Some(ops)
        } else {
            None
        };
        Ok(DecRefPicMarking {
            no_output_of_prior_pics_flag: false,
            long_term_reference_flag: false,
            adaptive_marking,
        })
    }
}

fn is_changing_map_type(pps: &Pps) -> bool {
    matches!(
        pps.slice_group_map,
        Some(crate::syntax::pps::SliceGroupMap::Changing { .. })
    )
}

fn slice_group_change_rate(pps: &Pps) -> u32 {
    match &pps.slice_group_map {
        Some(crate::syntax::pps::SliceGroupMap::Changing {
            change_rate_minus1, ..
        }) => *change_rate_minus1 + 1,
        _ => 1,
    }
}

fn ceil_log2(n: u32) -> u32 {
    if n <= 1 {
        0
    } else {
        32 - (n - 1).leading_zeros()
    }
}
