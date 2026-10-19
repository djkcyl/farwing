// SPDX-License-Identifier: MIT
// Derived from oxideav-h265 0.0.10, Copyright (c) 2026 Karpelès Lab Inc.
// Retained syntax/metadata primitives used by this client; see COPYING.OxideAV in this crate.

use crate::bitreader::{BitReader, BitReaderError};

use crate::scaling_list::{ScalingListData, ScalingListError};

use crate::vps::{HEVC_MAX_SUB_LAYERS, ProfileTierLevel, SubLayerOrderingInfo, VpsError};

use crate::vui::{VuiError, VuiParameters};

pub const HEVC_MAX_NUM_SHORT_TERM_RPS: usize = 64;

pub const HEVC_MAX_NUM_LONG_TERM_RPS: usize = 32;

pub const HEVC_MAX_RPS_PICS: usize = 16;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SpsError {
    Truncated,
    ValueOutOfRange { field: &'static str, got: u32 },
    ScalingList(ScalingListError),
    Bitstream(BitReaderError),
    Ptl(VpsError),
    Vui(VuiError),
}

impl core::fmt::Display for SpsError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Truncated => f.write_str("SPS RBSP truncated"),
            Self::ValueOutOfRange { field, got } => {
                write!(f, "SPS syntax element {field} out of range: {got}")
            }
            Self::ScalingList(e) => write!(f, "scaling-list error during SPS parse: {e}"),
            Self::Bitstream(e) => write!(f, "bitstream error during SPS parse: {e}"),
            Self::Ptl(e) => write!(f, "profile_tier_level error during SPS parse: {e}"),
            Self::Vui(e) => write!(f, "vui_parameters error during SPS parse: {e}"),
        }
    }
}

impl std::error::Error for SpsError {}

impl From<BitReaderError> for SpsError {
    fn from(e: BitReaderError) -> Self {
        match e {
            BitReaderError::EndOfBuffer => Self::Truncated,
            other => Self::Bitstream(other),
        }
    }
}

impl From<VpsError> for SpsError {
    fn from(e: VpsError) -> Self {
        // Surface the inner reader-truncation directly so callers can
        // distinguish "ran off the end mid-PTL" from other PTL faults.
        if matches!(e, VpsError::Truncated) {
            Self::Truncated
        } else {
            Self::Ptl(e)
        }
    }
}

impl From<ScalingListError> for SpsError {
    fn from(e: ScalingListError) -> Self {
        // Flatten truncation / raw-reader faults to the SPS-level
        // equivalents so the public surface stays predictable; carry
        // the structured scaling-list faults through as-is.
        match e {
            ScalingListError::Truncated => Self::Truncated,
            ScalingListError::Bitstream(b) => Self::Bitstream(b),
            other => Self::ScalingList(other),
        }
    }
}

impl From<VuiError> for SpsError {
    fn from(e: VuiError) -> Self {
        // Flatten truncation / raw-reader faults to the SPS-level
        // equivalents so the public surface stays predictable; carry
        // the structured VUI faults through as-is.
        match e {
            VuiError::Truncated => Self::Truncated,
            VuiError::Bitstream(b) => Self::Bitstream(b),
            other => Self::Vui(other),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ConformanceWindow {
    pub left_offset: u32,
    pub right_offset: u32,
    pub top_offset: u32,
    pub bottom_offset: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PcmInfo {
    pub bit_depth_luma_minus1: u8,
    pub bit_depth_chroma_minus1: u8,
    pub log2_min_pcm_luma_coding_block_size_minus3: u8,
    pub log2_diff_max_min_pcm_luma_coding_block_size: u8,
    pub loop_filter_disabled_flag: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ShortTermRefPicSet {
    pub inter_ref_pic_set_prediction_flag: bool,
    pub delta_idx_minus1: u32,
    pub delta_rps_sign: bool,
    pub abs_delta_rps_minus1: u32,
    pub used_by_curr_pic_flag: Vec<bool>,
    pub use_delta_flag: Vec<bool>,
    pub num_negative_pics: u32,
    pub num_positive_pics: u32,
    pub delta_poc_s0_minus1: Vec<u32>,
    pub used_by_curr_pic_s0_flag: Vec<bool>,
    pub delta_poc_s1_minus1: Vec<u32>,
    pub used_by_curr_pic_s1_flag: Vec<bool>,
}

impl ShortTermRefPicSet {
    pub fn num_delta_pocs(&self) -> u32 {
        if self.inter_ref_pic_set_prediction_flag {
            self.use_delta_flag.iter().filter(|&&v| v).count() as u32
        } else {
            self.num_negative_pics + self.num_positive_pics
        }
    }

    pub fn materialize(
        &self,
        source: Option<&MaterializedShortTermRefPicSet>,
    ) -> Result<MaterializedShortTermRefPicSet, ShortTermRefPicSetMaterializeError> {
        if self.inter_ref_pic_set_prediction_flag {
            let source = source.ok_or(ShortTermRefPicSetMaterializeError::MissingSource)?;
            // `deltaRps` per equation 7-60. The bit-width of
            // `abs_delta_rps_minus1` is bounded by `ue(v)` plus the
            // §7.4.8 range check (<= 2^15-1 -> deltaRps fits in i32).
            let abs = self.abs_delta_rps_minus1 as i64 + 1;
            let delta_rps = if self.delta_rps_sign { -abs } else { abs } as i32;
            let num_neg_src = source.num_negative_pics();
            let num_pos_src = source.num_positive_pics();
            let num_delta_src = source.num_delta_pocs(); // == num_neg_src + num_pos_src
            // Per §7.4.8 the `used_by_curr_pic_flag` / `use_delta_flag`
            // arrays have length `NumDeltaPocs[RefRpsIdx] + 1`.
            let expected = (num_delta_src + 1) as usize;
            if self.used_by_curr_pic_flag.len() != expected || self.use_delta_flag.len() != expected
            {
                return Err(ShortTermRefPicSetMaterializeError::SourceLengthMismatch {
                    expected: expected as u32,
                    got_used: self.used_by_curr_pic_flag.len(),
                    got_delta: self.use_delta_flag.len(),
                });
            }
            // Negative side (equation 7-61): walk source's positive
            // POCs in reverse (their `dPoc = DeltaPocS1[RefRpsIdx][j]
            // + deltaRps` may have crossed zero), then optionally
            // `deltaRps` itself (only if negative), then source's
            // negative POCs in forward order.
            let mut delta_poc_s0 = Vec::new();
            let mut used_by_curr_pic_s0 = Vec::new();
            for j in (0..num_pos_src).rev() {
                let d_poc = source.delta_poc_s1[j as usize] + delta_rps;
                if d_poc < 0 && self.use_delta_flag[(num_neg_src + j) as usize] {
                    delta_poc_s0.push(d_poc);
                    used_by_curr_pic_s0
                        .push(self.used_by_curr_pic_flag[(num_neg_src + j) as usize]);
                }
            }
            if delta_rps < 0 && self.use_delta_flag[num_delta_src as usize] {
                delta_poc_s0.push(delta_rps);
                used_by_curr_pic_s0.push(self.used_by_curr_pic_flag[num_delta_src as usize]);
            }
            for j in 0..num_neg_src {
                let d_poc = source.delta_poc_s0[j as usize] + delta_rps;
                if d_poc < 0 && self.use_delta_flag[j as usize] {
                    delta_poc_s0.push(d_poc);
                    used_by_curr_pic_s0.push(self.used_by_curr_pic_flag[j as usize]);
                }
            }
            // Positive side (equation 7-62): walk source's negative
            // POCs in reverse (their `dPoc = DeltaPocS0[RefRpsIdx][j]
            // + deltaRps` may have crossed zero), then optionally
            // `deltaRps` itself (only if positive), then source's
            // positive POCs in forward order.
            let mut delta_poc_s1 = Vec::new();
            let mut used_by_curr_pic_s1 = Vec::new();
            for j in (0..num_neg_src).rev() {
                let d_poc = source.delta_poc_s0[j as usize] + delta_rps;
                if d_poc > 0 && self.use_delta_flag[j as usize] {
                    delta_poc_s1.push(d_poc);
                    used_by_curr_pic_s1.push(self.used_by_curr_pic_flag[j as usize]);
                }
            }
            if delta_rps > 0 && self.use_delta_flag[num_delta_src as usize] {
                delta_poc_s1.push(delta_rps);
                used_by_curr_pic_s1.push(self.used_by_curr_pic_flag[num_delta_src as usize]);
            }
            for j in 0..num_pos_src {
                let d_poc = source.delta_poc_s1[j as usize] + delta_rps;
                if d_poc > 0 && self.use_delta_flag[(num_neg_src + j) as usize] {
                    delta_poc_s1.push(d_poc);
                    used_by_curr_pic_s1
                        .push(self.used_by_curr_pic_flag[(num_neg_src + j) as usize]);
                }
            }
            Ok(MaterializedShortTermRefPicSet {
                delta_poc_s0,
                used_by_curr_pic_s0,
                delta_poc_s1,
                used_by_curr_pic_s1,
            })
        } else {
            // Explicit form, equations 7-63..7-70.
            let mut delta_poc_s0 = Vec::with_capacity(self.num_negative_pics as usize);
            let mut prev: i32 = 0;
            for (i, &delta_minus1) in self.delta_poc_s0_minus1.iter().enumerate() {
                // delta_minus1 has been range-checked to 0..=2^15-1 on
                // parse; the cumulative sum cannot exceed i32 capacity
                // given num_negative_pics <= 16.
                let step = delta_minus1 as i32 + 1;
                let d = if i == 0 { -step } else { prev - step };
                delta_poc_s0.push(d);
                prev = d;
            }
            let mut delta_poc_s1 = Vec::with_capacity(self.num_positive_pics as usize);
            let mut prev: i32 = 0;
            for (i, &delta_minus1) in self.delta_poc_s1_minus1.iter().enumerate() {
                let step = delta_minus1 as i32 + 1;
                let d = if i == 0 { step } else { prev + step };
                delta_poc_s1.push(d);
                prev = d;
            }
            Ok(MaterializedShortTermRefPicSet {
                delta_poc_s0,
                used_by_curr_pic_s0: self.used_by_curr_pic_s0_flag.clone(),
                delta_poc_s1,
                used_by_curr_pic_s1: self.used_by_curr_pic_s1_flag.clone(),
            })
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShortTermRefPicSetMaterializeError {
    MissingSource,
    SourceLengthMismatch {
        expected: u32,
        got_used: usize,
        got_delta: usize,
    },
}

impl core::fmt::Display for ShortTermRefPicSetMaterializeError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::MissingSource => f.write_str(
                "short-term RPS materialise: inter_ref_pic_set_prediction_flag is set but no source RPS supplied",
            ),
            Self::SourceLengthMismatch {
                expected,
                got_used,
                got_delta,
            } => write!(
                f,
                "short-term RPS materialise: per-position array length mismatch: expected {expected}, got used={got_used} delta={got_delta}"
            ),
        }
    }
}

impl std::error::Error for ShortTermRefPicSetMaterializeError {}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MaterializedShortTermRefPicSet {
    pub delta_poc_s0: Vec<i32>,
    pub used_by_curr_pic_s0: Vec<bool>,
    pub delta_poc_s1: Vec<i32>,
    pub used_by_curr_pic_s1: Vec<bool>,
}

impl MaterializedShortTermRefPicSet {
    pub fn num_negative_pics(&self) -> u32 {
        self.delta_poc_s0.len() as u32
    }
    pub fn num_positive_pics(&self) -> u32 {
        self.delta_poc_s1.len() as u32
    }
    pub fn num_delta_pocs(&self) -> u32 {
        self.num_negative_pics() + self.num_positive_pics()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LongTermRefPicEntry {
    pub poc_lsb: u32,
    pub used_by_curr_pic: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpaqueTail {
    pub bytes: Vec<u8>,
    pub start_bit_in_first_byte: u8,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct SpsExtensionFlags {
    pub sps_range_extension_flag: bool,
    pub sps_multilayer_extension_flag: bool,
    pub sps_3d_extension_flag: bool,
    pub sps_scc_extension_flag: bool,
    pub sps_extension_4bits: u8,
}

impl SpsExtensionFlags {
    pub fn has_body(&self) -> bool {
        self.sps_range_extension_flag
            || self.sps_multilayer_extension_flag
            || self.sps_3d_extension_flag
            || self.sps_scc_extension_flag
            || self.sps_extension_4bits != 0
    }

    fn scc_decodable_in_place(&self) -> bool {
        self.sps_scc_extension_flag
            && !self.sps_multilayer_extension_flag
            && !self.sps_3d_extension_flag
    }

    fn has_opaque_body_after_decoded(&self) -> bool {
        if self.sps_multilayer_extension_flag || self.sps_3d_extension_flag {
            // The first un-decoded body is the multilayer / 3D one;
            // everything from there (incl. any SCC body) is opaque.
            return true;
        }
        // No multilayer / 3D body: SCC (if present) was decoded in
        // place, so only the sps_extension_data_flag while-loop may
        // remain.
        self.sps_extension_4bits != 0
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SpsSccExtension {
    pub sps_curr_pic_ref_enabled_flag: bool,
    pub palette_mode_enabled_flag: bool,
    pub palette_max_size: u32,
    pub delta_palette_max_predictor_size: u32,
    pub sps_palette_predictor_initializers_present_flag: bool,
    pub sps_num_palette_predictor_initializers_minus1: u32,
    pub sps_palette_predictor_initializer: Vec<Vec<u32>>,
    pub motion_vector_resolution_control_idc: u8,
    pub intra_boundary_filtering_disabled_flag: bool,
}

impl SpsSccExtension {
    fn parse(
        br: &mut BitReader,
        chroma_format_idc: u8,
        bit_depth_luma: u8,
        bit_depth_chroma: u8,
    ) -> Result<Self, SpsError> {
        let sps_curr_pic_ref_enabled_flag = br.u1()? != 0;
        let palette_mode_enabled_flag = br.u1()? != 0;
        let mut palette_max_size = 0u32;
        let mut delta_palette_max_predictor_size = 0u32;
        let mut sps_palette_predictor_initializers_present_flag = false;
        let mut sps_num_palette_predictor_initializers_minus1 = 0u32;
        let mut sps_palette_predictor_initializer = Vec::new();
        if palette_mode_enabled_flag {
            palette_max_size = br.ue()?;
            delta_palette_max_predictor_size = br.ue()?;
            // §7.4.3.2.3: when palette_max_size == 0 the
            // delta_palette_max_predictor_size must be 0 (bitstream
            // conformance — a zero-size palette cannot grow the
            // predictor).
            if palette_max_size == 0 && delta_palette_max_predictor_size != 0 {
                return Err(SpsError::ValueOutOfRange {
                    field: "delta_palette_max_predictor_size",
                    got: delta_palette_max_predictor_size,
                });
            }
            sps_palette_predictor_initializers_present_flag = br.u1()? != 0;
            // §7.4.3.2.3: likewise the initializers-present flag must be
            // 0 when palette_max_size == 0.
            if palette_max_size == 0 && sps_palette_predictor_initializers_present_flag {
                return Err(SpsError::ValueOutOfRange {
                    field: "sps_palette_predictor_initializers_present_flag",
                    got: 1,
                });
            }
            if sps_palette_predictor_initializers_present_flag {
                sps_num_palette_predictor_initializers_minus1 = br.ue()?;
                // §7.4.3.2.3: bounded by PaletteMaxPredictorSize − 1,
                // itself capped by the profile limits (§A.3.7:
                // PaletteMaxPredictorSize <= 128); reject anything past
                // the largest representable predictor so a malformed
                // count cannot drive the initializer allocation.
                if sps_num_palette_predictor_initializers_minus1 >= 128 {
                    return Err(SpsError::ValueOutOfRange {
                        field: "sps_num_palette_predictor_initializers_minus1",
                        got: sps_num_palette_predictor_initializers_minus1,
                    });
                }
                let num_comps = if chroma_format_idc == 0 { 1 } else { 3 };
                let num_entries = sps_num_palette_predictor_initializers_minus1 as usize + 1;
                sps_palette_predictor_initializer.reserve(num_comps);
                for comp in 0..num_comps {
                    let width = if comp == 0 {
                        bit_depth_luma
                    } else {
                        bit_depth_chroma
                    };
                    let mut row = Vec::with_capacity(num_entries);
                    for _ in 0..num_entries {
                        row.push(br.u(width)?);
                    }
                    sps_palette_predictor_initializer.push(row);
                }
            }
        }
        let motion_vector_resolution_control_idc = br.u(2)? as u8;
        // §7.4.3.2.3: the value 3 is reserved for future use and must
        // not appear in a conforming bitstream of this version.
        if motion_vector_resolution_control_idc == 3 {
            return Err(SpsError::ValueOutOfRange {
                field: "motion_vector_resolution_control_idc",
                got: 3,
            });
        }
        let intra_boundary_filtering_disabled_flag = br.u1()? != 0;
        Ok(Self {
            sps_curr_pic_ref_enabled_flag,
            palette_mode_enabled_flag,
            palette_max_size,
            delta_palette_max_predictor_size,
            sps_palette_predictor_initializers_present_flag,
            sps_num_palette_predictor_initializers_minus1,
            sps_palette_predictor_initializer,
            motion_vector_resolution_control_idc,
            intra_boundary_filtering_disabled_flag,
        })
    }

    pub fn palette_max_predictor_size(&self) -> u32 {
        self.palette_max_size + self.delta_palette_max_predictor_size
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct SpsRangeExtension {
    pub transform_skip_rotation_enabled_flag: bool,
    pub transform_skip_context_enabled_flag: bool,
    pub implicit_rdpcm_enabled_flag: bool,
    pub explicit_rdpcm_enabled_flag: bool,
    pub extended_precision_processing_flag: bool,
    pub intra_smoothing_disabled_flag: bool,
    pub high_precision_offsets_enabled_flag: bool,
    pub persistent_rice_adaptation_enabled_flag: bool,
    pub cabac_bypass_alignment_enabled_flag: bool,
}

impl SpsRangeExtension {
    fn parse(br: &mut BitReader) -> Result<Self, SpsError> {
        Ok(Self {
            transform_skip_rotation_enabled_flag: br.u1()? != 0,
            transform_skip_context_enabled_flag: br.u1()? != 0,
            implicit_rdpcm_enabled_flag: br.u1()? != 0,
            explicit_rdpcm_enabled_flag: br.u1()? != 0,
            extended_precision_processing_flag: br.u1()? != 0,
            intra_smoothing_disabled_flag: br.u1()? != 0,
            high_precision_offsets_enabled_flag: br.u1()? != 0,
            persistent_rice_adaptation_enabled_flag: br.u1()? != 0,
            cabac_bypass_alignment_enabled_flag: br.u1()? != 0,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SeqParameterSet {
    pub vps_id: u8,
    pub max_sub_layers_minus1: u8,
    pub temporal_id_nesting_flag: bool,
    pub ptl: ProfileTierLevel,
    pub sps_id: u8,
    pub chroma_format_idc: u8,
    pub separate_colour_plane_flag: bool,
    pub pic_width_in_luma_samples: u32,
    pub pic_height_in_luma_samples: u32,
    pub conformance_window_flag: bool,
    pub conformance_window: ConformanceWindow,
    pub bit_depth_luma_minus8: u8,
    pub bit_depth_chroma_minus8: u8,
    pub log2_max_pic_order_cnt_lsb_minus4: u8,
    pub sub_layer_ordering_info_present_flag: bool,
    pub sub_layer_ordering_info: [SubLayerOrderingInfo; HEVC_MAX_SUB_LAYERS],
    pub log2_min_luma_coding_block_size_minus3: u8,
    pub log2_diff_max_min_luma_coding_block_size: u8,
    pub log2_min_luma_transform_block_size_minus2: u8,
    pub log2_diff_max_min_luma_transform_block_size: u8,
    pub max_transform_hierarchy_depth_inter: u8,
    pub max_transform_hierarchy_depth_intra: u8,
    pub scaling_list_enabled_flag: bool,
    pub sps_scaling_list_data_present_flag: bool,
    pub scaling_list_data: Option<ScalingListData>,
    pub amp_enabled_flag: bool,
    pub sample_adaptive_offset_enabled_flag: bool,
    pub pcm_enabled_flag: bool,
    pub pcm: Option<PcmInfo>,
    pub num_short_term_ref_pic_sets: u32,
    pub short_term_ref_pic_sets: Vec<ShortTermRefPicSet>,
    pub long_term_ref_pics_present_flag: bool,
    pub num_long_term_ref_pics_sps: u32,
    pub long_term_ref_pics: Vec<LongTermRefPicEntry>,
    pub sps_temporal_mvp_enabled_flag: bool,
    pub strong_intra_smoothing_enabled_flag: bool,
    pub vui_parameters_present_flag: bool,
    pub vui_parameters: Option<VuiParameters>,
    pub sps_extension_present_flag: bool,
    pub extension_flags: Option<SpsExtensionFlags>,
    pub sps_range_extension: Option<SpsRangeExtension>,
    pub sps_scc_extension: Option<SpsSccExtension>,
    pub opaque_tail: Option<OpaqueTail>,
}

impl SeqParameterSet {
    pub fn parse(rbsp: &[u8]) -> Result<Self, SpsError> {
        let mut br = BitReader::new(rbsp);
        Self::parse_inner(&mut br, rbsp)
    }

    pub fn materialize_short_term_ref_pic_sets(
        &self,
    ) -> Result<Vec<MaterializedShortTermRefPicSet>, ShortTermRefPicSetMaterializeError> {
        let mut out: Vec<MaterializedShortTermRefPicSet> =
            Vec::with_capacity(self.short_term_ref_pic_sets.len());
        for (st_rps_idx, rps) in self.short_term_ref_pic_sets.iter().enumerate() {
            // `RefRpsIdx = stRpsIdx - (delta_idx_minus1 + 1)` per
            // equation 7-59. For the explicit form `source` is unused
            // and `RefRpsIdx` is not derived. For the inter form
            // `delta_idx_minus1` was parsed as 0 for any SPS-resident
            // entry that did not signal it explicitly (the wire signal
            // is only present at the slice-inline call site), which
            // maps to the immediately-preceding entry.
            let source = if rps.inter_ref_pic_set_prediction_flag {
                let ref_rps_idx = (st_rps_idx as i64) - (rps.delta_idx_minus1 as i64 + 1);
                if ref_rps_idx < 0 {
                    return Err(ShortTermRefPicSetMaterializeError::MissingSource);
                }
                out.get(ref_rps_idx as usize)
            } else {
                None
            };
            out.push(rps.materialize(source)?);
        }
        Ok(out)
    }

    fn parse_inner(br: &mut BitReader<'_>, rbsp: &[u8]) -> Result<Self, SpsError> {
        let vps_id = br.u(4)? as u8;
        let max_sub_layers_minus1 = br.u(3)? as u8;
        if max_sub_layers_minus1 > 6 {
            return Err(SpsError::ValueOutOfRange {
                field: "sps_max_sub_layers_minus1",
                got: max_sub_layers_minus1 as u32,
            });
        }
        let temporal_id_nesting_flag = br.u1()? != 0;

        // profile_tier_level( 1, sps_max_sub_layers_minus1 )
        let ptl = ProfileTierLevel::parse(br, true, max_sub_layers_minus1)?;

        let sps_id_raw = br.ue()?;
        if sps_id_raw > 15 {
            return Err(SpsError::ValueOutOfRange {
                field: "sps_seq_parameter_set_id",
                got: sps_id_raw,
            });
        }
        let sps_id = sps_id_raw as u8;

        let chroma_format_idc_raw = br.ue()?;
        if chroma_format_idc_raw > 3 {
            return Err(SpsError::ValueOutOfRange {
                field: "chroma_format_idc",
                got: chroma_format_idc_raw,
            });
        }
        let chroma_format_idc = chroma_format_idc_raw as u8;

        let separate_colour_plane_flag = if chroma_format_idc == 3 {
            br.u1()? != 0
        } else {
            false
        };

        // §A.4.1 items b) / c): each dimension "shall be less than or
        // equal to Sqrt( MaxLumaPs * 8 )". The largest Table A.8
        // MaxLumaPs is 142 606 336 (levels 7 .. 7.2), giving
        // Sqrt( 142 606 336 * 8 ) = 33 776 (integer part). Enforcing
        // the ceiling here keeps every downstream PicWidthInCtbsY /
        // PicSizeInCtbsY derivation (eqs. 7-15 .. 7-19) inside u32.
        const MAX_LUMA_DIMENSION: u32 = 33_776;
        let pic_width_in_luma_samples = br.ue()?;
        if pic_width_in_luma_samples == 0 || pic_width_in_luma_samples > MAX_LUMA_DIMENSION {
            return Err(SpsError::ValueOutOfRange {
                field: "pic_width_in_luma_samples",
                got: pic_width_in_luma_samples,
            });
        }
        let pic_height_in_luma_samples = br.ue()?;
        if pic_height_in_luma_samples == 0 || pic_height_in_luma_samples > MAX_LUMA_DIMENSION {
            return Err(SpsError::ValueOutOfRange {
                field: "pic_height_in_luma_samples",
                got: pic_height_in_luma_samples,
            });
        }

        let conformance_window_flag = br.u1()? != 0;
        let conformance_window = if conformance_window_flag {
            ConformanceWindow {
                left_offset: br.ue()?,
                right_offset: br.ue()?,
                top_offset: br.ue()?,
                bottom_offset: br.ue()?,
            }
        } else {
            ConformanceWindow::default()
        };

        let bit_depth_luma_minus8_raw = br.ue()?;
        if bit_depth_luma_minus8_raw > 8 {
            return Err(SpsError::ValueOutOfRange {
                field: "bit_depth_luma_minus8",
                got: bit_depth_luma_minus8_raw,
            });
        }
        let bit_depth_chroma_minus8_raw = br.ue()?;
        if bit_depth_chroma_minus8_raw > 8 {
            return Err(SpsError::ValueOutOfRange {
                field: "bit_depth_chroma_minus8",
                got: bit_depth_chroma_minus8_raw,
            });
        }

        let log2_max_pic_order_cnt_lsb_minus4_raw = br.ue()?;
        if log2_max_pic_order_cnt_lsb_minus4_raw > 12 {
            return Err(SpsError::ValueOutOfRange {
                field: "log2_max_pic_order_cnt_lsb_minus4",
                got: log2_max_pic_order_cnt_lsb_minus4_raw,
            });
        }

        let sub_layer_ordering_info_present_flag = br.u1()? != 0;
        let last = max_sub_layers_minus1 as usize;
        let start = if sub_layer_ordering_info_present_flag {
            0usize
        } else {
            last
        };
        let mut sub_layer_ordering_info = [SubLayerOrderingInfo::default(); HEVC_MAX_SUB_LAYERS];
        for entry in sub_layer_ordering_info
            .iter_mut()
            .take(last + 1)
            .skip(start)
        {
            let max_dpb = br.ue()?;
            let max_reorder = br.ue()?;
            let max_lat = br.ue()?;
            *entry = SubLayerOrderingInfo {
                max_dec_pic_buffering_minus1: max_dpb,
                max_num_reorder_pics: max_reorder,
                max_latency_increase_plus1: max_lat,
            };
        }
        if !sub_layer_ordering_info_present_flag {
            // §7.4.3.2.1: when the present flag is 0, every lower-indexed
            // sub-layer inherits the [max_sub_layers_minus1] triple.
            let copy = sub_layer_ordering_info[last];
            for entry in sub_layer_ordering_info.iter_mut().take(last) {
                *entry = copy;
            }
        }

        let log2_min_luma_coding_block_size_minus3_raw = br.ue()?;
        let log2_diff_max_min_luma_coding_block_size_raw = br.ue()?;
        // §7.4.3.2.1 eqs. 7-10 / 7-11: MinCbLog2SizeY =
        // log2_min_luma_coding_block_size_minus3 + 3 and CtbLog2SizeY =
        // MinCbLog2SizeY + log2_diff_max_min_luma_coding_block_size.
        // Every Annex A profile requires "CtbLog2SizeY derived
        // according to active SPSs ... shall be in the range of 4 to 6,
        // inclusive" (e.g. the §A.3.2 Main-profile item), and the
        // eq.-7-13 `CtbSizeY = 1 << CtbLog2SizeY` shift (re-derived all
        // over the slice/CTB layers) is only meaningful under that
        // bound — reject out-of-range values here.
        let ctb_log2_size_y = log2_min_luma_coding_block_size_minus3_raw
            .saturating_add(3)
            .saturating_add(log2_diff_max_min_luma_coding_block_size_raw);
        if !(4..=6).contains(&ctb_log2_size_y) {
            return Err(SpsError::ValueOutOfRange {
                field: "CtbLog2SizeY",
                got: ctb_log2_size_y,
            });
        }
        let log2_min_luma_coding_block_size_minus3 =
            log2_min_luma_coding_block_size_minus3_raw as u8;
        let log2_diff_max_min_luma_coding_block_size =
            log2_diff_max_min_luma_coding_block_size_raw as u8;
        let min_cb_log2_size_y = u32::from(log2_min_luma_coding_block_size_minus3) + 3;

        let log2_min_luma_transform_block_size_minus2_raw = br.ue()?;
        // §7.4.3.2.1: "The CVS shall not contain data that result in
        // MinTbLog2SizeY greater than or equal to MinCbLog2SizeY"
        // (MinTbLog2SizeY = log2_min_luma_transform_block_size_minus2
        // + 2).
        let min_tb_log2_size_y = log2_min_luma_transform_block_size_minus2_raw.saturating_add(2);
        if min_tb_log2_size_y >= min_cb_log2_size_y {
            return Err(SpsError::ValueOutOfRange {
                field: "log2_min_luma_transform_block_size_minus2",
                got: log2_min_luma_transform_block_size_minus2_raw,
            });
        }
        let log2_min_luma_transform_block_size_minus2 =
            log2_min_luma_transform_block_size_minus2_raw as u8;

        let log2_diff_max_min_luma_transform_block_size_raw = br.ue()?;
        // §7.4.3.2.1: "The CVS shall not contain data that result in
        // MaxTbLog2SizeY greater than Min( CtbLog2SizeY, 5 )".
        let max_tb_log2_size_y =
            min_tb_log2_size_y.saturating_add(log2_diff_max_min_luma_transform_block_size_raw);
        if max_tb_log2_size_y > ctb_log2_size_y.min(5) {
            return Err(SpsError::ValueOutOfRange {
                field: "log2_diff_max_min_luma_transform_block_size",
                got: log2_diff_max_min_luma_transform_block_size_raw,
            });
        }
        let log2_diff_max_min_luma_transform_block_size =
            log2_diff_max_min_luma_transform_block_size_raw as u8;

        // §7.4.3.2.1: both hierarchy depths "shall be in the range of
        // 0 to CtbLog2SizeY − MinTbLog2SizeY, inclusive".
        let max_hierarchy_depth = ctb_log2_size_y - min_tb_log2_size_y;
        let max_transform_hierarchy_depth_inter_raw = br.ue()?;
        if max_transform_hierarchy_depth_inter_raw > max_hierarchy_depth {
            return Err(SpsError::ValueOutOfRange {
                field: "max_transform_hierarchy_depth_inter",
                got: max_transform_hierarchy_depth_inter_raw,
            });
        }
        let max_transform_hierarchy_depth_inter = max_transform_hierarchy_depth_inter_raw as u8;
        let max_transform_hierarchy_depth_intra_raw = br.ue()?;
        if max_transform_hierarchy_depth_intra_raw > max_hierarchy_depth {
            return Err(SpsError::ValueOutOfRange {
                field: "max_transform_hierarchy_depth_intra",
                got: max_transform_hierarchy_depth_intra_raw,
            });
        }
        let max_transform_hierarchy_depth_intra = max_transform_hierarchy_depth_intra_raw as u8;

        let scaling_list_enabled_flag = br.u1()? != 0;
        let mut sps_scaling_list_data_present_flag = false;
        let mut scaling_list_data = None;
        if scaling_list_enabled_flag {
            // §7.3.2.2: when scaling_list_enabled_flag == 1, an inner
            // sps_scaling_list_data_present_flag gates the explicit
            // scaling_list_data() structure (§7.3.4). When the inner
            // flag is 0 the default scaling lists (§7.4.5 Tables 7-5 /
            // 7-6) apply, so the SPS still parses.
            sps_scaling_list_data_present_flag = br.u1()? != 0;
            if sps_scaling_list_data_present_flag {
                scaling_list_data = Some(ScalingListData::parse(br)?);
            }
        }

        let amp_enabled_flag = br.u1()? != 0;
        let sample_adaptive_offset_enabled_flag = br.u1()? != 0;

        let pcm_enabled_flag = br.u1()? != 0;
        let pcm = if pcm_enabled_flag {
            let bit_depth_luma_minus1 = br.u(4)? as u8;
            let pcm_bit_depth_y = bit_depth_luma_minus1 as u32 + 1;
            let bit_depth_y = 8 + bit_depth_luma_minus8_raw;
            if pcm_bit_depth_y > bit_depth_y {
                return Err(SpsError::ValueOutOfRange {
                    field: "pcm_sample_bit_depth_luma_minus1",
                    got: bit_depth_luma_minus1 as u32,
                });
            }
            let bit_depth_chroma_minus1 = br.u(4)? as u8;
            let pcm_bit_depth_c = bit_depth_chroma_minus1 as u32 + 1;
            let bit_depth_c = 8 + bit_depth_chroma_minus8_raw;
            if pcm_bit_depth_c > bit_depth_c {
                return Err(SpsError::ValueOutOfRange {
                    field: "pcm_sample_bit_depth_chroma_minus1",
                    got: bit_depth_chroma_minus1 as u32,
                });
            }
            let log2_min_pcm_luma_coding_block_size_minus3 = br.ue()? as u8;
            let log2_diff_max_min_pcm_luma_coding_block_size = br.ue()? as u8;
            let loop_filter_disabled_flag = br.u1()? != 0;
            Some(PcmInfo {
                bit_depth_luma_minus1,
                bit_depth_chroma_minus1,
                log2_min_pcm_luma_coding_block_size_minus3,
                log2_diff_max_min_pcm_luma_coding_block_size,
                loop_filter_disabled_flag,
            })
        } else {
            None
        };

        let num_short_term_ref_pic_sets_raw = br.ue()?;
        if num_short_term_ref_pic_sets_raw > HEVC_MAX_NUM_SHORT_TERM_RPS as u32 {
            return Err(SpsError::ValueOutOfRange {
                field: "num_short_term_ref_pic_sets",
                got: num_short_term_ref_pic_sets_raw,
            });
        }
        let num_short_term_ref_pic_sets = num_short_term_ref_pic_sets_raw;
        let mut short_term_ref_pic_sets = Vec::with_capacity(num_short_term_ref_pic_sets as usize);
        for st_rps_idx in 0..num_short_term_ref_pic_sets as usize {
            let prev = if st_rps_idx == 0 {
                None
            } else {
                short_term_ref_pic_sets.last()
            };
            let rps = ShortTermRefPicSet::parse(
                br,
                st_rps_idx as u32,
                num_short_term_ref_pic_sets,
                prev,
                &short_term_ref_pic_sets,
            )?;
            short_term_ref_pic_sets.push(rps);
        }

        let long_term_ref_pics_present_flag = br.u1()? != 0;
        let mut num_long_term_ref_pics_sps = 0u32;
        let mut long_term_ref_pics = Vec::new();
        if long_term_ref_pics_present_flag {
            let raw = br.ue()?;
            if raw > HEVC_MAX_NUM_LONG_TERM_RPS as u32 {
                return Err(SpsError::ValueOutOfRange {
                    field: "num_long_term_ref_pics_sps",
                    got: raw,
                });
            }
            num_long_term_ref_pics_sps = raw;
            let poc_lsb_bits = log2_max_pic_order_cnt_lsb_minus4_raw as u8 + 4;
            long_term_ref_pics.reserve(num_long_term_ref_pics_sps as usize);
            for _ in 0..num_long_term_ref_pics_sps {
                let poc_lsb = br.u(poc_lsb_bits)?;
                let used = br.u1()? != 0;
                long_term_ref_pics.push(LongTermRefPicEntry {
                    poc_lsb,
                    used_by_curr_pic: used,
                });
            }
        }

        let sps_temporal_mvp_enabled_flag = br.u1()? != 0;
        let strong_intra_smoothing_enabled_flag = br.u1()? != 0;

        let vui_parameters_present_flag = br.u1()? != 0;
        // §E.2.1: the vui_parameters() body is decoded in full when
        // signalled, with the nested hrd_parameters( 1,
        // sps_max_sub_layers_minus1 ) call taking the SPS-level
        // maxNumSubLayersMinus1. Parsing then continues to
        // sps_extension_present_flag in both paths.
        let vui_parameters = if vui_parameters_present_flag {
            Some(VuiParameters::parse(br, max_sub_layers_minus1)?)
        } else {
            None
        };

        let (
            sps_extension_present_flag,
            extension_flags,
            sps_range_extension,
            sps_scc_extension,
            opaque_tail,
        ) = if br.bits_left() == 0 {
            // The fixture corpus encoders sometimes elide the
            // sps_extension_present_flag if no extension is signalled
            // and the rbsp_trailing_bits happens to land on a byte
            // boundary; the field is still required, so a buffer with
            // no bits left here is a truncation.
            return Err(SpsError::Truncated);
        } else {
            let gate = br.u1()? != 0;
            if gate {
                // §7.3.2.2.1: when the gate is open, decode the eight
                // bits of typed extension flags first.
                let sps_range_extension_flag = br.u1()? != 0;
                let sps_multilayer_extension_flag = br.u1()? != 0;
                let sps_3d_extension_flag = br.u1()? != 0;
                let sps_scc_extension_flag = br.u1()? != 0;
                let sps_extension_4bits = br.u(4)? as u8;
                let flags = SpsExtensionFlags {
                    sps_range_extension_flag,
                    sps_multilayer_extension_flag,
                    sps_3d_extension_flag,
                    sps_scc_extension_flag,
                    sps_extension_4bits,
                };
                // §7.3.2.2.1: the range extension body (if signalled)
                // is the first to follow the eight typed flag bits, so
                // decode it in full.
                let range_ext = if flags.sps_range_extension_flag {
                    Some(SpsRangeExtension::parse(br)?)
                } else {
                    None
                };
                // §7.3.2.2.1 body order is range, multilayer, 3d, scc.
                // The SCC body can be decoded in place only when no
                // (still-opaque) multilayer / 3D body precedes it;
                // otherwise it stays inside the opaque tail.
                let scc_ext = if flags.scc_decodable_in_place() {
                    Some(SpsSccExtension::parse(
                        br,
                        chroma_format_idc,
                        8 + bit_depth_luma_minus8_raw as u8,
                        8 + bit_depth_chroma_minus8_raw as u8,
                    )?)
                } else {
                    None
                };
                // If any still-opaque body (a multilayer / 3D body, an
                // SCC body kept opaque by such a predecessor, or the
                // sps_extension_data_flag while-loop) follows, capture
                // the rest of the RBSP as an opaque tail starting at
                // the first un-decoded body's bit position. Otherwise
                // only rbsp_trailing_bits remains, consumed implicitly.
                let tail = if flags.has_opaque_body_after_decoded() {
                    Some(OpaqueTail::capture_at(br.bit_pos(), rbsp))
                } else {
                    None
                };
                (true, Some(flags), range_ext, scc_ext, tail)
            } else {
                // No extension present. Only the rbsp_trailing_bits
                // remain — a single `1` bit followed by zero-padding
                // to a byte boundary. We do not require the caller to
                // have validated it; surface nothing for the opaque tail.
                (false, None, None, None, None)
            }
        };

        Ok(Self {
            vps_id,
            max_sub_layers_minus1,
            temporal_id_nesting_flag,
            ptl,
            sps_id,
            chroma_format_idc,
            separate_colour_plane_flag,
            pic_width_in_luma_samples,
            pic_height_in_luma_samples,
            conformance_window_flag,
            conformance_window,
            bit_depth_luma_minus8: bit_depth_luma_minus8_raw as u8,
            bit_depth_chroma_minus8: bit_depth_chroma_minus8_raw as u8,
            log2_max_pic_order_cnt_lsb_minus4: log2_max_pic_order_cnt_lsb_minus4_raw as u8,
            sub_layer_ordering_info_present_flag,
            sub_layer_ordering_info,
            log2_min_luma_coding_block_size_minus3,
            log2_diff_max_min_luma_coding_block_size,
            log2_min_luma_transform_block_size_minus2,
            log2_diff_max_min_luma_transform_block_size,
            max_transform_hierarchy_depth_inter,
            max_transform_hierarchy_depth_intra,
            scaling_list_enabled_flag,
            sps_scaling_list_data_present_flag,
            scaling_list_data,
            amp_enabled_flag,
            sample_adaptive_offset_enabled_flag,
            pcm_enabled_flag,
            pcm,
            num_short_term_ref_pic_sets,
            short_term_ref_pic_sets,
            long_term_ref_pics_present_flag,
            num_long_term_ref_pics_sps,
            long_term_ref_pics,
            sps_temporal_mvp_enabled_flag,
            strong_intra_smoothing_enabled_flag,
            vui_parameters_present_flag,
            vui_parameters,
            sps_extension_present_flag,
            extension_flags,
            sps_range_extension,
            sps_scc_extension,
            opaque_tail,
        })
    }

    pub fn bit_depth_luma(&self) -> u8 {
        8 + self.bit_depth_luma_minus8
    }

    pub fn bit_depth_chroma(&self) -> u8 {
        8 + self.bit_depth_chroma_minus8
    }

    pub fn log2_min_cb_size(&self) -> u8 {
        self.log2_min_luma_coding_block_size_minus3 + 3
    }

    pub fn log2_ctb_size(&self) -> u8 {
        self.log2_min_cb_size() + self.log2_diff_max_min_luma_coding_block_size
    }

    pub fn log2_min_tb_size(&self) -> u8 {
        self.log2_min_luma_transform_block_size_minus2 + 2
    }

    pub fn max_pic_order_cnt_lsb(&self) -> u32 {
        1u32 << (self.log2_max_pic_order_cnt_lsb_minus4 + 4)
    }
}

impl OpaqueTail {
    pub fn capture_at(bit_pos: usize, rbsp: &[u8]) -> Self {
        let byte_index = bit_pos / 8;
        let bit_in_byte = (bit_pos % 8) as u8;
        Self {
            bytes: rbsp[byte_index..].to_vec(),
            start_bit_in_first_byte: bit_in_byte,
        }
    }
}

impl ShortTermRefPicSet {
    pub fn parse_slice_inline(
        br: &mut BitReader<'_>,
        sps: &SeqParameterSet,
    ) -> Result<Self, SpsError> {
        Self::parse(
            br,
            sps.num_short_term_ref_pic_sets,
            sps.num_short_term_ref_pic_sets,
            sps.short_term_ref_pic_sets.last(),
            &sps.short_term_ref_pic_sets,
        )
    }

    fn parse(
        br: &mut BitReader<'_>,
        st_rps_idx: u32,
        num_short_term_ref_pic_sets: u32,
        prev: Option<&ShortTermRefPicSet>,
        all_rps: &[ShortTermRefPicSet],
    ) -> Result<Self, SpsError> {
        let inter_ref_pic_set_prediction_flag = if st_rps_idx != 0 {
            br.u1()? != 0
        } else {
            false
        };
        if inter_ref_pic_set_prediction_flag {
            // delta_idx_minus1 is only signalled when the RPS being
            // constructed is the slice-header in-line RPS, i.e.
            // stRpsIdx == num_short_term_ref_pic_sets. For SPS-resident
            // entries the value is inferred to 0 per §7.4.8.
            let delta_idx_minus1 = if st_rps_idx == num_short_term_ref_pic_sets {
                br.ue()?
            } else {
                0
            };
            if delta_idx_minus1 >= st_rps_idx {
                return Err(SpsError::ValueOutOfRange {
                    field: "delta_idx_minus1",
                    got: delta_idx_minus1,
                });
            }
            let delta_rps_sign = br.u1()? != 0;
            let abs_delta_rps_minus1 = br.ue()?;
            if abs_delta_rps_minus1 > (1 << 15) - 1 {
                return Err(SpsError::ValueOutOfRange {
                    field: "abs_delta_rps_minus1",
                    got: abs_delta_rps_minus1,
                });
            }
            // RefRpsIdx = stRpsIdx − (delta_idx_minus1 + 1)
            let ref_rps_idx = (st_rps_idx as i64) - (delta_idx_minus1 as i64 + 1);
            let ref_rps = if ref_rps_idx >= 0 && (ref_rps_idx as usize) < all_rps.len() {
                Some(&all_rps[ref_rps_idx as usize])
            } else {
                // For SPS entries we expect ref_rps_idx in-range; the
                // only legal use of an out-of-range RefRpsIdx is when
                // st_rps_idx == num_short_term_ref_pic_sets, which is
                // the slice-header in-line case (handled elsewhere).
                prev
            };
            let num_delta_pocs = ref_rps.map(|r| r.num_delta_pocs()).unwrap_or(0);
            let entries = num_delta_pocs as usize + 1;
            let mut used_by_curr_pic_flag = Vec::with_capacity(entries);
            let mut use_delta_flag = Vec::with_capacity(entries);
            for _ in 0..entries {
                let used = br.u1()? != 0;
                used_by_curr_pic_flag.push(used);
                if !used {
                    let ud = br.u1()? != 0;
                    use_delta_flag.push(ud);
                } else {
                    // Per §7.4.8: when used_by_curr_pic_flag[j] is 1,
                    // use_delta_flag[j] is inferred to be 1.
                    use_delta_flag.push(true);
                }
            }
            Ok(Self {
                inter_ref_pic_set_prediction_flag,
                delta_idx_minus1,
                delta_rps_sign,
                abs_delta_rps_minus1,
                used_by_curr_pic_flag,
                use_delta_flag,
                num_negative_pics: 0,
                num_positive_pics: 0,
                delta_poc_s0_minus1: Vec::new(),
                used_by_curr_pic_s0_flag: Vec::new(),
                delta_poc_s1_minus1: Vec::new(),
                used_by_curr_pic_s1_flag: Vec::new(),
            })
        } else {
            let num_negative_pics = br.ue()?;
            if num_negative_pics > HEVC_MAX_RPS_PICS as u32 {
                return Err(SpsError::ValueOutOfRange {
                    field: "num_negative_pics",
                    got: num_negative_pics,
                });
            }
            let num_positive_pics = br.ue()?;
            if num_positive_pics > HEVC_MAX_RPS_PICS as u32 {
                return Err(SpsError::ValueOutOfRange {
                    field: "num_positive_pics",
                    got: num_positive_pics,
                });
            }
            let mut delta_poc_s0_minus1 = Vec::with_capacity(num_negative_pics as usize);
            let mut used_by_curr_pic_s0_flag = Vec::with_capacity(num_negative_pics as usize);
            for _ in 0..num_negative_pics {
                let dp = br.ue()?;
                if dp > (1 << 15) - 1 {
                    return Err(SpsError::ValueOutOfRange {
                        field: "delta_poc_s0_minus1",
                        got: dp,
                    });
                }
                delta_poc_s0_minus1.push(dp);
                used_by_curr_pic_s0_flag.push(br.u1()? != 0);
            }
            let mut delta_poc_s1_minus1 = Vec::with_capacity(num_positive_pics as usize);
            let mut used_by_curr_pic_s1_flag = Vec::with_capacity(num_positive_pics as usize);
            for _ in 0..num_positive_pics {
                let dp = br.ue()?;
                if dp > (1 << 15) - 1 {
                    return Err(SpsError::ValueOutOfRange {
                        field: "delta_poc_s1_minus1",
                        got: dp,
                    });
                }
                delta_poc_s1_minus1.push(dp);
                used_by_curr_pic_s1_flag.push(br.u1()? != 0);
            }
            Ok(Self {
                inter_ref_pic_set_prediction_flag,
                delta_idx_minus1: 0,
                delta_rps_sign: false,
                abs_delta_rps_minus1: 0,
                used_by_curr_pic_flag: Vec::new(),
                use_delta_flag: Vec::new(),
                num_negative_pics,
                num_positive_pics,
                delta_poc_s0_minus1,
                used_by_curr_pic_s0_flag,
                delta_poc_s1_minus1,
                used_by_curr_pic_s1_flag,
            })
        }
    }
}
