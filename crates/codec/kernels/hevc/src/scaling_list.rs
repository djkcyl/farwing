// SPDX-License-Identifier: MIT
// Derived from oxideav-h265 0.0.10, Copyright (c) 2026 Karpelès Lab Inc.
// Retained syntax/metadata primitives used by this client; see COPYING.OxideAV in this crate.

use crate::bitreader::{BitReader, BitReaderError};

use crate::scan::up_right_diagonal;

pub const NUM_SIZE_IDS: usize = 4;

pub const NUM_MATRIX_IDS: usize = 6;

pub const MAX_COEF_NUM: usize = 64;

fn coef_num(size_id: usize) -> usize {
    (1usize << (4 + (size_id << 1))).min(MAX_COEF_NUM)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScalingListError {
    Truncated,
    PredMatrixIdDeltaOutOfRange {
        size_id: u8,
        matrix_id: u8,
        got: u32,
    },
    DcCoefOutOfRange {
        size_id: u8,
        matrix_id: u8,
        got: i32,
    },
    DeltaCoefOutOfRange {
        size_id: u8,
        matrix_id: u8,
        index: u8,
        got: i32,
    },
    NonPositiveCoef {
        size_id: u8,
        matrix_id: u8,
        index: u8,
    },
    Bitstream(BitReaderError),
}

impl core::fmt::Display for ScalingListError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Truncated => f.write_str("scaling_list_data() truncated"),
            Self::PredMatrixIdDeltaOutOfRange {
                size_id,
                matrix_id,
                got,
            } => write!(
                f,
                "scaling_list_pred_matrix_id_delta[{size_id}][{matrix_id}] out of range: {got}"
            ),
            Self::DcCoefOutOfRange {
                size_id,
                matrix_id,
                got,
            } => write!(
                f,
                "scaling_list_dc_coef_minus8[{}][{matrix_id}] out of range: {got}",
                size_id - 2
            ),
            Self::DeltaCoefOutOfRange {
                size_id,
                matrix_id,
                index,
                got,
            } => write!(
                f,
                "scaling_list_delta_coef[{size_id}][{matrix_id}][{index}] out of range: {got}"
            ),
            Self::NonPositiveCoef {
                size_id,
                matrix_id,
                index,
            } => write!(
                f,
                "ScalingList[{size_id}][{matrix_id}][{index}] is not greater than 0"
            ),
            Self::Bitstream(e) => write!(f, "bitstream error during scaling_list_data(): {e}"),
        }
    }
}

impl std::error::Error for ScalingListError {}

impl From<BitReaderError> for ScalingListError {
    fn from(e: BitReaderError) -> Self {
        match e {
            BitReaderError::EndOfBuffer => Self::Truncated,
            other => Self::Bitstream(other),
        }
    }
}

const DEFAULT_4X4: [u16; 16] = [16; 16];

const DEFAULT_8X8_INTRA: [u16; 64] = [
    16, 16, 16, 16, 16, 16, 16, 16, 16, 16, 17, 16, 17, 16, 17, 18, // i = 0..15
    17, 18, 18, 17, 18, 21, 19, 20, 21, 20, 19, 21, 24, 22, 22, 24, // i = 16..31
    24, 22, 22, 24, 25, 25, 27, 30, 27, 25, 25, 29, 31, 35, 35, 31, // i = 32..47
    29, 36, 41, 44, 41, 36, 47, 54, 54, 47, 65, 70, 65, 88, 88, 115, // i = 48..63
];

const DEFAULT_8X8_INTER: [u16; 64] = [
    16, 16, 16, 16, 16, 16, 16, 16, 16, 16, 17, 17, 17, 17, 17, 18, // i = 0..15
    18, 18, 18, 18, 18, 20, 20, 20, 20, 20, 20, 20, 24, 24, 24, 24, // i = 16..31
    24, 24, 24, 24, 25, 25, 25, 25, 25, 25, 25, 28, 28, 28, 28, 28, // i = 32..47
    28, 33, 33, 33, 33, 33, 41, 41, 41, 41, 54, 54, 54, 71, 71, 91, // i = 48..63
];

pub fn default_scaling_list(size_id: usize, matrix_id: usize) -> &'static [u16] {
    if size_id == 0 {
        &DEFAULT_4X4
    } else if matrix_id < 3 {
        &DEFAULT_8X8_INTRA
    } else {
        &DEFAULT_8X8_INTER
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScalingListMatrix {
    pub coef: Vec<u16>,
    pub dc_coef: u16,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScalingListData {
    pub lists: [[ScalingListMatrix; NUM_MATRIX_IDS]; NUM_SIZE_IDS],
}

impl ScalingListData {
    #[must_use]
    pub fn all_default() -> Self {
        let lists: [[ScalingListMatrix; NUM_MATRIX_IDS]; NUM_SIZE_IDS] =
            core::array::from_fn(|size_id| {
                core::array::from_fn(|matrix_id| ScalingListMatrix {
                    coef: default_scaling_list(size_id, matrix_id).to_vec(),
                    // §7.4.5: the inferred scaling_list_dc_coef_minus8
                    // is 8 ⇒ ScalingFactor[sizeId][matrixId][0][0] = 16
                    // (matching the default lists' first coefficient).
                    dc_coef: 16,
                })
            });
        Self { lists }
    }

    pub fn parse(br: &mut BitReader<'_>) -> Result<Self, ScalingListError> {
        // Seed every slot with its default; signalled slots overwrite.
        // sizeId == 3 only visits matrixId 0 and 3 (the `+= 3` step),
        // so the other four slots stay at their (unused) default.
        let mut lists = Self::all_default().lists;

        for (size_id, size_lists) in lists.iter_mut().enumerate() {
            // matrixId += (sizeId == 3) ? 3 : 1  (§7.3.4)
            let step = if size_id == 3 { 3 } else { 1 };
            let mut matrix_id = 0usize;
            while matrix_id < NUM_MATRIX_IDS {
                let pred_mode_flag = br.u1()? != 0;
                if !pred_mode_flag {
                    // Prediction from a reference list of the same sizeId.
                    let delta = br.ue()?;
                    let max_delta = if size_id <= 2 {
                        matrix_id as u32
                    } else {
                        matrix_id as u32 / 3
                    };
                    if delta > max_delta {
                        return Err(ScalingListError::PredMatrixIdDeltaOutOfRange {
                            size_id: size_id as u8,
                            matrix_id: matrix_id as u8,
                            got: delta,
                        });
                    }
                    if delta == 0 {
                        // §7.4.5: inferred from the default scaling list.
                        let def = default_scaling_list(size_id, matrix_id);
                        size_lists[matrix_id] = ScalingListMatrix {
                            coef: def.to_vec(),
                            // §7.4.5: scaling_list_dc_coef_minus8 is
                            // inferred to be 8 for the default list,
                            // i.e. a DC scaling factor of 16.
                            dc_coef: 16,
                        };
                    } else {
                        // refMatrixId = matrixId − delta * step  (7-42),
                        // and copy the reference list (7-43) including
                        // its DC coefficient (§7.4.5 last paragraph).
                        let ref_matrix_id = matrix_id - (delta as usize) * step;
                        size_lists[matrix_id] = size_lists[ref_matrix_id].clone();
                    }
                } else {
                    // Explicitly signalled list.
                    let n = coef_num(size_id);
                    let mut next_coef: i32 = 8;
                    let mut dc_coef: u16 = 8;
                    if size_id > 1 {
                        let dc = br.se()?;
                        if !(-7..=247).contains(&dc) {
                            return Err(ScalingListError::DcCoefOutOfRange {
                                size_id: size_id as u8,
                                matrix_id: matrix_id as u8,
                                got: dc,
                            });
                        }
                        next_coef = dc + 8;
                        dc_coef = (dc + 8) as u16;
                    }
                    let mut coef = Vec::with_capacity(n);
                    for i in 0..n {
                        let delta_coef = br.se()?;
                        // §7.4.5: "The value of scaling_list_delta_coef
                        // shall be in the range of −128 to 127,
                        // inclusive." (Also keeps the eq. nextCoef sum
                        // below inside i32.)
                        if !(-128..=127).contains(&delta_coef) {
                            return Err(ScalingListError::DeltaCoefOutOfRange {
                                size_id: size_id as u8,
                                matrix_id: matrix_id as u8,
                                index: i as u8,
                                got: delta_coef,
                            });
                        }
                        // nextCoef = (nextCoef + delta_coef + 256) % 256
                        next_coef = (next_coef + delta_coef + 256).rem_euclid(256);
                        if next_coef <= 0 {
                            return Err(ScalingListError::NonPositiveCoef {
                                size_id: size_id as u8,
                                matrix_id: matrix_id as u8,
                                index: i as u8,
                            });
                        }
                        coef.push(next_coef as u16);
                    }
                    size_lists[matrix_id] = ScalingListMatrix { coef, dc_coef };
                }
                matrix_id += step;
            }
        }

        Ok(Self { lists })
    }

    pub fn scaling_factors(&self, chroma_array_type: u8) -> ScalingFactors {
        // ScanOrder[2][0] — up-right diagonal scan of a 4x4 block
        // (16 positions); ScanOrder[3][0] — of an 8x8 block (64).
        let scan_4 = up_right_diagonal(4);
        let scan_8 = up_right_diagonal(8);

        let mut out = ScalingFactors {
            factors: core::array::from_fn(|size_id| {
                let dim = 1usize << (2 + size_id); // 4, 8, 16, 32
                core::array::from_fn(|_matrix_id| ScalingFactorMatrix {
                    dim: dim as u8,
                    coef: vec![0u16; dim * dim],
                })
            }),
        };

        for matrix_id in 0..NUM_MATRIX_IDS {
            // 4x4 (sizeId 0) — equation 7-44: place ScalingList[0][m][i]
            // at (ScanOrder[2][0][i][0], ScanOrder[2][0][i][1]).
            place(
                &mut out.factors[0][matrix_id],
                &self.lists[0][matrix_id].coef,
                &scan_4,
                1,
            );

            // 8x8 (sizeId 1) — equation 7-45: 8x8 scan, no upsampling.
            place(
                &mut out.factors[1][matrix_id],
                &self.lists[1][matrix_id].coef,
                &scan_8,
                1,
            );

            // 16x16 (sizeId 2) — equation 7-46: 8x8 scan, each entry
            // replicated into a 2x2 block. Then 7-47 overrides [0][0]
            // with the DC coefficient.
            place(
                &mut out.factors[2][matrix_id],
                &self.lists[2][matrix_id].coef,
                &scan_8,
                2,
            );
            set_dc(
                &mut out.factors[2][matrix_id],
                self.lists[2][matrix_id].dc_coef,
            );
        }

        // 32x32 (sizeId 3) — only matrixId 0 (intra Y) and 3 (inter Y)
        // are signalled (the `matrixId += 3` step). Equation 7-48 uses
        // the 8x8 scan with each entry replicated into a 4x4 block,
        // 7-49 overrides [0][0] with the DC coefficient.
        for &matrix_id in &[0usize, 3usize] {
            place(
                &mut out.factors[3][matrix_id],
                &self.lists[3][matrix_id].coef,
                &scan_8,
                4,
            );
            set_dc(
                &mut out.factors[3][matrix_id],
                self.lists[3][matrix_id].dc_coef,
            );
        }

        // 32x32 chroma (matrixId 1, 2, 4, 5) — equations 7-50 / 7-51,
        // applicable only for ChromaArrayType == 3 (4:4:4). The flat
        // list comes from the 16x16 (sizeId 2) list of the same
        // matrixId, and the DC coefficient also from sizeId 2.
        if chroma_array_type == 3 {
            for &matrix_id in &[1usize, 2usize, 4usize, 5usize] {
                place(
                    &mut out.factors[3][matrix_id],
                    &self.lists[2][matrix_id].coef,
                    &scan_8,
                    4,
                );
                set_dc(
                    &mut out.factors[3][matrix_id],
                    self.lists[2][matrix_id].dc_coef,
                );
            }
        }

        out
    }
}

fn place(
    matrix: &mut ScalingFactorMatrix,
    coef: &[u16],
    scan: &[crate::scan::ScanPos],
    rep: usize,
) {
    let dim = matrix.dim as usize;
    for (i, value) in coef.iter().copied().enumerate() {
        let pos = scan[i];
        let bx = pos.x as usize * rep;
        let by = pos.y as usize * rep;
        for j in 0..rep {
            for k in 0..rep {
                let x = bx + k;
                let y = by + j;
                matrix.coef[y * dim + x] = value;
            }
        }
    }
}

fn set_dc(matrix: &mut ScalingFactorMatrix, dc_coef: u16) {
    matrix.coef[0] = dc_coef;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScalingFactorMatrix {
    pub dim: u8,
    pub coef: Vec<u16>,
}

impl ScalingFactorMatrix {
    #[inline]
    pub fn at(&self, x: usize, y: usize) -> u16 {
        let dim = self.dim as usize;
        assert!(x < dim && y < dim, "ScalingFactor index out of range");
        self.coef[y * dim + x]
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScalingFactors {
    pub factors: [[ScalingFactorMatrix; NUM_MATRIX_IDS]; NUM_SIZE_IDS],
}
