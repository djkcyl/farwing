// SPDX-License-Identifier: MIT
// Derived from oxideav-h264 0.1.8, Copyright (c) 2026 Karpelès Lab Inc.
// Retained syntax/metadata primitives used by this client; see COPYING.OxideAV in this crate.

use crate::syntax::bitstream::{BitError, BitReader};

pub const EXTENDED_SAR: u8 = 255;

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum VuiError {
    #[error("bitstream read failed: {0}")]
    Bitstream(#[from] BitError),
    #[error(
        "aspect_ratio_idc=255 (Extended_SAR) requires 16+16 trailing bits — got truncated input"
    )]
    ExtendedSarTruncated,
    #[error("invalid HRD cpb_cnt_minus1 (got {0}, max 31)")]
    HrdCpbCountOutOfRange(u32),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AspectRatioInfo {
    pub aspect_ratio_idc: u8,
    pub extended_sar: Option<(u16, u16)>, // (sar_width, sar_height)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VideoSignalType {
    pub video_format: u8,
    pub video_full_range_flag: bool,
    pub colour_description: Option<ColourDescription>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ColourDescription {
    pub colour_primaries: u8,
    pub transfer_characteristics: u8,
    pub matrix_coefficients: u8,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChromaLocInfo {
    pub chroma_sample_loc_type_top_field: u32,
    pub chroma_sample_loc_type_bottom_field: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TimingInfo {
    pub num_units_in_tick: u32,
    pub time_scale: u32,
    pub fixed_frame_rate_flag: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HrdParameters {
    pub cpb_cnt_minus1: u32,
    pub bit_rate_scale: u8,
    pub cpb_size_scale: u8,
    pub bit_rate_value_minus1: Vec<u32>,
    pub cpb_size_value_minus1: Vec<u32>,
    pub cbr_flag: Vec<bool>,
    pub initial_cpb_removal_delay_length_minus1: u8,
    pub cpb_removal_delay_length_minus1: u8,
    pub dpb_output_delay_length_minus1: u8,
    pub time_offset_length: u8,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BitstreamRestriction {
    pub motion_vectors_over_pic_boundaries_flag: bool,
    pub max_bytes_per_pic_denom: u32,
    pub max_bits_per_mb_denom: u32,
    pub log2_max_mv_length_horizontal: u32,
    pub log2_max_mv_length_vertical: u32,
    pub max_num_reorder_frames: u32,
    pub max_dec_frame_buffering: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct VuiParameters {
    pub aspect_ratio: Option<AspectRatioInfo>,
    pub overscan_appropriate_flag: Option<bool>,
    pub video_signal_type: Option<VideoSignalType>,
    pub chroma_loc_info: Option<ChromaLocInfo>,
    pub timing_info: Option<TimingInfo>,
    pub nal_hrd_parameters: Option<HrdParameters>,
    pub vcl_hrd_parameters: Option<HrdParameters>,
    pub low_delay_hrd_flag: Option<bool>,
    pub pic_struct_present_flag: bool,
    pub bitstream_restriction: Option<BitstreamRestriction>,
}

impl VuiParameters {
    pub fn parse(r: &mut BitReader<'_>) -> Result<Self, VuiError> {
        let mut out = VuiParameters::default();

        // §E.1.1 aspect_ratio_info_present_flag u(1)
        let aspect_ratio_info_present_flag = r.u(1)? == 1;
        if aspect_ratio_info_present_flag {
            // aspect_ratio_idc u(8)
            let aspect_ratio_idc = r.u(8)? as u8;
            let extended_sar = if aspect_ratio_idc == EXTENDED_SAR {
                // sar_width u(16), sar_height u(16)
                if r.bits_remaining() < 32 {
                    return Err(VuiError::ExtendedSarTruncated);
                }
                let sar_width = r.u(16)? as u16;
                let sar_height = r.u(16)? as u16;
                Some((sar_width, sar_height))
            } else {
                None
            };
            out.aspect_ratio = Some(AspectRatioInfo {
                aspect_ratio_idc,
                extended_sar,
            });
        }

        // §E.1.1 overscan_info_present_flag u(1)
        let overscan_info_present_flag = r.u(1)? == 1;
        if overscan_info_present_flag {
            // overscan_appropriate_flag u(1)
            out.overscan_appropriate_flag = Some(r.u(1)? == 1);
        }

        // §E.1.1 video_signal_type_present_flag u(1)
        let video_signal_type_present_flag = r.u(1)? == 1;
        if video_signal_type_present_flag {
            let video_format = r.u(3)? as u8;
            let video_full_range_flag = r.u(1)? == 1;
            let colour_description_present_flag = r.u(1)? == 1;
            let colour_description = if colour_description_present_flag {
                Some(ColourDescription {
                    colour_primaries: r.u(8)? as u8,
                    transfer_characteristics: r.u(8)? as u8,
                    matrix_coefficients: r.u(8)? as u8,
                })
            } else {
                None
            };
            out.video_signal_type = Some(VideoSignalType {
                video_format,
                video_full_range_flag,
                colour_description,
            });
        }

        // §E.1.1 chroma_loc_info_present_flag u(1)
        let chroma_loc_info_present_flag = r.u(1)? == 1;
        if chroma_loc_info_present_flag {
            out.chroma_loc_info = Some(ChromaLocInfo {
                chroma_sample_loc_type_top_field: r.ue()?,
                chroma_sample_loc_type_bottom_field: r.ue()?,
            });
        }

        // §E.1.1 timing_info_present_flag u(1)
        let timing_info_present_flag = r.u(1)? == 1;
        if timing_info_present_flag {
            out.timing_info = Some(TimingInfo {
                num_units_in_tick: r.u(32)?,
                time_scale: r.u(32)?,
                fixed_frame_rate_flag: r.u(1)? == 1,
            });
        }

        // §E.1.1 nal_hrd_parameters_present_flag u(1)
        let nal_hrd_parameters_present_flag = r.u(1)? == 1;
        if nal_hrd_parameters_present_flag {
            out.nal_hrd_parameters = Some(HrdParameters::parse(r)?);
        }

        // §E.1.1 vcl_hrd_parameters_present_flag u(1)
        let vcl_hrd_parameters_present_flag = r.u(1)? == 1;
        if vcl_hrd_parameters_present_flag {
            out.vcl_hrd_parameters = Some(HrdParameters::parse(r)?);
        }

        // §E.1.1: low_delay_hrd_flag u(1) only when either HRD block present.
        if nal_hrd_parameters_present_flag || vcl_hrd_parameters_present_flag {
            out.low_delay_hrd_flag = Some(r.u(1)? == 1);
        }

        // §E.1.1 pic_struct_present_flag u(1)
        out.pic_struct_present_flag = r.u(1)? == 1;

        // §E.1.1 bitstream_restriction_flag u(1)
        let bitstream_restriction_flag = r.u(1)? == 1;
        if bitstream_restriction_flag {
            out.bitstream_restriction = Some(BitstreamRestriction {
                motion_vectors_over_pic_boundaries_flag: r.u(1)? == 1,
                max_bytes_per_pic_denom: r.ue()?,
                max_bits_per_mb_denom: r.ue()?,
                log2_max_mv_length_horizontal: r.ue()?,
                log2_max_mv_length_vertical: r.ue()?,
                max_num_reorder_frames: r.ue()?,
                max_dec_frame_buffering: r.ue()?,
            });
        }

        Ok(out)
    }
}

impl HrdParameters {
    pub fn parse(r: &mut BitReader<'_>) -> Result<Self, VuiError> {
        // §E.1.2 cpb_cnt_minus1 ue(v). §E.2.2 restricts range to 0..=31.
        let cpb_cnt_minus1 = r.ue()?;
        if cpb_cnt_minus1 > 31 {
            return Err(VuiError::HrdCpbCountOutOfRange(cpb_cnt_minus1));
        }
        // bit_rate_scale u(4), cpb_size_scale u(4)
        let bit_rate_scale = r.u(4)? as u8;
        let cpb_size_scale = r.u(4)? as u8;

        let count = (cpb_cnt_minus1 as usize) + 1;
        let mut bit_rate_value_minus1 = Vec::with_capacity(count);
        let mut cpb_size_value_minus1 = Vec::with_capacity(count);
        let mut cbr_flag = Vec::with_capacity(count);
        for _ in 0..count {
            // bit_rate_value_minus1[SchedSelIdx] ue(v)
            bit_rate_value_minus1.push(r.ue()?);
            // cpb_size_value_minus1[SchedSelIdx] ue(v)
            cpb_size_value_minus1.push(r.ue()?);
            // cbr_flag[SchedSelIdx] u(1)
            cbr_flag.push(r.u(1)? == 1);
        }

        // u(5) each, §E.1.2.
        let initial_cpb_removal_delay_length_minus1 = r.u(5)? as u8;
        let cpb_removal_delay_length_minus1 = r.u(5)? as u8;
        let dpb_output_delay_length_minus1 = r.u(5)? as u8;
        let time_offset_length = r.u(5)? as u8;

        Ok(HrdParameters {
            cpb_cnt_minus1,
            bit_rate_scale,
            cpb_size_scale,
            bit_rate_value_minus1,
            cpb_size_value_minus1,
            cbr_flag,
            initial_cpb_removal_delay_length_minus1,
            cpb_removal_delay_length_minus1,
            dpb_output_delay_length_minus1,
            time_offset_length,
        })
    }
}
