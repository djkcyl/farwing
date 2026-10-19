// SPDX-License-Identifier: MIT
// Derived from oxideav-h265 0.0.10, Copyright (c) 2026 Karpelès Lab Inc.
// Retained syntax/metadata primitives used by this client; see COPYING.OxideAV in this crate.

use crate::bitreader::{BitReader, BitReaderError};

use crate::hrd::{HrdError, HrdParameters};

pub const EXTENDED_SAR: u8 = 255;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VuiError {
    Truncated,
    ValueOutOfRange { field: &'static str, got: u32 },
    Hrd(HrdError),
    Bitstream(BitReaderError),
}

impl core::fmt::Display for VuiError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Truncated => f.write_str("vui_parameters() RBSP truncated"),
            Self::ValueOutOfRange { field, got } => {
                write!(f, "VUI syntax element {field} out of range: {got}")
            }
            Self::Hrd(e) => write!(f, "hrd_parameters() error during VUI parse: {e}"),
            Self::Bitstream(e) => write!(f, "bitstream error during VUI parse: {e}"),
        }
    }
}

impl std::error::Error for VuiError {}

impl From<BitReaderError> for VuiError {
    fn from(e: BitReaderError) -> Self {
        match e {
            BitReaderError::EndOfBuffer => Self::Truncated,
            other => Self::Bitstream(other),
        }
    }
}

impl From<HrdError> for VuiError {
    fn from(e: HrdError) -> Self {
        // Flatten truncation to the VUI-level equivalent so the public
        // surface stays predictable; carry structured HRD faults through.
        match e {
            HrdError::Truncated => Self::Truncated,
            HrdError::Bitstream(b) => Self::Bitstream(b),
            other => Self::Hrd(other),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ColourDescription {
    pub colour_primaries: u8,
    pub transfer_characteristics: u8,
    pub matrix_coeffs: u8,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VideoSignalType {
    pub video_format: u8,
    pub video_full_range_flag: bool,
    pub colour_description: Option<ColourDescription>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct DefaultDisplayWindow {
    pub left_offset: u32,
    pub right_offset: u32,
    pub top_offset: u32,
    pub bottom_offset: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VuiTimingInfo {
    pub num_units_in_tick: u32,
    pub time_scale: u32,
    pub poc_proportional_to_timing_flag: bool,
    pub num_ticks_poc_diff_one_minus1: Option<u32>,
    pub hrd_parameters_present_flag: bool,
    pub hrd_parameters: Option<HrdParameters>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BitstreamRestriction {
    pub tiles_fixed_structure_flag: bool,
    pub motion_vectors_over_pic_boundaries_flag: bool,
    pub restricted_ref_pic_lists_flag: bool,
    pub min_spatial_segmentation_idc: u32,
    pub max_bytes_per_pic_denom: u32,
    pub max_bits_per_min_cu_denom: u32,
    pub log2_max_mv_length_horizontal: u32,
    pub log2_max_mv_length_vertical: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VuiParameters {
    pub aspect_ratio_info_present_flag: bool,
    pub aspect_ratio_idc: u8,
    pub sar_width: Option<u16>,
    pub sar_height: Option<u16>,
    pub overscan_info_present_flag: bool,
    pub overscan_appropriate_flag: Option<bool>,
    pub video_signal_type_present_flag: bool,
    pub video_signal_type: Option<VideoSignalType>,
    pub chroma_loc_info_present_flag: bool,
    pub chroma_sample_loc_type_top_field: u32,
    pub chroma_sample_loc_type_bottom_field: u32,
    pub neutral_chroma_indication_flag: bool,
    pub field_seq_flag: bool,
    pub frame_field_info_present_flag: bool,
    pub default_display_window_flag: bool,
    pub default_display_window: Option<DefaultDisplayWindow>,
    pub vui_timing_info_present_flag: bool,
    pub timing_info: Option<VuiTimingInfo>,
    pub bitstream_restriction_flag: bool,
    pub bitstream_restriction: Option<BitstreamRestriction>,
}

impl VuiParameters {
    pub fn parse(br: &mut BitReader<'_>, sps_max_sub_layers_minus1: u8) -> Result<Self, VuiError> {
        let aspect_ratio_info_present_flag = br.u1()? != 0;
        let mut aspect_ratio_idc = 0u8;
        let mut sar_width = None;
        let mut sar_height = None;
        if aspect_ratio_info_present_flag {
            aspect_ratio_idc = br.u(8)? as u8;
            if aspect_ratio_idc == EXTENDED_SAR {
                sar_width = Some(br.u(16)? as u16);
                sar_height = Some(br.u(16)? as u16);
            }
        }

        let overscan_info_present_flag = br.u1()? != 0;
        let overscan_appropriate_flag = if overscan_info_present_flag {
            Some(br.u1()? != 0)
        } else {
            None
        };

        let video_signal_type_present_flag = br.u1()? != 0;
        let video_signal_type = if video_signal_type_present_flag {
            let video_format = br.u(3)? as u8;
            let video_full_range_flag = br.u1()? != 0;
            let colour_description_present_flag = br.u1()? != 0;
            let colour_description = if colour_description_present_flag {
                Some(ColourDescription {
                    colour_primaries: br.u(8)? as u8,
                    transfer_characteristics: br.u(8)? as u8,
                    matrix_coeffs: br.u(8)? as u8,
                })
            } else {
                None
            };
            Some(VideoSignalType {
                video_format,
                video_full_range_flag,
                colour_description,
            })
        } else {
            None
        };

        let chroma_loc_info_present_flag = br.u1()? != 0;
        let (chroma_sample_loc_type_top_field, chroma_sample_loc_type_bottom_field) =
            if chroma_loc_info_present_flag {
                let top = br.ue()?;
                // §E.3.1: range 0..=5, inclusive.
                if top > 5 {
                    return Err(VuiError::ValueOutOfRange {
                        field: "chroma_sample_loc_type_top_field",
                        got: top,
                    });
                }
                let bottom = br.ue()?;
                if bottom > 5 {
                    return Err(VuiError::ValueOutOfRange {
                        field: "chroma_sample_loc_type_bottom_field",
                        got: bottom,
                    });
                }
                (top, bottom)
            } else {
                // §E.3.1: inferred to 0 when not present.
                (0, 0)
            };

        let neutral_chroma_indication_flag = br.u1()? != 0;
        let field_seq_flag = br.u1()? != 0;
        let frame_field_info_present_flag = br.u1()? != 0;

        let default_display_window_flag = br.u1()? != 0;
        let default_display_window = if default_display_window_flag {
            Some(DefaultDisplayWindow {
                left_offset: br.ue()?,
                right_offset: br.ue()?,
                top_offset: br.ue()?,
                bottom_offset: br.ue()?,
            })
        } else {
            None
        };

        let vui_timing_info_present_flag = br.u1()? != 0;
        let timing_info = if vui_timing_info_present_flag {
            let num_units_in_tick = br.u(32)?;
            // §E.3.1: vui_num_units_in_tick shall be greater than 0.
            if num_units_in_tick == 0 {
                return Err(VuiError::ValueOutOfRange {
                    field: "vui_num_units_in_tick",
                    got: 0,
                });
            }
            let time_scale = br.u(32)?;
            // §E.3.1: vui_time_scale shall be greater than 0.
            if time_scale == 0 {
                return Err(VuiError::ValueOutOfRange {
                    field: "vui_time_scale",
                    got: 0,
                });
            }
            let poc_proportional_to_timing_flag = br.u1()? != 0;
            let num_ticks_poc_diff_one_minus1 = if poc_proportional_to_timing_flag {
                // §E.3.1 range 0..=2^32 − 2, which is the ue(v) ceiling;
                // the reader already enforces it, so no extra check.
                Some(br.ue()?)
            } else {
                None
            };
            let hrd_parameters_present_flag = br.u1()? != 0;
            let hrd_parameters = if hrd_parameters_present_flag {
                // §E.2.1: hrd_parameters( 1, sps_max_sub_layers_minus1 ).
                // commonInfPresentFlag is 1, so no inheritance source is
                // needed.
                Some(HrdParameters::parse(
                    br,
                    true,
                    sps_max_sub_layers_minus1,
                    None,
                )?)
            } else {
                None
            };
            Some(VuiTimingInfo {
                num_units_in_tick,
                time_scale,
                poc_proportional_to_timing_flag,
                num_ticks_poc_diff_one_minus1,
                hrd_parameters_present_flag,
                hrd_parameters,
            })
        } else {
            None
        };

        let bitstream_restriction_flag = br.u1()? != 0;
        let bitstream_restriction = if bitstream_restriction_flag {
            let tiles_fixed_structure_flag = br.u1()? != 0;
            let motion_vectors_over_pic_boundaries_flag = br.u1()? != 0;
            let restricted_ref_pic_lists_flag = br.u1()? != 0;
            let min_spatial_segmentation_idc = br.ue()?;
            // §E.3.1: range 0..=4095, inclusive.
            if min_spatial_segmentation_idc > 4095 {
                return Err(VuiError::ValueOutOfRange {
                    field: "min_spatial_segmentation_idc",
                    got: min_spatial_segmentation_idc,
                });
            }
            let max_bytes_per_pic_denom = br.ue()?;
            // §E.3.1: range 0..=16, inclusive.
            if max_bytes_per_pic_denom > 16 {
                return Err(VuiError::ValueOutOfRange {
                    field: "max_bytes_per_pic_denom",
                    got: max_bytes_per_pic_denom,
                });
            }
            let max_bits_per_min_cu_denom = br.ue()?;
            // §E.3.1: range 0..=16, inclusive.
            if max_bits_per_min_cu_denom > 16 {
                return Err(VuiError::ValueOutOfRange {
                    field: "max_bits_per_min_cu_denom",
                    got: max_bits_per_min_cu_denom,
                });
            }
            let log2_max_mv_length_horizontal = br.ue()?;
            // §E.3.1: range 0..=15, inclusive.
            if log2_max_mv_length_horizontal > 15 {
                return Err(VuiError::ValueOutOfRange {
                    field: "log2_max_mv_length_horizontal",
                    got: log2_max_mv_length_horizontal,
                });
            }
            let log2_max_mv_length_vertical = br.ue()?;
            if log2_max_mv_length_vertical > 15 {
                return Err(VuiError::ValueOutOfRange {
                    field: "log2_max_mv_length_vertical",
                    got: log2_max_mv_length_vertical,
                });
            }
            Some(BitstreamRestriction {
                tiles_fixed_structure_flag,
                motion_vectors_over_pic_boundaries_flag,
                restricted_ref_pic_lists_flag,
                min_spatial_segmentation_idc,
                max_bytes_per_pic_denom,
                max_bits_per_min_cu_denom,
                log2_max_mv_length_horizontal,
                log2_max_mv_length_vertical,
            })
        } else {
            None
        };

        Ok(Self {
            aspect_ratio_info_present_flag,
            aspect_ratio_idc,
            sar_width,
            sar_height,
            overscan_info_present_flag,
            overscan_appropriate_flag,
            video_signal_type_present_flag,
            video_signal_type,
            chroma_loc_info_present_flag,
            chroma_sample_loc_type_top_field,
            chroma_sample_loc_type_bottom_field,
            neutral_chroma_indication_flag,
            field_seq_flag,
            frame_field_info_present_flag,
            default_display_window_flag,
            default_display_window,
            vui_timing_info_present_flag,
            timing_info,
            bitstream_restriction_flag,
            bitstream_restriction,
        })
    }
}
