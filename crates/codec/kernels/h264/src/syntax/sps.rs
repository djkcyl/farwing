// SPDX-License-Identifier: MIT
// Derived from oxideav-h264 0.1.8, Copyright (c) 2026 Karpelès Lab Inc.
// Retained syntax/metadata primitives used by this client; see COPYING.OxideAV in this crate.

use crate::syntax::bitstream::{BitError, BitReader};

use crate::syntax::scaling_list::{ScalingListError, ScalingListResult, parse_scaling_list};

use crate::syntax::vui::{VuiError, VuiParameters};

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum SpsError {
    #[error("bitstream read failed: {0}")]
    Bitstream(#[from] BitError),
    #[error("reserved_zero_2bits is not 0 (got {0})")]
    ReservedNotZero(u32),
    #[error("profile_idc {0} is reserved (not enumerated in §A.2)")]
    ProfileIdcReserved(u8),
    #[error("chroma_format_idc out of range (got {0}, max 3)")]
    ChromaFormatIdcOutOfRange(u32),
    #[error("bit_depth_luma_minus8 out of range (got {0}, max 6)")]
    BitDepthLumaOutOfRange(u32),
    #[error("bit_depth_chroma_minus8 out of range (got {0}, max 6)")]
    BitDepthChromaOutOfRange(u32),
    #[error("log2_max_frame_num_minus4 out of range (got {0}, max 12)")]
    Log2MaxFrameNumOutOfRange(u32),
    #[error("pic_order_cnt_type out of range (got {0}, max 2)")]
    PicOrderCntTypeOutOfRange(u32),
    #[error("log2_max_pic_order_cnt_lsb_minus4 out of range (got {0}, max 12)")]
    Log2MaxPocLsbOutOfRange(u32),
    #[error("num_ref_frames_in_pic_order_cnt_cycle out of range (got {0}, max 255)")]
    NumRefFramesInPocCycleOutOfRange(u32),
    #[error("seq_parameter_set_id out of range (got {0}, max 31)")]
    SpsIdOutOfRange(u32),
    #[error("pic_width_in_mbs_minus1 out of range (got {0}, max 510)")]
    PicWidthInMbsOutOfRange(u32),
    #[error("pic_height_in_map_units_minus1 out of range (got {0}, max 510)")]
    PicHeightInMapUnitsOutOfRange(u32),
    #[error("scaling_list: {0}")]
    ScalingList(#[from] ScalingListError),
    #[error("vui_parameters: {0}")]
    Vui(#[from] VuiError),
    #[error("max_num_ref_frames out of range (got {0}, max 16)")]
    MaxNumRefFramesOutOfRange(u32),
    #[error(
        "frame_cropping invalid: CropUnitX={cux}*(left={left}+right={right}+1) > PicWidthInSamplesL={width}, or CropUnitY={cuy}*(top={top}+bottom={bottom}+1) > FrameHeightInSamplesL={height}"
    )]
    FrameCroppingOutOfRange {
        cux: u32,
        cuy: u32,
        left: u32,
        right: u32,
        top: u32,
        bottom: u32,
        width: u32,
        height: u32,
    },
}

const MAX_PIC_DIM_IN_MBS_MINUS1: u32 = (1 << 9) - 2;

fn is_valid_profile_idc(profile_idc: u8) -> bool {
    matches!(
        profile_idc,
        66 | 77 | 88 | 100 | 110 | 122 | 244 | 44 | 83 | 86 | 118 | 128 | 138 | 139 | 134 | 135
    )
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ScalingListEntry {
    NotPresent,
    UseDefault,
    Explicit(Vec<i32>),
}

impl ScalingListEntry {
    fn from_result(r: ScalingListResult) -> Self {
        if r.use_default {
            Self::UseDefault
        } else {
            Self::Explicit(r.scaling_list)
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SeqScalingLists {
    pub entries: Vec<ScalingListEntry>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FrameCropping {
    pub left: u32,
    pub right: u32,
    pub top: u32,
    pub bottom: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Sps {
    // §7.3.2.1.1
    pub profile_idc: u8,
    pub constraint_set_flags: u8,
    pub level_idc: u8,
    pub seq_parameter_set_id: u32,

    // Only present when profile_idc is one of the "chroma-extended"
    // profiles listed in §7.3.2.1.1 (100/110/122/244/44/83/86/118/128/138/139/134/135).
    pub chroma_format_idc: u32,
    pub separate_colour_plane_flag: bool,
    pub bit_depth_luma_minus8: u32,
    pub bit_depth_chroma_minus8: u32,
    pub qpprime_y_zero_transform_bypass_flag: bool,
    pub seq_scaling_matrix_present_flag: bool,
    pub seq_scaling_lists: Option<SeqScalingLists>,

    pub log2_max_frame_num_minus4: u32,
    pub pic_order_cnt_type: u32,

    // pic_order_cnt_type == 0 branch:
    pub log2_max_pic_order_cnt_lsb_minus4: u32,

    // pic_order_cnt_type == 1 branch:
    pub delta_pic_order_always_zero_flag: bool,
    pub offset_for_non_ref_pic: i32,
    pub offset_for_top_to_bottom_field: i32,
    pub num_ref_frames_in_pic_order_cnt_cycle: u32,
    pub offset_for_ref_frame: Vec<i32>,

    // Common tail:
    pub max_num_ref_frames: u32,
    pub gaps_in_frame_num_value_allowed_flag: bool,
    pub pic_width_in_mbs_minus1: u32,
    pub pic_height_in_map_units_minus1: u32,
    pub frame_mbs_only_flag: bool,
    pub mb_adaptive_frame_field_flag: bool,
    pub direct_8x8_inference_flag: bool,
    pub frame_cropping: Option<FrameCropping>,
    pub vui_parameters_present_flag: bool,
    pub vui: Option<VuiParameters>,
}

impl Sps {
    pub fn parse(rbsp: &[u8]) -> Result<Self, SpsError> {
        let mut r = BitReader::new(rbsp);
        let sps = Self::parse_seq_parameter_set_data(&mut r)?;
        // §7.3.2.11 — trailing stop bit + zero alignment. Some producers
        // pad with extra zeros; don't reject on mis-match.
        let _ = r.rbsp_trailing_bits();
        Ok(sps)
    }

    pub fn parse_seq_parameter_set_data(r: &mut BitReader<'_>) -> Result<Self, SpsError> {
        // §7.3.2.1.1 — profile_idc / constraint_setN_flag / reserved_zero_2bits / level_idc.
        let profile_idc = r.u(8)? as u8;
        // §A.1 / §A.2 — only the 16 enumerated profiles are allowed;
        // other values are "reserved for future use" and we cannot
        // safely decode bitstreams using them. Reject up front to
        // match the strictness of common H.264 decoders (closes a fuzz-oracle
        // divergence on profile_idc=251 + 243 inputs).
        if !is_valid_profile_idc(profile_idc) {
            return Err(SpsError::ProfileIdcReserved(profile_idc));
        }
        let cs0 = r.u(1)? as u8;
        let cs1 = r.u(1)? as u8;
        let cs2 = r.u(1)? as u8;
        let cs3 = r.u(1)? as u8;
        let cs4 = r.u(1)? as u8;
        let cs5 = r.u(1)? as u8;
        // §7.3.2.1.1 — reserved_zero_2bits /* equal to 0 */.
        let reserved = r.u(2)?;
        if reserved != 0 {
            return Err(SpsError::ReservedNotZero(reserved));
        }
        let level_idc = r.u(8)? as u8;

        let constraint_set_flags =
            cs0 | (cs1 << 1) | (cs2 << 2) | (cs3 << 3) | (cs4 << 4) | (cs5 << 5);

        // §7.3.2.1.1 — seq_parameter_set_id ue(v); semantics clamp to 0..=31.
        let seq_parameter_set_id = r.ue()?;
        if seq_parameter_set_id > 31 {
            return Err(SpsError::SpsIdOutOfRange(seq_parameter_set_id));
        }

        // §7.3.2.1.1 — the "chroma-extended" profile gate.
        let mut chroma_format_idc: u32 = 1; // §7.4.2.1.1 default when absent
        let mut separate_colour_plane_flag = false;
        let mut bit_depth_luma_minus8: u32 = 0;
        let mut bit_depth_chroma_minus8: u32 = 0;
        let mut qpprime_y_zero_transform_bypass_flag = false;
        let mut seq_scaling_matrix_present_flag = false;

        if matches!(
            profile_idc,
            100 | 110 | 122 | 244 | 44 | 83 | 86 | 118 | 128 | 138 | 139 | 134 | 135
        ) {
            chroma_format_idc = r.ue()?;
            if chroma_format_idc > 3 {
                return Err(SpsError::ChromaFormatIdcOutOfRange(chroma_format_idc));
            }
            if chroma_format_idc == 3 {
                separate_colour_plane_flag = r.u(1)? == 1;
            }
            bit_depth_luma_minus8 = r.ue()?;
            if bit_depth_luma_minus8 > 6 {
                return Err(SpsError::BitDepthLumaOutOfRange(bit_depth_luma_minus8));
            }
            bit_depth_chroma_minus8 = r.ue()?;
            if bit_depth_chroma_minus8 > 6 {
                return Err(SpsError::BitDepthChromaOutOfRange(bit_depth_chroma_minus8));
            }
            qpprime_y_zero_transform_bypass_flag = r.u(1)? == 1;
            seq_scaling_matrix_present_flag = r.u(1)? == 1;
        }
        // §7.3.2.1.1 — scaling_list body outside the chroma-extended gate.
        // When `seq_scaling_matrix_present_flag` is only set by that gate,
        // this loop runs exactly when the flag is true.
        let seq_scaling_lists = if seq_scaling_matrix_present_flag {
            // §7.3.2.1.1 — loop bound: 8 for chroma_format_idc != 3, 12 for 4:4:4.
            let n = if chroma_format_idc != 3 { 8 } else { 12 };
            let mut entries = Vec::with_capacity(n);
            for i in 0..n {
                let present = r.u(1)? == 1;
                if !present {
                    entries.push(ScalingListEntry::NotPresent);
                } else {
                    // First 6 entries are 4x4 (size 16), rest are 8x8 (size 64).
                    let size = if i < 6 { 16 } else { 64 };
                    let result = parse_scaling_list(r, size)?;
                    entries.push(ScalingListEntry::from_result(result));
                }
            }
            Some(SeqScalingLists { entries })
        } else {
            None
        };

        // §7.3.2.1.1 — log2_max_frame_num_minus4 ue(v); range 0..=12.
        let log2_max_frame_num_minus4 = r.ue()?;
        if log2_max_frame_num_minus4 > 12 {
            return Err(SpsError::Log2MaxFrameNumOutOfRange(
                log2_max_frame_num_minus4,
            ));
        }

        // §7.3.2.1.1 — pic_order_cnt_type ue(v); range 0..=2.
        let pic_order_cnt_type = r.ue()?;
        if pic_order_cnt_type > 2 {
            return Err(SpsError::PicOrderCntTypeOutOfRange(pic_order_cnt_type));
        }

        let mut log2_max_pic_order_cnt_lsb_minus4: u32 = 0;
        let mut delta_pic_order_always_zero_flag = false;
        let mut offset_for_non_ref_pic: i32 = 0;
        let mut offset_for_top_to_bottom_field: i32 = 0;
        let mut num_ref_frames_in_pic_order_cnt_cycle: u32 = 0;
        let mut offset_for_ref_frame: Vec<i32> = Vec::new();

        if pic_order_cnt_type == 0 {
            log2_max_pic_order_cnt_lsb_minus4 = r.ue()?;
            if log2_max_pic_order_cnt_lsb_minus4 > 12 {
                return Err(SpsError::Log2MaxPocLsbOutOfRange(
                    log2_max_pic_order_cnt_lsb_minus4,
                ));
            }
        } else if pic_order_cnt_type == 1 {
            delta_pic_order_always_zero_flag = r.u(1)? == 1;
            offset_for_non_ref_pic = r.se()?;
            offset_for_top_to_bottom_field = r.se()?;
            num_ref_frames_in_pic_order_cnt_cycle = r.ue()?;
            if num_ref_frames_in_pic_order_cnt_cycle > 255 {
                return Err(SpsError::NumRefFramesInPocCycleOutOfRange(
                    num_ref_frames_in_pic_order_cnt_cycle,
                ));
            }
            offset_for_ref_frame.reserve_exact(num_ref_frames_in_pic_order_cnt_cycle as usize);
            for _ in 0..num_ref_frames_in_pic_order_cnt_cycle {
                offset_for_ref_frame.push(r.se()?);
            }
        }
        // pic_order_cnt_type == 2: no extra fields.

        // §7.3.2.1.1 — common tail.
        // §7.4.2.1.1 / Annex A.3.1 — `max_num_ref_frames` ≤ MaxDpbFrames
        // which is capped at 16 regardless of level. Beyond 16, downstream
        // ref-list construction is invalid and the bitstream is malformed.
        let max_num_ref_frames = r.ue()?;
        if max_num_ref_frames > 16 {
            return Err(SpsError::MaxNumRefFramesOutOfRange(max_num_ref_frames));
        }
        let gaps_in_frame_num_value_allowed_flag = r.u(1)? == 1;
        let pic_width_in_mbs_minus1 = r.ue()?;
        if pic_width_in_mbs_minus1 > MAX_PIC_DIM_IN_MBS_MINUS1 {
            return Err(SpsError::PicWidthInMbsOutOfRange(pic_width_in_mbs_minus1));
        }
        let pic_height_in_map_units_minus1 = r.ue()?;
        if pic_height_in_map_units_minus1 > MAX_PIC_DIM_IN_MBS_MINUS1 {
            return Err(SpsError::PicHeightInMapUnitsOutOfRange(
                pic_height_in_map_units_minus1,
            ));
        }
        let frame_mbs_only_flag = r.u(1)? == 1;
        let mb_adaptive_frame_field_flag = if !frame_mbs_only_flag {
            r.u(1)? == 1
        } else {
            false
        };
        let direct_8x8_inference_flag = r.u(1)? == 1;

        // §7.3.2.1.1 — frame_cropping_flag + four offsets.
        let frame_cropping_flag = r.u(1)? == 1;
        let frame_cropping = if frame_cropping_flag {
            let left = r.ue()?;
            let right = r.ue()?;
            let top = r.ue()?;
            let bottom = r.ue()?;

            // §7.4.2.1.1 — validate the cropping rectangle stays
            // within the coded picture. Required by:
            //   `frame_crop_left_offset ∈ [0, (PicWidthInSamplesL /
            //    CropUnitX) − (frame_crop_right_offset + 1)]`,
            //   `frame_crop_top_offset  ∈ [0, (16 * FrameHeightInMbs
            //    / CropUnitY) − (frame_crop_bottom_offset + 1)]`.
            //
            // CropUnitX / CropUnitY (eqs. 7-19..7-22) depend on
            // ChromaArrayType:
            //   * ChromaArrayType == 0 (monochrome OR
            //     separate_colour_plane_flag == 1):
            //       CropUnitX = 1, CropUnitY = 2 − frame_mbs_only_flag
            //   * Otherwise (1=4:2:0, 2=4:2:2, 3=4:4:4):
            //       CropUnitX = SubWidthC,
            //       CropUnitY = SubHeightC * (2 − frame_mbs_only_flag)
            // with SubWidthC / SubHeightC from §6.2 Table 6-1:
            //       1 (4:2:0) → 2, 2
            //       2 (4:2:2) → 2, 1
            //       3 (4:4:4) → 1, 1
            //
            // The cast to u32 is safe: chroma_format_idc was already
            // bounded to 0..=3 above, separate_colour_plane_flag is
            // a single bit, frame_mbs_only_flag likewise.
            let chroma_array_type = if separate_colour_plane_flag {
                0
            } else {
                chroma_format_idc
            };
            let (sub_w_c, sub_h_c) = match chroma_array_type {
                1 => (2u32, 2u32),
                2 => (2u32, 1u32),
                3 => (1u32, 1u32),
                _ => (1u32, 1u32), // ChromaArrayType == 0
            };
            let mbs_only_factor = if frame_mbs_only_flag { 1u32 } else { 2u32 };
            let (cux, cuy) = if chroma_array_type == 0 {
                (1u32, mbs_only_factor)
            } else {
                (sub_w_c, sub_h_c * mbs_only_factor)
            };
            // PicWidthInSamplesL = (pic_width_in_mbs_minus1 + 1) * 16
            // FrameHeightInSamplesL = (2 - frame_mbs_only_flag) *
            //                          (pic_height_in_map_units_minus1 + 1) * 16
            let width_samples = (pic_width_in_mbs_minus1 + 1) * 16;
            let height_samples = mbs_only_factor * (pic_height_in_map_units_minus1 + 1) * 16;
            // Use u64 to avoid an overflow on the multiplication when
            // the input is pathological (cropping offsets are ue(v)
            // and the picture-dim cap is 510 mbs/axis = 8160 samples,
            // so cux * (left + right + 1) at worst is 2 * (u32::MAX +
            // 1) — fits comfortably in u64).
            let need_w = u64::from(cux) * (u64::from(left) + u64::from(right) + 1);
            let need_h = u64::from(cuy) * (u64::from(top) + u64::from(bottom) + 1);
            if need_w > u64::from(width_samples) || need_h > u64::from(height_samples) {
                return Err(SpsError::FrameCroppingOutOfRange {
                    cux,
                    cuy,
                    left,
                    right,
                    top,
                    bottom,
                    width: width_samples,
                    height: height_samples,
                });
            }

            Some(FrameCropping {
                left,
                right,
                top,
                bottom,
            })
        } else {
            None
        };

        // §7.3.2.1.1 / §E.1 — VUI parameters.
        let vui_parameters_present_flag = r.u(1)? == 1;
        let vui = if vui_parameters_present_flag {
            Some(VuiParameters::parse(r)?)
        } else {
            None
        };

        Ok(Sps {
            profile_idc,
            constraint_set_flags,
            level_idc,
            seq_parameter_set_id,
            chroma_format_idc,
            separate_colour_plane_flag,
            bit_depth_luma_minus8,
            bit_depth_chroma_minus8,
            qpprime_y_zero_transform_bypass_flag,
            seq_scaling_matrix_present_flag,
            seq_scaling_lists,
            log2_max_frame_num_minus4,
            pic_order_cnt_type,
            log2_max_pic_order_cnt_lsb_minus4,
            delta_pic_order_always_zero_flag,
            offset_for_non_ref_pic,
            offset_for_top_to_bottom_field,
            num_ref_frames_in_pic_order_cnt_cycle,
            offset_for_ref_frame,
            max_num_ref_frames,
            gaps_in_frame_num_value_allowed_flag,
            pic_width_in_mbs_minus1,
            pic_height_in_map_units_minus1,
            frame_mbs_only_flag,
            mb_adaptive_frame_field_flag,
            direct_8x8_inference_flag,
            frame_cropping,
            vui_parameters_present_flag,
            vui,
        })
    }

    pub fn chroma_array_type(&self) -> u32 {
        if self.separate_colour_plane_flag {
            0
        } else {
            self.chroma_format_idc
        }
    }

    pub fn pic_width_in_mbs(&self) -> u32 {
        self.pic_width_in_mbs_minus1 + 1
    }

    pub fn pic_height_in_map_units(&self) -> u32 {
        self.pic_height_in_map_units_minus1 + 1
    }

    pub fn frame_height_in_mbs(&self) -> u32 {
        let factor = if self.frame_mbs_only_flag { 1 } else { 2 };
        factor * self.pic_height_in_map_units()
    }

    pub fn pic_height_in_mbs(&self, field_pic_flag: bool) -> u32 {
        self.frame_height_in_mbs() / (1 + u32::from(field_pic_flag))
    }

    pub fn pic_size_in_mbs(&self, field_pic_flag: bool) -> u32 {
        self.pic_width_in_mbs()
            .saturating_mul(self.pic_height_in_mbs(field_pic_flag))
    }

    pub fn max_frame_num(&self) -> u32 {
        1u32 << (self.log2_max_frame_num_minus4 + 4)
    }
}
