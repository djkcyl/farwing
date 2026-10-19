// SPDX-License-Identifier: LGPL-2.1-or-later
use super::{
    av1_params as abi,
    dxva::{Codec, Failure, Picture, Pool},
};
use anyhow::{Context, Result, ensure};
use bytemuck::Zeroable;
use farwing_codec::syntax::av1::syntax::*;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use windows::Win32::Graphics::Direct3D11::ID3D11Device;

pub struct Av1 {
    device: ID3D11Device,
    pool: Option<Arc<Pool>>,
    parser: Parser,
    refs: [Option<Picture>; 8],
    fresh: bool,
}
impl Av1 {
    pub fn new(device: ID3D11Device) -> Self {
        Self {
            device,
            pool: None,
            parser: Parser::default(),
            refs: Default::default(),
            fresh: true,
        }
    }
    pub fn reset(&mut self) {
        self.refs = Default::default();
        self.fresh = true;
        self.parser = Parser::default();
    }
    pub fn decode(&mut self, data: &[u8], cancel: &AtomicBool) -> Result<Option<Picture>> {
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            self.decode_inner(data, cancel)
        }))
        .unwrap_or_else(|_| Err(anyhow::anyhow!("invalid AV1 bitstream")));
        if result.is_err() {
            self.reset();
        }
        result
    }
    fn decode_inner(&mut self, data: &[u8], cancel: &AtomicBool) -> Result<Option<Picture>> {
        ensure!(!cancel.load(Ordering::Acquire), "decode cancelled");
        // Parse one temporal unit transactionally. Never commit references after
        // failed GPU submission; the caller will request a new sequence/keyframe.
        let mut header = None;
        let mut controls = Vec::new();
        let mut bitstream = Vec::new();
        for unit in crate::media::av1::units(data)? {
            let unit = unit?.bytes;
            let obu = self.parser.read_obu(unit).map_err(anyhow::Error::msg)?;
            let parsed = self.parser.parse_obu(obu).map_err(anyhow::Error::msg)?;
            let group = match parsed {
                ParsedObu::SequenceHeader(_) => {
                    self.refs = Default::default();
                    self.fresh = true;
                    None
                }
                ParsedObu::Frame(f) => {
                    ensure!(header.is_none(), "multiple AV1 pictures in temporal unit");
                    header = Some(f.header);
                    Some(f.tile_group)
                }
                ParsedObu::FrameHeader(h) => {
                    ensure!(header.is_none(), "duplicate AV1 frame header");
                    header = Some(h);
                    None
                }
                ParsedObu::TileGroup(g) => Some(g),
                _ => None,
            };
            if let Some(group) = group {
                let bytes = group.obu.as_ref();
                for tile in group.tiles {
                    ensure!(controls.len() < 256, "too many AV1 tiles");
                    let start = tile.tile_offset as usize;
                    let end = start
                        .checked_add(tile.tile_size as usize)
                        .context("tile overflow")?;
                    let tile_bytes = bytes.get(start..end).context("truncated AV1 tile")?;
                    controls.push(abi::Tile {
                        offset: bitstream.len() as u32,
                        size: tile.tile_size,
                        row: tile.tile_row.try_into()?,
                        col: tile.tile_col.try_into()?,
                        reserved: 0,
                        anchor: 255,
                        reserved8: 0,
                    });
                    bitstream.extend_from_slice(tile_bytes);
                }
            }
        }
        let Some(h) = header else { return Ok(None) };
        if h.show_existing_frame {
            ensure!(!self.fresh, "AV1 reference unavailable after reset");
            let mut picture = self
                .refs
                .get(h.frame_to_show_map_idx as usize)
                .and_then(Option::as_ref)
                .context("missing AV1 display reference")?
                .clone();
            self.parser
                .ref_frame_update(&h)
                .map_err(anyhow::Error::msg)?;
            picture.needed_for_output = true;
            picture.sequence_start = None;
            if h.frame_type == FrameType::KeyFrame {
                self.refs.fill(Some(picture.clone()));
            }
            return Ok(Some(picture));
        }
        let s = self
            .parser
            .sequence_header
            .as_ref()
            .context("missing AV1 sequence")?
            .clone();
        if h.frame_type == FrameType::KeyFrame {
            // With frame ID numbers the parser rejects a frame whose
            // references are not the frames the encoder used (a loss).
            tracing::debug!(
                frame_ids = s.frame_id_numbers_present_flag,
                "AV1 keyframe sequence"
            );
        }
        ensure!(
            !s.color_config.mono_chrome
                && matches!(s.bit_depth, BitDepth::Depth8 | BitDepth::Depth10),
            Failure::Unsupported
        );
        let chroma = match (
            s.seq_profile,
            s.color_config.subsampling_x,
            s.color_config.subsampling_y,
        ) {
            (Profile::Profile0, true, true) => 1,
            (Profile::Profile1, false, false) => 3,
            _ => anyhow::bail!(Failure::Unsupported),
        };
        let key = h.frame_type == FrameType::KeyFrame;
        ensure!(!self.fresh || key, "waiting for AV1 keyframe");
        if key {
            self.refs = Default::default();
        }
        let width = h.upscaled_width;
        let height = h.frame_height;
        let depth = 8 + 2 * s.bit_depth as u8;
        ensure!(
            width > 0 && height > 0 && width <= 16384 && height <= 16384,
            "invalid AV1 geometry"
        );
        if self.pool.as_ref().is_none_or(|p| {
            p.width != width || p.height != height || p.depth != depth || p.chroma != chroma
        }) {
            ensure!(key, "AV1 resolution change requires keyframe");
            self.pool = Some(Pool::new(
                self.device.clone(),
                Codec::Av1,
                width,
                height,
                depth,
                chroma,
                9,
            )?);
        }
        let pool = self.pool.as_ref().unwrap();
        let current = Arc::new(pool.lease(false)?);
        let mut p = params(&s, &h, &self.refs)?;
        p.current = current.index.try_into()?;
        ensure!(
            controls.len() == h.tile_info.tile_cols as usize * h.tile_info.tile_rows as usize,
            "missing AV1 tiles"
        );
        for (i, t) in controls.iter().enumerate() {
            ensure!(
                usize::from(t.row) * h.tile_info.tile_cols as usize + usize::from(t.col) == i,
                "out-of-order AV1 tiles"
            );
        }
        pool.submit_av1(
            &current,
            bytemuck::bytes_of(&p),
            bytemuck::cast_slice(&controls),
            &bitstream,
            cancel,
        )?;
        self.parser
            .ref_frame_update(&h)
            .map_err(anyhow::Error::msg)?;
        let picture = Picture {
            surface: current,
            left: 0,
            top: 0,
            width,
            height,
            poc: 0,
            reorder_limit: 0,
            sequence_start: key.then_some(true),
            needed_for_output: h.show_frame,
        };
        for i in 0..8 {
            if h.refresh_frame_flags & (1 << i) != 0 {
                self.refs[i] = Some(picture.clone());
            }
        }
        self.fresh = false;
        Ok(Some(picture))
    }
}
fn params(
    s: &SequenceHeaderObu,
    h: &FrameHeaderObu,
    refs: &[Option<Picture>; 8],
) -> Result<abi::Params> {
    let mut p = abi::Params::zeroed();
    p.width = h.upscaled_width;
    p.height = h.frame_height;
    p.max_width = u32::from(s.max_frame_width_minus_1) + 1;
    p.max_height = u32::from(s.max_frame_height_minus_1) + 1;
    p.superres = h.superres_denom as u8;
    p.depth = 8 + 2 * s.bit_depth as u8;
    p.profile = s.seq_profile as u8;
    let t = &h.tile_info;
    ensure!(
        (1..=64).contains(&t.tile_cols) && (1..=64).contains(&t.tile_rows),
        "invalid AV1 tile layout"
    );
    p.tiles.cols = t.tile_cols as u8;
    p.tiles.rows = t.tile_rows as u8;
    p.tiles.context = t.context_update_tile_id.try_into()?;
    for i in 0..t.tile_cols as usize {
        p.tiles.widths[i] = (t.width_in_sbs_minus_1[i] + 1).try_into()?;
    }
    for i in 0..t.tile_rows as usize {
        p.tiles.heights[i] = (t.height_in_sbs_minus_1[i] + 1).try_into()?;
    }
    for (bit, on) in [
        s.use_128x128_superblock,
        s.enable_intra_edge_filter,
        s.enable_interintra_compound,
        s.enable_masked_compound,
        h.allow_warped_motion,
        s.enable_dual_filter,
        s.enable_jnt_comp,
        h.allow_screen_content_tools != 0,
        h.force_integer_mv != 0,
        s.enable_cdef,
        s.enable_restoration,
        s.film_grain_params_present,
        h.allow_intrabc,
        h.allow_high_precision_mv,
        h.is_motion_mode_switchable,
        s.enable_filter_intra,
        h.disable_frame_end_update_cdf,
        h.disable_cdf_update,
        h.reference_select,
        h.skip_mode_present,
        h.reduced_tx_set,
        h.use_superres,
    ]
    .into_iter()
    .enumerate()
    {
        p.coding |= u32::from(on) << bit;
    }
    p.coding |= (h.tx_mode as u32) << 22
        | u32::from(h.use_ref_frame_mvs) << 24
        | u32::from(s.enable_ref_frame_mvs) << 25
        | 1 << 26;
    p.format = h.frame_type as u8
        | u8::from(h.show_frame) << 2
        | u8::from(h.showable_frame) << 3
        | u8::from(s.color_config.subsampling_x) << 4
        | u8::from(s.color_config.subsampling_y) << 5
        | u8::from(s.color_config.mono_chrome) << 6;
    p.primary = h.primary_ref_frame as u8;
    p.order = h.order_hint as u8;
    p.order_bits = s.order_hint_bits.max(0) as u8;
    p.map = [255; 8];
    for (i, r) in refs.iter().enumerate() {
        if let Some(r) = r {
            p.map[i] = r.surface.index.try_into()?;
        }
    }
    for i in 0..7 {
        let index = h.ref_frame_idx[i] as usize;
        let r = refs
            .get(index)
            .context("invalid AV1 reference index")?
            .as_ref();
        if !h.frame_is_intra {
            ensure!(r.is_some(), "missing AV1 prediction reference");
        }
        p.refs[i] = abi::Reference {
            width: r.map_or(0, |r| r.width),
            height: r.map_or(0, |r| r.height),
            motion: h.global_motion_params.gm_params[i + 1],
            flags: u8::from(!h.global_motion_params.warp_valid[i + 1])
                | ((h.global_motion_params.gm_type[i + 1] as u8) << 1),
            index: if r.is_some() { index as u8 } else { 255 },
            reserved: 0,
        };
    }
    let f = &h.loop_filter_params;
    p.filter.levels = f.loop_filter_level;
    p.filter.sharpness = f.loop_filter_sharpness;
    p.filter.flags = u8::from(f.loop_filter_delta_enabled)
        | u8::from(f.loop_filter_delta_update) << 1
        | u8::from(f.delta_lf_multi) << 2
        | u8::from(f.delta_lf_present) << 3;
    p.filter.refs = f.loop_filter_ref_deltas;
    p.filter.modes = f.loop_filter_mode_deltas;
    p.filter.delta_res = f.delta_lf_res;
    let lr = &h.loop_restoration_params;
    for i in 0..3 {
        p.filter.restoration[i] = lr.frame_restoration_type[i] as u8;
        p.filter.unit[i] = if lr.uses_lr {
            lr.loop_restoration_size[i]
                .checked_ilog2()
                .context("invalid restoration unit")? as u16
        } else {
            8
        };
    }
    let q = &h.quantization_params;
    p.quant.flags = u8::from(q.delta_q_present) | ((q.delta_q_res as u8) << 1);
    p.quant.base = q.base_q_idx as u8;
    p.quant.deltas = [
        q.delta_q_y_dc as i8,
        q.delta_q_u_dc as i8,
        q.delta_q_v_dc as i8,
        q.delta_q_u_ac as i8,
        q.delta_q_v_ac as i8,
    ];
    p.quant.matrices = if q.using_qmatrix {
        [q.qm_y as u8, q.qm_u as u8, q.qm_v as u8]
    } else {
        [255; 3]
    };
    let c = &h.cdef_params;
    p.cdef.flags = c.cdef_damping.saturating_sub(3) as u8 | ((c.cdef_bits as u8) << 2);
    for i in 0..8 {
        let sec = |v: u32| if v == 4 { 3 } else { v as u8 };
        p.cdef.y[i] = c.cdef_y_pri_strength[i] as u8 | sec(c.cdef_y_sec_strength[i]) << 6;
        p.cdef.uv[i] = c.cdef_uv_pri_strength[i] as u8 | sec(c.cdef_uv_sec_strength[i]) << 6;
    }
    p.interpolation = h.interpolation_filter as u8;
    let seg = &h.segmentation_params;
    p.segments.flags = u8::from(seg.segmentation_enabled)
        | u8::from(seg.segmentation_update_map) << 1
        | u8::from(seg.segmentation_update_data) << 2
        | u8::from(seg.segmentation_temporal_update) << 3;
    p.segments.data = seg.feature_data;
    for i in 0..8 {
        for j in 0..8 {
            p.segments.masks[i] |= u8::from(seg.feature_enabled[i][j]) << j;
        }
    }
    let g = &h.film_grain_params;
    if g.apply_grain {
        ensure!(
            g.num_y_points <= 14 && g.num_cb_points <= 10 && g.num_cr_points <= 10,
            "invalid AV1 grain points"
        );
        p.grain.flags = 1
            | (g.grain_scaling_minus_8 as u16) << 1
            | u16::from(g.chroma_scaling_from_luma) << 3
            | (g.ar_coeff_lag as u16) << 4
            | (g.ar_coeff_shift_minus_6 as u16) << 6
            | (g.grain_scale_shift as u16) << 8
            | u16::from(g.overlap_flag) << 10
            | u16::from(g.clip_to_restricted_range) << 11
            | u16::from(s.color_config.matrix_coefficients as u8 == 0) << 12;
        p.grain.seed = g.grain_seed;
        p.grain.num_y = g.num_y_points;
        p.grain.num_cb = g.num_cb_points;
        p.grain.num_cr = g.num_cr_points;
        for i in 0..g.num_y_points as usize {
            p.grain.y[i] = [g.point_y_value[i], g.point_y_scaling[i]];
        }
        for i in 0..g.num_cb_points as usize {
            p.grain.cb[i] = [g.point_cb_value[i], g.point_cb_scaling[i]];
        }
        for i in 0..g.num_cr_points as usize {
            p.grain.cr[i] = [g.point_cr_value[i], g.point_cr_scaling[i]];
        }
        p.grain.ar_y = g.ar_coeffs_y_plus_128[..24].try_into()?;
        p.grain.ar_cb = g.ar_coeffs_cb_plus_128;
        p.grain.ar_cr = g.ar_coeffs_cr_plus_128;
        p.grain.cb_mult = g.cb_mult;
        p.grain.cb_luma = g.cb_luma_mult;
        p.grain.cr_mult = g.cr_mult;
        p.grain.cr_luma = g.cr_luma_mult;
        p.grain.cb_offset = g.cb_offset as i16;
        p.grain.cr_offset = g.cr_offset as i16;
    }
    Ok(p)
}
