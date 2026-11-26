//! Progressive H.264 VA-API picture preparation.
//!
//! The bitstream work — NAL parsing, POC derivation, DPB marking and reference
//! list construction — is the same `openuuyc_h264` machinery the Windows DXVA
//! backend uses. Only the parameter buffers differ: VA-API always wants full
//! per-slice parameters, including the reference lists, where DXVA's short
//! slice-control format leaves the driver to parse the slice headers itself.
use std::sync::atomic::{AtomicBool, Ordering};

use cros_libva::{
    BufferType, H264PicFields, H264SeqFields, IQMatrix, IQMatrixBufferH264, Picture, PictureH264,
    PictureParameter, PictureParameterBufferH264, SliceParameter, SliceParameterBufferH264,
    VAProfile, VASurfaceID,
};
use openuuyc_h264::headers::Headers;
use openuuyc_h264::syntax::{
    poc::{PocSlice, PocSps, PocState, derive_poc},
    pps::Pps,
    ref_list::{DpbEntry, PicStructure, RefMarking, perform_marking},
    slice_header::{RefPicListModificationOp, SliceHeader, SliceType},
    sps::Sps,
    transform::{select_scaling_list_4x4, select_scaling_list_8x8},
};

use super::display::{self, Pool};
use super::output;
use super::ref_list::{RplmOp, init_ref_pic_list_p, init_ref_pic_lists_b, modify_ref_pic_list};
use crate::media::decode_api::DecodeError;

/// VA flags for a reference entry in `VAPictureParameterBufferH264`.
const PICTURE_SHORT_TERM: u32 = cros_libva::VA_PICTURE_H264_SHORT_TERM_REFERENCE;
const PICTURE_LONG_TERM: u32 = cros_libva::VA_PICTURE_H264_LONG_TERM_REFERENCE;
const PICTURE_INVALID: u32 = cros_libva::VA_PICTURE_H264_INVALID;
const SLICE_DATA_FLAG_ALL: u32 = cros_libva::VA_SLICE_DATA_FLAG_ALL;

/// Surfaces beyond the reference DPB and the picture being decoded, reserved
/// for pictures whose dmabuf the renderer still holds.
///
/// The decoder runs ahead of presentation, and both the pending frame and the
/// one on screen keep their surfaces leased, so a handful of extra slots keeps
/// `take` from ever running dry. They cost one surface's worth of memory each.
const DMABUF_SURFACE_HEADROOM: usize = 8;

/// A picture the driver produced, either on the CPU or exported as a dmabuf.
///
/// The dmabuf variant keeps the surface alive through the [`SurfaceLease`] so
/// the decoder cannot reuse it while the renderer still reads it. The lease is
/// dropped (returning the surface) once the frame is released.
pub(in crate::platform::linux::decoder) struct Frame {
    pub(in crate::platform::linux::decoder) width: u32,
    pub(in crate::platform::linux::decoder) height: u32,
    pub(in crate::platform::linux::decoder) data: FrameData,
}

/// A decoded picture after the pool attached its surface lease.
pub(in crate::platform::linux::decoder) enum FrameData {
    /// Packed NV12 on the CPU: `width * height` luma then interleaved CbCr.
    Cpu(Vec<u8>),
    /// The surface exported as DRM PRIME, kept alive by its lease.
    Dmabuf(crate::platform::linux::dmabuf::DmabufFrame),
}

/// A picture that came back from the driver, before the pool attaches a lease.
pub(in crate::platform::linux::decoder) enum Decoded {
    Cpu(Vec<u8>),
    Dmabuf {
        modifier: u64,
        planes: [crate::platform::linux::dmabuf::Plane; 2],
    },
}

/// A slice of the access unit being assembled.
struct Slice<'a> {
    nal: &'a [u8],
    header: SliceHeader,
    /// Bit offset of `slice_data()` inside `nal`, counting the NAL header byte
    /// and any emulation-prevention bytes the parser removed.
    data_bit_offset: u32,
}

pub(super) struct Avc {
    parser: Headers,
    pool: Option<Pool>,
    profile: VAProfile::Type,
    poc: PocState,
    refs: Vec<DpbEntry>,
    mmco5: bool,
    prev_top: i32,
    need_idr: bool,
    prev_ref_frame_num: Option<u32>,
    timestamp: u64,
}

impl Avc {
    pub(super) fn new() -> Self {
        Self {
            parser: Headers::new(),
            pool: None,
            profile: VAProfile::VAProfileH264High,
            poc: PocState::default(),
            refs: Vec::new(),
            mmco5: false,
            prev_top: 0,
            need_idr: true,
            prev_ref_frame_num: None,
            timestamp: 0,
        }
    }

    pub(super) fn reset(&mut self) {
        self.refs.clear();
        self.poc = PocState::default();
        self.mmco5 = false;
        self.prev_top = 0;
        self.need_idr = true;
        self.prev_ref_frame_num = None;
        if let Some(pool) = self.pool.as_mut() {
            pool.release_all();
        }
    }

    pub(super) fn decode(
        &mut self,
        data: &[u8],
        cancel: &AtomicBool,
    ) -> Result<Frame, DecodeError> {
        let result = self.decode_inner(data, cancel);
        if result.is_err() {
            self.reset();
        }
        result
    }

    #[allow(clippy::too_many_lines, reason = "One access unit, start to finish.")]
    fn decode_inner(&mut self, data: &[u8], cancel: &AtomicBool) -> Result<Frame, DecodeError> {
        if cancel.load(Ordering::Acquire) {
            return Err(DecodeError::Closed);
        }
        let mut slices: Vec<Slice<'_>> = Vec::new();
        let mut first: Option<(u8, u8, SliceHeader, Sps, Pps)> = None;
        for nal in openuuyc_h264::syntax::nal::AnnexBSplitter::new(data) {
            let Some(openuuyc_h264::headers::Slice {
                nal_unit_type,
                nal_ref_idc,
                header,
                slice_data_cursor,
                pps,
                sps,
                ..
            }) = self.parser.process(nal).map_err(parse_error)?
            else {
                continue;
            };
            if !sps.frame_mbs_only_flag
                || header.field_pic_flag
                || !matches!(nal_unit_type, 1 | 5)
                || sps.chroma_format_idc != 1
                || sps.bit_depth_luma_minus8 != 0
                || sps.bit_depth_chroma_minus8 != 0
                || pps.num_slice_groups_minus1 != 0
            {
                return Err(DecodeError::Unsupported);
            }
            if let Some((kind, ref_idc, previous, previous_sps, previous_pps)) = &first {
                let same_picture = previous_sps == &sps
                    && previous_pps == &pps
                    && (*kind == 5) == (nal_unit_type == 5)
                    && (*ref_idc == 0) == (nal_ref_idc == 0)
                    && previous.frame_num == header.frame_num
                    && previous.pic_parameter_set_id == header.pic_parameter_set_id
                    && previous.idr_pic_id == header.idr_pic_id
                    && previous.pic_order_cnt_lsb == header.pic_order_cnt_lsb;
                if !same_picture {
                    return Err(DecodeError::InvalidInput);
                }
            } else if header.first_mb_in_slice != 0 {
                // Arbitrary slice order would need the whole AU buffered first.
                return Err(DecodeError::Unsupported);
            }
            slices.push(Slice {
                nal,
                header: header.clone(),
                data_bit_offset: slice_data_bit_offset(nal, slice_data_cursor),
            });
            if first.is_none() {
                first = Some((nal_unit_type, nal_ref_idc, header, sps, pps));
            }
        }
        let (kind, ref_idc, head, sps, pps) = first.ok_or(DecodeError::InvalidInput)?;
        let idr = kind == 5;
        if self.need_idr && !idr {
            return Err(DecodeError::NeedKeyframe);
        }
        if idr {
            self.reset();
        }
        if let Some(previous) = self.prev_ref_frame_num {
            let next = (previous + 1) % (1 << (sps.log2_max_frame_num_minus4 + 4));
            if head.frame_num != previous && head.frame_num != next {
                // A gap means the reference the next slice needs never arrived.
                return Err(DecodeError::NeedKeyframe);
            }
        }
        if sps.pic_width_in_mbs_minus1 >= 1024 || sps.pic_height_in_map_units_minus1 >= 1024 {
            return Err(DecodeError::Unsupported);
        }
        let coded_width = (sps.pic_width_in_mbs_minus1 + 1) * 16;
        let coded_height = (sps.pic_height_in_map_units_minus1 + 1) * 16;
        let visible = visible_rect((coded_width, coded_height), sps.frame_cropping.as_ref())?;
        let dpb = dpb_capacity(&sps)?;
        self.ensure_pool(&sps, coded_width, coded_height, dpb)?;

        let mut poc_state = self.poc.clone();
        let poc = derive_poc(
            &PocSps {
                pic_order_cnt_type: sps.pic_order_cnt_type,
                log2_max_pic_order_cnt_lsb_minus4: sps.log2_max_pic_order_cnt_lsb_minus4,
                log2_max_frame_num_minus4: sps.log2_max_frame_num_minus4,
                delta_pic_order_always_zero_flag: sps.delta_pic_order_always_zero_flag,
                offset_for_non_ref_pic: sps.offset_for_non_ref_pic,
                offset_for_top_to_bottom_field: sps.offset_for_top_to_bottom_field,
                num_ref_frames_in_pic_order_cnt_cycle: sps.num_ref_frames_in_pic_order_cnt_cycle,
                offset_for_ref_frame: sps.offset_for_ref_frame.clone(),
                frame_mbs_only_flag: true,
            },
            &PocSlice {
                is_reference: ref_idc != 0,
                is_idr: idr,
                frame_num: head.frame_num,
                field_pic_flag: false,
                bottom_field_flag: false,
                pic_order_cnt_lsb: head.pic_order_cnt_lsb,
                delta_pic_order_cnt_bottom: head.delta_pic_order_cnt_bottom,
                delta_pic_order_cnt: head.delta_pic_order_cnt,
                prev_had_mmco5: self.mmco5,
                prev_reference_top_foc_for_mmco5: self.prev_top,
            },
            &mut poc_state,
        )
        .map_err(|_| DecodeError::InvalidInput)?;

        let pool = self.pool.as_mut().ok_or(DecodeError::Backend)?;
        // Surfaces the renderer finished with since the last picture.
        pool.drain_returned();
        let (index, surface) = pool.take()?;
        let surface_id = pool.id(index);
        let max_frame_num = 1u32 << (sps.log2_max_frame_num_minus4 + 4);
        let picture_parameter = picture_parameter(
            &sps,
            &pps,
            &head,
            ref_idc != 0,
            idr,
            &poc,
            surface_id,
            &self.refs,
            pool,
        );
        let matrix = iq_matrix(&sps, &pps);
        let mut slice_parameters = SliceParameterBufferH264::new_array();
        let mut slice_data: Vec<u8> = Vec::new();
        for slice in &slices {
            let offset = u32::try_from(slice_data.len()).map_err(|_| DecodeError::InvalidInput)?;
            slice_data.extend_from_slice(slice.nal);
            add_slice_parameter(
                &mut slice_parameters,
                slice,
                offset,
                &self.refs,
                pool,
                poc.pic_order_cnt,
                head.frame_num,
                max_frame_num,
            )?;
        }

        self.timestamp = self.timestamp.wrapping_add(1);
        let buffers = vec![
            BufferType::PictureParameter(PictureParameter::H264(picture_parameter)),
            BufferType::IQMatrix(IQMatrix::H264(matrix)),
            BufferType::SliceParameter(SliceParameter::H264(slice_parameters)),
            BufferType::SliceData(slice_data),
        ];
        let context = pool.context.clone();
        let readback = Readback {
            format: pool.nv12,
            coded: (pool.width, pool.height),
            visible,
        };
        let decoded = submit(&context, surface, buffers, self.timestamp, readback);
        let (width, height, decoded, surface) = match decoded {
            Ok(decoded) => decoded,
            Err(error) => {
                // The failed picture took the surface with it, so the pool can
                // no longer account for its slots.
                self.pool = None;
                return Err(error);
            }
        };
        let pool = self.pool.as_mut().ok_or(DecodeError::Backend)?;
        // An exported picture keeps its surface reserved until the renderer
        // releases the lease; a CPU picture frees the slot right away.
        let data = match decoded {
            Decoded::Cpu(bytes) => {
                pool.put(index, surface);
                FrameData::Cpu(bytes)
            }
            Decoded::Dmabuf { modifier, planes } => {
                let lease = pool.put_leased(index, surface);
                FrameData::Dmabuf(crate::platform::linux::dmabuf::DmabufFrame::new(
                    modifier, planes, lease,
                ))
            }
        };
        let frame = Frame {
            width,
            height,
            data,
        };

        self.poc = poc_state;
        self.mmco5 = false;
        if ref_idc != 0 {
            let mut entry = DpbEntry {
                frame_num: head.frame_num,
                top_field_order_cnt: poc.top_field_order_cnt,
                bottom_field_order_cnt: poc.bottom_field_order_cnt,
                pic_order_cnt: poc.pic_order_cnt,
                structure: PicStructure::Frame,
                marking: RefMarking::ShortTerm,
                long_term_frame_idx: 0,
                dpb_key: index as u32,
                field_markings: [RefMarking::ShortTerm; 2],
            };
            let marking = head
                .dec_ref_pic_marking
                .as_ref()
                .ok_or(DecodeError::InvalidInput)?;
            let ops = marking
                .adaptive_marking
                .as_ref()
                .map(|ops| ops.iter().map(map_mmco).collect::<Vec<_>>());
            self.mmco5 = perform_marking(
                &mut self.refs,
                &mut entry,
                sps.max_num_ref_frames,
                idr,
                marking.long_term_reference_flag,
                marking.no_output_of_prior_pics_flag,
                ops.as_deref(),
                head.frame_num,
                max_frame_num,
            );
            if self.mmco5 {
                // §8.2.1: an MMCO 5 rebases this picture's order counts to zero.
                let origin = entry.pic_order_cnt;
                entry.frame_num = 0;
                entry.top_field_order_cnt -= origin;
                entry.bottom_field_order_cnt -= origin;
                entry.pic_order_cnt = entry.top_field_order_cnt.min(entry.bottom_field_order_cnt);
                self.poc = PocState::default();
            }
            self.refs
                .retain(|entry| entry.marking != RefMarking::Unused);
            self.prev_top = entry.top_field_order_cnt;
            self.prev_ref_frame_num = Some(entry.frame_num);
            self.refs.push(entry);
        }
        let reserved: Vec<usize> = self
            .refs
            .iter()
            .map(|entry| entry.dpb_key as usize)
            .collect();
        self.pool
            .as_mut()
            .ok_or(DecodeError::Backend)?
            .reserve_only(&reserved);
        self.need_idr = false;
        Ok(frame)
    }

    fn ensure_pool(
        &mut self,
        sps: &Sps,
        width: u32,
        height: u32,
        dpb: usize,
    ) -> Result<(), DecodeError> {
        let profile = profile_for(sps)?;
        let matches = self.pool.as_ref().is_some_and(|pool| {
            pool.width == width && pool.height == height && self.profile == profile
        });
        if matches {
            return Ok(());
        }
        if !self.refs.is_empty() {
            return Err(DecodeError::NeedKeyframe);
        }
        if !display::supports(profile, width, height) {
            return Err(DecodeError::Unsupported);
        }
        // One extra surface for the picture being decoded right now, plus
        // headroom for the pictures the renderer still holds as dmabufs. The
        // decoder runs ahead of presentation, so without that headroom it would
        // run out of free surfaces while the GPU still reads them.
        let extra = if crate::platform::linux::dmabuf::enabled() {
            DMABUF_SURFACE_HEADROOM
        } else {
            0
        };
        self.pool = Some(Pool::new(profile, width, height, dpb + 1 + extra)?);
        self.profile = profile;
        tracing::info!(profile, width, height, dpb, "VA-API H.264 session created");
        Ok(())
    }
}

fn profile_for(sps: &Sps) -> Result<VAProfile::Type, DecodeError> {
    Ok(match sps.profile_idc {
        66 => VAProfile::VAProfileH264ConstrainedBaseline,
        77 => VAProfile::VAProfileH264Main,
        88 | 100 => VAProfile::VAProfileH264High,
        _ => return Err(DecodeError::Unsupported),
    })
}

/// H.264 Annex A Table A-1 MaxDpbMbs, bounded by the 16-frame ceiling.
fn dpb_capacity(sps: &Sps) -> Result<usize, DecodeError> {
    let max_dpb_mbs: u64 = match sps.level_idc {
        0..=10 => 396,
        11 => 900,
        12..=20 => 2376,
        21 => 4752,
        22..=30 => 8100,
        31 => 18000,
        32 => 20480,
        40 | 41 => 32768,
        42 => 34816,
        50 => 110_400,
        51 | 52 => 184_320,
        _ => 184_320,
    };
    let mbs = u64::from(sps.pic_width_in_mbs_minus1 + 1)
        * u64::from(sps.pic_height_in_map_units_minus1 + 1);
    if mbs == 0 {
        return Err(DecodeError::InvalidInput);
    }
    let frames = (max_dpb_mbs / mbs).clamp(1, 16) as usize;
    Ok(frames.max(sps.max_num_ref_frames as usize + 1).min(16))
}

/// Translate an RBSP bit position into one in the raw NAL the driver receives.
fn slice_data_bit_offset(nal: &[u8], cursor: (usize, u8)) -> u32 {
    let (byte, bit) = cursor;
    // The parser strips the NAL header byte before counting.
    let mut raw_byte = byte + 1;
    let mut zeroes = 0u32;
    let mut scanned = 0usize;
    for (index, value) in nal.iter().enumerate().skip(1) {
        if scanned >= byte {
            break;
        }
        if zeroes >= 2 && *value == 3 {
            // An emulation-prevention byte the RBSP does not contain.
            raw_byte += 1;
            zeroes = 0;
            let _ = index;
            continue;
        }
        zeroes = if *value == 0 { zeroes + 1 } else { 0 };
        scanned += 1;
    }
    (raw_byte as u32) * 8 + u32::from(bit)
}

fn parse_error(error: openuuyc_h264::Error) -> DecodeError {
    use openuuyc_h264::Error;
    match error {
        Error::Unsupported(_) => DecodeError::Unsupported,
        Error::NeedKeyframe => DecodeError::NeedKeyframe,
        Error::Cancelled | Error::Closed => DecodeError::Closed,
        Error::Allocation => DecodeError::Backend,
        Error::Truncated | Error::Invalid(_) => DecodeError::InvalidInput,
    }
}

fn map_mmco(
    op: &openuuyc_h264::syntax::slice_header::MmcoOp,
) -> openuuyc_h264::syntax::ref_list::MmcoOp {
    use openuuyc_h264::syntax::ref_list::MmcoOp;
    use openuuyc_h264::syntax::slice_header::MmcoOp as Parsed;
    match *op {
        Parsed::MarkShortTermUnused(value) => MmcoOp::MarkShortTermUnused(value),
        Parsed::MarkLongTermUnused(value) => MmcoOp::MarkLongTermUnused(value),
        Parsed::AssignLongTerm(a, b) => MmcoOp::AssignLongTerm(a, b),
        Parsed::SetMaxLongTermIdx(value) => MmcoOp::SetMaxLongTermIdx(value),
        Parsed::MarkAllUnused => MmcoOp::MarkAllUnused,
        Parsed::AssignCurrentLongTerm(value) => MmcoOp::AssignCurrentLongTerm(value),
    }
}

fn map_rplm(op: &RefPicListModificationOp) -> RplmOp {
    match *op {
        RefPicListModificationOp::Subtract(value) => RplmOp::Subtract(value),
        RefPicListModificationOp::Add(value) => RplmOp::Add(value),
        RefPicListModificationOp::LongTerm(value) => RplmOp::LongTerm(value),
    }
}

fn invalid_picture() -> PictureH264 {
    PictureH264::new(cros_libva::VA_INVALID_ID, 0, PICTURE_INVALID, 0, 0)
}

fn reference_picture(entry: &DpbEntry, pool: &Pool) -> PictureH264 {
    let long_term = entry.marking == RefMarking::LongTerm;
    PictureH264::new(
        pool.id(entry.dpb_key as usize),
        if long_term {
            entry.long_term_frame_idx
        } else {
            entry.frame_num
        },
        if long_term {
            PICTURE_LONG_TERM
        } else {
            PICTURE_SHORT_TERM
        },
        entry.top_field_order_cnt,
        entry.bottom_field_order_cnt,
    )
}

#[allow(clippy::too_many_arguments, reason = "One VA parameter buffer.")]
fn picture_parameter(
    sps: &Sps,
    pps: &Pps,
    head: &SliceHeader,
    is_reference: bool,
    idr: bool,
    poc: &openuuyc_h264::syntax::poc::PocResult,
    surface: VASurfaceID,
    refs: &[DpbEntry],
    pool: &Pool,
) -> PictureParameterBufferH264 {
    let extension = pps.extension.as_ref();
    let current = PictureH264::new(
        surface,
        head.frame_num,
        if is_reference { PICTURE_SHORT_TERM } else { 0 },
        poc.top_field_order_cnt,
        poc.bottom_field_order_cnt,
    );
    let mut reference_frames = std::array::from_fn(|_| invalid_picture());
    for (slot, entry) in reference_frames.iter_mut().zip(refs.iter()) {
        *slot = reference_picture(entry, pool);
    }
    let sequence = H264SeqFields::new(
        u32::from(sps.chroma_format_idc),
        0,
        u32::from(sps.gaps_in_frame_num_value_allowed_flag),
        1,
        0,
        u32::from(sps.direct_8x8_inference_flag),
        // MinLumaBiPredSize8x8 applies from level 3.1 upwards.
        u32::from(sps.level_idc >= 31),
        sps.log2_max_frame_num_minus4,
        sps.pic_order_cnt_type,
        sps.log2_max_pic_order_cnt_lsb_minus4,
        u32::from(sps.delta_pic_order_always_zero_flag),
    );
    let picture = H264PicFields::new(
        u32::from(pps.entropy_coding_mode_flag),
        u32::from(pps.weighted_pred_flag),
        pps.weighted_bipred_idc,
        u32::from(extension.is_some_and(|extension| extension.transform_8x8_mode_flag)),
        // Frame pictures only.
        0,
        u32::from(pps.constrained_intra_pred_flag),
        u32::from(pps.bottom_field_pic_order_in_frame_present_flag),
        u32::from(pps.deblocking_filter_control_present_flag),
        u32::from(pps.redundant_pic_cnt_present_flag),
        u32::from(is_reference),
    );
    let _ = idr;
    PictureParameterBufferH264::new(
        current,
        reference_frames,
        sps.pic_width_in_mbs_minus1 as u16,
        sps.pic_height_in_map_units_minus1 as u16,
        sps.bit_depth_luma_minus8 as u8,
        sps.bit_depth_chroma_minus8 as u8,
        sps.max_num_ref_frames as u8,
        &sequence,
        0,
        0,
        0,
        pps.pic_init_qp_minus26 as i8,
        pps.pic_init_qs_minus26 as i8,
        pps.chroma_qp_index_offset as i8,
        extension.map_or(pps.chroma_qp_index_offset, |extension| {
            extension.second_chroma_qp_index_offset
        }) as i8,
        &picture,
        head.frame_num as u16,
    )
}

/// VA-API takes the scaling lists in zig-zag order; the parser hands them over
/// in raster order, the same conversion the DXVA backend does.
fn iq_matrix(sps: &Sps, pps: &Pps) -> IQMatrixBufferH264 {
    const SCAN_4X4: [usize; 16] = [0, 1, 4, 8, 5, 2, 3, 6, 9, 12, 13, 10, 7, 11, 14, 15];
    let mut scan_8x8 = Vec::with_capacity(64);
    for sum in 0usize..15 {
        let mut diagonal: Vec<usize> = (0usize..8)
            .filter_map(|x| sum.checked_sub(x).filter(|y| *y < 8).map(|y| y * 8 + x))
            .collect();
        if sum % 2 == 1 {
            diagonal.reverse();
        }
        scan_8x8.extend(diagonal);
    }
    let mut lists_4x4 = [[0u8; 16]; 6];
    for (index, list) in lists_4x4.iter_mut().enumerate() {
        let source = select_scaling_list_4x4(index, sps, pps);
        for (target, scan) in list.iter_mut().zip(SCAN_4X4) {
            *target = source[scan] as u8;
        }
    }
    let mut lists_8x8 = [[0u8; 64]; 2];
    for (index, list) in lists_8x8.iter_mut().enumerate() {
        let source = select_scaling_list_8x8(index, sps, pps);
        for (target, scan) in list.iter_mut().zip(scan_8x8.iter()) {
            *target = source[*scan] as u8;
        }
    }
    IQMatrixBufferH264::new(lists_4x4, lists_8x8)
}

/// Reference lists, weights and the slice-header fields VA-API needs per slice.
#[allow(clippy::too_many_arguments, reason = "One VA slice parameter buffer.")]
fn add_slice_parameter(
    parameters: &mut SliceParameterBufferH264,
    slice: &Slice<'_>,
    data_offset: u32,
    refs: &[DpbEntry],
    pool: &Pool,
    current_poc: i32,
    frame_num: u32,
    max_frame_num: u32,
) -> Result<(), DecodeError> {
    let head = &slice.header;
    let kind = head.slice_type;
    let active_l0 = head.num_ref_idx_l0_active_minus1 + 1;
    let active_l1 = head.num_ref_idx_l1_active_minus1 + 1;
    let (mut list_0, mut list_1) = match kind {
        SliceType::P | SliceType::SP => (
            init_ref_pic_list_p(refs, frame_num, max_frame_num, PicStructure::Frame, false),
            Vec::new(),
        ),
        SliceType::B => init_ref_pic_lists_b(refs, current_poc, PicStructure::Frame, false),
        SliceType::I | SliceType::SI => (Vec::new(), Vec::new()),
    };
    if !matches!(kind, SliceType::I | SliceType::SI) {
        let ops: Vec<RplmOp> = head
            .ref_pic_list_modification
            .modifications_l0
            .iter()
            .map(map_rplm)
            .collect();
        modify_ref_pic_list(
            &mut list_0,
            &ops,
            refs,
            active_l0,
            frame_num,
            max_frame_num,
            false,
            false,
        );
    }
    if kind == SliceType::B {
        let ops: Vec<RplmOp> = head
            .ref_pic_list_modification
            .modifications_l1
            .iter()
            .map(map_rplm)
            .collect();
        modify_ref_pic_list(
            &mut list_1,
            &ops,
            refs,
            active_l1,
            frame_num,
            max_frame_num,
            false,
            false,
        );
    }
    let entries = |list: &[u32]| -> [PictureH264; 32] {
        std::array::from_fn(|index| {
            list.get(index)
                .and_then(|key| refs.iter().find(|entry| entry.dpb_key == *key))
                .map_or_else(invalid_picture, |entry| reference_picture(entry, pool))
        })
    };
    let weights = Weights::from(head, active_l0 as usize, active_l1 as usize);
    let size = u32::try_from(slice.nal.len()).map_err(|_| DecodeError::InvalidInput)?;
    parameters.add_slice_parameter(
        size,
        data_offset,
        SLICE_DATA_FLAG_ALL,
        u16::try_from(slice.data_bit_offset).map_err(|_| DecodeError::InvalidInput)?,
        u16::try_from(head.first_mb_in_slice).map_err(|_| DecodeError::InvalidInput)?,
        // VA takes the 0..=4 slice type, not the 5..=9 "all slices" spelling.
        (head.slice_type_raw % 5) as u8,
        u8::from(head.direct_spatial_mv_pred_flag),
        head.num_ref_idx_l0_active_minus1 as u8,
        head.num_ref_idx_l1_active_minus1 as u8,
        head.cabac_init_idc as u8,
        head.slice_qp_delta as i8,
        head.disable_deblocking_filter_idc as u8,
        head.slice_alpha_c0_offset_div2 as i8,
        head.slice_beta_offset_div2 as i8,
        entries(&list_0),
        entries(&list_1),
        weights.luma_log2_denom,
        weights.chroma_log2_denom,
        weights.luma_flag_l0,
        weights.luma_weight_l0,
        weights.luma_offset_l0,
        weights.chroma_flag_l0,
        weights.chroma_weight_l0,
        weights.chroma_offset_l0,
        weights.luma_flag_l1,
        weights.luma_weight_l1,
        weights.luma_offset_l1,
        weights.chroma_flag_l1,
        weights.chroma_weight_l1,
        weights.chroma_offset_l1,
    );
    Ok(())
}

/// The prediction weight table flattened into the fixed arrays VA-API expects.
struct Weights {
    luma_log2_denom: u8,
    chroma_log2_denom: u8,
    luma_flag_l0: u8,
    luma_weight_l0: [i16; 32],
    luma_offset_l0: [i16; 32],
    chroma_flag_l0: u8,
    chroma_weight_l0: [[i16; 2]; 32],
    chroma_offset_l0: [[i16; 2]; 32],
    luma_flag_l1: u8,
    luma_weight_l1: [i16; 32],
    luma_offset_l1: [i16; 32],
    chroma_flag_l1: u8,
    chroma_weight_l1: [[i16; 2]; 32],
    chroma_offset_l1: [[i16; 2]; 32],
}

impl Weights {
    fn from(head: &SliceHeader, active_l0: usize, active_l1: usize) -> Self {
        let Some(table) = head.pred_weight_table.as_ref() else {
            // Without a table every entry uses the default weight of 1.
            return Self {
                luma_log2_denom: 0,
                chroma_log2_denom: 0,
                luma_flag_l0: 0,
                luma_weight_l0: [1; 32],
                luma_offset_l0: [0; 32],
                chroma_flag_l0: 0,
                chroma_weight_l0: [[1; 2]; 32],
                chroma_offset_l0: [[0; 2]; 32],
                luma_flag_l1: 0,
                luma_weight_l1: [1; 32],
                luma_offset_l1: [0; 32],
                chroma_flag_l1: 0,
                chroma_weight_l1: [[1; 2]; 32],
                chroma_offset_l1: [[0; 2]; 32],
            };
        };
        let default_luma = 1i16 << table.luma_log2_weight_denom.min(14);
        let default_chroma = 1i16 << table.chroma_log2_weight_denom.min(14);
        let mut weights = Self {
            luma_log2_denom: table.luma_log2_weight_denom as u8,
            chroma_log2_denom: table.chroma_log2_weight_denom as u8,
            luma_flag_l0: u8::from(table.luma_weights_l0.iter().any(Option::is_some)),
            luma_weight_l0: [default_luma; 32],
            luma_offset_l0: [0; 32],
            chroma_flag_l0: u8::from(table.chroma_weights_l0.iter().any(Option::is_some)),
            chroma_weight_l0: [[default_chroma; 2]; 32],
            chroma_offset_l0: [[0; 2]; 32],
            luma_flag_l1: u8::from(table.luma_weights_l1.iter().any(Option::is_some)),
            luma_weight_l1: [default_luma; 32],
            luma_offset_l1: [0; 32],
            chroma_flag_l1: u8::from(table.chroma_weights_l1.iter().any(Option::is_some)),
            chroma_weight_l1: [[default_chroma; 2]; 32],
            chroma_offset_l1: [[0; 2]; 32],
        };
        fill_luma(
            &table.luma_weights_l0,
            active_l0,
            &mut weights.luma_weight_l0,
            &mut weights.luma_offset_l0,
        );
        fill_luma(
            &table.luma_weights_l1,
            active_l1,
            &mut weights.luma_weight_l1,
            &mut weights.luma_offset_l1,
        );
        fill_chroma(
            &table.chroma_weights_l0,
            active_l0,
            &mut weights.chroma_weight_l0,
            &mut weights.chroma_offset_l0,
        );
        fill_chroma(
            &table.chroma_weights_l1,
            active_l1,
            &mut weights.chroma_weight_l1,
            &mut weights.chroma_offset_l1,
        );
        weights
    }
}

fn fill_luma(
    source: &[Option<(i32, i32)>],
    active: usize,
    weight: &mut [i16; 32],
    offset: &mut [i16; 32],
) {
    for index in 0..active.min(32) {
        if let Some(Some((entry_weight, entry_offset))) = source.get(index) {
            weight[index] = *entry_weight as i16;
            offset[index] = *entry_offset as i16;
        }
    }
}

fn fill_chroma(
    source: &[Option<[(i32, i32); 2]>],
    active: usize,
    weight: &mut [[i16; 2]; 32],
    offset: &mut [[i16; 2]; 32],
) {
    for index in 0..active.min(32) {
        if let Some(Some(entries)) = source.get(index) {
            for (component, (entry_weight, entry_offset)) in entries.iter().enumerate() {
                weight[index][component] = *entry_weight as i16;
                offset[index][component] = *entry_offset as i16;
            }
        }
    }
}

type Surface = cros_libva::Surface<()>;

/// How a decoded surface is copied back to the CPU.
#[derive(Clone, Copy)]
struct Readback {
    format: cros_libva::VAImageFormat,
    coded: (u32, u32),
    visible: output::VisibleRect,
}

/// This backend accepts only progressive 4:2:0: both SPS crop units are 2.
fn visible_rect(
    coded: (u32, u32),
    crop: Option<&openuuyc_h264::syntax::sps::FrameCropping>,
) -> Result<output::VisibleRect, DecodeError> {
    let (left, right, top, bottom) =
        crop.map_or((0, 0, 0, 0), |c| (c.left, c.right, c.top, c.bottom));
    let pixels = |units: u32| units.checked_mul(2).ok_or(DecodeError::InvalidInput);
    let (x, y) = (pixels(left)?, pixels(top)?);
    let width = coded
        .0
        .checked_sub(x)
        .and_then(|w| w.checked_sub(right.checked_mul(2)?))
        .filter(|w| *w > 0)
        .ok_or(DecodeError::InvalidInput)?;
    let height = coded
        .1
        .checked_sub(y)
        .and_then(|h| h.checked_sub(bottom.checked_mul(2)?))
        .filter(|h| *h > 0)
        .ok_or(DecodeError::InvalidInput)?;
    Ok(output::VisibleRect {
        x,
        y,
        width,
        height,
    })
}

#[cfg(test)]
mod crop_tests {
    use super::*;
    use openuuyc_h264::syntax::sps::FrameCropping;

    #[test]
    #[ignore = "requires a VA-API H.264 decoder and render-node access"]
    fn vaapi_decodes_1080p_cropped_fixture() {
        let frame = Avc::new()
            .decode(
                include_bytes!("testdata/crop-1080.h264"),
                &AtomicBool::new(false),
            )
            .expect("VA-API decode");
        assert_eq!((frame.width, frame.height), (1920, 1080));
        let FrameData::Cpu(data) = &frame.data else {
            panic!("zero-copy export is opt-in; this checks the readback");
        };
        let plane = 1920 * 1080;
        assert_eq!(data.len(), plane * 3 / 2);
        // All rows, including the cropped bottom edge, must be red rather
        // than undefined macroblock padding. Allow driver rounding by 2.
        assert!(data[..plane].iter().all(|&y| (79..=83).contains(&y)));
        for uv in data[plane..].chunks_exact(2) {
            assert!((88..=92).contains(&uv[0]));
            assert!((238..=242).contains(&uv[1]));
        }
    }

    #[test]
    fn real_1080p_sps_crops_macroblock_padding() {
        // One red frame generated with FFmpeg/libx264 at 1920x1080.
        let mut parser = Headers::new();
        for nal in openuuyc_h264::syntax::nal::AnnexBSplitter::new(include_bytes!(
            "testdata/crop-1080.h264"
        )) {
            if let Some(slice) = parser.process(nal).unwrap() {
                let sps = slice.sps;
                let coded = (
                    (sps.pic_width_in_mbs_minus1 + 1) * 16,
                    (sps.pic_height_in_map_units_minus1 + 1) * 16,
                );
                assert_eq!(coded, (1920, 1088));
                assert_eq!(
                    visible_rect(coded, sps.frame_cropping.as_ref()).unwrap(),
                    output::VisibleRect {
                        x: 0,
                        y: 0,
                        width: 1920,
                        height: 1080
                    }
                );
                return;
            }
        }
        panic!("fixture contains no H.264 slice");
    }

    #[test]
    fn crop_keeps_left_and_top_origin_and_rejects_invalid_sizes() {
        let mut crop = FrameCropping {
            left: 1,
            right: 2,
            top: 3,
            bottom: 4,
        };
        assert_eq!(
            visible_rect((64, 48), Some(&crop)).unwrap(),
            output::VisibleRect {
                x: 2,
                y: 6,
                width: 58,
                height: 34
            }
        );
        crop.right = u32::MAX;
        assert_eq!(
            visible_rect((64, 48), Some(&crop)),
            Err(DecodeError::InvalidInput)
        );
        crop.right = 31;
        assert_eq!(
            visible_rect((64, 48), Some(&crop)),
            Err(DecodeError::InvalidInput)
        );
    }
}

/// Hand the picture to the driver and copy or export the result.
///
/// A failure before the sync point consumes the surface with the picture, so
/// the caller must drop the whole pool rather than return the surface to it.
fn submit(
    context: &std::rc::Rc<cros_libva::Context>,
    surface: Surface,
    buffers: Vec<BufferType>,
    timestamp: u64,
    readback: Readback,
) -> Result<(u32, u32, Decoded, Surface), DecodeError> {
    // A driver-level failure is not worth retrying on the next keyframe; the
    // pool switches decoders immediately on HardwareFailure.
    let backend = |stage: &'static str| {
        move |error: cros_libva::VaError| {
            tracing::debug!(%error, stage, "VA-API decode failed");
            DecodeError::HardwareFailure
        }
    };
    let mut picture = Picture::new(timestamp, std::rc::Rc::clone(context), surface);
    for buffer in buffers {
        let buffer = context
            .create_buffer(buffer)
            .map_err(backend("create_buffer"))?;
        picture.add_buffer(buffer);
    }
    let picture = picture
        .begin()
        .map_err(backend("begin"))?
        .render()
        .map_err(backend("render"))?
        .end()
        .map_err(backend("end"))?;
    let picture = picture
        .sync()
        .map_err(|(error, _)| backend("sync")(error))?;
    let output::VisibleRect {
        x,
        y,
        width,
        height,
    } = readback.visible;
    // Prefer exporting the surface: the renderer imports the dmabuf instead of
    // the CPU copying the picture out and uploading it again. Drivers without
    // PRIME 2 or a usable modifier fall back to the readback below, and so
    // does a picture cropped on the left or top: the renderer samples an
    // exported surface from its top-left corner.
    let exported = if crate::platform::linux::dmabuf::enabled() && (x, y) == (0, 0) {
        export_nv12(picture.surface())
    } else {
        None
    };
    let decoded = match exported {
        Some((modifier, planes)) => Decoded::Dmabuf { modifier, planes },
        None => {
            // vaDeriveImage is optional and several drivers, including the
            // NVIDIA one, refuse it; vaCreateImage + vaGetImage is the
            // portable readback.
            let frame = picture
                .create_image(readback.format, readback.coded, readback.coded)
                .map_err(backend("create_image"))
                .and_then(|image| output::pack_nv12(&image, readback.visible));
            Decoded::Cpu(frame?)
        }
    };
    let surface = picture
        .take_surface()
        .unwrap_or_else(|_| unreachable!("the picture owns its surface alone"));
    Ok((width, height, decoded, surface))
}

/// Exports an NV12 surface as one dmabuf with a luma and a chroma plane.
///
/// Returns `None` when the driver cannot export it or the layout is not the
/// single-buffer NV12 this renderer imports; the caller then reads back on the
/// CPU. The handles are duplicated because the renderer's import consumes one
/// per plane while the descriptor keeps the originals alive.
fn export_nv12(surface: &Surface) -> Option<(u64, [crate::platform::linux::dmabuf::Plane; 2])> {
    use crate::platform::linux::dmabuf::Plane;
    let descriptor = match surface.export_prime() {
        Ok(descriptor) => descriptor,
        Err(error) => {
            tracing::debug!(%error, "VA-API dmabuf export unavailable");
            return None;
        }
    };
    if descriptor.fourcc != cros_libva::VA_FOURCC_NV12 {
        tracing::debug!(fourcc = descriptor.fourcc, "unexpected exported format");
        return None;
    }
    let layer = descriptor.layers.first()?;
    if descriptor.objects.len() != 1 || layer.num_planes != 2 {
        // Only the single-buffer, two-plane layout maps onto two single-plane
        // Vulkan images; anything else keeps the CPU path.
        tracing::debug!(
            objects = descriptor.objects.len(),
            planes = layer.num_planes,
            "dmabuf layout is not a single-buffer NV12"
        );
        return None;
    }
    let object = descriptor.objects.first()?;
    let planes = [
        Plane {
            fd: object.fd.try_clone().ok()?,
            offset: u64::from(layer.offset[0]),
            stride: u64::from(layer.pitch[0]),
        },
        Plane {
            fd: object.fd.try_clone().ok()?,
            offset: u64::from(layer.offset[1]),
            stride: u64::from(layer.pitch[1]),
        },
    ];
    // The descriptor closes the original fd on drop; the planes own their own.
    Some((object.drm_format_modifier, planes))
}
