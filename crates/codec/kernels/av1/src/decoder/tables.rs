use std::ffi::c_uint;

use strum::EnumCount;

use crate::decoder::align::{Align16, Align4, Align64, Align8};
use crate::decoder::const_fn::const_for;
use crate::decoder::enum_map::DefaultValue;
use crate::decoder::in_range::InRange;
use crate::decoder::include::dav1d::headers::{
    Rav1dFilterMode, Rav1dWarpedMotionParams, Rav1dWarpedMotionType,
};
use crate::decoder::levels::{
    BlockLevel, BlockPartition, BlockSize, Filter2d, InterPredMode, TxClass, TxfmSize, TxfmType,
    ADST_ADST, ADST_DCT, ADST_FLIPADST, DCT_ADST, DCT_DCT, DCT_FLIPADST, DC_PRED,
    DIAG_DOWN_LEFT_PRED, DIAG_DOWN_RIGHT_PRED, FLIPADST_ADST, FLIPADST_DCT, FLIPADST_FLIPADST,
    GLOBALMV, GLOBALMV_GLOBALMV, HOR_DOWN_PRED, HOR_PRED, HOR_UP_PRED, H_ADST, H_DCT, H_FLIPADST,
    IDTX, NEARESTMV, NEARESTMV_NEARESTMV, NEARESTMV_NEWMV, NEARMV, NEARMV_NEARMV, NEARMV_NEWMV,
    NEWMV, NEWMV_NEARESTMV, NEWMV_NEARMV, NEWMV_NEWMV, N_COMP_INTER_PRED_MODES, N_INTRA_PRED_MODES,
    N_TX_TYPES_PLUS_LL, N_UV_INTRA_PRED_MODES, PAETH_PRED, SMOOTH_H_PRED, SMOOTH_PRED,
    SMOOTH_V_PRED, VERT_LEFT_PRED, VERT_PRED, VERT_RIGHT_PRED, V_ADST, V_DCT, V_FLIPADST,
};

#[repr(C)]
pub struct TxfmInfo {
    pub w: u8,
    pub h: u8,
    pub lw: u8,
    pub lh: u8,
    pub min: u8,
    pub max: u8,
    pub sub: TxfmSize,
    pub ctx: u8,
}

pub static DAV1D_AL_PART_CTX: [[[u8; BlockPartition::COUNT]; BlockLevel::COUNT]; 2] = [
    [
        [0x00, 0x00, 0x10, 0xff, 0x00, 0x10, 0x10, 0x10, 0xff, 0xff],
        [0x10, 0x10, 0x18, 0xff, 0x10, 0x18, 0x18, 0x18, 0x10, 0x1c],
        [0x18, 0x18, 0x1c, 0xff, 0x18, 0x1c, 0x1c, 0x1c, 0x18, 0x1e],
        [0x1c, 0x1c, 0x1e, 0xff, 0x1c, 0x1e, 0x1e, 0x1e, 0x1c, 0x1f],
        [0x1e, 0x1e, 0x1f, 0x1f, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff],
    ],
    [
        [0x00, 0x10, 0x00, 0xff, 0x10, 0x10, 0x00, 0x10, 0xff, 0xff],
        [0x10, 0x18, 0x10, 0xff, 0x18, 0x18, 0x10, 0x18, 0x1c, 0x10],
        [0x18, 0x1c, 0x18, 0xff, 0x1c, 0x1c, 0x18, 0x1c, 0x1e, 0x18],
        [0x1c, 0x1e, 0x1c, 0xff, 0x1e, 0x1e, 0x1c, 0x1e, 0x1f, 0x1c],
        [0x1e, 0x1f, 0x1e, 0x1f, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff],
    ],
];

pub static DAV1D_BLOCK_SIZES: [[[BlockSize; 2]; BlockPartition::COUNT]; BlockLevel::COUNT] = {
    use BlockSize::*;

    const DEFAULT: BlockSize = BlockSize::Bs128x128;

    [
        [
            [Bs128x128, DEFAULT],
            [Bs128x64, DEFAULT],
            [Bs64x128, DEFAULT],
            [DEFAULT; 2],
            [Bs64x64, Bs128x64],
            [Bs128x64, Bs64x64],
            [Bs64x64, Bs64x128],
            [Bs64x128, Bs64x64],
            [DEFAULT; 2],
            [DEFAULT; 2],
        ],
        [
            [Bs64x64, DEFAULT],
            [Bs64x32, DEFAULT],
            [Bs32x64, DEFAULT],
            [DEFAULT; 2],
            [Bs32x32, Bs64x32],
            [Bs64x32, Bs32x32],
            [Bs32x32, Bs32x64],
            [Bs32x64, Bs32x32],
            [Bs64x16, DEFAULT],
            [Bs16x64, DEFAULT],
        ],
        [
            [Bs32x32, DEFAULT],
            [Bs32x16, DEFAULT],
            [Bs16x32, DEFAULT],
            [DEFAULT; 2],
            [Bs16x16, Bs32x16],
            [Bs32x16, Bs16x16],
            [Bs16x16, Bs16x32],
            [Bs16x32, Bs16x16],
            [Bs32x8, DEFAULT],
            [Bs8x32, DEFAULT],
        ],
        [
            [Bs16x16, DEFAULT],
            [Bs16x8, DEFAULT],
            [Bs8x16, DEFAULT],
            [DEFAULT; 2],
            [Bs8x8, Bs16x8],
            [Bs16x8, Bs8x8],
            [Bs8x8, Bs8x16],
            [Bs8x16, Bs8x8],
            [Bs16x4, DEFAULT],
            [Bs4x16, DEFAULT],
        ],
        [
            [Bs8x8, DEFAULT],
            [Bs8x4, DEFAULT],
            [Bs4x8, DEFAULT],
            [Bs4x4, DEFAULT],
            [DEFAULT; 2],
            [DEFAULT; 2],
            [DEFAULT; 2],
            [DEFAULT; 2],
            [DEFAULT; 2],
            [DEFAULT; 2],
        ],
    ]
};

static DAV1D_BLOCK_DIMENSIONS: [[u8; 4]; BlockSize::COUNT] = [
    [32, 32, 5, 5],
    [32, 16, 5, 4],
    [16, 32, 4, 5],
    [16, 16, 4, 4],
    [16, 8, 4, 3],
    [16, 4, 4, 2],
    [8, 16, 3, 4],
    [8, 8, 3, 3],
    [8, 4, 3, 2],
    [8, 2, 3, 1],
    [4, 16, 2, 4],
    [4, 8, 2, 3],
    [4, 4, 2, 2],
    [4, 2, 2, 1],
    [4, 1, 2, 0],
    [2, 8, 1, 3],
    [2, 4, 1, 2],
    [2, 2, 1, 1],
    [2, 1, 1, 0],
    [1, 4, 0, 2],
    [1, 2, 0, 1],
    [1, 1, 0, 0],
];

impl BlockSize {
    #[inline]
    pub fn dimensions(self) -> &'static [u8; 4] {
        &DAV1D_BLOCK_DIMENSIONS[self as usize]
    }
}

pub static DAV1D_TXFM_DIMENSIONS: [TxfmInfo; TxfmSize::COUNT] = {
    use TxfmSize::*;
    [
        TxfmInfo {
            w: 1,
            h: 1,
            lw: 0,
            lh: 0,
            min: 0,
            max: 0,
            sub: DefaultValue::DEFAULT,
            ctx: 0,
        },
        TxfmInfo {
            w: 2,
            h: 2,
            lw: 1,
            lh: 1,
            min: 1,
            max: 1,
            sub: S4x4,
            ctx: 1,
        },
        TxfmInfo {
            w: 4,
            h: 4,
            lw: 2,
            lh: 2,
            min: 2,
            max: 2,
            sub: S8x8,
            ctx: 2,
        },
        TxfmInfo {
            w: 8,
            h: 8,
            lw: 3,
            lh: 3,
            min: 3,
            max: 3,
            sub: S16x16,
            ctx: 3,
        },
        TxfmInfo {
            w: 16,
            h: 16,
            lw: 4,
            lh: 4,
            min: 4,
            max: 4,
            sub: S32x32,
            ctx: 4,
        },
        TxfmInfo {
            w: 1,
            h: 2,
            lw: 0,
            lh: 1,
            min: 0,
            max: 1,
            sub: S4x4,
            ctx: 1,
        },
        TxfmInfo {
            w: 2,
            h: 1,
            lw: 1,
            lh: 0,
            min: 0,
            max: 1,
            sub: S4x4,
            ctx: 1,
        },
        TxfmInfo {
            w: 2,
            h: 4,
            lw: 1,
            lh: 2,
            min: 1,
            max: 2,
            sub: S8x8,
            ctx: 2,
        },
        TxfmInfo {
            w: 4,
            h: 2,
            lw: 2,
            lh: 1,
            min: 1,
            max: 2,
            sub: S8x8,
            ctx: 2,
        },
        TxfmInfo {
            w: 4,
            h: 8,
            lw: 2,
            lh: 3,
            min: 2,
            max: 3,
            sub: S16x16,
            ctx: 3,
        },
        TxfmInfo {
            w: 8,
            h: 4,
            lw: 3,
            lh: 2,
            min: 2,
            max: 3,
            sub: S16x16,
            ctx: 3,
        },
        TxfmInfo {
            w: 8,
            h: 16,
            lw: 3,
            lh: 4,
            min: 3,
            max: 4,
            sub: S32x32,
            ctx: 4,
        },
        TxfmInfo {
            w: 16,
            h: 8,
            lw: 4,
            lh: 3,
            min: 3,
            max: 4,
            sub: S32x32,
            ctx: 4,
        },
        TxfmInfo {
            w: 1,
            h: 4,
            lw: 0,
            lh: 2,
            min: 0,
            max: 2,
            sub: R4x8,
            ctx: 1,
        },
        TxfmInfo {
            w: 4,
            h: 1,
            lw: 2,
            lh: 0,
            min: 0,
            max: 2,
            sub: R8x4,
            ctx: 1,
        },
        TxfmInfo {
            w: 2,
            h: 8,
            lw: 1,
            lh: 3,
            min: 1,
            max: 3,
            sub: R8x16,
            ctx: 2,
        },
        TxfmInfo {
            w: 8,
            h: 2,
            lw: 3,
            lh: 1,
            min: 1,
            max: 3,
            sub: R16x8,
            ctx: 2,
        },
        TxfmInfo {
            w: 4,
            h: 16,
            lw: 2,
            lh: 4,
            min: 2,
            max: 4,
            sub: R16x32,
            ctx: 3,
        },
        TxfmInfo {
            w: 16,
            h: 4,
            lw: 4,
            lh: 2,
            min: 2,
            max: 4,
            sub: R32x16,
            ctx: 3,
        },
    ]
};

pub static DAV1D_MAX_TXFM_SIZE_FOR_BS: [[TxfmSize; 4]; BlockSize::COUNT] = {
    use TxfmSize::*;
    const DEFAULT: TxfmSize = DefaultValue::DEFAULT;
    [
        [S64x64, S32x32, S32x32, S32x32],
        [S64x64, S32x32, S32x32, S32x32],
        [S64x64, S32x32, DEFAULT, S32x32],
        [S64x64, S32x32, S32x32, S32x32],
        [R64x32, R32x16, S32x32, S32x32],
        [R64x16, R32x8, R32x16, R32x16],
        [R32x64, R16x32, DEFAULT, S32x32],
        [S32x32, S16x16, R16x32, S32x32],
        [R32x16, R16x8, S16x16, R32x16],
        [R32x8, R16x4, R16x8, R32x8],
        [R16x64, R8x32, DEFAULT, R16x32],
        [R16x32, R8x16, DEFAULT, R16x32],
        [S16x16, S8x8, R8x16, S16x16],
        [R16x8, R8x4, S8x8, R16x8],
        [R16x4, R8x4, R8x4, R16x4],
        [R8x32, R4x16, DEFAULT, R8x32],
        [R8x16, R4x8, DEFAULT, R8x16],
        [S8x8, S4x4, R4x8, S8x8],
        [R8x4, S4x4, S4x4, R8x4],
        [R4x16, R4x8, DEFAULT, R4x16],
        [R4x8, S4x4, DEFAULT, R4x8],
        [S4x4, S4x4, S4x4, S4x4],
    ]
};

pub static DAV1D_TXTP_FROM_UVMODE: [TxfmType; N_UV_INTRA_PRED_MODES] = {
    let mut tbl = [0; N_UV_INTRA_PRED_MODES];
    tbl[DC_PRED as usize] = DCT_DCT;
    tbl[VERT_PRED as usize] = ADST_DCT;
    tbl[HOR_PRED as usize] = DCT_ADST;
    tbl[DIAG_DOWN_LEFT_PRED as usize] = DCT_DCT;
    tbl[DIAG_DOWN_RIGHT_PRED as usize] = ADST_ADST;
    tbl[VERT_RIGHT_PRED as usize] = ADST_DCT;
    tbl[HOR_DOWN_PRED as usize] = DCT_ADST;
    tbl[HOR_UP_PRED as usize] = DCT_ADST;
    tbl[VERT_LEFT_PRED as usize] = ADST_DCT;
    tbl[SMOOTH_PRED as usize] = ADST_ADST;
    tbl[SMOOTH_V_PRED as usize] = ADST_DCT;
    tbl[SMOOTH_H_PRED as usize] = DCT_ADST;
    tbl[PAETH_PRED as usize] = ADST_ADST;
    tbl
};

pub static DAV1D_COMP_INTER_PRED_MODES: [[InterPredMode; 2]; N_COMP_INTER_PRED_MODES] = {
    let mut tbl = [[0; 2]; 8];
    tbl[NEARESTMV_NEARESTMV as usize] = [NEARESTMV, NEARESTMV];
    tbl[NEARMV_NEARMV as usize] = [NEARMV, NEARMV];
    tbl[NEWMV_NEWMV as usize] = [NEWMV, NEWMV];
    tbl[GLOBALMV_GLOBALMV as usize] = [GLOBALMV, GLOBALMV];
    tbl[NEWMV_NEARESTMV as usize] = [NEWMV, NEARESTMV];
    tbl[NEWMV_NEARMV as usize] = [NEWMV, NEARMV];
    tbl[NEARESTMV_NEWMV as usize] = [NEARESTMV, NEWMV];
    tbl[NEARMV_NEWMV as usize] = [NEARMV, NEWMV];
    tbl
};

pub static DAV1D_PARTITION_TYPE_COUNT: [u8; BlockLevel::COUNT] = [
    BlockPartition::COUNT as u8 - 3,
    BlockPartition::COUNT as u8 - 1,
    BlockPartition::COUNT as u8 - 1,
    BlockPartition::COUNT as u8 - 1,
    BlockPartition::N_SUB8X8_PARTITIONS as u8 - 1,
];

pub static DAV1D_TX_TYPES_PER_SET: [u8; 40] = [
    IDTX as u8,
    DCT_DCT as u8,
    ADST_ADST as u8,
    ADST_DCT as u8,
    DCT_ADST as u8,
    IDTX as u8,
    DCT_DCT as u8,
    V_DCT as u8,
    H_DCT as u8,
    ADST_ADST as u8,
    ADST_DCT as u8,
    DCT_ADST as u8,
    IDTX as u8,
    V_DCT as u8,
    H_DCT as u8,
    DCT_DCT as u8,
    ADST_DCT as u8,
    DCT_ADST as u8,
    FLIPADST_DCT as u8,
    DCT_FLIPADST as u8,
    ADST_ADST as u8,
    FLIPADST_FLIPADST as u8,
    ADST_FLIPADST as u8,
    FLIPADST_ADST as u8,
    IDTX as u8,
    V_DCT as u8,
    H_DCT as u8,
    V_ADST as u8,
    H_ADST as u8,
    V_FLIPADST as u8,
    H_FLIPADST as u8,
    DCT_DCT as u8,
    ADST_DCT as u8,
    DCT_ADST as u8,
    FLIPADST_DCT as u8,
    DCT_FLIPADST as u8,
    ADST_ADST as u8,
    FLIPADST_FLIPADST as u8,
    ADST_FLIPADST as u8,
    FLIPADST_ADST as u8,
];

pub static DAV1D_YMODE_SIZE_CONTEXT: [u8; BlockSize::COUNT] = [
    3, 3, 3, 3, 3, 2, 3, 3, 2, 1, 2, 2, 2, 1, 0, 1, 1, 1, 0, 0, 0, 0,
];

/// Extend `a` from length `M` to length `N`, repeating the last element.
/// That is, `a[cmp::min(i, M - 1)] = extend_array(a)[cmp::min(i, N - 1)]`.
const fn extend_array<T: DefaultValue + Copy, const M: usize, const N: usize>(a: [T; M]) -> [T; N] {
    let mut b = [DefaultValue::DEFAULT; N];
    const_for!(i in 0..M => {
        b[i] = a[i];
    });
    const_for!(i in M..N => {
        b[i] = a[M - 1];
    });
    b
}

pub type LoCtxOffset = InRange<u8, 0, 21>;

/// The core of each of these 3 tables is a 5x5 table of offsets.
/// However, to use that 5x5 table in [`get_lo_ctx`],
/// we first have to clamp the `x` and `y` values from `[0, 32)` to `[0, 5)`.
/// That involves two `min`s, which LLVM makes into conditional moves that have to
/// happen in series before we can actually load the element we want.  These add latency.
/// Simply unroll the array: duplicate everything from the 5th row and 5th column out
/// so that we get the same effect as clamping without the latency.
/// Obviously, we pay for it in cache efficiency and binary size,
/// but the tradeoff seems worth it on older x86_64 systems, and not harmful elsewhere.
pub static DAV1D_LO_CTX_OFFSETS: [[[LoCtxOffset; 32]; 32]; 3] = {
    const fn extend_offsets(a: [[u8; 5]; 5]) -> [[LoCtxOffset; 32]; 32] {
        extend_array([
            extend_array(LoCtxOffset::new_array(a[0])),
            extend_array(LoCtxOffset::new_array(a[1])),
            extend_array(LoCtxOffset::new_array(a[2])),
            extend_array(LoCtxOffset::new_array(a[3])),
            extend_array(LoCtxOffset::new_array(a[4])),
        ])
    }

    [
        extend_offsets([
            [0, 1, 6, 6, 21],
            [1, 6, 6, 21, 21],
            [6, 6, 21, 21, 21],
            [6, 21, 21, 21, 21],
            [21, 21, 21, 21, 21],
        ]),
        extend_offsets([
            [0, 16, 6, 6, 21],
            [16, 16, 6, 21, 21],
            [16, 16, 21, 21, 21],
            [16, 16, 21, 21, 21],
            [16, 16, 21, 21, 21],
        ]),
        extend_offsets([
            [0, 11, 11, 11, 11],
            [11, 11, 11, 11, 11],
            [6, 6, 21, 21, 21],
            [6, 21, 21, 21, 21],
            [21, 21, 21, 21, 21],
        ]),
    ]
};

pub static DAV1D_SKIP_CTX: [[u8; 5]; 5] = [
    [1, 2, 2, 2, 3],
    [2, 4, 4, 4, 5],
    [2, 4, 4, 4, 5],
    [2, 4, 4, 4, 5],
    [3, 5, 5, 5, 6],
];

pub static DAV1D_TX_TYPE_CLASS: [TxClass; N_TX_TYPES_PLUS_LL] = [
    TxClass::TwoD,
    TxClass::TwoD,
    TxClass::TwoD,
    TxClass::TwoD,
    TxClass::TwoD,
    TxClass::TwoD,
    TxClass::TwoD,
    TxClass::TwoD,
    TxClass::TwoD,
    TxClass::TwoD,
    TxClass::V,
    TxClass::H,
    TxClass::V,
    TxClass::H,
    TxClass::V,
    TxClass::H,
    TxClass::TwoD,
];

pub const DAV1D_FILTER_2D: [[Filter2d; Rav1dFilterMode::N_FILTERS]; Rav1dFilterMode::N_FILTERS] = {
    use Filter2d::*;

    const DEFAULT: Filter2d = Filter2d::Regular8Tap;

    [
        [Regular8Tap, RegularSmooth8Tap, RegularSharp8Tap, DEFAULT],
        [SmoothRegular8Tap, Smooth8Tap, SmoothSharp8Tap, DEFAULT],
        [SharpRegular8Tap, SharpSmooth8Tap, Sharp8Tap, DEFAULT],
        [DEFAULT, DEFAULT, DEFAULT, Bilinear],
    ]
};

pub const DAV1D_FILTER_DIR: [[Rav1dFilterMode; 2]; Filter2d::COUNT] = [
    [Rav1dFilterMode::Regular8Tap, Rav1dFilterMode::Regular8Tap],
    [Rav1dFilterMode::Smooth8Tap, Rav1dFilterMode::Regular8Tap],
    [Rav1dFilterMode::Sharp8Tap, Rav1dFilterMode::Regular8Tap],
    [Rav1dFilterMode::Regular8Tap, Rav1dFilterMode::Sharp8Tap],
    [Rav1dFilterMode::Smooth8Tap, Rav1dFilterMode::Sharp8Tap],
    [Rav1dFilterMode::Sharp8Tap, Rav1dFilterMode::Sharp8Tap],
    [Rav1dFilterMode::Regular8Tap, Rav1dFilterMode::Smooth8Tap],
    [Rav1dFilterMode::Smooth8Tap, Rav1dFilterMode::Smooth8Tap],
    [Rav1dFilterMode::Sharp8Tap, Rav1dFilterMode::Smooth8Tap],
    [Rav1dFilterMode::Bilinear, Rav1dFilterMode::Bilinear],
];

pub static DAV1D_FILTER_MODE_TO_Y_MODE: [u8; 5] = [
    DC_PRED as u8,
    VERT_PRED as u8,
    HOR_PRED as u8,
    HOR_DOWN_PRED as u8,
    DC_PRED as u8,
];

pub static DAV1D_INTRA_MODE_CONTEXT: [u8; N_INTRA_PRED_MODES] =
    [0, 1, 2, 3, 4, 4, 4, 4, 3, 0, 1, 2, 0];

pub static DAV1D_WEDGE_CTX_LUT: [u8; BlockSize::COUNT] = [
    0, 0, 0, 0, 0, 0, 0, 6, 5, 8, 0, 4, 3, 2, 0, 7, 1, 0, 0, 0, 0, 0,
];

pub const CFL_ALLOWED_MASK: c_uint = {
    use BlockSize::*;

    1 << Bs32x32 as u8
        | 1 << Bs32x16 as u8
        | 1 << Bs32x8 as u8
        | 1 << Bs16x32 as u8
        | 1 << Bs16x16 as u8
        | 1 << Bs16x8 as u8
        | 1 << Bs16x4 as u8
        | 1 << Bs8x32 as u8
        | 1 << Bs8x16 as u8
        | 1 << Bs8x8 as u8
        | 1 << Bs8x4 as u8
        | 1 << Bs4x16 as u8
        | 1 << Bs4x8 as u8
        | 1 << Bs4x4 as u8
};

pub const WEDGE_ALLOWED_MASK: c_uint = {
    use BlockSize::*;

    1 << Bs32x32 as u8
        | 1 << Bs32x16 as u8
        | 1 << Bs32x8 as u8
        | 1 << Bs16x32 as u8
        | 1 << Bs16x16 as u8
        | 1 << Bs16x8 as u8
        | 1 << Bs8x32 as u8
        | 1 << Bs8x16 as u8
        | 1 << Bs8x8 as u8
};

pub const INTERINTRA_ALLOWED_MASK: c_uint = {
    use BlockSize::*;

    1 << Bs32x32 as u8
        | 1 << Bs32x16 as u8
        | 1 << Bs16x32 as u8
        | 1 << Bs16x16 as u8
        | 1 << Bs16x8 as u8
        | 1 << Bs8x16 as u8
        | 1 << Bs8x8 as u8
};

impl Default for Rav1dWarpedMotionParams {
    fn default() -> Self {
        Self {
            r#type: Rav1dWarpedMotionType::Identity,
            matrix: [0, 0, 1 << 16, 0, 0, 1 << 16],
            abcd: Default::default(),
        }
    }
}

pub static DAV1D_CDEF_DIRECTIONS: [[i8; 2]; 12] = [
    [1 * 12 + 0, 2 * 12 + 0],
    [1 * 12 + 0, 2 * 12 - 1],
    [-1 * 12 + 1, -2 * 12 + 2],
    [0 * 12 + 1, -1 * 12 + 2],
    [0 * 12 + 1, 0 * 12 + 2],
    [0 * 12 + 1, 1 * 12 + 2],
    [1 * 12 + 1, 2 * 12 + 2],
    [1 * 12 + 0, 2 * 12 + 1],
    [1 * 12 + 0, 2 * 12 + 0],
    [1 * 12 + 0, 2 * 12 - 1],
    [-1 * 12 + 1, -2 * 12 + 2],
    [0 * 12 + 1, -1 * 12 + 2],
];

pub static DAV1D_SGR_PARAMS: Align4<[[u16; 2]; 16]> = Align4([
    [140, 3236],
    [112, 2158],
    [93, 1618],
    [80, 1438],
    [70, 1295],
    [58, 1177],
    [47, 1079],
    [37, 996],
    [30, 925],
    [25, 863],
    [0, 2589],
    [0, 1618],
    [0, 1177],
    [0, 925],
    [56, 0],
    [22, 0],
]);

#[no_mangle]
pub static dav1d_sgr_x_by_x: Align64<[u8; 256]> = Align64([
    255, 128, 85, 64, 51, 43, 37, 32, 28, 26, 23, 21, 20, 18, 17, 16, 15, 14, 13, 13, 12, 12, 11,
    11, 10, 10, 9, 9, 9, 9, 8, 8, 8, 8, 7, 7, 7, 7, 7, 6, 6, 6, 6, 6, 6, 6, 5, 5, 5, 5, 5, 5, 5, 5,
    5, 5, 4, 4, 4, 4, 4, 4, 4, 4, 4, 4, 4, 4, 4, 4, 4, 4, 4, 3, 3, 3, 3, 3, 3, 3, 3, 3, 3, 3, 3, 3,
    3, 3, 3, 3, 3, 3, 3, 3, 3, 3, 3, 3, 3, 3, 3, 3, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2,
    2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2,
    2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1,
    1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1,
    1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1,
    1, 1, 1, 1, 1, 1, 1, 1, 1, 0,
]);

#[no_mangle]
pub static dav1d_mc_subpel_filters: Align8<[[[i8; 8]; 15]; 6]> = Align8([
    [
        [0, 1, -3, 63, 4, -1, 0, 0],
        [0, 1, -5, 61, 9, -2, 0, 0],
        [0, 1, -6, 58, 14, -4, 1, 0],
        [0, 1, -7, 55, 19, -5, 1, 0],
        [0, 1, -7, 51, 24, -6, 1, 0],
        [0, 1, -8, 47, 29, -6, 1, 0],
        [0, 1, -7, 42, 33, -6, 1, 0],
        [0, 1, -7, 38, 38, -7, 1, 0],
        [0, 1, -6, 33, 42, -7, 1, 0],
        [0, 1, -6, 29, 47, -8, 1, 0],
        [0, 1, -6, 24, 51, -7, 1, 0],
        [0, 1, -5, 19, 55, -7, 1, 0],
        [0, 1, -4, 14, 58, -6, 1, 0],
        [0, 0, -2, 9, 61, -5, 1, 0],
        [0, 0, -1, 4, 63, -3, 1, 0],
    ],
    [
        [0, 1, 14, 31, 17, 1, 0, 0],
        [0, 0, 13, 31, 18, 2, 0, 0],
        [0, 0, 11, 31, 20, 2, 0, 0],
        [0, 0, 10, 30, 21, 3, 0, 0],
        [0, 0, 9, 29, 22, 4, 0, 0],
        [0, 0, 8, 28, 23, 5, 0, 0],
        [0, -1, 8, 27, 24, 6, 0, 0],
        [0, -1, 7, 26, 26, 7, -1, 0],
        [0, 0, 6, 24, 27, 8, -1, 0],
        [0, 0, 5, 23, 28, 8, 0, 0],
        [0, 0, 4, 22, 29, 9, 0, 0],
        [0, 0, 3, 21, 30, 10, 0, 0],
        [0, 0, 2, 20, 31, 11, 0, 0],
        [0, 0, 2, 18, 31, 13, 0, 0],
        [0, 0, 1, 17, 31, 14, 1, 0],
    ],
    [
        [-1, 1, -3, 63, 4, -1, 1, 0],
        [-1, 3, -6, 62, 8, -3, 2, -1],
        [-1, 4, -9, 60, 13, -5, 3, -1],
        [-2, 5, -11, 58, 19, -7, 3, -1],
        [-2, 5, -11, 54, 24, -9, 4, -1],
        [-2, 5, -12, 50, 30, -10, 4, -1],
        [-2, 5, -12, 45, 35, -11, 5, -1],
        [-2, 6, -12, 40, 40, -12, 6, -2],
        [-1, 5, -11, 35, 45, -12, 5, -2],
        [-1, 4, -10, 30, 50, -12, 5, -2],
        [-1, 4, -9, 24, 54, -11, 5, -2],
        [-1, 3, -7, 19, 58, -11, 5, -2],
        [-1, 3, -5, 13, 60, -9, 4, -1],
        [-1, 2, -3, 8, 62, -6, 3, -1],
        [0, 1, -1, 4, 63, -3, 1, -1],
    ],
    [
        [0, 0, -2, 63, 4, -1, 0, 0],
        [0, 0, -4, 61, 9, -2, 0, 0],
        [0, 0, -5, 58, 14, -3, 0, 0],
        [0, 0, -6, 55, 19, -4, 0, 0],
        [0, 0, -6, 51, 24, -5, 0, 0],
        [0, 0, -7, 47, 29, -5, 0, 0],
        [0, 0, -6, 42, 33, -5, 0, 0],
        [0, 0, -6, 38, 38, -6, 0, 0],
        [0, 0, -5, 33, 42, -6, 0, 0],
        [0, 0, -5, 29, 47, -7, 0, 0],
        [0, 0, -5, 24, 51, -6, 0, 0],
        [0, 0, -4, 19, 55, -6, 0, 0],
        [0, 0, -3, 14, 58, -5, 0, 0],
        [0, 0, -2, 9, 61, -4, 0, 0],
        [0, 0, -1, 4, 63, -2, 0, 0],
    ],
    [
        [0, 0, 15, 31, 17, 1, 0, 0],
        [0, 0, 13, 31, 18, 2, 0, 0],
        [0, 0, 11, 31, 20, 2, 0, 0],
        [0, 0, 10, 30, 21, 3, 0, 0],
        [0, 0, 9, 29, 22, 4, 0, 0],
        [0, 0, 8, 28, 23, 5, 0, 0],
        [0, 0, 7, 27, 24, 6, 0, 0],
        [0, 0, 6, 26, 26, 6, 0, 0],
        [0, 0, 6, 24, 27, 7, 0, 0],
        [0, 0, 5, 23, 28, 8, 0, 0],
        [0, 0, 4, 22, 29, 9, 0, 0],
        [0, 0, 3, 21, 30, 10, 0, 0],
        [0, 0, 2, 20, 31, 11, 0, 0],
        [0, 0, 2, 18, 31, 13, 0, 0],
        [0, 0, 1, 17, 31, 15, 0, 0],
    ],
    [
        [0, 0, 0, 60, 4, 0, 0, 0],
        [0, 0, 0, 56, 8, 0, 0, 0],
        [0, 0, 0, 52, 12, 0, 0, 0],
        [0, 0, 0, 48, 16, 0, 0, 0],
        [0, 0, 0, 44, 20, 0, 0, 0],
        [0, 0, 0, 40, 24, 0, 0, 0],
        [0, 0, 0, 36, 28, 0, 0, 0],
        [0, 0, 0, 32, 32, 0, 0, 0],
        [0, 0, 0, 28, 36, 0, 0, 0],
        [0, 0, 0, 24, 40, 0, 0, 0],
        [0, 0, 0, 20, 44, 0, 0, 0],
        [0, 0, 0, 16, 48, 0, 0, 0],
        [0, 0, 0, 12, 52, 0, 0, 0],
        [0, 0, 0, 8, 56, 0, 0, 0],
        [0, 0, 0, 4, 60, 0, 0, 0],
    ],
]);

#[no_mangle]
pub static dav1d_mc_warp_filter: Align8<[[i8; 8]; 193]> = Align8([
    [0, 0, 127, 1, 0, 0, 0, 0],
    [0, -1, 127, 2, 0, 0, 0, 0],
    [1, -3, 127, 4, -1, 0, 0, 0],
    [1, -4, 126, 6, -2, 1, 0, 0],
    [1, -5, 126, 8, -3, 1, 0, 0],
    [1, -6, 125, 11, -4, 1, 0, 0],
    [1, -7, 124, 13, -4, 1, 0, 0],
    [2, -8, 123, 15, -5, 1, 0, 0],
    [2, -9, 122, 18, -6, 1, 0, 0],
    [2, -10, 121, 20, -6, 1, 0, 0],
    [2, -11, 120, 22, -7, 2, 0, 0],
    [2, -12, 119, 25, -8, 2, 0, 0],
    [3, -13, 117, 27, -8, 2, 0, 0],
    [3, -13, 116, 29, -9, 2, 0, 0],
    [3, -14, 114, 32, -10, 3, 0, 0],
    [3, -15, 113, 35, -10, 2, 0, 0],
    [3, -15, 111, 37, -11, 3, 0, 0],
    [3, -16, 109, 40, -11, 3, 0, 0],
    [3, -16, 108, 42, -12, 3, 0, 0],
    [4, -17, 106, 45, -13, 3, 0, 0],
    [4, -17, 104, 47, -13, 3, 0, 0],
    [4, -17, 102, 50, -14, 3, 0, 0],
    [4, -17, 100, 52, -14, 3, 0, 0],
    [4, -18, 98, 55, -15, 4, 0, 0],
    [4, -18, 96, 58, -15, 3, 0, 0],
    [4, -18, 94, 60, -16, 4, 0, 0],
    [4, -18, 91, 63, -16, 4, 0, 0],
    [4, -18, 89, 65, -16, 4, 0, 0],
    [4, -18, 87, 68, -17, 4, 0, 0],
    [4, -18, 85, 70, -17, 4, 0, 0],
    [4, -18, 82, 73, -17, 4, 0, 0],
    [4, -18, 80, 75, -17, 4, 0, 0],
    [4, -18, 78, 78, -18, 4, 0, 0],
    [4, -17, 75, 80, -18, 4, 0, 0],
    [4, -17, 73, 82, -18, 4, 0, 0],
    [4, -17, 70, 85, -18, 4, 0, 0],
    [4, -17, 68, 87, -18, 4, 0, 0],
    [4, -16, 65, 89, -18, 4, 0, 0],
    [4, -16, 63, 91, -18, 4, 0, 0],
    [4, -16, 60, 94, -18, 4, 0, 0],
    [3, -15, 58, 96, -18, 4, 0, 0],
    [4, -15, 55, 98, -18, 4, 0, 0],
    [3, -14, 52, 100, -17, 4, 0, 0],
    [3, -14, 50, 102, -17, 4, 0, 0],
    [3, -13, 47, 104, -17, 4, 0, 0],
    [3, -13, 45, 106, -17, 4, 0, 0],
    [3, -12, 42, 108, -16, 3, 0, 0],
    [3, -11, 40, 109, -16, 3, 0, 0],
    [3, -11, 37, 111, -15, 3, 0, 0],
    [2, -10, 35, 113, -15, 3, 0, 0],
    [3, -10, 32, 114, -14, 3, 0, 0],
    [2, -9, 29, 116, -13, 3, 0, 0],
    [2, -8, 27, 117, -13, 3, 0, 0],
    [2, -8, 25, 119, -12, 2, 0, 0],
    [2, -7, 22, 120, -11, 2, 0, 0],
    [1, -6, 20, 121, -10, 2, 0, 0],
    [1, -6, 18, 122, -9, 2, 0, 0],
    [1, -5, 15, 123, -8, 2, 0, 0],
    [1, -4, 13, 124, -7, 1, 0, 0],
    [1, -4, 11, 125, -6, 1, 0, 0],
    [1, -3, 8, 126, -5, 1, 0, 0],
    [1, -2, 6, 126, -4, 1, 0, 0],
    [0, -1, 4, 127, -3, 1, 0, 0],
    [0, 0, 2, 127, -1, 0, 0, 0],
    [0, 0, 0, 127, 1, 0, 0, 0],
    [0, 0, -1, 127, 2, 0, 0, 0],
    [0, 1, -3, 127, 4, -2, 1, 0],
    [0, 1, -5, 127, 6, -2, 1, 0],
    [0, 2, -6, 126, 8, -3, 1, 0],
    [-1, 2, -7, 126, 11, -4, 2, -1],
    [-1, 3, -8, 125, 13, -5, 2, -1],
    [-1, 3, -10, 124, 16, -6, 3, -1],
    [-1, 4, -11, 123, 18, -7, 3, -1],
    [-1, 4, -12, 122, 20, -7, 3, -1],
    [-1, 4, -13, 121, 23, -8, 3, -1],
    [-2, 5, -14, 120, 25, -9, 4, -1],
    [-1, 5, -15, 119, 27, -10, 4, -1],
    [-1, 5, -16, 118, 30, -11, 4, -1],
    [-2, 6, -17, 116, 33, -12, 5, -1],
    [-2, 6, -17, 114, 35, -12, 5, -1],
    [-2, 6, -18, 113, 38, -13, 5, -1],
    [-2, 7, -19, 111, 41, -14, 6, -2],
    [-2, 7, -19, 110, 43, -15, 6, -2],
    [-2, 7, -20, 108, 46, -15, 6, -2],
    [-2, 7, -20, 106, 49, -16, 6, -2],
    [-2, 7, -21, 104, 51, -16, 7, -2],
    [-2, 7, -21, 102, 54, -17, 7, -2],
    [-2, 8, -21, 100, 56, -18, 7, -2],
    [-2, 8, -22, 98, 59, -18, 7, -2],
    [-2, 8, -22, 96, 62, -19, 7, -2],
    [-2, 8, -22, 94, 64, -19, 7, -2],
    [-2, 8, -22, 91, 67, -20, 8, -2],
    [-2, 8, -22, 89, 69, -20, 8, -2],
    [-2, 8, -22, 87, 72, -21, 8, -2],
    [-2, 8, -21, 84, 74, -21, 8, -2],
    [-2, 8, -22, 82, 77, -21, 8, -2],
    [-2, 8, -21, 79, 79, -21, 8, -2],
    [-2, 8, -21, 77, 82, -22, 8, -2],
    [-2, 8, -21, 74, 84, -21, 8, -2],
    [-2, 8, -21, 72, 87, -22, 8, -2],
    [-2, 8, -20, 69, 89, -22, 8, -2],
    [-2, 8, -20, 67, 91, -22, 8, -2],
    [-2, 7, -19, 64, 94, -22, 8, -2],
    [-2, 7, -19, 62, 96, -22, 8, -2],
    [-2, 7, -18, 59, 98, -22, 8, -2],
    [-2, 7, -18, 56, 100, -21, 8, -2],
    [-2, 7, -17, 54, 102, -21, 7, -2],
    [-2, 7, -16, 51, 104, -21, 7, -2],
    [-2, 6, -16, 49, 106, -20, 7, -2],
    [-2, 6, -15, 46, 108, -20, 7, -2],
    [-2, 6, -15, 43, 110, -19, 7, -2],
    [-2, 6, -14, 41, 111, -19, 7, -2],
    [-1, 5, -13, 38, 113, -18, 6, -2],
    [-1, 5, -12, 35, 114, -17, 6, -2],
    [-1, 5, -12, 33, 116, -17, 6, -2],
    [-1, 4, -11, 30, 118, -16, 5, -1],
    [-1, 4, -10, 27, 119, -15, 5, -1],
    [-1, 4, -9, 25, 120, -14, 5, -2],
    [-1, 3, -8, 23, 121, -13, 4, -1],
    [-1, 3, -7, 20, 122, -12, 4, -1],
    [-1, 3, -7, 18, 123, -11, 4, -1],
    [-1, 3, -6, 16, 124, -10, 3, -1],
    [-1, 2, -5, 13, 125, -8, 3, -1],
    [-1, 2, -4, 11, 126, -7, 2, -1],
    [0, 1, -3, 8, 126, -6, 2, 0],
    [0, 1, -2, 6, 127, -5, 1, 0],
    [0, 1, -2, 4, 127, -3, 1, 0],
    [0, 0, 0, 2, 127, -1, 0, 0],
    [0, 0, 0, 1, 127, 0, 0, 0],
    [0, 0, 0, -1, 127, 2, 0, 0],
    [0, 0, 1, -3, 127, 4, -1, 0],
    [0, 0, 1, -4, 126, 6, -2, 1],
    [0, 0, 1, -5, 126, 8, -3, 1],
    [0, 0, 1, -6, 125, 11, -4, 1],
    [0, 0, 1, -7, 124, 13, -4, 1],
    [0, 0, 2, -8, 123, 15, -5, 1],
    [0, 0, 2, -9, 122, 18, -6, 1],
    [0, 0, 2, -10, 121, 20, -6, 1],
    [0, 0, 2, -11, 120, 22, -7, 2],
    [0, 0, 2, -12, 119, 25, -8, 2],
    [0, 0, 3, -13, 117, 27, -8, 2],
    [0, 0, 3, -13, 116, 29, -9, 2],
    [0, 0, 3, -14, 114, 32, -10, 3],
    [0, 0, 3, -15, 113, 35, -10, 2],
    [0, 0, 3, -15, 111, 37, -11, 3],
    [0, 0, 3, -16, 109, 40, -11, 3],
    [0, 0, 3, -16, 108, 42, -12, 3],
    [0, 0, 4, -17, 106, 45, -13, 3],
    [0, 0, 4, -17, 104, 47, -13, 3],
    [0, 0, 4, -17, 102, 50, -14, 3],
    [0, 0, 4, -17, 100, 52, -14, 3],
    [0, 0, 4, -18, 98, 55, -15, 4],
    [0, 0, 4, -18, 96, 58, -15, 3],
    [0, 0, 4, -18, 94, 60, -16, 4],
    [0, 0, 4, -18, 91, 63, -16, 4],
    [0, 0, 4, -18, 89, 65, -16, 4],
    [0, 0, 4, -18, 87, 68, -17, 4],
    [0, 0, 4, -18, 85, 70, -17, 4],
    [0, 0, 4, -18, 82, 73, -17, 4],
    [0, 0, 4, -18, 80, 75, -17, 4],
    [0, 0, 4, -18, 78, 78, -18, 4],
    [0, 0, 4, -17, 75, 80, -18, 4],
    [0, 0, 4, -17, 73, 82, -18, 4],
    [0, 0, 4, -17, 70, 85, -18, 4],
    [0, 0, 4, -17, 68, 87, -18, 4],
    [0, 0, 4, -16, 65, 89, -18, 4],
    [0, 0, 4, -16, 63, 91, -18, 4],
    [0, 0, 4, -16, 60, 94, -18, 4],
    [0, 0, 3, -15, 58, 96, -18, 4],
    [0, 0, 4, -15, 55, 98, -18, 4],
    [0, 0, 3, -14, 52, 100, -17, 4],
    [0, 0, 3, -14, 50, 102, -17, 4],
    [0, 0, 3, -13, 47, 104, -17, 4],
    [0, 0, 3, -13, 45, 106, -17, 4],
    [0, 0, 3, -12, 42, 108, -16, 3],
    [0, 0, 3, -11, 40, 109, -16, 3],
    [0, 0, 3, -11, 37, 111, -15, 3],
    [0, 0, 2, -10, 35, 113, -15, 3],
    [0, 0, 3, -10, 32, 114, -14, 3],
    [0, 0, 2, -9, 29, 116, -13, 3],
    [0, 0, 2, -8, 27, 117, -13, 3],
    [0, 0, 2, -8, 25, 119, -12, 2],
    [0, 0, 2, -7, 22, 120, -11, 2],
    [0, 0, 1, -6, 20, 121, -10, 2],
    [0, 0, 1, -6, 18, 122, -9, 2],
    [0, 0, 1, -5, 15, 123, -8, 2],
    [0, 0, 1, -4, 13, 124, -7, 1],
    [0, 0, 1, -4, 11, 125, -6, 1],
    [0, 0, 1, -3, 8, 126, -5, 1],
    [0, 0, 1, -2, 6, 126, -4, 1],
    [0, 0, 0, -1, 4, 127, -3, 1],
    [0, 0, 0, 0, 2, 127, -1, 0],
    [0, 0, 0, 0, 2, 127, -1, 0],
]);

#[no_mangle]
pub static dav1d_resize_filter: Align8<[[i8; 8]; 64]> = Align8([
    [0, 0, 0, -128, 0, 0, 0, 0],
    [0, 0, 1, -128, -2, 1, 0, 0],
    [0, -1, 3, -127, -4, 2, -1, 0],
    [0, -1, 4, -127, -6, 3, -1, 0],
    [0, -2, 6, -126, -8, 3, -1, 0],
    [0, -2, 7, -125, -11, 4, -1, 0],
    [1, -2, 8, -125, -13, 5, -2, 0],
    [1, -3, 9, -124, -15, 6, -2, 0],
    [1, -3, 10, -123, -18, 6, -2, 1],
    [1, -3, 11, -122, -20, 7, -3, 1],
    [1, -4, 12, -121, -22, 8, -3, 1],
    [1, -4, 13, -120, -25, 9, -3, 1],
    [1, -4, 14, -118, -28, 9, -3, 1],
    [1, -4, 15, -117, -30, 10, -4, 1],
    [1, -5, 16, -116, -32, 11, -4, 1],
    [1, -5, 16, -114, -35, 12, -4, 1],
    [1, -5, 17, -112, -38, 12, -4, 1],
    [1, -5, 18, -111, -40, 13, -5, 1],
    [1, -5, 18, -109, -43, 14, -5, 1],
    [1, -6, 19, -107, -45, 14, -5, 1],
    [1, -6, 19, -105, -48, 15, -5, 1],
    [1, -6, 19, -103, -51, 16, -5, 1],
    [1, -6, 20, -101, -53, 16, -6, 1],
    [1, -6, 20, -99, -56, 17, -6, 1],
    [1, -6, 20, -97, -58, 17, -6, 1],
    [1, -6, 20, -95, -61, 18, -6, 1],
    [2, -7, 20, -93, -64, 18, -6, 2],
    [2, -7, 20, -91, -66, 19, -6, 1],
    [2, -7, 20, -88, -69, 19, -6, 1],
    [2, -7, 20, -86, -71, 19, -6, 1],
    [2, -7, 20, -84, -74, 20, -7, 2],
    [2, -7, 20, -81, -76, 20, -7, 1],
    [2, -7, 20, -79, -79, 20, -7, 2],
    [1, -7, 20, -76, -81, 20, -7, 2],
    [2, -7, 20, -74, -84, 20, -7, 2],
    [1, -6, 19, -71, -86, 20, -7, 2],
    [1, -6, 19, -69, -88, 20, -7, 2],
    [1, -6, 19, -66, -91, 20, -7, 2],
    [2, -6, 18, -64, -93, 20, -7, 2],
    [1, -6, 18, -61, -95, 20, -6, 1],
    [1, -6, 17, -58, -97, 20, -6, 1],
    [1, -6, 17, -56, -99, 20, -6, 1],
    [1, -6, 16, -53, -101, 20, -6, 1],
    [1, -5, 16, -51, -103, 19, -6, 1],
    [1, -5, 15, -48, -105, 19, -6, 1],
    [1, -5, 14, -45, -107, 19, -6, 1],
    [1, -5, 14, -43, -109, 18, -5, 1],
    [1, -5, 13, -40, -111, 18, -5, 1],
    [1, -4, 12, -38, -112, 17, -5, 1],
    [1, -4, 12, -35, -114, 16, -5, 1],
    [1, -4, 11, -32, -116, 16, -5, 1],
    [1, -4, 10, -30, -117, 15, -4, 1],
    [1, -3, 9, -28, -118, 14, -4, 1],
    [1, -3, 9, -25, -120, 13, -4, 1],
    [1, -3, 8, -22, -121, 12, -4, 1],
    [1, -3, 7, -20, -122, 11, -3, 1],
    [1, -2, 6, -18, -123, 10, -3, 1],
    [0, -2, 6, -15, -124, 9, -3, 1],
    [0, -2, 5, -13, -125, 8, -2, 1],
    [0, -1, 4, -11, -125, 7, -2, 0],
    [0, -1, 3, -8, -126, 6, -2, 0],
    [0, -1, 3, -6, -127, 4, -1, 0],
    [0, -1, 2, -4, -127, 3, -1, 0],
    [0, 0, 1, -2, -128, 1, 0, 0],
]);

#[no_mangle]
pub static dav1d_sm_weights: Align16<[u8; 128]> = Align16([
    0, 0, 255, 128, 255, 149, 85, 64, 255, 197, 146, 105, 73, 50, 37, 32, 255, 225, 196, 170, 145,
    123, 102, 84, 68, 54, 43, 33, 26, 20, 17, 16, 255, 240, 225, 210, 196, 182, 169, 157, 145, 133,
    122, 111, 101, 92, 83, 74, 66, 59, 52, 45, 39, 34, 29, 25, 21, 17, 14, 12, 10, 9, 8, 8, 255,
    248, 240, 233, 225, 218, 210, 203, 196, 189, 182, 176, 169, 163, 156, 150, 144, 138, 133, 127,
    121, 116, 111, 106, 101, 96, 91, 86, 82, 77, 73, 69, 65, 61, 57, 54, 50, 47, 44, 41, 38, 35,
    32, 29, 27, 25, 22, 20, 18, 16, 15, 13, 12, 10, 9, 8, 7, 6, 6, 5, 5, 4, 4, 4,
]);

#[no_mangle]
pub static dav1d_dr_intra_derivative: [u16; 44] = [
    0, 1023, 0, 547, 372, 0, 0, 273, 215, 0, 178, 151, 0, 132, 116, 0, 102, 0, 90, 80, 0, 71, 64,
    0, 57, 51, 0, 45, 0, 40, 35, 0, 31, 27, 0, 23, 19, 0, 15, 0, 11, 0, 7, 3,
];

pub const FLT_INCR: usize = 2;

const FILTER_INDICES: [usize; 7] = [0, 1, 16, 17, 32, 33, 48];

pub fn filter_fn(flt_ptr: &[i8], p: [i32; 7]) -> i32 {
    let flt_ptr = &flt_ptr[..48 + 1];
    let mut sum = 0;
    for i in 0..7 {
        sum += flt_ptr[FILTER_INDICES[i]] as i32 * p[i] as i32;
    }
    sum
}

const fn gen_filter(mut a: [i8; 64], idx: usize, f: [i8; 7]) -> [i8; 64] {
    let mut i = 0;
    while i < 7 {
        a[FLT_INCR * idx + FILTER_INDICES[i]] = f[i];
        i += 1;
    }
    a
}

const fn gen_filters(f: [[i8; 7]; 8]) -> Align64<[i8; 64]> {
    let mut a = [0; 64];

    let mut i = 0;
    while i < 8 {
        a = gen_filter(a, i, f[i]);
        i += 1;
    }

    Align64(a)
}

#[no_mangle]
pub static dav1d_filter_intra_taps: [Align64<[i8; 64]>; 5] = [
    gen_filters([
        [-6, 10, 0, 0, 0, 12, 0],
        [-5, 2, 10, 0, 0, 9, 0],
        [-3, 1, 1, 10, 0, 7, 0],
        [-3, 1, 1, 2, 10, 5, 0],
        [-4, 6, 0, 0, 0, 2, 12],
        [-3, 2, 6, 0, 0, 2, 9],
        [-3, 2, 2, 6, 0, 2, 7],
        [-3, 1, 2, 2, 6, 3, 5],
    ]),
    gen_filters([
        [-10, 16, 0, 0, 0, 10, 0],
        [-6, 0, 16, 0, 0, 6, 0],
        [-4, 0, 0, 16, 0, 4, 0],
        [-2, 0, 0, 0, 16, 2, 0],
        [-10, 16, 0, 0, 0, 0, 10],
        [-6, 0, 16, 0, 0, 0, 6],
        [-4, 0, 0, 16, 0, 0, 4],
        [-2, 0, 0, 0, 16, 0, 2],
    ]),
    gen_filters([
        [-8, 8, 0, 0, 0, 16, 0],
        [-8, 0, 8, 0, 0, 16, 0],
        [-8, 0, 0, 8, 0, 16, 0],
        [-8, 0, 0, 0, 8, 16, 0],
        [-4, 4, 0, 0, 0, 0, 16],
        [-4, 0, 4, 0, 0, 0, 16],
        [-4, 0, 0, 4, 0, 0, 16],
        [-4, 0, 0, 0, 4, 0, 16],
    ]),
    gen_filters([
        [-2, 8, 0, 0, 0, 10, 0],
        [-1, 3, 8, 0, 0, 6, 0],
        [-1, 2, 3, 8, 0, 4, 0],
        [0, 1, 2, 3, 8, 2, 0],
        [-1, 4, 0, 0, 0, 3, 10],
        [-1, 3, 4, 0, 0, 4, 6],
        [-1, 2, 3, 4, 0, 4, 4],
        [-1, 2, 2, 3, 4, 3, 3],
    ]),
    gen_filters([
        [-12, 14, 0, 0, 0, 14, 0],
        [-10, 0, 14, 0, 0, 12, 0],
        [-9, 0, 0, 14, 0, 11, 0],
        [-8, 0, 0, 0, 14, 10, 0],
        [-10, 12, 0, 0, 0, 0, 14],
        [-9, 1, 12, 0, 0, 0, 12],
        [-8, 0, 0, 12, 0, 1, 11],
        [-7, 0, 0, 1, 12, 1, 9],
    ]),
];

#[no_mangle]
pub static dav1d_obmc_masks: Align16<[u8; 64]> = Align16([
    0, 0, 19, 0, 25, 14, 5, 0, 28, 22, 16, 11, 7, 3, 0, 0, 30, 27, 24, 21, 18, 15, 12, 10, 8, 6, 4,
    3, 0, 0, 0, 0, 31, 29, 28, 26, 24, 23, 21, 20, 19, 17, 16, 14, 13, 12, 11, 9, 8, 7, 6, 5, 4, 4,
    3, 2, 0, 0, 0, 0, 0, 0, 0, 0,
]);
