// SPDX-License-Identifier: LGPL-2.1-or-later
//! Picture order is independent of pixel/DPB ownership and input identity.
use crate::syntax::{
    poc::{PocSlice, PocSps, PocState, derive_poc},
    slice_header::{MmcoOp, SliceHeader},
    sps::Sps,
};
use crate::{Error, Result};

#[derive(Default)]
pub(crate) struct Order {
    state: PocState,
    mmco5: bool,
    top: i32,
}
pub(crate) struct Ticket {
    state: PocState,
    pub poc: i32,
    top: i32,
    pub mmco5: bool,
}
impl Order {
    pub fn prepare(
        &self,
        s: &Sps,
        h: &SliceHeader,
        idr: bool,
        is_reference: bool,
    ) -> Result<Ticket> {
        let mut state = self.state.clone();
        let result = derive_poc(
            &PocSps {
                pic_order_cnt_type: s.pic_order_cnt_type,
                log2_max_pic_order_cnt_lsb_minus4: s.log2_max_pic_order_cnt_lsb_minus4,
                log2_max_frame_num_minus4: s.log2_max_frame_num_minus4,
                delta_pic_order_always_zero_flag: s.delta_pic_order_always_zero_flag,
                offset_for_non_ref_pic: s.offset_for_non_ref_pic,
                offset_for_top_to_bottom_field: s.offset_for_top_to_bottom_field,
                num_ref_frames_in_pic_order_cnt_cycle: s.num_ref_frames_in_pic_order_cnt_cycle,
                offset_for_ref_frame: s.offset_for_ref_frame.clone(),
                frame_mbs_only_flag: s.frame_mbs_only_flag,
            },
            &PocSlice {
                is_reference,
                is_idr: idr,
                frame_num: h.frame_num,
                field_pic_flag: h.field_pic_flag,
                bottom_field_flag: h.bottom_field_flag,
                pic_order_cnt_lsb: h.pic_order_cnt_lsb,
                delta_pic_order_cnt_bottom: h.delta_pic_order_cnt_bottom,
                delta_pic_order_cnt: h.delta_pic_order_cnt,
                prev_had_mmco5: self.mmco5,
                prev_reference_top_foc_for_mmco5: self.top,
            },
            &mut state,
        )
        .map_err(|_| Error::Invalid(crate::Fault::PictureOrder))?;
        let mmco5 = h
            .dec_ref_pic_marking
            .as_ref()
            .and_then(|m| m.adaptive_marking.as_ref())
            .is_some_and(|ops| ops.iter().any(|m| matches!(m, MmcoOp::MarkAllUnused)));
        let top = if mmco5 {
            result
                .top_field_order_cnt
                .checked_sub(result.pic_order_cnt)
                .ok_or(Error::Invalid(crate::Fault::Mmco5OrderOverflow))?
        } else {
            result.top_field_order_cnt
        };
        let poc = if mmco5 { 0 } else { result.pic_order_cnt };
        Ok(Ticket {
            state,
            poc,
            top,
            mmco5,
        })
    }
    pub fn commit(&mut self, t: Ticket) {
        self.state = t.state;
        self.mmco5 = t.mmco5;
        self.top = t.top;
    }
}
