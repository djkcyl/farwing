// SPDX-License-Identifier: MIT
// Derived from oxideav-h265 0.0.10, Copyright (c) 2026 Karpelès Lab Inc.
// Retained syntax/metadata primitives used by this client; see COPYING.OxideAV in this crate.

use crate::bitreader::{BitReader, BitReaderError};

use crate::pps::PicParameterSet;

use crate::sps::{OpaqueTail, SeqParameterSet, ShortTermRefPicSet, SpsError};

pub const BLA_W_LP: u8 = 16;

pub const IDR_W_RADL: u8 = 19;

pub const IDR_N_LP: u8 = 20;

pub const RSV_IRAP_VCL23: u8 = 23;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SliceType {
    B,
    P,
    I,
}

impl SliceType {
    fn from_raw(v: u32) -> Result<Self, SliceError> {
        match v {
            0 => Ok(Self::B),
            1 => Ok(Self::P),
            2 => Ok(Self::I),
            other => Err(SliceError::ValueOutOfRange {
                field: "slice_type",
                got: other as i64,
            }),
        }
    }

    pub fn is_inter(self) -> bool {
        matches!(self, Self::P | Self::B)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SliceError {
    Truncated,
    ValueOutOfRange { field: &'static str, got: i64 },
    Bitstream(BitReaderError),
    InlineShortTermRpsParse(SpsError),
}

impl core::fmt::Display for SliceError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Truncated => f.write_str("slice segment header RBSP truncated"),
            Self::ValueOutOfRange { field, got } => {
                write!(f, "slice header syntax element {field} out of range: {got}")
            }
            Self::Bitstream(e) => write!(f, "bitstream error during slice header parse: {e}"),
            Self::InlineShortTermRpsParse(e) => {
                write!(f, "in-line slice-header st_ref_pic_set parse failed: {e}")
            }
        }
    }
}

impl std::error::Error for SliceError {}

impl From<BitReaderError> for SliceError {
    fn from(e: BitReaderError) -> Self {
        match e {
            BitReaderError::EndOfBuffer => Self::Truncated,
            other => Self::Bitstream(other),
        }
    }
}

impl From<SpsError> for SliceError {
    fn from(e: SpsError) -> Self {
        match e {
            SpsError::Truncated => Self::Truncated,
            SpsError::Bitstream(b) => Self::Bitstream(b),
            other => Self::InlineShortTermRpsParse(other),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SliceDeblocking {
    pub disabled_flag: bool,
    pub beta_offset_div2: i8,
    pub tc_offset_div2: i8,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SliceLongTermRefPic {
    pub source: SliceLongTermRefPicSource,
    pub delta_poc_msb_present_flag: bool,
    pub delta_poc_msb_cycle_lt: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SliceLongTermRefPicSource {
    Sps {
        lt_idx_sps: u32,
    },
    InSlice {
        poc_lsb_lt: u32,
        used_by_curr_pic_lt_flag: bool,
    },
}

impl SliceLongTermRefPic {
    pub fn used_by_curr_pic_lt(&self, sps: &SeqParameterSet) -> Option<bool> {
        match self.source {
            SliceLongTermRefPicSource::Sps { lt_idx_sps } => sps
                .long_term_ref_pics
                .get(lt_idx_sps as usize)
                .map(|entry| entry.used_by_curr_pic),
            SliceLongTermRefPicSource::InSlice {
                used_by_curr_pic_lt_flag,
                ..
            } => Some(used_by_curr_pic_lt_flag),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EntryPointOffsets {
    pub num_entry_point_offsets: u32,
    pub offset_len_minus1: u8,
    pub entry_point_offset_minus1: Vec<u32>,
}

impl EntryPointOffsets {
    pub fn subset_length(&self, i: usize) -> Option<u64> {
        self.entry_point_offset_minus1
            .get(i)
            .map(|v| u64::from(*v) + 1)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RefPicListsModification {
    pub ref_pic_list_modification_flag_l0: bool,
    pub list_entry_l0: Vec<u32>,
    pub ref_pic_list_modification_flag_l1: Option<bool>,
    pub list_entry_l1: Vec<u32>,
}

impl RefPicListsModification {
    pub fn parse(
        br: &mut BitReader<'_>,
        slice_type: SliceType,
        num_ref_idx_l0_active_minus1: u8,
        num_ref_idx_l1_active_minus1: u8,
        num_pic_total_curr: u32,
    ) -> Result<Self, SliceError> {
        if slice_type == SliceType::I {
            return Err(SliceError::ValueOutOfRange {
                field: "ref_pic_lists_modification/slice_type",
                got: 2,
            });
        }
        if num_pic_total_curr <= 1 {
            return Err(SliceError::ValueOutOfRange {
                field: "ref_pic_lists_modification/NumPicTotalCurr",
                got: num_pic_total_curr as i64,
            });
        }
        // §7.4.7.1 ranges `num_ref_idx_lX_active_minus1` at 0..=14;
        // defensively cap the per-list loop length so a corrupted call
        // can't drive an unbounded allocation. (The cap matches the
        // spec maximum; a value above 14 would be rejected by the
        // §7.4.7.1 slice-header parse before reaching here.)
        if num_ref_idx_l0_active_minus1 > 14 {
            return Err(SliceError::ValueOutOfRange {
                field: "num_ref_idx_l0_active_minus1",
                got: num_ref_idx_l0_active_minus1 as i64,
            });
        }
        if slice_type == SliceType::B && num_ref_idx_l1_active_minus1 > 14 {
            return Err(SliceError::ValueOutOfRange {
                field: "num_ref_idx_l1_active_minus1",
                got: num_ref_idx_l1_active_minus1 as i64,
            });
        }

        let entry_bits = ceil_log2(num_pic_total_curr);
        let max_entry = num_pic_total_curr - 1;

        let ref_pic_list_modification_flag_l0 = br.u1()? != 0;
        let mut list_entry_l0: Vec<u32> = Vec::new();
        if ref_pic_list_modification_flag_l0 {
            let n = num_ref_idx_l0_active_minus1 as u32 + 1;
            list_entry_l0.reserve(n as usize);
            for _ in 0..n {
                let v = br.u(entry_bits)?;
                if v > max_entry {
                    return Err(SliceError::ValueOutOfRange {
                        field: "list_entry_l0",
                        got: v as i64,
                    });
                }
                list_entry_l0.push(v);
            }
        }

        let (ref_pic_list_modification_flag_l1, list_entry_l1) = if slice_type == SliceType::B {
            let flag = br.u1()? != 0;
            let mut entries: Vec<u32> = Vec::new();
            if flag {
                let n = num_ref_idx_l1_active_minus1 as u32 + 1;
                entries.reserve(n as usize);
                for _ in 0..n {
                    let v = br.u(entry_bits)?;
                    if v > max_entry {
                        return Err(SliceError::ValueOutOfRange {
                            field: "list_entry_l1",
                            got: v as i64,
                        });
                    }
                    entries.push(v);
                }
            }
            (Some(flag), entries)
        } else {
            (None, Vec::new())
        };

        Ok(Self {
            ref_pic_list_modification_flag_l0,
            list_entry_l0,
            ref_pic_list_modification_flag_l1,
            list_entry_l1,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NumPicTotalCurrInputs<'a> {
    pub used_by_curr_pic_s0: &'a [bool],
    pub used_by_curr_pic_s1: &'a [bool],
    pub used_by_curr_pic_lt: &'a [bool],
    pub pps_curr_pic_ref_enabled_flag: bool,
    pub nal_unit_type: u8,
    pub num_active_ref_layer_pics: u32,
    pub multilayer_extension: bool,
}

impl<'a> NumPicTotalCurrInputs<'a> {
    pub fn from_used_flags(
        used_by_curr_pic_s0: &'a [bool],
        used_by_curr_pic_s1: &'a [bool],
        used_by_curr_pic_lt: &'a [bool],
    ) -> Self {
        Self {
            used_by_curr_pic_s0,
            used_by_curr_pic_s1,
            used_by_curr_pic_lt,
            pps_curr_pic_ref_enabled_flag: false,
            nal_unit_type: 0,
            num_active_ref_layer_pics: 0,
            multilayer_extension: false,
        }
    }

    pub fn from_explicit_short_term_rps(
        curr_rps: &'a ShortTermRefPicSet,
        used_by_curr_pic_lt: &'a [bool],
    ) -> Option<Self> {
        if curr_rps.inter_ref_pic_set_prediction_flag {
            return None;
        }
        Some(Self::from_used_flags(
            &curr_rps.used_by_curr_pic_s0_flag,
            &curr_rps.used_by_curr_pic_s1_flag,
            used_by_curr_pic_lt,
        ))
    }

    pub fn with_pps_curr_pic_ref_enabled(mut self, flag: bool) -> Self {
        self.pps_curr_pic_ref_enabled_flag = flag;
        self
    }

    pub fn with_multilayer_extension(
        mut self,
        nal_unit_type: u8,
        num_active_ref_layer_pics: u32,
    ) -> Self {
        self.multilayer_extension = true;
        self.nal_unit_type = nal_unit_type;
        self.num_active_ref_layer_pics = num_active_ref_layer_pics;
        self
    }

    pub fn compute(&self) -> u32 {
        let is_idr = self.nal_unit_type == IDR_W_RADL || self.nal_unit_type == IDR_N_LP;
        let skip_temporal_loops = self.multilayer_extension && is_idr;

        let mut n: u32 = 0;
        if !skip_temporal_loops {
            n += self.used_by_curr_pic_s0.iter().filter(|&&v| v).count() as u32;
            n += self.used_by_curr_pic_s1.iter().filter(|&&v| v).count() as u32;
            n += self.used_by_curr_pic_lt.iter().filter(|&&v| v).count() as u32;
        }
        if self.pps_curr_pic_ref_enabled_flag {
            n += 1;
        }
        if self.multilayer_extension {
            n += self.num_active_ref_layer_pics;
        }
        n
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct PredWeightEntry {
    pub luma_weight_flag: bool,
    pub chroma_weight_flag: bool,
    pub delta_luma_weight: i32,
    pub luma_offset: i32,
    pub delta_chroma_weight: [i32; 2],
    pub delta_chroma_offset: [i32; 2],
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PredWeightTable {
    pub luma_log2_weight_denom: u8,
    pub delta_chroma_log2_weight_denom: i32,
    pub entries_l0: Vec<PredWeightEntry>,
    pub entries_l1: Vec<PredWeightEntry>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PredWeightTableInputs<'a> {
    pub slice_type: SliceType,
    pub num_ref_idx_l0_active_minus1: u8,
    pub num_ref_idx_l1_active_minus1: u8,
    pub chroma_array_type: u8,
    pub high_precision_offsets_enabled_flag: bool,
    pub bit_depth_y: u8,
    pub bit_depth_c: u8,
    pub signal_luma_l0: Option<&'a [bool]>,
    pub signal_chroma_l0: Option<&'a [bool]>,
    pub signal_luma_l1: Option<&'a [bool]>,
    pub signal_chroma_l1: Option<&'a [bool]>,
}

impl<'a> PredWeightTableInputs<'a> {
    pub fn base_profile(
        slice_type: SliceType,
        num_ref_idx_l0_active_minus1: u8,
        num_ref_idx_l1_active_minus1: u8,
        chroma_array_type: u8,
        bit_depth_y: u8,
        bit_depth_c: u8,
    ) -> Self {
        Self {
            slice_type,
            num_ref_idx_l0_active_minus1,
            num_ref_idx_l1_active_minus1,
            chroma_array_type,
            high_precision_offsets_enabled_flag: false,
            bit_depth_y,
            bit_depth_c,
            signal_luma_l0: None,
            signal_chroma_l0: None,
            signal_luma_l1: None,
            signal_chroma_l1: None,
        }
    }

    fn wp_offset_half_range_y(&self) -> i32 {
        let shift = if self.high_precision_offsets_enabled_flag {
            (self.bit_depth_y as i32) - 1
        } else {
            7
        };
        1i32 << shift
    }

    fn wp_offset_half_range_c(&self) -> i32 {
        let shift = if self.high_precision_offsets_enabled_flag {
            (self.bit_depth_c as i32) - 1
        } else {
            7
        };
        1i32 << shift
    }
}

impl PredWeightTable {
    pub fn parse(
        br: &mut BitReader<'_>,
        inputs: &PredWeightTableInputs<'_>,
    ) -> Result<Self, SliceError> {
        if inputs.slice_type == SliceType::I {
            return Err(SliceError::ValueOutOfRange {
                field: "pred_weight_table/slice_type",
                got: 2,
            });
        }
        if inputs.num_ref_idx_l0_active_minus1 > 14 {
            return Err(SliceError::ValueOutOfRange {
                field: "num_ref_idx_l0_active_minus1",
                got: inputs.num_ref_idx_l0_active_minus1 as i64,
            });
        }
        if inputs.slice_type == SliceType::B && inputs.num_ref_idx_l1_active_minus1 > 14 {
            return Err(SliceError::ValueOutOfRange {
                field: "num_ref_idx_l1_active_minus1",
                got: inputs.num_ref_idx_l1_active_minus1 as i64,
            });
        }

        let n_l0 = inputs.num_ref_idx_l0_active_minus1 as usize + 1;
        let n_l1 = if inputs.slice_type == SliceType::B {
            inputs.num_ref_idx_l1_active_minus1 as usize + 1
        } else {
            0
        };
        validate_signal_slice("signal_luma_l0", inputs.signal_luma_l0, n_l0)?;
        validate_signal_slice("signal_chroma_l0", inputs.signal_chroma_l0, n_l0)?;
        validate_signal_slice("signal_luma_l1", inputs.signal_luma_l1, n_l1)?;
        validate_signal_slice("signal_chroma_l1", inputs.signal_chroma_l1, n_l1)?;

        let chroma_present = inputs.chroma_array_type != 0;

        let luma_log2_weight_denom_u = br.ue()?;
        if luma_log2_weight_denom_u > 7 {
            return Err(SliceError::ValueOutOfRange {
                field: "luma_log2_weight_denom",
                got: luma_log2_weight_denom_u as i64,
            });
        }
        let luma_log2_weight_denom = luma_log2_weight_denom_u as u8;

        let delta_chroma_log2_weight_denom: i32 = if chroma_present {
            let v = br.se()?;
            let chroma_denom = luma_log2_weight_denom as i32 + v;
            if !(0..=7).contains(&chroma_denom) {
                return Err(SliceError::ValueOutOfRange {
                    field: "ChromaLog2WeightDenom",
                    got: chroma_denom as i64,
                });
            }
            v
        } else {
            0
        };

        // L0: parse the two flag passes (luma, then chroma when
        // chroma is present), then the per-reference delta block.
        let entries_l0 = parse_pred_weight_list(
            br,
            n_l0,
            chroma_present,
            inputs.signal_luma_l0,
            inputs.signal_chroma_l0,
            inputs.wp_offset_half_range_y(),
            inputs.wp_offset_half_range_c(),
            "l0",
        )?;

        // L1: B slices only.
        let entries_l1 = if inputs.slice_type == SliceType::B {
            parse_pred_weight_list(
                br,
                n_l1,
                chroma_present,
                inputs.signal_luma_l1,
                inputs.signal_chroma_l1,
                inputs.wp_offset_half_range_y(),
                inputs.wp_offset_half_range_c(),
                "l1",
            )?
        } else {
            Vec::new()
        };

        // §7.4.7.3 sumWeightLXFlags cap: ≤ 24 per list contribution.
        let sum_l0 = sum_weight_flags(&entries_l0);
        if inputs.slice_type == SliceType::P && sum_l0 > 24 {
            return Err(SliceError::ValueOutOfRange {
                field: "sumWeightL0Flags",
                got: sum_l0 as i64,
            });
        }
        if inputs.slice_type == SliceType::B {
            let sum_l1 = sum_weight_flags(&entries_l1);
            if sum_l0 + sum_l1 > 24 {
                return Err(SliceError::ValueOutOfRange {
                    field: "sumWeightL0Flags+sumWeightL1Flags",
                    got: (sum_l0 + sum_l1) as i64,
                });
            }
        }

        Ok(Self {
            luma_log2_weight_denom,
            delta_chroma_log2_weight_denom,
            entries_l0,
            entries_l1,
        })
    }

    pub fn chroma_log2_weight_denom(&self) -> u8 {
        // The parser's range check on `ChromaLog2WeightDenom ∈ 0..=7`
        // guarantees the sum fits in a `u8`.
        (self.luma_log2_weight_denom as i32 + self.delta_chroma_log2_weight_denom) as u8
    }

    pub fn luma_weight_l0(&self, i: usize) -> Option<i32> {
        self.entries_l0
            .get(i)
            .map(|e| self.luma_weight_value(e.luma_weight_flag, e.delta_luma_weight))
    }

    pub fn luma_weight_l1(&self, i: usize) -> Option<i32> {
        self.entries_l1
            .get(i)
            .map(|e| self.luma_weight_value(e.luma_weight_flag, e.delta_luma_weight))
    }

    pub fn chroma_weight_l0(&self, i: usize, j: usize) -> Option<i32> {
        let e = self.entries_l0.get(i)?;
        let v = *e.delta_chroma_weight.get(j)?;
        Some(self.chroma_weight_value(e.chroma_weight_flag, v))
    }

    pub fn chroma_weight_l1(&self, i: usize, j: usize) -> Option<i32> {
        let e = self.entries_l1.get(i)?;
        let v = *e.delta_chroma_weight.get(j)?;
        Some(self.chroma_weight_value(e.chroma_weight_flag, v))
    }

    pub fn chroma_offset_l0(&self, i: usize, j: usize, wp_offset_half_range_c: i32) -> Option<i32> {
        let e = self.entries_l0.get(i)?;
        if !e.chroma_weight_flag {
            return Some(0);
        }
        let delta_off = *e.delta_chroma_offset.get(j)?;
        let chroma_w = self.chroma_weight_value(true, *e.delta_chroma_weight.get(j)?);
        Some(chroma_offset_eq_7_58(
            wp_offset_half_range_c,
            delta_off,
            chroma_w,
            self.chroma_log2_weight_denom(),
        ))
    }

    pub fn chroma_offset_l1(&self, i: usize, j: usize, wp_offset_half_range_c: i32) -> Option<i32> {
        let e = self.entries_l1.get(i)?;
        if !e.chroma_weight_flag {
            return Some(0);
        }
        let delta_off = *e.delta_chroma_offset.get(j)?;
        let chroma_w = self.chroma_weight_value(true, *e.delta_chroma_weight.get(j)?);
        Some(chroma_offset_eq_7_58(
            wp_offset_half_range_c,
            delta_off,
            chroma_w,
            self.chroma_log2_weight_denom(),
        ))
    }

    fn luma_weight_value(&self, flag: bool, delta: i32) -> i32 {
        let base = 1i32 << self.luma_log2_weight_denom;
        if flag { base + delta } else { base }
    }

    fn chroma_weight_value(&self, flag: bool, delta: i32) -> i32 {
        let base = 1i32 << self.chroma_log2_weight_denom();
        if flag { base + delta } else { base }
    }
}

fn validate_signal_slice(
    field: &'static str,
    slice: Option<&[bool]>,
    expected: usize,
) -> Result<(), SliceError> {
    match slice {
        None => Ok(()),
        Some(s) if s.len() == expected => Ok(()),
        Some(s) => Err(SliceError::ValueOutOfRange {
            field,
            got: s.len() as i64,
        }),
    }
}

#[allow(clippy::too_many_arguments)]
fn parse_pred_weight_list(
    br: &mut BitReader<'_>,
    n: usize,
    chroma_present: bool,
    signal_luma: Option<&[bool]>,
    signal_chroma: Option<&[bool]>,
    wp_off_half_y: i32,
    wp_off_half_c: i32,
    list_tag: &'static str,
) -> Result<Vec<PredWeightEntry>, SliceError> {
    let mut entries: Vec<PredWeightEntry> = (0..n).map(|_| PredWeightEntry::default()).collect();

    // Luma flag pass.
    for (i, e) in entries.iter_mut().enumerate() {
        let signalled = signal_luma.map(|s| s[i]).unwrap_or(true);
        e.luma_weight_flag = if signalled { br.u1()? != 0 } else { false };
    }

    // Chroma flag pass — present only when ChromaArrayType != 0.
    if chroma_present {
        for (i, e) in entries.iter_mut().enumerate() {
            let signalled = signal_chroma.map(|s| s[i]).unwrap_or(true);
            e.chroma_weight_flag = if signalled { br.u1()? != 0 } else { false };
        }
    }

    // Per-reference delta block.
    for (i, e) in entries.iter_mut().enumerate() {
        if e.luma_weight_flag {
            let d = br.se()?;
            if !(-128..=127).contains(&d) {
                return Err(SliceError::ValueOutOfRange {
                    field: if list_tag == "l0" {
                        "delta_luma_weight_l0"
                    } else {
                        "delta_luma_weight_l1"
                    },
                    got: d as i64,
                });
            }
            e.delta_luma_weight = d;

            let off = br.se()?;
            if off < -wp_off_half_y || off > wp_off_half_y - 1 {
                return Err(SliceError::ValueOutOfRange {
                    field: if list_tag == "l0" {
                        "luma_offset_l0"
                    } else {
                        "luma_offset_l1"
                    },
                    got: off as i64,
                });
            }
            e.luma_offset = off;
        }
        if e.chroma_weight_flag {
            for j in 0..2 {
                let d = br.se()?;
                if !(-128..=127).contains(&d) {
                    return Err(SliceError::ValueOutOfRange {
                        field: if list_tag == "l0" {
                            "delta_chroma_weight_l0"
                        } else {
                            "delta_chroma_weight_l1"
                        },
                        got: d as i64,
                    });
                }
                e.delta_chroma_weight[j] = d;

                let off = br.se()?;
                if off < -4 * wp_off_half_c || off > 4 * wp_off_half_c - 1 {
                    return Err(SliceError::ValueOutOfRange {
                        field: if list_tag == "l0" {
                            "delta_chroma_offset_l0"
                        } else {
                            "delta_chroma_offset_l1"
                        },
                        got: off as i64,
                    });
                }
                e.delta_chroma_offset[j] = off;
            }
        }
        let _ = i; // silence the unused-`i` when iter_mut().enumerate() is mixed with explicit loop
    }

    Ok(entries)
}

fn sum_weight_flags(entries: &[PredWeightEntry]) -> u32 {
    entries
        .iter()
        .map(|e| u32::from(e.luma_weight_flag) + 2 * u32::from(e.chroma_weight_flag))
        .sum()
}

fn chroma_offset_eq_7_58(
    wp_off_half_c: i32,
    delta_chroma_offset: i32,
    chroma_weight: i32,
    chroma_log2_weight_denom: u8,
) -> i32 {
    let raw = wp_off_half_c + delta_chroma_offset
        - ((wp_off_half_c * chroma_weight) >> chroma_log2_weight_denom);
    raw.clamp(-wp_off_half_c, wp_off_half_c - 1)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SliceSegmentHeader {
    pub first_slice_segment_in_pic_flag: bool,
    pub no_output_of_prior_pics_flag: Option<bool>,
    pub slice_pic_parameter_set_id: u8,
    pub dependent_slice_segment_flag: bool,
    pub slice_segment_address: u32,
    pub slice_reserved_flags: Vec<bool>,
    pub slice_type: Option<SliceType>,
    pub pic_output_flag: bool,
    pub colour_plane_id: Option<u8>,
    pub slice_pic_order_cnt_lsb: Option<u32>,
    pub short_term_ref_pic_set_sps_flag: Option<bool>,
    pub inline_short_term_ref_pic_set: Option<ShortTermRefPicSet>,
    pub short_term_ref_pic_set_idx: Option<u32>,
    pub num_long_term_sps: Option<u32>,
    pub num_long_term_pics: Option<u32>,
    pub long_term_ref_pics: Vec<SliceLongTermRefPic>,
    pub slice_temporal_mvp_enabled_flag: bool,
    pub slice_sao_luma_flag: bool,
    pub slice_sao_chroma_flag: bool,
    pub num_ref_idx_active_override_flag: Option<bool>,
    pub num_ref_idx_l0_active_minus1: Option<u8>,
    pub num_ref_idx_l1_active_minus1: Option<u8>,
    pub mvd_l1_zero_flag: Option<bool>,
    pub cabac_init_flag: Option<bool>,
    pub collocated_from_l0_flag: Option<bool>,
    pub collocated_ref_idx: Option<u32>,
    pub five_minus_max_num_merge_cand: Option<u32>,
    pub use_integer_mv_flag: bool,
    pub pred_weight_table: Option<PredWeightTable>,
    pub slice_qp_delta: Option<i32>,
    pub slice_cb_qp_offset: i8,
    pub slice_cr_qp_offset: i8,
    pub slice_act_y_qp_offset: i32,
    pub slice_act_cb_qp_offset: i32,
    pub slice_act_cr_qp_offset: i32,
    pub cu_chroma_qp_offset_enabled_flag: bool,
    pub deblocking: Option<SliceDeblocking>,
    pub slice_loop_filter_across_slices_enabled_flag: Option<bool>,
    pub entry_point_offsets: Option<EntryPointOffsets>,
    pub slice_segment_header_extension_length: Option<u32>,
    pub byte_offset_to_slice_data: Option<usize>,
    pub ref_pic_lists_modification: Option<RefPicListsModification>,
    pub opaque_tail: Option<OpaqueTail>,
}

impl SliceSegmentHeader {
    pub fn parse(
        rbsp: &[u8],
        nal_unit_type: u8,
        sps: &SeqParameterSet,
        pps: &PicParameterSet,
    ) -> Result<Self, SliceError> {
        let mut br = BitReader::new(rbsp);

        let first_slice_segment_in_pic_flag = br.u1()? != 0;

        let no_output_of_prior_pics_flag = if (BLA_W_LP..=RSV_IRAP_VCL23).contains(&nal_unit_type) {
            Some(br.u1()? != 0)
        } else {
            None
        };

        let slice_pic_parameter_set_id = br.ue()?;
        if slice_pic_parameter_set_id > 63 {
            return Err(SliceError::ValueOutOfRange {
                field: "slice_pic_parameter_set_id",
                got: slice_pic_parameter_set_id as i64,
            });
        }
        let slice_pic_parameter_set_id = slice_pic_parameter_set_id as u8;

        // §7.3.6.1: dependent_slice_segment_flag / slice_segment_address
        // only appear for non-first slice segments.
        let mut dependent_slice_segment_flag = false;
        let mut slice_segment_address = 0u32;
        if !first_slice_segment_in_pic_flag {
            if pps.dependent_slice_segments_enabled_flag {
                dependent_slice_segment_flag = br.u1()? != 0;
            }
            // slice_segment_address width is Ceil( Log2( PicSizeInCtbsY ) )
            // bits (§7.4.7.1).
            let addr_bits = ceil_log2(pic_size_in_ctbs_y(sps));
            slice_segment_address = br.u(addr_bits)?;
            if slice_segment_address >= pic_size_in_ctbs_y(sps) {
                return Err(SliceError::ValueOutOfRange {
                    field: "slice_segment_address",
                    got: slice_segment_address as i64,
                });
            }
        }

        // Defaults / inferences (§7.4.7.1).
        let mut slice_reserved_flags = Vec::new();
        let mut slice_type = None;
        let mut pic_output_flag = true;
        let mut colour_plane_id = None;
        let mut slice_pic_order_cnt_lsb: Option<u32> = None;
        let mut short_term_ref_pic_set_sps_flag: Option<bool> = None;
        let mut inline_short_term_ref_pic_set: Option<ShortTermRefPicSet> = None;
        let mut short_term_ref_pic_set_idx: Option<u32> = None;
        let mut num_long_term_sps: Option<u32> = None;
        let mut num_long_term_pics: Option<u32> = None;
        let mut long_term_ref_pics: Vec<SliceLongTermRefPic> = Vec::new();
        let mut slice_temporal_mvp_enabled_flag = false;

        if !dependent_slice_segment_flag {
            for _ in 0..pps.num_extra_slice_header_bits {
                slice_reserved_flags.push(br.u1()? != 0);
            }
            let st = SliceType::from_raw(br.ue()?)?;
            slice_type = Some(st);

            if pps.output_flag_present_flag {
                pic_output_flag = br.u1()? != 0;
            }

            if sps.separate_colour_plane_flag {
                let id = br.u(2)? as u8;
                colour_plane_id = Some(id);
            }

            // Non-IDR POC + reference-picture-set block (§7.3.6.1).
            let is_idr = nal_unit_type == IDR_W_RADL || nal_unit_type == IDR_N_LP;
            if !is_idr {
                // slice_pic_order_cnt_lsb u(v), width log2_max_poc_lsb_minus4+4.
                let poc_lsb_bits = sps.log2_max_pic_order_cnt_lsb_minus4 + 4;
                let poc_lsb = br.u(poc_lsb_bits)?;
                if poc_lsb >= sps.max_pic_order_cnt_lsb() {
                    return Err(SliceError::ValueOutOfRange {
                        field: "slice_pic_order_cnt_lsb",
                        got: poc_lsb as i64,
                    });
                }
                slice_pic_order_cnt_lsb = Some(poc_lsb);

                // short_term_ref_pic_set_sps_flag u(1).
                let st_sps_flag = br.u1()? != 0;
                short_term_ref_pic_set_sps_flag = Some(st_sps_flag);
                if st_sps_flag && sps.num_short_term_ref_pic_sets == 0 {
                    // §7.4.7.1: when num_short_term_ref_pic_sets == 0,
                    // short_term_ref_pic_set_sps_flag shall be 0.
                    return Err(SliceError::ValueOutOfRange {
                        field: "short_term_ref_pic_set_sps_flag",
                        got: 1,
                    });
                }

                if !st_sps_flag {
                    let inline = ShortTermRefPicSet::parse_slice_inline(&mut br, sps)?;
                    inline_short_term_ref_pic_set = Some(inline);
                } else if sps.num_short_term_ref_pic_sets > 1 {
                    let idx_bits = ceil_log2(sps.num_short_term_ref_pic_sets);
                    let idx = br.u(idx_bits)?;
                    if idx >= sps.num_short_term_ref_pic_sets {
                        return Err(SliceError::ValueOutOfRange {
                            field: "short_term_ref_pic_set_idx",
                            got: idx as i64,
                        });
                    }
                    short_term_ref_pic_set_idx = Some(idx);
                }
                // else: short_term_ref_pic_set_idx is inferred to 0
                // (and left as None in the struct to signal "absent").

                if sps.long_term_ref_pics_present_flag {
                    let (nl_sps, nl_pics, entries) = parse_long_term_ref_pic_block(&mut br, sps)?;
                    num_long_term_sps = Some(nl_sps);
                    num_long_term_pics = Some(nl_pics);
                    long_term_ref_pics = entries;
                }
                // §7.3.6.1: slice_temporal_mvp_enabled_flag is signalled
                // inside the non-IDR block — an IDR picture never
                // carries it and §7.4.7.1 infers it to 0.
                if sps.sps_temporal_mvp_enabled_flag {
                    slice_temporal_mvp_enabled_flag = br.u1()? != 0;
                }
            }
        }

        // SAO block (§7.3.6.1) — inside the !dependent gate: a dependent
        // slice segment inherits the SAO flags from the associated
        // independent slice segment and does not re-signal them.
        let mut slice_sao_luma_flag = false;
        let mut slice_sao_chroma_flag = false;
        if !dependent_slice_segment_flag && sps.sample_adaptive_offset_enabled_flag {
            slice_sao_luma_flag = br.u1()? != 0;
            if chroma_array_type(sps) != 0 {
                slice_sao_chroma_flag = br.u1()? != 0;
            }
        }

        // The remaining body lives inside the !dependent gate. For a
        // dependent slice segment the header ends after the SAO block,
        // before byte_alignment() (the rest of the header is inherited).
        if dependent_slice_segment_flag {
            // §7.3.6.1: the entry-point block and the header-extension
            // block sit OUTSIDE the !dependent gate — a dependent slice
            // segment signals its own substream entry points.
            let entry_point_offsets = parse_entry_point_offsets(&mut br, sps, pps)?;
            let slice_segment_header_extension_length = parse_header_extension(&mut br, pps)?;
            let byte_offset = consume_byte_alignment(&mut br)?;
            return Ok(Self {
                first_slice_segment_in_pic_flag,
                no_output_of_prior_pics_flag,
                slice_pic_parameter_set_id,
                dependent_slice_segment_flag,
                slice_segment_address,
                slice_reserved_flags,
                slice_type,
                pic_output_flag,
                colour_plane_id,
                slice_pic_order_cnt_lsb: None,
                short_term_ref_pic_set_sps_flag: None,
                inline_short_term_ref_pic_set: None,
                short_term_ref_pic_set_idx: None,
                num_long_term_sps: None,
                num_long_term_pics: None,
                long_term_ref_pics: Vec::new(),
                slice_temporal_mvp_enabled_flag,
                slice_sao_luma_flag,
                slice_sao_chroma_flag,
                num_ref_idx_active_override_flag: None,
                num_ref_idx_l0_active_minus1: None,
                num_ref_idx_l1_active_minus1: None,
                mvd_l1_zero_flag: None,
                cabac_init_flag: None,
                collocated_from_l0_flag: None,
                collocated_ref_idx: None,
                five_minus_max_num_merge_cand: None,
                use_integer_mv_flag: false,
                pred_weight_table: None,
                slice_qp_delta: None,
                slice_cb_qp_offset: 0,
                slice_cr_qp_offset: 0,
                slice_act_y_qp_offset: 0,
                slice_act_cb_qp_offset: 0,
                slice_act_cr_qp_offset: 0,
                cu_chroma_qp_offset_enabled_flag: false,
                deblocking: None,
                slice_loop_filter_across_slices_enabled_flag: None,
                entry_point_offsets,
                slice_segment_header_extension_length,
                byte_offset_to_slice_data: Some(byte_offset),
                ref_pic_lists_modification: None,
                opaque_tail: None,
            });
        }

        // §7.3.6.1: for P / B slices, the SAO block is immediately
        // followed by the `num_ref_idx_active_override_flag` /
        // `num_ref_idx_lX_active_minus1` block. This is the in-place
        // prerequisite for the later `ref_pic_lists_modification()`
        // call (its `list_entry_lX[]` loop indexes `0..=
        // num_ref_idx_lX_active_minus1`). The §7.4.7.1 inference rule
        // fills both `num_ref_idx_lX_active_minus1` values from the PPS
        // defaults when the override flag is 0; both values are capped
        // at 14.
        let st = slice_type.expect("independent slice has a slice_type");
        let (
            num_ref_idx_active_override_flag,
            num_ref_idx_l0_active_minus1,
            num_ref_idx_l1_active_minus1,
        ) = if st.is_inter() {
            let override_flag = br.u1()? != 0;
            let (n0, n1) = if override_flag {
                let n0 = br.ue()?;
                if n0 > 14 {
                    return Err(SliceError::ValueOutOfRange {
                        field: "num_ref_idx_l0_active_minus1",
                        got: n0 as i64,
                    });
                }
                let n1 = if matches!(st, SliceType::B) {
                    let v = br.ue()?;
                    if v > 14 {
                        return Err(SliceError::ValueOutOfRange {
                            field: "num_ref_idx_l1_active_minus1",
                            got: v as i64,
                        });
                    }
                    Some(v as u8)
                } else {
                    None
                };
                (n0 as u8, n1)
            } else {
                // §7.4.7.1 inference defaults from the PPS.
                let n1 = if matches!(st, SliceType::B) {
                    Some(pps.num_ref_idx_l1_default_active_minus1)
                } else {
                    None
                };
                (pps.num_ref_idx_l0_default_active_minus1, n1)
            };
            (Some(override_flag), Some(n0), n1)
        } else {
            (None, None, None)
        };

        // §7.3.6.1 inter-slice continuation: after the
        // `num_ref_idx_active_override_flag` block, the spec emits
        //   if( lists_modification_present_flag && NumPicTotalCurr > 1 )
        //       ref_pic_lists_modification( )
        //   if( slice_type == B )           mvd_l1_zero_flag      u(1)
        //   if( cabac_init_present_flag )   cabac_init_flag       u(1)
        //   if( slice_temporal_mvp_enabled_flag ) {
        //       if( slice_type == B )       collocated_from_l0_flag  u(1)   (else inferred 1, §7.4.7.1)
        //       if( ( collocated_from_l0_flag && num_ref_idx_l0_active_minus1 > 0 ) ||
        //           ( !collocated_from_l0_flag && num_ref_idx_l1_active_minus1 > 0 ) )
        //           collocated_ref_idx     ue(v)                          (else inferred 0)
        //   }
        // followed by the weighted-pred-table gate.
        //
        // The `ref_pic_lists_modification()` gate consumes the
        // §7.4.7.2 `NumPicTotalCurr` derivation (equation 7-57). When
        // `pps.lists_modification_present_flag == 0` the
        // `if(... && NumPicTotalCurr > 1)` short-circuit applies
        // without needing the derivation, so the bit stream advances
        // straight to `mvd_l1_zero_flag`. When the flag is 1 we attempt
        // to derive `NumPicTotalCurr` from the resolved slice-header
        // state (active short-term RPS + long-term entries):
        //
        // * For the inline-form short-term RPS
        //   (`short_term_ref_pic_set_sps_flag == 0`) the on-wire form
        //   per §7.4.8 has `inter_ref_pic_set_prediction_flag == 0`
        //   when `stRpsIdx == num_short_term_ref_pic_sets` (the
        //   slice-inline index): the `used_by_curr_pic_s{0,1}_flag`
        //   arrays come directly from the inline RPS. The §7.4.8 form
        //   *is* allowed at the slice-inline index when the SPS has
        //   `num_short_term_ref_pic_sets > 0`; in that case the parser
        //   defers (the derivation requires walking the source RPS
        //   chain and is out of scope here).
        // * For the SPS-form (`short_term_ref_pic_set_sps_flag == 1`)
        //   the active RPS is `sps.short_term_ref_pic_sets[idx]`. When
        //   that RPS uses the explicit form the per-position
        //   `used_by_curr_pic_sX_flag` arrays are usable directly; when
        //   it uses inter-prediction the §7.4.8 derivation must be run
        //   first and the parser defers.
        //
        // The §F.7.4.7.2 multilayer-extension variant and the
        // SCC `pps_curr_pic_ref_enabled_flag` closing-clause are wired
        // through [`NumPicTotalCurrInputs`] but the base-profile call
        // site here leaves both at their `false` defaults (the PPS
        // SCC extension is not yet surfaced; multilayer extension is
        // forwarded via the long-term-ref builder).
        //
        // For IDR slices the entire non-IDR POC/RPS block is absent
        // and `inline_short_term_ref_pic_set` / `long_term_ref_pics`
        // are empty: `NumPicTotalCurr` is `0` and the gate is
        // statically false.
        let (ref_pic_lists_modification, num_pic_total_curr_resolved) = if st.is_inter()
            && pps.lists_modification_present_flag
        {
            match resolve_active_short_term_rps(
                sps,
                short_term_ref_pic_set_sps_flag,
                inline_short_term_ref_pic_set.as_ref(),
                short_term_ref_pic_set_idx,
            ) {
                ActiveShortTermRps::Materialized(m) => {
                    let lt_used = collect_used_by_curr_pic_lt(&long_term_ref_pics, sps);
                    let inputs = NumPicTotalCurrInputs::from_used_flags(
                        &m.used_by_curr_pic_s0,
                        &m.used_by_curr_pic_s1,
                        &lt_used,
                    );
                    let npc = inputs.compute();
                    if npc > 1 {
                        let l0_active =
                            num_ref_idx_l0_active_minus1.ok_or(SliceError::ValueOutOfRange {
                                field: "num_ref_idx_l0_active_minus1",
                                got: -1,
                            })?;
                        let l1_active = num_ref_idx_l1_active_minus1.unwrap_or(0);
                        let rplm =
                            RefPicListsModification::parse(&mut br, st, l0_active, l1_active, npc)?;
                        (Some(rplm), Some(npc))
                    } else {
                        // `NumPicTotalCurr <= 1` — the §7.3.6.1
                        // gate is statically false; the structure
                        // is not signalled and we continue at
                        // `mvd_l1_zero_flag`.
                        (None, Some(npc))
                    }
                }
                ActiveShortTermRps::Empty => {
                    // IDR or no RPS picked: `NumPicTotalCurr == 0`,
                    // gate is statically false.
                    (None, Some(0))
                }
                ActiveShortTermRps::MaterializeFailed => {
                    // §7.4.8 derivation could not run (malformed
                    // `RefRpsIdx` chain or array-length mismatch);
                    // defer to opaque tail so the caller can salvage
                    // the rest of the bitstream.
                    return Ok(Self {
                        first_slice_segment_in_pic_flag,
                        no_output_of_prior_pics_flag,
                        slice_pic_parameter_set_id,
                        dependent_slice_segment_flag,
                        slice_segment_address,
                        slice_reserved_flags,
                        slice_type,
                        pic_output_flag,
                        colour_plane_id,
                        slice_pic_order_cnt_lsb,
                        short_term_ref_pic_set_sps_flag,
                        inline_short_term_ref_pic_set,
                        short_term_ref_pic_set_idx,
                        num_long_term_sps,
                        num_long_term_pics,
                        long_term_ref_pics,
                        slice_temporal_mvp_enabled_flag,
                        slice_sao_luma_flag,
                        slice_sao_chroma_flag,
                        num_ref_idx_active_override_flag,
                        num_ref_idx_l0_active_minus1,
                        num_ref_idx_l1_active_minus1,
                        mvd_l1_zero_flag: None,
                        cabac_init_flag: None,
                        collocated_from_l0_flag: None,
                        collocated_ref_idx: None,
                        five_minus_max_num_merge_cand: None,
                        use_integer_mv_flag: false,
                        pred_weight_table: None,
                        slice_qp_delta: None,
                        slice_cb_qp_offset: 0,
                        slice_cr_qp_offset: 0,
                        slice_act_y_qp_offset: 0,
                        slice_act_cb_qp_offset: 0,
                        slice_act_cr_qp_offset: 0,
                        cu_chroma_qp_offset_enabled_flag: false,
                        deblocking: None,
                        slice_loop_filter_across_slices_enabled_flag: None,
                        entry_point_offsets: None,
                        slice_segment_header_extension_length: None,
                        byte_offset_to_slice_data: None,
                        ref_pic_lists_modification: None,
                        opaque_tail: Some(OpaqueTail::capture_at(br.bit_pos(), rbsp)),
                    });
                }
            }
        } else {
            (None, None)
        };
        // `num_pic_total_curr_resolved` is currently only used to gate
        // the in-place RPLM parse above; future rounds may surface it
        // on the slice header (the §8.3.4 implicit reference-list
        // derivation needs the same value).
        let _ = num_pic_total_curr_resolved;

        // §7.3.6.1 inter-slice mvd / cabac-init / collocated block —
        // reached only when `slice_type` is P or B and the
        // `ref_pic_lists_modification()` block is statically absent
        // (`pps.lists_modification_present_flag == 0`, which makes the
        // §7.3.6.1 outer `if(... && NumPicTotalCurr > 1)` false
        // unconditionally). For an I slice the entire block is absent
        // and the four fields stay `None`.
        let (mvd_l1_zero_flag, cabac_init_flag, collocated_from_l0_flag, collocated_ref_idx) =
            if st.is_inter() {
                // §7.3.6.1 `if( slice_type == B ) mvd_l1_zero_flag u(1)`.
                let mvd_l1_zero = if matches!(st, SliceType::B) {
                    Some(br.u1()? != 0)
                } else {
                    None
                };

                // §7.3.6.1 `if( cabac_init_present_flag ) cabac_init_flag
                // u(1)`. §7.4.7.1: inferred to 0 when absent.
                let cabac_init = if pps.cabac_init_present_flag {
                    Some(br.u1()? != 0)
                } else {
                    Some(false)
                };

                // §7.3.6.1 temporal-MVP block.
                let (coll_from_l0, coll_ref_idx) = if slice_temporal_mvp_enabled_flag {
                    // `if( slice_type == B ) collocated_from_l0_flag u(1)`,
                    // else §7.4.7.1 inference to 1.
                    let from_l0 = if matches!(st, SliceType::B) {
                        br.u1()? != 0
                    } else {
                        true
                    };

                    // §7.3.6.1: `collocated_ref_idx` is present iff the
                    // active list (selected by `collocated_from_l0_flag`)
                    // has more than one entry. §7.4.7.1: inferred to 0
                    // when absent. Both `num_ref_idx_lX_active_minus1`
                    // values are `Some(_)` at this point (the override
                    // block populated them for every inter slice, and L1
                    // is populated for B slices).
                    let n0 = num_ref_idx_l0_active_minus1.expect("L0 active populated for inter");
                    let needs_ref_idx = if from_l0 {
                        n0 > 0
                    } else {
                        // !from_l0 implies slice_type == B (an I/P slice
                        // takes the inferred `true` branch). For a B slice
                        // L1 is signalled by the override block.
                        let n1 = num_ref_idx_l1_active_minus1.expect("L1 active populated for B");
                        n1 > 0
                    };
                    let ref_idx = if needs_ref_idx {
                        let raw = br.ue()?;
                        let max = if from_l0 {
                            n0 as u32
                        } else {
                            num_ref_idx_l1_active_minus1.unwrap() as u32
                        };
                        if raw > max {
                            return Err(SliceError::ValueOutOfRange {
                                field: "collocated_ref_idx",
                                got: raw as i64,
                            });
                        }
                        raw
                    } else {
                        0
                    };

                    (Some(from_l0), Some(ref_idx))
                } else {
                    (None, None)
                };

                (mvd_l1_zero, cabac_init, coll_from_l0, coll_ref_idx)
            } else {
                (None, None, None, None)
            };

        // §7.3.6.1 P / B `pred_weight_table()` gate. The table is
        // signalled iff either `(weighted_pred_flag && slice_type == P)`
        // or `(weighted_bipred_flag && slice_type == B)`. When the gate
        // is statically absent the parser walks straight past it into
        // the merge-candidate block; when it is present the standalone
        // [`PredWeightTable::parse`] is invoked in place with the
        // base-profile single-layer assumption (every per-i §7.3.6.3
        // outer gate `true` — see [`PredWeightTableInputs::base_profile`]).
        // This is the only single-layer configuration this crate
        // currently surfaces: the SPS range / multilayer / SCC
        // extensions are not yet wired through, so the bit-depth
        // arguments fall back to the SPS `BitDepthY` / `BitDepthC`
        // (`WpOffsetHalfRangeY` = `WpOffsetHalfRangeC` = 128 per §7.4.7.3
        // because `high_precision_offsets_enabled_flag` defaults to 0).
        // The per-i outer-gate decisions, when needed for the
        // multilayer-extension / SCC self-reference cases, will be
        // threaded through here once those extensions are surfaced.
        let weighted_pred_table_present = (pps.weighted_pred_flag && matches!(st, SliceType::P))
            || (pps.weighted_bipred_flag && matches!(st, SliceType::B));
        let pred_weight_table = if weighted_pred_table_present {
            // Resolve the active L0 / L1 cardinalities the table parser
            // needs. Both have been populated by the override block
            // above (§7.4.7.1 inference fills L0 for P/B and L1 for B
            // when override == 0).
            let n0 = num_ref_idx_l0_active_minus1.expect("L0 active populated for inter");
            let n1 = if matches!(st, SliceType::B) {
                num_ref_idx_l1_active_minus1.expect("L1 active populated for B")
            } else {
                0
            };
            let inputs = PredWeightTableInputs::base_profile(
                st,
                n0,
                n1,
                chroma_array_type(sps),
                sps.bit_depth_luma(),
                sps.bit_depth_chroma(),
            );
            Some(PredWeightTable::parse(&mut br, &inputs)?)
        } else {
            None
        };

        // §7.3.6.1 `five_minus_max_num_merge_cand` (ue(v)), signalled
        // for every inter slice immediately after the (optional)
        // pred_weight_table(). §7.4.7.1 derives
        // `MaxNumMergeCand = 5 - five_minus_max_num_merge_cand`, with
        // the conformance constraint `1 <= MaxNumMergeCand <= 5` —
        // i.e. the wire value lies in 0..=4. The SCC
        // `use_integer_mv_flag` (gated on
        // `motion_vector_resolution_control_idc == 2`) is statically
        // absent because the PPS SCC extension is not surfaced by this
        // crate yet (§7.4.7.1: when not present,
        // `motion_vector_resolution_control_idc` is inferred to 0).
        let five_minus_max_num_merge_cand = if st.is_inter() {
            let v = br.ue()?;
            if v > 4 {
                return Err(SliceError::ValueOutOfRange {
                    field: "five_minus_max_num_merge_cand",
                    got: v as i64,
                });
            }
            Some(v)
        } else {
            None
        };

        // §7.3.6.1: use_integer_mv_flag is present for inter slices
        // when motion_vector_resolution_control_idc == 2; otherwise
        // §7.4.7.1 infers it equal to the idc value.
        let mv_res_idc = sps
            .sps_scc_extension
            .as_ref()
            .map_or(0, |s| s.motion_vector_resolution_control_idc);
        let use_integer_mv_flag = if st.is_inter() && mv_res_idc == 2 {
            br.u1()? != 0
        } else {
            mv_res_idc == 1
        };

        // Slice QP / chroma QP / deblocking / loop-filter / entry-points
        // tail (§7.3.6.1) — shared by I, P and B independent slice
        // segments.
        let slice_qp_delta = br.se()?;

        let mut slice_cb_qp_offset = 0i8;
        let mut slice_cr_qp_offset = 0i8;
        if pps.pps_slice_chroma_qp_offsets_present_flag {
            slice_cb_qp_offset = parse_qp_offset(&mut br, "slice_cb_qp_offset")?;
            slice_cr_qp_offset = parse_qp_offset(&mut br, "slice_cr_qp_offset")?;
        }

        // SCC adaptive-colour-transform per-slice QP offsets (§7.3.6.1),
        // present only when the SCC PPS body set
        // `pps_slice_act_qp_offsets_present_flag`. §7.4.7.1 bounds the
        // sum `PpsActQpOffset{Y,Cb,Cr} + slice_act_{y,cb,cr}_qp_offset`
        // to −12..=12; the per-element offsets themselves are se(v) with
        // no independent bound, so the conformance check is applied to
        // the combined value using the PPS-level offsets.
        let mut slice_act_y_qp_offset = 0i32;
        let mut slice_act_cb_qp_offset = 0i32;
        let mut slice_act_cr_qp_offset = 0i32;
        let pps_slice_act_qp_offsets_present_flag = pps
            .pps_scc_extension
            .as_ref()
            .map(|scc| scc.pps_slice_act_qp_offsets_present_flag)
            .unwrap_or(false);
        if pps_slice_act_qp_offsets_present_flag {
            // The presence of the offsets implies a decoded SCC body.
            let scc = pps
                .pps_scc_extension
                .as_ref()
                .expect("pps_slice_act_qp_offsets_present_flag implies SCC body");
            slice_act_y_qp_offset = parse_slice_act_qp_offset(
                &mut br,
                "slice_act_y_qp_offset",
                scc.pps_act_qp_offset_y(),
            )?;
            slice_act_cb_qp_offset = parse_slice_act_qp_offset(
                &mut br,
                "slice_act_cb_qp_offset",
                scc.pps_act_qp_offset_cb(),
            )?;
            slice_act_cr_qp_offset = parse_slice_act_qp_offset(
                &mut br,
                "slice_act_cr_qp_offset",
                scc.pps_act_qp_offset_cr(),
            )?;
        }

        // `cu_chroma_qp_offset_enabled_flag` (§7.3.6.1), present only
        // when the range-extension `chroma_qp_offset_list_enabled_flag`
        // is set; inferred to 0 otherwise (§7.4.7.1).
        let chroma_qp_offset_list_enabled_flag = pps
            .pps_range_extension
            .as_ref()
            .map(|re| re.chroma_qp_offset_list_enabled_flag)
            .unwrap_or(false);
        let cu_chroma_qp_offset_enabled_flag = if chroma_qp_offset_list_enabled_flag {
            br.u1()? != 0
        } else {
            false
        };

        // Deblocking override (§7.3.6.1).
        let deblocking = parse_slice_deblocking(&mut br, pps)?;

        // slice_loop_filter_across_slices_enabled_flag gate (§7.3.6.1).
        let slice_loop_filter_across_slices_enabled_flag = if pps
            .pps_loop_filter_across_slices_enabled_flag
            && (slice_sao_luma_flag || slice_sao_chroma_flag || !deblocking.disabled_flag)
        {
            br.u1()? != 0
        } else {
            pps.pps_loop_filter_across_slices_enabled_flag
        };

        // Entry-point-offset block (§7.3.6.1). §7.4.7.1 bounds
        // `num_entry_point_offsets` by the active partitioning: the
        // tile count when only `tiles_enabled_flag == 1`,
        // `PicHeightInCtbsY` when only
        // `entropy_coding_sync_enabled_flag == 1`, and
        // `NumTileColumns * PicHeightInCtbsY` when both are 1
        // (wavefronts inside every tile). Each
        // `entry_point_offset_minus1[i]` is `offset_len_minus1 + 1`
        // bits wide and is read into [`EntryPointOffsets::
        // entry_point_offset_minus1`] (a per-index `Vec<u32>`); the
        // byte length of subset `i` follows as
        // `entry_point_offset_minus1[i] + 1` (§7.4.7.1) and is exposed
        // via [`EntryPointOffsets::subset_length`].
        let entry_point_offsets = parse_entry_point_offsets(&mut br, sps, pps)?;

        // Slice-segment-header extension block (§7.3.6.1).
        let slice_segment_header_extension_length = parse_header_extension(&mut br, pps)?;

        let byte_offset = consume_byte_alignment(&mut br)?;

        Ok(Self {
            first_slice_segment_in_pic_flag,
            no_output_of_prior_pics_flag,
            slice_pic_parameter_set_id,
            dependent_slice_segment_flag,
            slice_segment_address,
            slice_reserved_flags,
            slice_type,
            pic_output_flag,
            colour_plane_id,
            slice_pic_order_cnt_lsb,
            short_term_ref_pic_set_sps_flag,
            inline_short_term_ref_pic_set,
            short_term_ref_pic_set_idx,
            num_long_term_sps,
            num_long_term_pics,
            long_term_ref_pics,
            slice_temporal_mvp_enabled_flag,
            slice_sao_luma_flag,
            slice_sao_chroma_flag,
            num_ref_idx_active_override_flag,
            num_ref_idx_l0_active_minus1,
            num_ref_idx_l1_active_minus1,
            mvd_l1_zero_flag,
            cabac_init_flag,
            collocated_from_l0_flag,
            collocated_ref_idx,
            five_minus_max_num_merge_cand,
            use_integer_mv_flag,
            pred_weight_table,
            slice_qp_delta: Some(slice_qp_delta),
            slice_cb_qp_offset,
            slice_cr_qp_offset,
            slice_act_y_qp_offset,
            slice_act_cb_qp_offset,
            slice_act_cr_qp_offset,
            cu_chroma_qp_offset_enabled_flag,
            deblocking: Some(deblocking),
            slice_loop_filter_across_slices_enabled_flag: Some(
                slice_loop_filter_across_slices_enabled_flag,
            ),
            entry_point_offsets,
            slice_segment_header_extension_length,
            byte_offset_to_slice_data: Some(byte_offset),
            ref_pic_lists_modification,
            opaque_tail: None,
        })
    }

    pub fn slice_qp_y(&self, pps: &PicParameterSet) -> Option<i32> {
        self.slice_qp_delta.map(|d| 26 + pps.init_qp_minus26 + d)
    }

    pub fn max_num_merge_cand(&self) -> Option<u8> {
        self.five_minus_max_num_merge_cand
            .map(|v| 5u8.saturating_sub(v as u8))
    }
}

fn chroma_array_type(sps: &SeqParameterSet) -> u8 {
    if sps.separate_colour_plane_flag {
        0
    } else {
        sps.chroma_format_idc
    }
}

fn pic_size_in_ctbs_y(sps: &SeqParameterSet) -> u32 {
    let ctb_size = 1u32 << sps.log2_ctb_size();
    let width_in_ctbs = sps.pic_width_in_luma_samples.div_ceil(ctb_size);
    let height_in_ctbs = sps.pic_height_in_luma_samples.div_ceil(ctb_size);
    width_in_ctbs * height_in_ctbs
}

fn pic_height_in_ctbs_y(sps: &SeqParameterSet) -> u32 {
    let ctb_size = 1u32 << sps.log2_ctb_size();
    sps.pic_height_in_luma_samples.div_ceil(ctb_size)
}

fn parse_entry_point_offsets(
    br: &mut BitReader<'_>,
    sps: &SeqParameterSet,
    pps: &PicParameterSet,
) -> Result<Option<EntryPointOffsets>, SliceError> {
    if !(pps.tiles_enabled_flag || pps.entropy_coding_sync_enabled_flag) {
        return Ok(None);
    }
    let num_entry_point_offsets = br.ue()?;
    let max_num_entry_point_offsets = num_entry_point_offsets_upper_bound(sps, pps);
    if num_entry_point_offsets > max_num_entry_point_offsets {
        return Err(SliceError::ValueOutOfRange {
            field: "num_entry_point_offsets",
            got: num_entry_point_offsets as i64,
        });
    }
    let (offset_len_minus1, entry_point_offset_minus1) = if num_entry_point_offsets > 0 {
        let v = br.ue()?;
        if v > 31 {
            return Err(SliceError::ValueOutOfRange {
                field: "offset_len_minus1",
                got: v as i64,
            });
        }
        let len = v as u8;
        let bits = len + 1;
        let mut offsets = Vec::with_capacity(num_entry_point_offsets as usize);
        for _ in 0..num_entry_point_offsets {
            offsets.push(br.u(bits)?);
        }
        (len, offsets)
    } else {
        (0, Vec::new())
    };
    Ok(Some(EntryPointOffsets {
        num_entry_point_offsets,
        offset_len_minus1,
        entry_point_offset_minus1,
    }))
}

fn parse_header_extension(
    br: &mut BitReader<'_>,
    pps: &PicParameterSet,
) -> Result<Option<u32>, SliceError> {
    if !pps.slice_segment_header_extension_present_flag {
        return Ok(None);
    }
    let len = br.ue()?;
    for _ in 0..len {
        br.skip(8)?;
    }
    Ok(Some(len))
}

fn num_entry_point_offsets_upper_bound(sps: &SeqParameterSet, pps: &PicParameterSet) -> u32 {
    // §7.4.7.1, three-way constraint:
    //   * tiles == 0, sync == 1  ⇒ 0 .. PicHeightInCtbsY − 1
    //   * tiles == 1, sync == 0  ⇒ 0 .. cols * rows − 1
    //   * tiles == 1, sync == 1  ⇒ 0 .. cols * PicHeightInCtbsY − 1
    // (wavefront rows counted per tile COLUMN — every tile row of a
    // column contributes its CTB rows, summing to PicHeightInCtbsY).
    // Arithmetic in u64 keeps the parser defensive against a
    // pathological PPS even though no conforming level overflows u32.
    let bound = if pps.tiles_enabled_flag {
        let cols = u64::from(pps.tiles.num_tile_columns_minus1) + 1;
        if pps.entropy_coding_sync_enabled_flag {
            cols.saturating_mul(u64::from(pic_height_in_ctbs_y(sps)))
                .saturating_sub(1)
        } else {
            let rows = u64::from(pps.tiles.num_tile_rows_minus1) + 1;
            cols.saturating_mul(rows).saturating_sub(1)
        }
    } else {
        u64::from(pic_height_in_ctbs_y(sps)).saturating_sub(1)
    };
    u32::try_from(bound).unwrap_or(u32::MAX)
}

fn ceil_log2(n: u32) -> u8 {
    if n <= 1 {
        0
    } else {
        // Ceil(Log2(n)) = bit-width of (n - 1).
        (32 - (n - 1).leading_zeros()) as u8
    }
}

fn parse_qp_offset(br: &mut BitReader<'_>, field: &'static str) -> Result<i8, SliceError> {
    let v = br.se()?;
    if !(-12..=12).contains(&v) {
        return Err(SliceError::ValueOutOfRange {
            field,
            got: v as i64,
        });
    }
    Ok(v as i8)
}

fn parse_slice_act_qp_offset(
    br: &mut BitReader<'_>,
    field: &'static str,
    pps_act_qp_offset: i32,
) -> Result<i32, SliceError> {
    let v = br.se()?;
    let sum = pps_act_qp_offset + v;
    if !(-12..=12).contains(&sum) {
        return Err(SliceError::ValueOutOfRange {
            field,
            got: sum as i64,
        });
    }
    Ok(v)
}

fn parse_slice_deblocking(
    br: &mut BitReader<'_>,
    pps: &PicParameterSet,
) -> Result<SliceDeblocking, SliceError> {
    // deblocking_filter_override_flag only present when
    // deblocking_filter_override_enabled_flag (PPS).
    let override_flag = if pps.deblocking.override_enabled_flag {
        br.u1()? != 0
    } else {
        false
    };

    if !override_flag {
        // Inferred from the PPS (§7.4.7.1).
        return Ok(SliceDeblocking {
            disabled_flag: pps.deblocking.disabled_flag,
            beta_offset_div2: pps.deblocking.beta_offset_div2,
            tc_offset_div2: pps.deblocking.tc_offset_div2,
        });
    }

    let disabled_flag = br.u1()? != 0;
    let (beta, tc) = if !disabled_flag {
        let beta = br.se()?;
        if !(-6..=6).contains(&beta) {
            return Err(SliceError::ValueOutOfRange {
                field: "slice_beta_offset_div2",
                got: beta as i64,
            });
        }
        let tc = br.se()?;
        if !(-6..=6).contains(&tc) {
            return Err(SliceError::ValueOutOfRange {
                field: "slice_tc_offset_div2",
                got: tc as i64,
            });
        }
        (beta as i8, tc as i8)
    } else {
        // When deblocking is disabled the offsets are not signalled and
        // are inferred to 0 (their effect is moot when disabled).
        (0, 0)
    };

    Ok(SliceDeblocking {
        disabled_flag,
        beta_offset_div2: beta,
        tc_offset_div2: tc,
    })
}

fn parse_long_term_ref_pic_block(
    br: &mut BitReader<'_>,
    sps: &SeqParameterSet,
) -> Result<(u32, u32, Vec<SliceLongTermRefPic>), SliceError> {
    let num_long_term_sps = if sps.num_long_term_ref_pics_sps > 0 {
        let v = br.ue()?;
        if v > sps.num_long_term_ref_pics_sps {
            return Err(SliceError::ValueOutOfRange {
                field: "num_long_term_sps",
                got: v as i64,
            });
        }
        v
    } else {
        0
    };
    let num_long_term_pics = br.ue()?;
    // §7.4.7.1 bounds num_long_term_pics by the SPS DPB capacity; we
    // apply a defensive sanity ceiling instead of computing the full
    // DPB-derived bound (which needs RPS counts not yet wired through
    // here). A pathological encoder could otherwise drive an unbounded
    // allocation.
    if num_long_term_pics > HEVC_MAX_LONG_TERM_PICS_IN_SLICE as u32 {
        return Err(SliceError::ValueOutOfRange {
            field: "num_long_term_pics",
            got: num_long_term_pics as i64,
        });
    }
    let total = num_long_term_sps + num_long_term_pics;
    let lt_idx_bits = if sps.num_long_term_ref_pics_sps > 1 {
        ceil_log2(sps.num_long_term_ref_pics_sps)
    } else {
        0
    };
    let poc_lsb_bits = sps.log2_max_pic_order_cnt_lsb_minus4 + 4;

    let mut entries = Vec::with_capacity(total as usize);
    for i in 0..total {
        let source = if i < num_long_term_sps {
            let lt_idx_sps = if lt_idx_bits > 0 {
                let v = br.u(lt_idx_bits)?;
                if v >= sps.num_long_term_ref_pics_sps {
                    return Err(SliceError::ValueOutOfRange {
                        field: "lt_idx_sps",
                        got: v as i64,
                    });
                }
                v
            } else {
                0
            };
            SliceLongTermRefPicSource::Sps { lt_idx_sps }
        } else {
            let poc_lsb_lt = br.u(poc_lsb_bits)?;
            let used_by_curr_pic_lt_flag = br.u1()? != 0;
            SliceLongTermRefPicSource::InSlice {
                poc_lsb_lt,
                used_by_curr_pic_lt_flag,
            }
        };
        let delta_poc_msb_present_flag = br.u1()? != 0;
        let delta_poc_msb_cycle_lt = if delta_poc_msb_present_flag {
            br.ue()?
        } else {
            0
        };
        entries.push(SliceLongTermRefPic {
            source,
            delta_poc_msb_present_flag,
            delta_poc_msb_cycle_lt,
        });
    }
    Ok((num_long_term_sps, num_long_term_pics, entries))
}

enum ActiveShortTermRps {
    Materialized(crate::sps::MaterializedShortTermRefPicSet),
    Empty,
    MaterializeFailed,
}

fn resolve_active_short_term_rps(
    sps: &SeqParameterSet,
    short_term_ref_pic_set_sps_flag: Option<bool>,
    inline_rps: Option<&ShortTermRefPicSet>,
    short_term_ref_pic_set_idx: Option<u32>,
) -> ActiveShortTermRps {
    // Materialise the SPS list once; we may need it both as a source
    // for the slice-inline inter-RPS-prediction and as the active RPS
    // for the SPS form. The list is short (cap
    // `HEVC_MAX_NUM_SHORT_TERM_RPS = 64`) so this is inexpensive
    // relative to a frame decode.
    let sps_materialised = match sps.materialize_short_term_ref_pic_sets() {
        Ok(v) => v,
        Err(_) => return ActiveShortTermRps::MaterializeFailed,
    };
    match short_term_ref_pic_set_sps_flag {
        None => ActiveShortTermRps::Empty,
        Some(false) => match inline_rps {
            None => ActiveShortTermRps::Empty,
            Some(rps) => {
                let source = if rps.inter_ref_pic_set_prediction_flag {
                    // For the slice-inline form `stRpsIdx ==
                    // num_short_term_ref_pic_sets` and the source is
                    // `RefRpsIdx = num_short_term_ref_pic_sets -
                    // (delta_idx_minus1 + 1)` per equation 7-59.
                    let st_rps_idx = sps.num_short_term_ref_pic_sets as i64;
                    let ref_rps_idx = st_rps_idx - (rps.delta_idx_minus1 as i64 + 1);
                    if ref_rps_idx < 0 {
                        return ActiveShortTermRps::MaterializeFailed;
                    }
                    sps_materialised.get(ref_rps_idx as usize)
                } else {
                    None
                };
                match rps.materialize(source) {
                    Ok(m) => ActiveShortTermRps::Materialized(m),
                    Err(_) => ActiveShortTermRps::MaterializeFailed,
                }
            }
        },
        Some(true) => {
            // §7.4.7.1: when not signalled (because
            // `num_short_term_ref_pic_sets <= 1`), the index is
            // inferred to 0.
            let idx = short_term_ref_pic_set_idx.unwrap_or(0) as usize;
            match sps_materialised.into_iter().nth(idx) {
                None => ActiveShortTermRps::Empty,
                Some(m) => ActiveShortTermRps::Materialized(m),
            }
        }
    }
}

fn collect_used_by_curr_pic_lt(
    entries: &[SliceLongTermRefPic],
    sps: &SeqParameterSet,
) -> Vec<bool> {
    entries
        .iter()
        .map(|e| e.used_by_curr_pic_lt(sps).unwrap_or(false))
        .collect()
}

const HEVC_MAX_LONG_TERM_PICS_IN_SLICE: usize = 16;

fn consume_byte_alignment(br: &mut BitReader<'_>) -> Result<usize, SliceError> {
    // alignment_bit_equal_to_one.
    let _ = br.u1()?;
    while br.bit_pos() % 8 != 0 {
        let _ = br.u1()?;
    }
    Ok(br.bit_pos() / 8)
}
