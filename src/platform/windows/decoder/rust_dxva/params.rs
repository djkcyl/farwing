// SPDX-License-Identifier: LGPL-2.1-or-later
// DXVA_PicParams_HEVC_RangeExt, Windows SDK dxva.h; packed ABI extends HEVC.
#[repr(C, packed)]
#[derive(Default)]
pub struct HevcRangeParams {
    pub base: HevcParams,
    pub flags: u16,
    pub diff_cu_chroma_qp_offset_depth: u8,
    pub log2_sao_offset_scale_luma: u8,
    pub log2_sao_offset_scale_chroma: u8,
    pub log2_max_transform_skip_block_size_minus2: u8,
    pub cb_qp_offset_list: [i8; 6],
    pub cr_qp_offset_list: [i8; 6],
    pub chroma_qp_offset_list_len_minus1: u8,
    pub reserved: u16,
}

#[repr(C, packed)]
#[allow(non_snake_case)]
pub struct HevcParams {
    pub PicWidthInMinCbsY: u16,
    pub PicHeightInMinCbsY: u16,
    pub wFormatAndSequenceInfoFlags: u16,
    pub CurrPic: u8,
    pub sps_max_dec_pic_buffering_minus1: u8,
    pub log2_min_luma_coding_block_size_minus3: u8,
    pub log2_diff_max_min_luma_coding_block_size: u8,
    pub log2_min_transform_block_size_minus2: u8,
    pub log2_diff_max_min_transform_block_size: u8,
    pub max_transform_hierarchy_depth_inter: u8,
    pub max_transform_hierarchy_depth_intra: u8,
    pub num_short_term_ref_pic_sets: u8,
    pub num_long_term_ref_pics_sps: u8,
    pub num_ref_idx_l0_default_active_minus1: u8,
    pub num_ref_idx_l1_default_active_minus1: u8,
    pub init_qp_minus26: i8,
    pub ucNumDeltaPocsOfRefRpsIdx: u8,
    pub wNumBitsForShortTermRPSInSlice: u16,
    pub ReservedBits2: u16,
    pub dwCodingParamToolFlags: u32,
    pub dwCodingSettingPicturePropertyFlags: u32,
    pub pps_cb_qp_offset: i8,
    pub pps_cr_qp_offset: i8,
    pub num_tile_columns_minus1: u8,
    pub num_tile_rows_minus1: u8,
    pub column_width_minus1: [u16; 19],
    pub row_height_minus1: [u16; 21],
    pub diff_cu_qp_delta_depth: u8,
    pub pps_beta_offset_div2: i8,
    pub pps_tc_offset_div2: i8,
    pub log2_parallel_merge_level_minus2: u8,
    pub CurrPicOrderCntVal: i32,
    pub RefPicList: [u8; 15],
    pub ReservedBits5: u8,
    pub PicOrderCntValList: [i32; 15],
    pub RefPicSetStCurrBefore: [u8; 8],
    pub RefPicSetStCurrAfter: [u8; 8],
    pub RefPicSetLtCurr: [u8; 8],
    pub ReservedBits6: u16,
    pub ReservedBits7: u16,
    pub StatusReportFeedbackNumber: u32,
}
impl Default for HevcParams {
    fn default() -> Self {
        unsafe { std::mem::zeroed() }
    }
}
