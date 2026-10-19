// SPDX-License-Identifier: MIT
// Derived from oxideav-h265 0.0.10, Copyright (c) 2026 Karpelès Lab Inc.
// Retained syntax/metadata primitives used by this client; see COPYING.OxideAV in this crate.

use crate::bitreader::{BitReader, BitReaderError};

use crate::vps::HEVC_MAX_SUB_LAYERS;

pub const HEVC_MAX_CPB_CNT: usize = 32;

pub const HEVC_MAX_ELEMENTAL_DURATION_IN_TC_MINUS1: u32 = 2047;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HrdError {
    Truncated,
    ValueOutOfRange { field: &'static str, got: u32 },
    InvalidMaxNumSubLayers { got: u8 },
    Bitstream(BitReaderError),
}

impl core::fmt::Display for HrdError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Truncated => f.write_str("hrd_parameters() RBSP truncated"),
            Self::ValueOutOfRange { field, got } => {
                write!(f, "syntax element {field} out of range: {got}")
            }
            Self::InvalidMaxNumSubLayers { got } => write!(
                f,
                "maxNumSubLayersMinus1 = {got} exceeds the HEVC u(3) ceiling of 6"
            ),
            Self::Bitstream(e) => write!(f, "bitstream error during hrd_parameters() parse: {e}"),
        }
    }
}

impl std::error::Error for HrdError {}

impl From<BitReaderError> for HrdError {
    fn from(e: BitReaderError) -> Self {
        match e {
            BitReaderError::EndOfBuffer => Self::Truncated,
            other => Self::Bitstream(other),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HrdCommonInfo {
    pub nal_hrd_parameters_present_flag: bool,
    pub vcl_hrd_parameters_present_flag: bool,
    pub sub_pic_hrd_params_present_flag: bool,
    pub tick_divisor_minus2: u8,
    pub du_cpb_removal_delay_increment_length_minus1: u8,
    pub sub_pic_cpb_params_in_pic_timing_sei_flag: bool,
    pub dpb_output_delay_du_length_minus1: u8,
    pub bit_rate_scale: u8,
    pub cpb_size_scale: u8,
    pub cpb_size_du_scale: u8,
    pub initial_cpb_removal_delay_length_minus1: u8,
    pub au_cpb_removal_delay_length_minus1: u8,
    pub dpb_output_delay_length_minus1: u8,
}

impl HrdCommonInfo {
    pub fn has_any_hrd(&self) -> bool {
        self.nal_hrd_parameters_present_flag || self.vcl_hrd_parameters_present_flag
    }

    fn parse(br: &mut BitReader<'_>) -> Result<Self, HrdError> {
        let nal_hrd_parameters_present_flag = br.u1()? != 0;
        let vcl_hrd_parameters_present_flag = br.u1()? != 0;
        let mut info = Self {
            nal_hrd_parameters_present_flag,
            vcl_hrd_parameters_present_flag,
            sub_pic_hrd_params_present_flag: false,
            tick_divisor_minus2: 0,
            du_cpb_removal_delay_increment_length_minus1: 0,
            sub_pic_cpb_params_in_pic_timing_sei_flag: false,
            dpb_output_delay_du_length_minus1: 0,
            bit_rate_scale: 0,
            cpb_size_scale: 0,
            cpb_size_du_scale: 0,
            initial_cpb_removal_delay_length_minus1: 0,
            au_cpb_removal_delay_length_minus1: 0,
            dpb_output_delay_length_minus1: 0,
        };
        if nal_hrd_parameters_present_flag || vcl_hrd_parameters_present_flag {
            info.sub_pic_hrd_params_present_flag = br.u1()? != 0;
            if info.sub_pic_hrd_params_present_flag {
                info.tick_divisor_minus2 = br.u(8)? as u8;
                info.du_cpb_removal_delay_increment_length_minus1 = br.u(5)? as u8;
                info.sub_pic_cpb_params_in_pic_timing_sei_flag = br.u1()? != 0;
                info.dpb_output_delay_du_length_minus1 = br.u(5)? as u8;
            }
            info.bit_rate_scale = br.u(4)? as u8;
            info.cpb_size_scale = br.u(4)? as u8;
            if info.sub_pic_hrd_params_present_flag {
                info.cpb_size_du_scale = br.u(4)? as u8;
            }
            info.initial_cpb_removal_delay_length_minus1 = br.u(5)? as u8;
            info.au_cpb_removal_delay_length_minus1 = br.u(5)? as u8;
            info.dpb_output_delay_length_minus1 = br.u(5)? as u8;
        }
        Ok(info)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct CpbEntry {
    pub bit_rate_value_minus1: u32,
    pub cpb_size_value_minus1: u32,
    pub cpb_size_du_value_minus1: Option<u32>,
    pub bit_rate_du_value_minus1: Option<u32>,
    pub cbr_flag: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SubLayerHrdParameters {
    pub cpb: Vec<CpbEntry>,
}

impl SubLayerHrdParameters {
    pub fn parse(
        br: &mut BitReader<'_>,
        cpb_cnt: u32,
        sub_pic_hrd_params_present: bool,
    ) -> Result<Self, HrdError> {
        // cpb_cnt = cpb_cnt_minus1 + 1; cpb_cnt_minus1 is in 0..=31, so
        // cpb_cnt is in 1..=32. Bound for safety against a corrupt
        // parent that supplies a larger value.
        if cpb_cnt == 0 || cpb_cnt as usize > HEVC_MAX_CPB_CNT {
            return Err(HrdError::ValueOutOfRange {
                field: "CpbCnt",
                got: cpb_cnt,
            });
        }
        let mut cpb = Vec::with_capacity(cpb_cnt as usize);
        let mut prev_bit_rate: Option<u32> = None;
        let mut prev_cpb_size: Option<u32> = None;
        let mut prev_cpb_size_du: Option<u32> = None;
        let mut prev_bit_rate_du: Option<u32> = None;
        for _ in 0..cpb_cnt {
            let bit_rate_value_minus1 = br.ue()?;
            // §E.3.3: "For any i > 0, bit_rate_value_minus1[ i ] shall
            // be greater than bit_rate_value_minus1[ i − 1 ]."
            if let Some(prev) = prev_bit_rate {
                if bit_rate_value_minus1 <= prev {
                    return Err(HrdError::ValueOutOfRange {
                        field: "bit_rate_value_minus1",
                        got: bit_rate_value_minus1,
                    });
                }
            }
            prev_bit_rate = Some(bit_rate_value_minus1);

            let cpb_size_value_minus1 = br.ue()?;
            // §E.3.3: "For any i greater than 0, cpb_size_value_minus1[ i ]
            // shall be less than or equal to cpb_size_value_minus1[ i − 1 ]."
            if let Some(prev) = prev_cpb_size {
                if cpb_size_value_minus1 > prev {
                    return Err(HrdError::ValueOutOfRange {
                        field: "cpb_size_value_minus1",
                        got: cpb_size_value_minus1,
                    });
                }
            }
            prev_cpb_size = Some(cpb_size_value_minus1);

            let (cpb_size_du_value_minus1, bit_rate_du_value_minus1) = if sub_pic_hrd_params_present
            {
                let du_cpb = br.ue()?;
                // §E.3.3: "For any i greater than 0,
                // cpb_size_du_value_minus1[ i ] shall be less than or
                // equal to cpb_size_du_value_minus1[ i − 1 ]."
                if let Some(prev) = prev_cpb_size_du {
                    if du_cpb > prev {
                        return Err(HrdError::ValueOutOfRange {
                            field: "cpb_size_du_value_minus1",
                            got: du_cpb,
                        });
                    }
                }
                prev_cpb_size_du = Some(du_cpb);
                let du_bit = br.ue()?;
                // §E.3.3: "For any i > 0, bit_rate_du_value_minus1[ i ]
                // shall be greater than bit_rate_du_value_minus1[ i − 1 ]."
                if let Some(prev) = prev_bit_rate_du {
                    if du_bit <= prev {
                        return Err(HrdError::ValueOutOfRange {
                            field: "bit_rate_du_value_minus1",
                            got: du_bit,
                        });
                    }
                }
                prev_bit_rate_du = Some(du_bit);
                (Some(du_cpb), Some(du_bit))
            } else {
                (None, None)
            };

            let cbr_flag = br.u1()? != 0;

            cpb.push(CpbEntry {
                bit_rate_value_minus1,
                cpb_size_value_minus1,
                cpb_size_du_value_minus1,
                bit_rate_du_value_minus1,
                cbr_flag,
            });
        }
        Ok(Self { cpb })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SubLayerHrd {
    pub fixed_pic_rate_general_flag: bool,
    pub fixed_pic_rate_within_cvs_flag: bool,
    pub elemental_duration_in_tc_minus1: Option<u32>,
    pub low_delay_hrd_flag: bool,
    pub cpb_cnt_minus1: u32,
    pub nal_hrd: Option<SubLayerHrdParameters>,
    pub vcl_hrd: Option<SubLayerHrdParameters>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HrdParameters {
    pub max_num_sub_layers_minus1: u8,
    pub common: Option<HrdCommonInfo>,
    pub sub_layers: Vec<SubLayerHrd>,
}

impl HrdParameters {
    pub fn parse(
        br: &mut BitReader<'_>,
        common_inf_present: bool,
        max_num_sub_layers_minus1: u8,
        prev_common: Option<&HrdCommonInfo>,
    ) -> Result<Self, HrdError> {
        if max_num_sub_layers_minus1 as usize > HEVC_MAX_SUB_LAYERS {
            return Err(HrdError::InvalidMaxNumSubLayers {
                got: max_num_sub_layers_minus1,
            });
        }
        let common = if common_inf_present {
            Some(HrdCommonInfo::parse(br)?)
        } else {
            None
        };
        // Pick the effective common info: the freshly-parsed block if
        // present, else the inherited one from prev_common. When neither
        // is available, sub-layer parsing falls back to "no HRD gates",
        // which matches the spec's silent-default behaviour.
        let effective = common.as_ref().or(prev_common);
        let nal_hrd_present = effective.is_some_and(|c| c.nal_hrd_parameters_present_flag);
        let vcl_hrd_present = effective.is_some_and(|c| c.vcl_hrd_parameters_present_flag);
        let sub_pic_present = effective.is_some_and(|c| c.sub_pic_hrd_params_present_flag);

        let count = max_num_sub_layers_minus1 as usize + 1;
        let mut sub_layers = Vec::with_capacity(count);
        for _ in 0..count {
            let fixed_pic_rate_general_flag = br.u1()? != 0;
            // §E.3.2: when fixed_pic_rate_general_flag[i] == 1, the
            // value of fixed_pic_rate_within_cvs_flag[i] is inferred to
            // be equal to 1. Otherwise we read the explicit bit.
            let fixed_pic_rate_within_cvs_flag = if fixed_pic_rate_general_flag {
                true
            } else {
                br.u1()? != 0
            };
            let mut sl = SubLayerHrd {
                fixed_pic_rate_general_flag,
                fixed_pic_rate_within_cvs_flag,
                ..SubLayerHrd::default()
            };
            if fixed_pic_rate_within_cvs_flag {
                let v = br.ue()?;
                // §E.3.2: "elemental_duration_in_tc_minus1[ i ] shall
                // be in the range of 0 to 2 047, inclusive."
                if v > HEVC_MAX_ELEMENTAL_DURATION_IN_TC_MINUS1 {
                    return Err(HrdError::ValueOutOfRange {
                        field: "elemental_duration_in_tc_minus1",
                        got: v,
                    });
                }
                sl.elemental_duration_in_tc_minus1 = Some(v);
            } else {
                sl.low_delay_hrd_flag = br.u1()? != 0;
            }
            if !sl.low_delay_hrd_flag {
                let v = br.ue()?;
                // §E.3.2: "cpb_cnt_minus1[ i ] shall be in the range
                // of 0 to 31, inclusive."
                if v as usize >= HEVC_MAX_CPB_CNT {
                    return Err(HrdError::ValueOutOfRange {
                        field: "cpb_cnt_minus1",
                        got: v,
                    });
                }
                sl.cpb_cnt_minus1 = v;
            } else {
                // §E.3.2: "When not present, the value of
                // cpb_cnt_minus1[ i ] is inferred to be equal to 0."
                sl.cpb_cnt_minus1 = 0;
            }
            if nal_hrd_present {
                sl.nal_hrd = Some(SubLayerHrdParameters::parse(
                    br,
                    sl.cpb_cnt_minus1 + 1,
                    sub_pic_present,
                )?);
            }
            if vcl_hrd_present {
                sl.vcl_hrd = Some(SubLayerHrdParameters::parse(
                    br,
                    sl.cpb_cnt_minus1 + 1,
                    sub_pic_present,
                )?);
            }
            sub_layers.push(sl);
        }
        Ok(Self {
            max_num_sub_layers_minus1,
            common,
            sub_layers,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VpsHrdEntry {
    pub hrd_layer_set_idx: u32,
    pub cprms_present_flag: bool,
    pub hrd: HrdParameters,
}

impl VpsHrdEntry {
    pub fn parse(
        br: &mut BitReader<'_>,
        index: u32,
        vps_max_sub_layers_minus1: u8,
        prev: Option<&VpsHrdEntry>,
    ) -> Result<Self, HrdError> {
        let hrd_layer_set_idx = br.ue()?;
        // Spec-side allocation safety: hrd_layer_set_idx is bounded by
        // vps_num_layer_sets_minus1 + 1 (= 1024). Anything larger is
        // guaranteed-out-of-range regardless of VPS context; cap here
        // so the cross-check against the active VPS in §7.4.3.1 is
        // free to look at a believable value.
        if hrd_layer_set_idx > crate::vps::HEVC_VPS_MAX_NUM_LAYER_SETS as u32 {
            return Err(HrdError::ValueOutOfRange {
                field: "hrd_layer_set_idx",
                got: hrd_layer_set_idx,
            });
        }
        let cprms_present_flag = if index > 0 { br.u1()? != 0 } else { true };
        let hrd = HrdParameters::parse(
            br,
            cprms_present_flag,
            vps_max_sub_layers_minus1,
            prev.map(|p| &p.hrd).and_then(|h| h.common.as_ref()),
        )?;
        Ok(Self {
            hrd_layer_set_idx,
            cprms_present_flag,
            hrd,
        })
    }
}
