// SPDX-License-Identifier: MIT
// Derived from oxideav-h264 0.1.8, Copyright (c) 2026 Karpelès Lab Inc.
// Retained syntax/metadata primitives used by this client; see COPYING.OxideAV in this crate.

use crate::syntax::bitstream::{BitError, BitReader};

use crate::syntax::scaling_list::{ScalingListError, parse_scaling_list};

use crate::syntax::sps::ScalingListEntry;

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum PpsError {
    #[error("bitstream read failed: {0}")]
    Bitstream(#[from] BitError),
    #[error("pic_parameter_set_id out of range (got {0}, max 255)")]
    PpsIdOutOfRange(u32),
    #[error("seq_parameter_set_id out of range (got {0}, max 31)")]
    SpsIdOutOfRange(u32),
    #[error("num_slice_groups_minus1 out of range (got {0}, max 7)")]
    NumSliceGroupsOutOfRange(u32),
    #[error("slice_group_map_type out of range (got {0}, max 6)")]
    SliceGroupMapTypeOutOfRange(u32),
    #[error("weighted_bipred_idc out of range (got {0}, max 2)")]
    WeightedBipredIdcOutOfRange(u32),
    #[error("chroma_qp_index_offset out of range (got {0}, must be -12..=12)")]
    ChromaQpIndexOffsetOutOfRange(i32),
    #[error("second_chroma_qp_index_offset out of range (got {0}, must be -12..=12)")]
    SecondChromaQpIndexOffsetOutOfRange(i32),
    #[error("pic_size_in_map_units_minus1 out of range (got {0}, max {1})")]
    PicSizeInMapUnitsOutOfRange(u32, u32),
    #[error("scaling_list: {0}")]
    ScalingList(#[from] ScalingListError),
}

const MAX_PIC_SIZE_IN_MAP_UNITS_MINUS1: u32 = (1u32 << 22) - 2;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PicScalingLists {
    pub entries: Vec<ScalingListEntry>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SliceGroupMap {
    Interleaved {
        run_length_minus1: Vec<u32>,
    },
    Dispersed,
    Foreground {
        top_left: Vec<u32>,
        bottom_right: Vec<u32>,
    },
    Changing {
        slice_group_map_type: u32, // 3, 4, or 5
        change_direction_flag: bool,
        change_rate_minus1: u32,
    },
    Explicit {
        pic_size_in_map_units_minus1: u32,
        slice_group_id: Vec<u32>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PpsExtension {
    pub transform_8x8_mode_flag: bool,
    pub pic_scaling_matrix_present_flag: bool,
    pub pic_scaling_lists: Option<PicScalingLists>,
    pub second_chroma_qp_index_offset: i32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pps {
    pub pic_parameter_set_id: u32,
    pub seq_parameter_set_id: u32,
    pub entropy_coding_mode_flag: bool,
    pub bottom_field_pic_order_in_frame_present_flag: bool,
    pub num_slice_groups_minus1: u32,
    pub slice_group_map: Option<SliceGroupMap>,
    pub num_ref_idx_l0_default_active_minus1: u32,
    pub num_ref_idx_l1_default_active_minus1: u32,
    pub weighted_pred_flag: bool,
    pub weighted_bipred_idc: u32,
    pub pic_init_qp_minus26: i32,
    pub pic_init_qs_minus26: i32,
    pub chroma_qp_index_offset: i32,
    pub deblocking_filter_control_present_flag: bool,
    pub constrained_intra_pred_flag: bool,
    pub redundant_pic_cnt_present_flag: bool,
    pub extension: Option<PpsExtension>,
}

impl Pps {
    pub fn parse(rbsp: &[u8]) -> Result<Self, PpsError> {
        Self::parse_with_chroma_format(rbsp, 1)
    }

    pub fn parse_with_chroma_format(rbsp: &[u8], chroma_format_idc: u32) -> Result<Self, PpsError> {
        let mut r = BitReader::new(rbsp);

        // §7.4.2.2 — pic_parameter_set_id ue(v), 0..=255.
        let pic_parameter_set_id = r.ue()?;
        if pic_parameter_set_id > 255 {
            return Err(PpsError::PpsIdOutOfRange(pic_parameter_set_id));
        }

        // §7.4.2.2 — seq_parameter_set_id ue(v), 0..=31.
        let seq_parameter_set_id = r.ue()?;
        if seq_parameter_set_id > 31 {
            return Err(PpsError::SpsIdOutOfRange(seq_parameter_set_id));
        }

        let entropy_coding_mode_flag = r.u(1)? == 1;
        let bottom_field_pic_order_in_frame_present_flag = r.u(1)? == 1;

        // §7.4.2.2 — num_slice_groups_minus1. Annex A caps this at 7.
        let num_slice_groups_minus1 = r.ue()?;
        if num_slice_groups_minus1 > 7 {
            return Err(PpsError::NumSliceGroupsOutOfRange(num_slice_groups_minus1));
        }

        // §7.3.2.2 — FMO description, when more than one slice group.
        let slice_group_map = if num_slice_groups_minus1 > 0 {
            let slice_group_map_type = r.ue()?;
            if slice_group_map_type > 6 {
                return Err(PpsError::SliceGroupMapTypeOutOfRange(slice_group_map_type));
            }
            let map = match slice_group_map_type {
                0 => {
                    // §7.3.2.2: run_length_minus1[i] for i in 0..=num_slice_groups_minus1.
                    let mut run_length_minus1 =
                        Vec::with_capacity(num_slice_groups_minus1 as usize + 1);
                    for _ in 0..=num_slice_groups_minus1 {
                        run_length_minus1.push(r.ue()?);
                    }
                    SliceGroupMap::Interleaved { run_length_minus1 }
                }
                1 => SliceGroupMap::Dispersed,
                2 => {
                    // §7.3.2.2: top_left / bottom_right for i in 0..num_slice_groups_minus1.
                    // That's `num_slice_groups_minus1` iterations (i.e. the
                    // "background" group is implicit), per the loop's `<`.
                    let mut top_left = Vec::with_capacity(num_slice_groups_minus1 as usize);
                    let mut bottom_right = Vec::with_capacity(num_slice_groups_minus1 as usize);
                    for _ in 0..num_slice_groups_minus1 {
                        top_left.push(r.ue()?);
                        bottom_right.push(r.ue()?);
                    }
                    SliceGroupMap::Foreground {
                        top_left,
                        bottom_right,
                    }
                }
                3..=5 => {
                    let change_direction_flag = r.u(1)? == 1;
                    let change_rate_minus1 = r.ue()?;
                    SliceGroupMap::Changing {
                        slice_group_map_type,
                        change_direction_flag,
                        change_rate_minus1,
                    }
                }
                6 => {
                    // §7.3.2.2: pic_size_in_map_units_minus1, then
                    // slice_group_id[i] for i in 0..=pic_size_in_map_units_minus1.
                    // §7.4.2.2: each slice_group_id[i] is a u(v) with
                    // v = Ceil(Log2(num_slice_groups_minus1 + 1)) bits.
                    let pic_size_in_map_units_minus1 = r.ue()?;
                    if pic_size_in_map_units_minus1 > MAX_PIC_SIZE_IN_MAP_UNITS_MINUS1 {
                        return Err(PpsError::PicSizeInMapUnitsOutOfRange(
                            pic_size_in_map_units_minus1,
                            MAX_PIC_SIZE_IN_MAP_UNITS_MINUS1,
                        ));
                    }
                    let v = ceil_log2(num_slice_groups_minus1 + 1);
                    let count = pic_size_in_map_units_minus1 as usize + 1;
                    let mut slice_group_id = Vec::with_capacity(count);
                    for _ in 0..count {
                        // v==0 means "at most one slice group", which is
                        // only possible for num_slice_groups_minus1==0;
                        // but that branch is excluded by the outer `if`.
                        // Defensive: if v somehow is 0 we read nothing.
                        let id = if v == 0 { 0 } else { r.u(v)? };
                        slice_group_id.push(id);
                    }
                    SliceGroupMap::Explicit {
                        pic_size_in_map_units_minus1,
                        slice_group_id,
                    }
                }
                _ => unreachable!("slice_group_map_type bound-checked above"),
            };
            Some(map)
        } else {
            None
        };

        // §7.3.2.2 — remaining "always present" fields.
        let num_ref_idx_l0_default_active_minus1 = r.ue()?;
        let num_ref_idx_l1_default_active_minus1 = r.ue()?;
        let weighted_pred_flag = r.u(1)? == 1;
        let weighted_bipred_idc = r.u(2)?;
        if weighted_bipred_idc > 2 {
            return Err(PpsError::WeightedBipredIdcOutOfRange(weighted_bipred_idc));
        }
        let pic_init_qp_minus26 = r.se()?;
        let pic_init_qs_minus26 = r.se()?;
        let chroma_qp_index_offset = r.se()?;
        if !(-12..=12).contains(&chroma_qp_index_offset) {
            return Err(PpsError::ChromaQpIndexOffsetOutOfRange(
                chroma_qp_index_offset,
            ));
        }
        let deblocking_filter_control_present_flag = r.u(1)? == 1;
        let constrained_intra_pred_flag = r.u(1)? == 1;
        let redundant_pic_cnt_present_flag = r.u(1)? == 1;

        // §7.3.2.2 — optional trailing group gated on more_rbsp_data().
        let extension = if r.more_rbsp_data() {
            let transform_8x8_mode_flag = r.u(1)? == 1;
            let pic_scaling_matrix_present_flag = r.u(1)? == 1;
            let pic_scaling_lists = if pic_scaling_matrix_present_flag {
                // §7.3.2.2 loop bound:
                //   6 + ((chroma_format_idc != 3) ? 2 : 6) * transform_8x8_mode_flag
                let extra_8x8 = if !transform_8x8_mode_flag {
                    0
                } else if chroma_format_idc != 3 {
                    2
                } else {
                    6
                };
                let n = 6 + extra_8x8;
                let mut entries = Vec::with_capacity(n);
                for i in 0..n {
                    let present = r.u(1)? == 1;
                    if !present {
                        entries.push(ScalingListEntry::NotPresent);
                    } else {
                        let size = if i < 6 { 16 } else { 64 };
                        let result = parse_scaling_list(&mut r, size)?;
                        entries.push(if result.use_default {
                            ScalingListEntry::UseDefault
                        } else {
                            ScalingListEntry::Explicit(result.scaling_list)
                        });
                    }
                }
                Some(PicScalingLists { entries })
            } else {
                None
            };
            let second_chroma_qp_index_offset = r.se()?;
            if !(-12..=12).contains(&second_chroma_qp_index_offset) {
                return Err(PpsError::SecondChromaQpIndexOffsetOutOfRange(
                    second_chroma_qp_index_offset,
                ));
            }
            // §7.3.2.2 closes with rbsp_trailing_bits(); be lenient:
            // extra padding bytes are common, so ignore errors.
            let _ = r.rbsp_trailing_bits();
            Some(PpsExtension {
                transform_8x8_mode_flag,
                pic_scaling_matrix_present_flag,
                pic_scaling_lists,
                second_chroma_qp_index_offset,
            })
        } else {
            // §7.4.2.2 — when the optional tail is absent,
            // `second_chroma_qp_index_offset` is inferred to equal
            // `chroma_qp_index_offset` and the two flags are 0. We
            // represent that by `extension: None` and let callers apply
            // the inferred defaults.
            None
        };

        Ok(Pps {
            pic_parameter_set_id,
            seq_parameter_set_id,
            entropy_coding_mode_flag,
            bottom_field_pic_order_in_frame_present_flag,
            num_slice_groups_minus1,
            slice_group_map,
            num_ref_idx_l0_default_active_minus1,
            num_ref_idx_l1_default_active_minus1,
            weighted_pred_flag,
            weighted_bipred_idc,
            pic_init_qp_minus26,
            pic_init_qs_minus26,
            chroma_qp_index_offset,
            deblocking_filter_control_present_flag,
            constrained_intra_pred_flag,
            redundant_pic_cnt_present_flag,
            extension,
        })
    }

    pub fn second_chroma_qp_index_offset(&self) -> i32 {
        self.extension
            .as_ref()
            .map(|e| e.second_chroma_qp_index_offset)
            .unwrap_or(self.chroma_qp_index_offset)
    }

    pub fn transform_8x8_mode_flag(&self) -> bool {
        self.extension
            .as_ref()
            .is_some_and(|e| e.transform_8x8_mode_flag)
    }
}

fn ceil_log2(n: u32) -> u32 {
    if n <= 1 {
        0
    } else {
        // floor(log2(n-1)) + 1
        32 - (n - 1).leading_zeros()
    }
}
