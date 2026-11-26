use crate::media::VideoCodec;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct VideoFormatSignature {
    /// H.264 `profile_idc`, HEVC `general_profile_idc` or AV1 `seq_profile`. Decides which
    /// hardware profile a backend has to ask the driver for.
    pub profile_idc: u8,
    pub coded_width: u32,
    pub coded_height: u32,
    pub visible_width: u32,
    pub visible_height: u32,
    pub crop_left: u32,
    pub crop_top: u32,
    pub chroma_format_idc: u8,
    pub bit_depth_luma: u8,
    pub bit_depth_chroma: u8,
}

pub(crate) fn parse_stream_format(codec: VideoCodec, data: &[u8]) -> Option<VideoFormatSignature> {
    if codec == VideoCodec::Av1 {
        return super::av1::format(data);
    }
    annex_b_units(data)
        .into_iter()
        .find_map(|unit| match codec {
            VideoCodec::H264 if unit.first().is_some_and(|header| header & 0x1f == 7) => {
                parse_h264_sps(unit)
            }
            VideoCodec::H265
                if unit
                    .first()
                    .is_some_and(|header| (header >> 1) & 0x3f == 33) =>
            {
                parse_h265_sps(unit)
            }
            _ => None,
        })
}

pub(crate) fn annex_b_units(data: &[u8]) -> Vec<&[u8]> {
    let mut marks = Vec::new();
    let mut index = 0usize;
    while index + 3 <= data.len() {
        let prefix = if data[index..].starts_with(&[0, 0, 0, 1]) {
            Some(4)
        } else if data[index..].starts_with(&[0, 0, 1]) {
            Some(3)
        } else {
            None
        };
        if let Some(prefix) = prefix {
            marks.push((index, prefix));
            index += prefix;
        } else {
            index += 1;
        }
    }
    let mut units = Vec::with_capacity(marks.len());
    for (position, (start, prefix)) in marks.iter().copied().enumerate() {
        let payload_start = start + prefix;
        let payload_end = marks
            .get(position + 1)
            .map_or(data.len(), |(next, _)| *next);
        let unit = &data[payload_start..payload_end];
        let end = unit
            .iter()
            .rposition(|byte| *byte != 0)
            .map_or(0, |last| last + 1);
        if end != 0 {
            units.push(&unit[..end]);
        }
    }
    units
}

pub(crate) fn remove_emulation_prevention(data: &[u8]) -> Vec<u8> {
    let mut output = Vec::with_capacity(data.len());
    let mut zero_run = 0u8;
    for &byte in data {
        if zero_run >= 2 && byte == 3 {
            zero_run = 0;
            continue;
        }
        output.push(byte);
        zero_run = if byte == 0 {
            zero_run.saturating_add(1)
        } else {
            0
        };
    }
    output
}

pub(crate) struct BitReader<'a> {
    data: &'a [u8],
    bit: usize,
}

impl<'a> BitReader<'a> {
    pub(crate) const fn new(data: &'a [u8]) -> Self {
        Self { data, bit: 0 }
    }

    pub(crate) fn read_bit(&mut self) -> Option<u32> {
        let byte = *self.data.get(self.bit / 8)?;
        let value = u32::from((byte >> (7 - self.bit % 8)) & 1);
        self.bit += 1;
        Some(value)
    }

    pub(crate) fn read_bits(&mut self, count: u32) -> Option<u32> {
        if count > 32 {
            return None;
        }
        let mut value = 0u32;
        for _ in 0..count {
            value = (value << 1) | self.read_bit()?;
        }
        Some(value)
    }

    pub(crate) fn skip_bits(&mut self, count: usize) -> Option<()> {
        let end = self.bit.checked_add(count)?;
        (end <= self.data.len().checked_mul(8)?).then(|| self.bit = end)
    }

    pub(crate) fn read_ue(&mut self) -> Option<u32> {
        let mut zeroes = 0u32;
        while self.read_bit()? == 0 {
            zeroes += 1;
            if zeroes >= 32 {
                return None;
            }
        }
        if zeroes == 0 {
            return Some(0);
        }
        (1u32.checked_shl(zeroes)? - 1).checked_add(self.read_bits(zeroes)?)
    }

    pub(crate) fn read_se(&mut self) -> Option<i32> {
        let code = self.read_ue()?;
        let magnitude = i32::try_from(code.div_ceil(2)).ok()?;
        Some(if code & 1 == 0 { -magnitude } else { magnitude })
    }

    pub(crate) const fn position(&self) -> usize {
        self.bit
    }
}

pub(crate) fn parse_h264_sps(unit: &[u8]) -> Option<VideoFormatSignature> {
    let rbsp = remove_emulation_prevention(unit.get(1..)?);
    let profile_idc = *rbsp.first()?;
    let mut reader = BitReader::new(rbsp.get(3..)?);
    let _sps_id = reader.read_ue()?;
    let mut chroma_format_idc = 1u32;
    let mut separate_colour_plane = false;
    let mut bit_depth_luma = 8u32;
    let mut bit_depth_chroma = 8u32;
    if matches!(
        profile_idc,
        44 | 83 | 86 | 100 | 110 | 118 | 122 | 128 | 134 | 135 | 138 | 139 | 244
    ) {
        chroma_format_idc = reader.read_ue()?;
        if chroma_format_idc == 3 {
            separate_colour_plane = reader.read_bit()? != 0;
        }
        bit_depth_luma = reader.read_ue()?.checked_add(8)?;
        bit_depth_chroma = reader.read_ue()?.checked_add(8)?;
        let _qpprime_y_zero_transform_bypass_flag = reader.read_bit()?;
        if reader.read_bit()? != 0 {
            let scaling_lists = if chroma_format_idc == 3 { 12 } else { 8 };
            for index in 0..scaling_lists {
                if reader.read_bit()? != 0 {
                    skip_h264_scaling_list(&mut reader, if index < 6 { 16 } else { 64 })?;
                }
            }
        }
    }
    let _log2_max_frame_num_minus4 = reader.read_ue()?;
    match reader.read_ue()? {
        0 => {
            let _log2_max_pic_order_cnt_lsb_minus4 = reader.read_ue()?;
        }
        1 => {
            let _delta_pic_order_always_zero_flag = reader.read_bit()?;
            let _offset_for_non_ref_pic = reader.read_se()?;
            let _offset_for_top_to_bottom_field = reader.read_se()?;
            for _ in 0..reader.read_ue()? {
                let _offset_for_ref_frame = reader.read_se()?;
            }
        }
        2 => {}
        _ => return None,
    }
    let _max_num_ref_frames = reader.read_ue()?;
    let _gaps_in_frame_num_value_allowed_flag = reader.read_bit()?;
    let width_in_mbs = reader.read_ue()?.checked_add(1)?;
    let height_in_map_units = reader.read_ue()?.checked_add(1)?;
    let frame_mbs_only = reader.read_bit()? != 0;
    if !frame_mbs_only {
        let _mb_adaptive_frame_field_flag = reader.read_bit()?;
    }
    let _direct_8x8_inference_flag = reader.read_bit()?;
    let (crop_left_offset, crop_right_offset, crop_top_offset, crop_bottom_offset) =
        if reader.read_bit()? != 0 {
            (
                reader.read_ue()?,
                reader.read_ue()?,
                reader.read_ue()?,
                reader.read_ue()?,
            )
        } else {
            (0, 0, 0, 0)
        };
    let coded_width = width_in_mbs.checked_mul(16)?;
    let coded_height = height_in_map_units
        .checked_mul(16)?
        .checked_mul(if frame_mbs_only { 1 } else { 2 })?;
    let effective_chroma = if separate_colour_plane {
        0
    } else {
        chroma_format_idc
    };
    let (sub_width, sub_height) = match effective_chroma {
        0 => (1, 1),
        1 => (2, 2),
        2 => (2, 1),
        3 => (1, 1),
        _ => return None,
    };
    let frame_factor = if frame_mbs_only { 1 } else { 2 };
    make_signature(
        profile_idc,
        coded_width,
        coded_height,
        crop_left_offset.checked_mul(sub_width)?,
        crop_right_offset.checked_mul(sub_width)?,
        crop_top_offset
            .checked_mul(sub_height)?
            .checked_mul(frame_factor)?,
        crop_bottom_offset
            .checked_mul(sub_height)?
            .checked_mul(frame_factor)?,
        chroma_format_idc,
        bit_depth_luma,
        bit_depth_chroma,
    )
}

fn skip_h264_scaling_list(reader: &mut BitReader<'_>, size: usize) -> Option<()> {
    let mut last_scale = 8i32;
    let mut next_scale = 8i32;
    for _ in 0..size {
        if next_scale != 0 {
            next_scale = last_scale
                .checked_add(reader.read_se()?)?
                .checked_add(256)?
                % 256;
        }
        if next_scale != 0 {
            last_scale = next_scale;
        }
    }
    Some(())
}

pub(crate) fn parse_h265_sps(unit: &[u8]) -> Option<VideoFormatSignature> {
    let rbsp = remove_emulation_prevention(unit.get(2..)?);
    let mut reader = BitReader::new(&rbsp);
    let _sps_video_parameter_set_id = reader.read_bits(4)?;
    let max_sub_layers_minus1 = reader.read_bits(3)? as usize;
    let _sps_temporal_id_nesting_flag = reader.read_bit()?;
    let profile_idc = read_h265_profile_tier_level(&mut reader, max_sub_layers_minus1)?;
    let _sps_seq_parameter_set_id = reader.read_ue()?;
    let chroma_format_idc = reader.read_ue()?;
    let separate_colour_plane = chroma_format_idc == 3 && reader.read_bit()? != 0;
    let coded_width = reader.read_ue()?;
    let coded_height = reader.read_ue()?;
    let (left_offset, right_offset, top_offset, bottom_offset) = if reader.read_bit()? != 0 {
        (
            reader.read_ue()?,
            reader.read_ue()?,
            reader.read_ue()?,
            reader.read_ue()?,
        )
    } else {
        (0, 0, 0, 0)
    };
    let bit_depth_luma = reader.read_ue()?.checked_add(8)?;
    let bit_depth_chroma = reader.read_ue()?.checked_add(8)?;
    let effective_chroma = if separate_colour_plane {
        0
    } else {
        chroma_format_idc
    };
    let (sub_width, sub_height) = match effective_chroma {
        0 => (1, 1),
        1 => (2, 2),
        2 => (2, 1),
        3 => (1, 1),
        _ => return None,
    };
    make_signature(
        profile_idc,
        coded_width,
        coded_height,
        left_offset.checked_mul(sub_width)?,
        right_offset.checked_mul(sub_width)?,
        top_offset.checked_mul(sub_height)?,
        bottom_offset.checked_mul(sub_height)?,
        chroma_format_idc,
        bit_depth_luma,
        bit_depth_chroma,
    )
}

pub(crate) fn skip_h265_profile_tier_level(
    reader: &mut BitReader<'_>,
    max_sub_layers_minus1: usize,
) -> Option<()> {
    read_h265_profile_tier_level(reader, max_sub_layers_minus1).map(|_| ())
}

/// Same walk as [`skip_h265_profile_tier_level`], returning `general_profile_idc`.
pub(crate) fn read_h265_profile_tier_level(
    reader: &mut BitReader<'_>,
    max_sub_layers_minus1: usize,
) -> Option<u8> {
    // general_profile_space(2) + general_tier_flag(1)
    reader.skip_bits(2 + 1)?;
    let general_profile_idc = u8::try_from(reader.read_bits(5)?).ok()?;
    reader.skip_bits(32 + 4 + 44 + 8)?;
    let mut profile_present = [false; 7];
    let mut level_present = [false; 7];
    for index in 0..max_sub_layers_minus1 {
        profile_present[index] = reader.read_bit()? != 0;
        level_present[index] = reader.read_bit()? != 0;
    }
    if max_sub_layers_minus1 > 0 {
        reader.skip_bits((8 - max_sub_layers_minus1) * 2)?;
    }
    for index in 0..max_sub_layers_minus1 {
        if profile_present[index] {
            reader.skip_bits(2 + 1 + 5 + 32 + 4 + 44)?;
        }
        if level_present[index] {
            reader.skip_bits(8)?;
        }
    }
    Some(general_profile_idc)
}

#[allow(clippy::too_many_arguments)]
fn make_signature(
    profile_idc: u8,
    coded_width: u32,
    coded_height: u32,
    crop_left: u32,
    crop_right: u32,
    crop_top: u32,
    crop_bottom: u32,
    chroma_format_idc: u32,
    bit_depth_luma: u32,
    bit_depth_chroma: u32,
) -> Option<VideoFormatSignature> {
    let visible_width = coded_width.checked_sub(crop_left.checked_add(crop_right)?)?;
    let visible_height = coded_height.checked_sub(crop_top.checked_add(crop_bottom)?)?;
    if visible_width == 0 || visible_height == 0 {
        return None;
    }
    Some(VideoFormatSignature {
        profile_idc,
        coded_width,
        coded_height,
        visible_width,
        visible_height,
        crop_left,
        crop_top,
        chroma_format_idc: u8::try_from(chroma_format_idc).ok()?,
        bit_depth_luma: u8::try_from(bit_depth_luma).ok()?,
        bit_depth_chroma: u8::try_from(bit_depth_chroma).ok()?,
    })
}
