// SPDX-License-Identifier: MIT
// Derived from oxideav-h265 0.0.10, Copyright (c) 2026 Karpelès Lab Inc.
// Retained syntax/metadata primitives used by this client; see COPYING.OxideAV in this crate.

use crate::bitreader::{BitReader, BitReaderError};

use crate::scaling_list::{ScalingListData, ScalingListError};

use crate::sps::OpaqueTail;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PpsError {
    Truncated,
    ValueOutOfRange { field: &'static str, got: i64 },
    ScalingList(ScalingListError),
    Bitstream(BitReaderError),
}

impl core::fmt::Display for PpsError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Truncated => f.write_str("PPS RBSP truncated"),
            Self::ValueOutOfRange { field, got } => {
                write!(f, "PPS syntax element {field} out of range: {got}")
            }
            Self::ScalingList(e) => write!(f, "scaling-list error during PPS parse: {e}"),
            Self::Bitstream(e) => write!(f, "bitstream error during PPS parse: {e}"),
        }
    }
}

impl std::error::Error for PpsError {}

impl From<BitReaderError> for PpsError {
    fn from(e: BitReaderError) -> Self {
        match e {
            BitReaderError::EndOfBuffer => Self::Truncated,
            other => Self::Bitstream(other),
        }
    }
}

impl From<ScalingListError> for PpsError {
    fn from(e: ScalingListError) -> Self {
        // Flatten truncation / raw-reader faults to the PPS-level
        // equivalents; carry structured scaling-list faults as-is.
        match e {
            ScalingListError::Truncated => Self::Truncated,
            ScalingListError::Bitstream(b) => Self::Bitstream(b),
            other => Self::ScalingList(other),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TileInfo {
    pub num_tile_columns_minus1: u32,
    pub num_tile_rows_minus1: u32,
    pub uniform_spacing_flag: bool,
    pub column_width_minus1: Vec<u32>,
    pub row_height_minus1: Vec<u32>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct DeblockingFilterControl {
    pub override_enabled_flag: bool,
    pub disabled_flag: bool,
    pub beta_offset_div2: i8,
    pub tc_offset_div2: i8,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct PpsExtensionFlags {
    pub pps_range_extension_flag: bool,
    pub pps_multilayer_extension_flag: bool,
    pub pps_3d_extension_flag: bool,
    pub pps_scc_extension_flag: bool,
    pub pps_extension_4bits: u8,
}

impl PpsExtensionFlags {
    pub fn has_body(&self) -> bool {
        self.pps_range_extension_flag
            || self.pps_multilayer_extension_flag
            || self.pps_3d_extension_flag
            || self.pps_scc_extension_flag
            || self.pps_extension_4bits != 0
    }

    fn scc_decodable_in_place(&self) -> bool {
        self.pps_scc_extension_flag
            && !self.pps_multilayer_extension_flag
            && !self.pps_3d_extension_flag
    }

    fn has_opaque_body_after_decoded(&self) -> bool {
        if self.pps_multilayer_extension_flag || self.pps_3d_extension_flag {
            // The first un-decoded body is the multilayer / 3D one;
            // everything from there (incl. any SCC body) is opaque.
            return true;
        }
        // No multilayer / 3D body: SCC (if present) was decoded in
        // place, so only the pps_extension_data_flag while-loop may
        // remain.
        self.pps_extension_4bits != 0
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ChromaQpOffsetListEntry {
    pub cb_qp_offset: i8,
    pub cr_qp_offset: i8,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct PpsRangeExtension {
    pub log2_max_transform_skip_block_size_minus2: u32,
    pub cross_component_prediction_enabled_flag: bool,
    pub chroma_qp_offset_list_enabled_flag: bool,
    pub diff_cu_chroma_qp_offset_depth: u32,
    pub chroma_qp_offset_list_len_minus1: u32,
    pub chroma_qp_offset_list: Vec<ChromaQpOffsetListEntry>,
    pub log2_sao_offset_scale_luma: u32,
    pub log2_sao_offset_scale_chroma: u32,
}

impl PpsRangeExtension {
    const MAX_CHROMA_QP_OFFSET_LIST_LEN_MINUS1: u32 = 5;

    fn parse(br: &mut BitReader, transform_skip_enabled_flag: bool) -> Result<Self, PpsError> {
        let log2_max_transform_skip_block_size_minus2 = if transform_skip_enabled_flag {
            br.ue()?
        } else {
            0
        };
        let cross_component_prediction_enabled_flag = br.u1()? != 0;
        let chroma_qp_offset_list_enabled_flag = br.u1()? != 0;
        let mut diff_cu_chroma_qp_offset_depth = 0u32;
        let mut chroma_qp_offset_list_len_minus1 = 0u32;
        let mut chroma_qp_offset_list = Vec::new();
        if chroma_qp_offset_list_enabled_flag {
            diff_cu_chroma_qp_offset_depth = br.ue()?;
            chroma_qp_offset_list_len_minus1 = br.ue()?;
            if chroma_qp_offset_list_len_minus1 > Self::MAX_CHROMA_QP_OFFSET_LIST_LEN_MINUS1 {
                return Err(PpsError::ValueOutOfRange {
                    field: "chroma_qp_offset_list_len_minus1",
                    got: chroma_qp_offset_list_len_minus1 as i64,
                });
            }
            let len = chroma_qp_offset_list_len_minus1 as usize + 1;
            chroma_qp_offset_list.reserve(len);
            for _ in 0..len {
                let cb = br.se()?;
                let cr = br.se()?;
                for (field, v) in [("cb_qp_offset_list", cb), ("cr_qp_offset_list", cr)] {
                    if !(-12..=12).contains(&v) {
                        return Err(PpsError::ValueOutOfRange {
                            field,
                            got: v as i64,
                        });
                    }
                }
                chroma_qp_offset_list.push(ChromaQpOffsetListEntry {
                    cb_qp_offset: cb as i8,
                    cr_qp_offset: cr as i8,
                });
            }
        }
        let log2_sao_offset_scale_luma = br.ue()?;
        let log2_sao_offset_scale_chroma = br.ue()?;
        Ok(Self {
            log2_max_transform_skip_block_size_minus2,
            cross_component_prediction_enabled_flag,
            chroma_qp_offset_list_enabled_flag,
            diff_cu_chroma_qp_offset_depth,
            chroma_qp_offset_list_len_minus1,
            chroma_qp_offset_list,
            log2_sao_offset_scale_luma,
            log2_sao_offset_scale_chroma,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct PpsSccExtension {
    pub pps_curr_pic_ref_enabled_flag: bool,
    pub residual_adaptive_colour_transform_enabled_flag: bool,
    pub pps_slice_act_qp_offsets_present_flag: bool,
    pub pps_act_y_qp_offset_plus5: i32,
    pub pps_act_cb_qp_offset_plus5: i32,
    pub pps_act_cr_qp_offset_plus3: i32,
    pub pps_palette_predictor_initializers_present_flag: bool,
    pub pps_num_palette_predictor_initializers: u32,
    pub monochrome_palette_flag: bool,
    pub luma_bit_depth_entry_minus8: u32,
    pub chroma_bit_depth_entry_minus8: u32,
    pub pps_palette_predictor_initializer: Vec<Vec<u32>>,
}

impl PpsSccExtension {
    fn parse(br: &mut BitReader) -> Result<Self, PpsError> {
        let pps_curr_pic_ref_enabled_flag = br.u1()? != 0;
        let residual_adaptive_colour_transform_enabled_flag = br.u1()? != 0;
        let mut pps_slice_act_qp_offsets_present_flag = false;
        let mut pps_act_y_qp_offset_plus5 = 0i32;
        let mut pps_act_cb_qp_offset_plus5 = 0i32;
        let mut pps_act_cr_qp_offset_plus3 = 0i32;
        if residual_adaptive_colour_transform_enabled_flag {
            pps_slice_act_qp_offsets_present_flag = br.u1()? != 0;
            pps_act_y_qp_offset_plus5 = br.se()?;
            pps_act_cb_qp_offset_plus5 = br.se()?;
            pps_act_cr_qp_offset_plus3 = br.se()?;
            // §7.4.3.3.3: PpsActQpOffsetY/Cb/Cr (eq. 7-39/40/41 — the
            // raw values minus 5/5/3) must each lie in −12..=12 in a
            // conforming bitstream.
            // Widened arithmetic: se(v) can carry the full i32 range on
            // a malformed stream, so the eq.-7-39..7-41 "minus 5/5/3"
            // must not overflow before the range check rejects it.
            for (field, qp) in [
                (
                    "pps_act_y_qp_offset_plus5",
                    i64::from(pps_act_y_qp_offset_plus5) - 5,
                ),
                (
                    "pps_act_cb_qp_offset_plus5",
                    i64::from(pps_act_cb_qp_offset_plus5) - 5,
                ),
                (
                    "pps_act_cr_qp_offset_plus3",
                    i64::from(pps_act_cr_qp_offset_plus3) - 3,
                ),
            ] {
                if !(-12..=12).contains(&qp) {
                    return Err(PpsError::ValueOutOfRange { field, got: qp });
                }
            }
        }
        let pps_palette_predictor_initializers_present_flag = br.u1()? != 0;
        let mut pps_num_palette_predictor_initializers = 0u32;
        let mut monochrome_palette_flag = false;
        let mut luma_bit_depth_entry_minus8 = 0u32;
        let mut chroma_bit_depth_entry_minus8 = 0u32;
        let mut pps_palette_predictor_initializer = Vec::new();
        if pps_palette_predictor_initializers_present_flag {
            pps_num_palette_predictor_initializers = br.ue()?;
            // §7.4.3.3.3: bounded by PaletteMaxPredictorSize, itself
            // capped by the profile limits (palette_max_size +
            // delta_palette_max_predictor_size); reject anything past
            // the largest representable predictor so a malformed count
            // cannot drive the initializer allocation.
            if pps_num_palette_predictor_initializers > 128 {
                return Err(PpsError::ValueOutOfRange {
                    field: "pps_num_palette_predictor_initializers",
                    got: i64::from(pps_num_palette_predictor_initializers),
                });
            }
            if pps_num_palette_predictor_initializers > 0 {
                monochrome_palette_flag = br.u1()? != 0;
                luma_bit_depth_entry_minus8 = br.ue()?;
                if !monochrome_palette_flag {
                    chroma_bit_depth_entry_minus8 = br.ue()?;
                }
                // §7.4.3.3.3: the entry bit depths are BitDepth values
                // (8..=16), i.e. the minus8 fields lie in 0..=8 — checked
                // before the `+ 8` width arithmetic below.
                for (field, v) in [
                    ("luma_bit_depth_entry_minus8", luma_bit_depth_entry_minus8),
                    (
                        "chroma_bit_depth_entry_minus8",
                        chroma_bit_depth_entry_minus8,
                    ),
                ] {
                    if v > 8 {
                        return Err(PpsError::ValueOutOfRange {
                            field,
                            got: i64::from(v),
                        });
                    }
                }
                let num_comps = if monochrome_palette_flag { 1 } else { 3 };
                let num_entries = pps_num_palette_predictor_initializers as usize;
                pps_palette_predictor_initializer.reserve(num_comps);
                for comp in 0..num_comps {
                    let width = if comp == 0 {
                        (luma_bit_depth_entry_minus8 + 8) as u8
                    } else {
                        (chroma_bit_depth_entry_minus8 + 8) as u8
                    };
                    let mut row = Vec::with_capacity(num_entries);
                    for _ in 0..num_entries {
                        row.push(br.u(width)?);
                    }
                    pps_palette_predictor_initializer.push(row);
                }
            }
        }
        Ok(Self {
            pps_curr_pic_ref_enabled_flag,
            residual_adaptive_colour_transform_enabled_flag,
            pps_slice_act_qp_offsets_present_flag,
            pps_act_y_qp_offset_plus5,
            pps_act_cb_qp_offset_plus5,
            pps_act_cr_qp_offset_plus3,
            pps_palette_predictor_initializers_present_flag,
            pps_num_palette_predictor_initializers,
            monochrome_palette_flag,
            luma_bit_depth_entry_minus8,
            chroma_bit_depth_entry_minus8,
            pps_palette_predictor_initializer,
        })
    }

    pub fn pps_act_qp_offset_y(&self) -> i32 {
        self.pps_act_y_qp_offset_plus5 - 5
    }

    pub fn pps_act_qp_offset_cb(&self) -> i32 {
        self.pps_act_cb_qp_offset_plus5 - 5
    }

    pub fn pps_act_qp_offset_cr(&self) -> i32 {
        self.pps_act_cr_qp_offset_plus3 - 3
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PicParameterSet {
    pub pps_id: u8,
    pub sps_id: u8,
    pub dependent_slice_segments_enabled_flag: bool,
    pub output_flag_present_flag: bool,
    pub num_extra_slice_header_bits: u8,
    pub sign_data_hiding_enabled_flag: bool,
    pub cabac_init_present_flag: bool,
    pub num_ref_idx_l0_default_active_minus1: u8,
    pub num_ref_idx_l1_default_active_minus1: u8,
    pub init_qp_minus26: i32,
    pub constrained_intra_pred_flag: bool,
    pub transform_skip_enabled_flag: bool,
    pub cu_qp_delta_enabled_flag: bool,
    pub diff_cu_qp_delta_depth: u32,
    pub pps_cb_qp_offset: i8,
    pub pps_cr_qp_offset: i8,
    pub pps_slice_chroma_qp_offsets_present_flag: bool,
    pub weighted_pred_flag: bool,
    pub weighted_bipred_flag: bool,
    pub transquant_bypass_enabled_flag: bool,
    pub tiles_enabled_flag: bool,
    pub entropy_coding_sync_enabled_flag: bool,
    pub tiles: TileInfo,
    pub loop_filter_across_tiles_enabled_flag: bool,
    pub pps_loop_filter_across_slices_enabled_flag: bool,
    pub deblocking_filter_control_present_flag: bool,
    pub deblocking: DeblockingFilterControl,
    pub pps_scaling_list_data_present_flag: bool,
    pub scaling_list_data: Option<ScalingListData>,
    pub lists_modification_present_flag: bool,
    pub log2_parallel_merge_level_minus2: u32,
    pub slice_segment_header_extension_present_flag: bool,
    pub pps_extension_present_flag: bool,
    pub extension_flags: Option<PpsExtensionFlags>,
    pub pps_range_extension: Option<PpsRangeExtension>,
    pub pps_scc_extension: Option<PpsSccExtension>,
    pub opaque_tail: Option<OpaqueTail>,
}

impl PicParameterSet {
    pub fn parse(rbsp: &[u8]) -> Result<Self, PpsError> {
        let mut br = BitReader::new(rbsp);
        Self::parse_inner(&mut br, rbsp)
    }

    fn parse_inner(br: &mut BitReader<'_>, rbsp: &[u8]) -> Result<Self, PpsError> {
        let pps_id_raw = br.ue()?;
        if pps_id_raw > 63 {
            return Err(PpsError::ValueOutOfRange {
                field: "pps_pic_parameter_set_id",
                got: pps_id_raw as i64,
            });
        }
        let sps_id_raw = br.ue()?;
        if sps_id_raw > 15 {
            return Err(PpsError::ValueOutOfRange {
                field: "pps_seq_parameter_set_id",
                got: sps_id_raw as i64,
            });
        }

        let dependent_slice_segments_enabled_flag = br.u1()? != 0;
        let output_flag_present_flag = br.u1()? != 0;
        let num_extra_slice_header_bits = br.u(3)? as u8;
        let sign_data_hiding_enabled_flag = br.u1()? != 0;
        let cabac_init_present_flag = br.u1()? != 0;

        let num_ref_idx_l0_default_active_minus1_raw = br.ue()?;
        if num_ref_idx_l0_default_active_minus1_raw > 14 {
            return Err(PpsError::ValueOutOfRange {
                field: "num_ref_idx_l0_default_active_minus1",
                got: num_ref_idx_l0_default_active_minus1_raw as i64,
            });
        }
        let num_ref_idx_l1_default_active_minus1_raw = br.ue()?;
        if num_ref_idx_l1_default_active_minus1_raw > 14 {
            return Err(PpsError::ValueOutOfRange {
                field: "num_ref_idx_l1_default_active_minus1",
                got: num_ref_idx_l1_default_active_minus1_raw as i64,
            });
        }

        let init_qp_minus26 = br.se()?;
        // §7.4.3.3.1: −( 26 + QpBdOffsetY ) .. +25. The lower bound
        // depends on the active SPS bit depth; QpBdOffsetY is at most
        // 6 * 8 = 48 (the maximum legal bit_depth_luma_minus8), so the
        // loosest legal lower bound is −74. We range-check against the
        // loosest bound here (no SPS context) and provide
        // `init_qp_in_range()` for callers that have resolved the SPS.
        if !(-74..=25).contains(&init_qp_minus26) {
            return Err(PpsError::ValueOutOfRange {
                field: "init_qp_minus26",
                got: init_qp_minus26 as i64,
            });
        }

        let constrained_intra_pred_flag = br.u1()? != 0;
        let transform_skip_enabled_flag = br.u1()? != 0;

        let cu_qp_delta_enabled_flag = br.u1()? != 0;
        let diff_cu_qp_delta_depth = if cu_qp_delta_enabled_flag {
            br.ue()?
        } else {
            // §7.4.3.3.1 inference.
            0
        };

        let pps_cb_qp_offset = br.se()?;
        if !(-12..=12).contains(&pps_cb_qp_offset) {
            return Err(PpsError::ValueOutOfRange {
                field: "pps_cb_qp_offset",
                got: pps_cb_qp_offset as i64,
            });
        }
        let pps_cr_qp_offset = br.se()?;
        if !(-12..=12).contains(&pps_cr_qp_offset) {
            return Err(PpsError::ValueOutOfRange {
                field: "pps_cr_qp_offset",
                got: pps_cr_qp_offset as i64,
            });
        }

        let pps_slice_chroma_qp_offsets_present_flag = br.u1()? != 0;
        let weighted_pred_flag = br.u1()? != 0;
        let weighted_bipred_flag = br.u1()? != 0;
        let transquant_bypass_enabled_flag = br.u1()? != 0;
        let tiles_enabled_flag = br.u1()? != 0;
        let entropy_coding_sync_enabled_flag = br.u1()? != 0;

        let (tiles, loop_filter_across_tiles_enabled_flag) = if tiles_enabled_flag {
            let num_tile_columns_minus1 = br.ue()?;
            let num_tile_rows_minus1 = br.ue()?;
            // §A.4.1 item f): "num_tile_columns_minus1 shall be less
            // than MaxTileCols and num_tile_rows_minus1 shall be less
            // than MaxTileRows". The largest Table A.8 entries are
            // MaxTileCols = 40 / MaxTileRows = 44 (levels 7 .. 7.2);
            // the tighter §7.4.3.3.1 PicWidthInCtbsY-relative bound
            // needs the active SPS, which the PPS parse does not see.
            // Rejecting here also bounds the explicit width / height
            // array allocations below.
            if num_tile_columns_minus1 >= 40 {
                return Err(PpsError::ValueOutOfRange {
                    field: "num_tile_columns_minus1",
                    got: num_tile_columns_minus1 as i64,
                });
            }
            if num_tile_rows_minus1 >= 44 {
                return Err(PpsError::ValueOutOfRange {
                    field: "num_tile_rows_minus1",
                    got: num_tile_rows_minus1 as i64,
                });
            }
            let uniform_spacing_flag = br.u1()? != 0;
            let (column_width_minus1, row_height_minus1) = if !uniform_spacing_flag {
                let mut cols = Vec::with_capacity(num_tile_columns_minus1 as usize);
                for _ in 0..num_tile_columns_minus1 {
                    cols.push(br.ue()?);
                }
                let mut rows = Vec::with_capacity(num_tile_rows_minus1 as usize);
                for _ in 0..num_tile_rows_minus1 {
                    rows.push(br.ue()?);
                }
                (cols, rows)
            } else {
                (Vec::new(), Vec::new())
            };
            let loop_filter_across_tiles_enabled_flag = br.u1()? != 0;
            (
                TileInfo {
                    num_tile_columns_minus1,
                    num_tile_rows_minus1,
                    uniform_spacing_flag,
                    column_width_minus1,
                    row_height_minus1,
                },
                loop_filter_across_tiles_enabled_flag,
            )
        } else {
            // §7.4.3.3.1 inference: one column, one row, uniform
            // spacing, loop filtering across tiles enabled.
            (
                TileInfo {
                    num_tile_columns_minus1: 0,
                    num_tile_rows_minus1: 0,
                    uniform_spacing_flag: true,
                    column_width_minus1: Vec::new(),
                    row_height_minus1: Vec::new(),
                },
                true,
            )
        };

        let pps_loop_filter_across_slices_enabled_flag = br.u1()? != 0;

        let deblocking_filter_control_present_flag = br.u1()? != 0;
        let deblocking = if deblocking_filter_control_present_flag {
            let override_enabled_flag = br.u1()? != 0;
            let disabled_flag = br.u1()? != 0;
            let (beta_offset_div2, tc_offset_div2) = if !disabled_flag {
                let beta = br.se()?;
                if !(-6..=6).contains(&beta) {
                    return Err(PpsError::ValueOutOfRange {
                        field: "pps_beta_offset_div2",
                        got: beta as i64,
                    });
                }
                let tc = br.se()?;
                if !(-6..=6).contains(&tc) {
                    return Err(PpsError::ValueOutOfRange {
                        field: "pps_tc_offset_div2",
                        got: tc as i64,
                    });
                }
                (beta as i8, tc as i8)
            } else {
                // §7.4.3.3.1: beta/tc offsets inferred to 0 when the
                // deblocking filter is disabled at PPS level.
                (0, 0)
            };
            DeblockingFilterControl {
                override_enabled_flag,
                disabled_flag,
                beta_offset_div2,
                tc_offset_div2,
            }
        } else {
            DeblockingFilterControl::default()
        };

        let pps_scaling_list_data_present_flag = br.u1()? != 0;
        let scaling_list_data = if pps_scaling_list_data_present_flag {
            // scaling_list_data() (§7.3.4), shared with the SPS path.
            Some(ScalingListData::parse(br)?)
        } else {
            None
        };

        let lists_modification_present_flag = br.u1()? != 0;
        let log2_parallel_merge_level_minus2 = br.ue()?;
        let slice_segment_header_extension_present_flag = br.u1()? != 0;

        let pps_extension_present_flag = br.u1()? != 0;
        let (extension_flags, pps_range_extension, pps_scc_extension, opaque_tail) =
            if pps_extension_present_flag {
                // §7.3.2.3.1: when the gate is open, decode the eight bits
                // of typed extension flags first.
                let pps_range_extension_flag = br.u1()? != 0;
                let pps_multilayer_extension_flag = br.u1()? != 0;
                let pps_3d_extension_flag = br.u1()? != 0;
                let pps_scc_extension_flag = br.u1()? != 0;
                let pps_extension_4bits = br.u(4)? as u8;
                let flags = PpsExtensionFlags {
                    pps_range_extension_flag,
                    pps_multilayer_extension_flag,
                    pps_3d_extension_flag,
                    pps_scc_extension_flag,
                    pps_extension_4bits,
                };
                // §7.3.2.3.1: the range extension (if signalled) is the
                // first body to follow the eight typed flag bits, so decode
                // it in full. Its leading
                // log2_max_transform_skip_block_size_minus2 is present only
                // when transform_skip_enabled_flag was set in the general
                // body.
                let range_ext = if flags.pps_range_extension_flag {
                    Some(PpsRangeExtension::parse(br, transform_skip_enabled_flag)?)
                } else {
                    None
                };
                // §7.3.2.3.1 body order is range, multilayer, 3d, scc. The
                // SCC body can be decoded in place only when no
                // (still-opaque) multilayer / 3D body precedes it;
                // otherwise it stays inside the opaque tail.
                let scc_ext = if flags.scc_decodable_in_place() {
                    Some(PpsSccExtension::parse(br)?)
                } else {
                    None
                };
                // If any still-opaque body (a multilayer / 3D body, an SCC
                // body kept opaque by such a predecessor, or the
                // pps_extension_data_flag while-loop) follows, capture the
                // rest of the RBSP as an opaque tail starting at the first
                // un-decoded body's bit position. Otherwise only
                // rbsp_trailing_bits remains, consumed implicitly.
                let tail = if flags.has_opaque_body_after_decoded() {
                    Some(OpaqueTail::capture_at(br.bit_pos(), rbsp))
                } else {
                    None
                };
                (Some(flags), range_ext, scc_ext, tail)
            } else {
                // §7.4.3.3.1: every extension flag inferred to 0; only
                // rbsp_trailing_bits remains, consumed implicitly.
                (None, None, None, None)
            };

        Ok(Self {
            pps_id: pps_id_raw as u8,
            sps_id: sps_id_raw as u8,
            dependent_slice_segments_enabled_flag,
            output_flag_present_flag,
            num_extra_slice_header_bits,
            sign_data_hiding_enabled_flag,
            cabac_init_present_flag,
            num_ref_idx_l0_default_active_minus1: num_ref_idx_l0_default_active_minus1_raw as u8,
            num_ref_idx_l1_default_active_minus1: num_ref_idx_l1_default_active_minus1_raw as u8,
            init_qp_minus26,
            constrained_intra_pred_flag,
            transform_skip_enabled_flag,
            cu_qp_delta_enabled_flag,
            diff_cu_qp_delta_depth,
            pps_cb_qp_offset: pps_cb_qp_offset as i8,
            pps_cr_qp_offset: pps_cr_qp_offset as i8,
            pps_slice_chroma_qp_offsets_present_flag,
            weighted_pred_flag,
            weighted_bipred_flag,
            transquant_bypass_enabled_flag,
            tiles_enabled_flag,
            entropy_coding_sync_enabled_flag,
            tiles,
            loop_filter_across_tiles_enabled_flag,
            pps_loop_filter_across_slices_enabled_flag,
            deblocking_filter_control_present_flag,
            deblocking,
            pps_scaling_list_data_present_flag,
            scaling_list_data,
            lists_modification_present_flag,
            log2_parallel_merge_level_minus2,
            slice_segment_header_extension_present_flag,
            pps_extension_present_flag,
            extension_flags,
            pps_range_extension,
            pps_scc_extension,
            opaque_tail,
        })
    }

    pub fn init_qp(&self) -> i32 {
        self.init_qp_minus26 + 26
    }

    pub fn num_ref_idx_l0_default_active(&self) -> u8 {
        self.num_ref_idx_l0_default_active_minus1 + 1
    }

    pub fn num_ref_idx_l1_default_active(&self) -> u8 {
        self.num_ref_idx_l1_default_active_minus1 + 1
    }

    pub fn num_tile_columns(&self) -> u32 {
        self.tiles.num_tile_columns_minus1 + 1
    }

    pub fn num_tile_rows(&self) -> u32 {
        self.tiles.num_tile_rows_minus1 + 1
    }

    pub fn log2_par_mrg_level(&self) -> u32 {
        self.log2_parallel_merge_level_minus2 + 2
    }

    pub fn init_qp_in_range(&self, bit_depth_luma_minus8: u8) -> bool {
        let qp_bd_offset_y = 6 * bit_depth_luma_minus8 as i32;
        let lower = -(26 + qp_bd_offset_y);
        (lower..=25).contains(&self.init_qp_minus26)
    }
}
