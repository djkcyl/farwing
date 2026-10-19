// SPDX-License-Identifier: LGPL-2.1-or-later
// Macroblock-at-a-time reconstruction, following FFmpeg h264_mb_template.c,
// h264_mc_template.c and h264_slice.c. Copyright (c) 2003-2011 Michael
// Niedermayer; Rust adaptation Copyright (c) 2026 OpenUUYC contributors.
use crate::{
    Error, Result,
    dsp::{
        MacroblockPlane, PartitionGeometry,
        filter::{self, Edge, Filter, Motion},
        intra::{self, Edges},
        motion, transform,
    },
    picture::{Chroma, IntraBorders, Picture},
};
use std::sync::Arc;

#[derive(Clone, Copy, Default)]
pub struct Prediction {
    pub reference: Option<u8>,
    pub mv: [i16; 2],
}
#[derive(Clone, Copy)]
pub struct Weights {
    pub denominator: u8,
    pub weight: i16,
    pub offset: i16,
}
#[derive(Clone, Copy, Default)]
pub struct Partition {
    pub x: u8,
    pub y: u8,
    pub width: u8,
    pub height: u8,
    pub prediction: Prediction,
    /// None uses ordinary prediction; explicit P weights are derived
    /// once at slice/partition setup, never re-read from the bitstream here.
    pub weights: [Option<Weights>; 3],
}
pub enum Kind<'a> {
    Intra4([u8; 16]),
    Intra8([u8; 4]),
    Intra16(u8),
    Inter { parts: &'a [Partition] },
}
pub struct Macroblock<'a> {
    pub kind: Kind<'a>,
    pub cbp: u8,
    /// FFmpeg-compatible transposed coefficients, 256 per plane. For 420,
    /// only the first 64 chroma positions (four 4x4 blocks) are used.
    pub coefficients: &'a mut [i16; 768],
    pub nonzero: [u8; 48],
    pub transform8: bool,
    pub bypass: bool,
    pub qp: [i8; 3],
    pub chroma_mode: u8,
}
pub struct Reference {
    /// DPB position is stable throughout this picture, including slice list
    /// reordering. Motion data is not retained across pictures in I/P mode.
    pub slot: u8,
    pub picture: Arc<Picture>,
}
pub struct References<'a> {
    pub pictures: &'a [Reference],
}
#[derive(Clone, Copy)]
pub struct Slice {
    pub id: u32,
    pub constrained_intra: bool,
    pub deblock_idc: u8,
    pub alpha_offset: i8,
    pub beta_offset: i8,
}
#[derive(Clone)]
struct Metadata {
    address: usize,
    slice: Slice,
    intra: bool,
    transform8: bool,
    qp: [i8; 3],
    coded_luma: u16,
    skip_internal: bool,
    motion: [Motion; 16],
}
#[derive(Clone, Copy, Default)]
struct IntraAvailability {
    left: bool,
    top: bool,
    top_left: bool,
    top_right: bool,
}
impl IntraAvailability {
    #[inline]
    fn available<const SIZE: usize>(self, x: i32, y: i32, visited: u16) -> bool {
        if y < 0 {
            if x < 0 {
                self.top_left
            } else if x < SIZE as i32 {
                self.top
            } else {
                x < (2 * SIZE) as i32 && self.top_right
            }
        } else if x < 0 {
            y < SIZE as i32 && self.left
        } else if x < SIZE as i32 && y < SIZE as i32 {
            visited & (1 << (y as usize / 4 * 4 + x as usize / 4)) != 0
        } else {
            false
        }
    }
}
/// One raster-ordered presence bit per luma 4x4 block. All color planes reuse
/// luma boundary strengths; CAVLC's four 8x8 substreams share one coded flag.
fn coded_luma(mb: &Macroblock<'_>) -> u16 {
    let mut mask = 0;
    if mb.transform8 {
        for i in 0..4 {
            if mb.nonzero[i * 4..i * 4 + 4].iter().any(|&n| n != 0) {
                mask |= 0x33 << (i / 2 * 8 + i % 2 * 2);
            }
        }
    } else {
        for i in 0..16 {
            if mb.nonzero[i] != 0 {
                let raster = i / 8 * 8 + i % 4 / 2 * 4 + i / 4 % 2 * 2 + i % 2;
                mask |= 1 << raster;
            }
        }
    }
    mask
}

/// Owns only one mutable picture and one macroblock's scratch. The entropy
/// walker submits each macroblock immediately; no full-slice residual tree.
pub struct Reconstruction {
    picture: Picture,
    metadata: Vec<Option<Metadata>>,
    total: usize,
    base: usize,
    next: usize,
    mb_width: usize,
    borders: IntraBorders,
    failed: bool,
}
impl Reconstruction {
    fn metadata_at(&self, address: usize) -> Option<&Metadata> {
        let slot = if address >= self.base {
            address - self.base
        } else {
            address
                .checked_add(self.metadata.len())?
                .checked_sub(self.base)?
        };
        self.metadata
            .get(slot)
            .and_then(Option::as_ref)
            .filter(|m| m.address == address)
    }
    fn put_metadata(&mut self, address: usize, value: Metadata) {
        if address >= self.base + self.metadata.len() {
            self.base += self.metadata.len();
        }
        self.metadata[address - self.base] = Some(value);
    }

    pub fn new(picture: Picture) -> Self {
        let mb_width = picture.planes[0].width / 16;
        let count = mb_width * (picture.planes[0].height / 16);
        let borders = IntraBorders::new(&picture);
        Self {
            picture,
            metadata: vec![None; count.min(2 * mb_width)],
            total: count,
            base: 0,
            next: 0,
            mb_width,
            borders,
            failed: false,
        }
    }
    pub fn submit(
        &mut self,
        address: usize,
        mb: &mut Macroblock,
        slice: Slice,
        refs: References<'_>,
    ) -> Result<()> {
        if self.failed || address != self.next || address >= self.total {
            return Err(Error::Invalid(crate::Fault::MacroblockSequence));
        }
        let result = match self.picture.chroma {
            Chroma::Yuv444 => self.submit_inner::<true>(address, mb, slice, refs),
            Chroma::Yuv420 => self.submit_inner::<false>(address, mb, slice, refs),
        };
        if result.is_err() {
            self.failed = true;
        }
        result
    }
    pub fn submit_pcm(&mut self, address: usize, bytes: &[u8], slice: Slice) -> Result<()> {
        if self.failed || address != self.next || address >= self.total {
            return Err(Error::Invalid(crate::Fault::PcmMacroblockSequence));
        }
        let expected = if self.picture.chroma == Chroma::Yuv444 {
            768
        } else {
            384
        };
        if bytes.len() != expected {
            self.failed = true;
            return Err(Error::Invalid(crate::Fault::PcmSampleCount));
        }
        let (mx, my) = (address % self.mb_width, address / self.mb_width);
        let mut offset = 0;
        for plane in 0..3 {
            let size = if plane > 0 && self.picture.chroma == Chroma::Yuv420 {
                8
            } else {
                16
            };
            for y in 0..size {
                self.picture.planes[plane].row_mut(my * size + y)[mx * size..mx * size + size]
                    .copy_from_slice(&bytes[offset + y * size..offset + (y + 1) * size]);
            }
            offset += size * size;
        }
        self.put_metadata(
            address,
            Metadata {
                address,
                slice,
                intra: true,
                transform8: false,
                qp: [0; 3],
                coded_luma: u16::MAX,
                skip_internal: false,
                motion: [Motion::default(); 16],
            },
        );
        self.next += 1;
        if self.next % self.mb_width == 0 {
            self.borders.save_before_filter(&self.picture, my)?;
            self.filter_row(my)?;
        }
        Ok(())
    }
    /// P_SKIP is always a complete 16x16 prediction with no coded residual.
    /// Keep reference/weight/filter semantics, but do not construct a residual
    /// macroblock, visit coefficient arrays or validate partition coverage.
    pub(crate) fn submit_skip(
        &mut self,
        address: usize,
        part: &Partition,
        qp: [i8; 3],
        slice: Slice,
        references: &[Reference],
    ) -> Result<()> {
        if self.failed || address != self.next || address >= self.total {
            return Err(Error::Invalid(crate::Fault::MacroblockSequence));
        }
        let result = self.submit_skip_inner(address, part, qp, slice, references);
        if result.is_err() {
            self.failed = true;
        }
        result
    }
    fn submit_skip_inner(
        &mut self,
        address: usize,
        part: &Partition,
        qp: [i8; 3],
        slice: Slice,
        references: &[Reference],
    ) -> Result<()> {
        #[cfg(feature = "profile")]
        let _total = crate::profile::Timer::new(1);
        if slice.deblock_idc > 2 || qp.iter().any(|&q| !(0..=51).contains(&q)) {
            return Err(Error::Invalid(crate::Fault::MacroblockParameters));
        }
        let index = part
            .prediction
            .reference
            .ok_or(Error::Invalid(crate::Fault::PartitionWithoutReference))?;
        let reference = references
            .get(index as usize)
            .ok_or(Error::Invalid(crate::Fault::ReferenceIndex))?;
        let source = &reference.picture;
        if source.chroma != self.picture.chroma
            || source.planes[0].width != self.picture.planes[0].width
            || source.planes[0].height != self.picture.planes[0].height
        {
            return Err(Error::Invalid(crate::Fault::IncompatibleReferenceLayout));
        }
        let (mx, my) = (address % self.mb_width, address / self.mb_width);
        for (plane, dst) in self.picture.planes.iter_mut().enumerate() {
            let chroma = plane != 0 && source.chroma == Chroma::Yuv420;
            let size = if chroma { 8 } else { 16 };
            let (x, y) = (mx * size, my * size);
            let mut target = dst.block_mut(x, y, size, size)?;
            let mv = part.prediction.mv;
            if !chroma {
                if let Some(w) = part.weights[plane] {
                    motion::weighted_luma(
                        &source.planes[plane],
                        (x * 4) as i32 + mv[0] as i32,
                        (y * 4) as i32 + mv[1] as i32,
                        &mut target,
                        w.denominator,
                        w.weight,
                        w.offset,
                    )?;
                    continue;
                }
            }
            if chroma {
                motion::chroma(
                    &source.planes[plane],
                    (x * 8) as i32 + mv[0] as i32,
                    (y * 8) as i32 + mv[1] as i32,
                    &mut target,
                )?;
            } else {
                motion::luma(
                    &source.planes[plane],
                    (x * 4) as i32 + mv[0] as i32,
                    (y * 4) as i32 + mv[1] as i32,
                    &mut target,
                )?;
            }
            if let Some(w) = part.weights[plane] {
                motion::weight(&mut target, w.denominator, w.weight, w.offset)?;
            }
        }
        self.put_metadata(
            address,
            Metadata {
                address,
                slice,
                intra: false,
                transform8: false,
                qp,
                coded_luma: 0,
                skip_internal: true,
                motion: [Motion {
                    reference: reference.slot,
                    x: part.prediction.mv[0],
                    y: part.prediction.mv[1],
                }; 16],
            },
        );
        self.next += 1;
        if self.next % self.mb_width == 0 {
            self.borders.save_before_filter(&self.picture, my)?;
            self.filter_row(my)?;
        }
        Ok(())
    }
    fn submit_inner<const FULL: bool>(
        &mut self,
        address: usize,
        mb: &mut Macroblock,
        slice: Slice,
        refs: References<'_>,
    ) -> Result<()> {
        #[cfg(feature = "profile")]
        let _total = crate::profile::Timer::new(1);
        if slice.deblock_idc > 2
            || mb.qp.iter().any(|&q| !(0..=51).contains(&q))
            || matches!(mb.kind, Kind::Intra8(_)) && !mb.transform8
            || matches!(mb.kind, Kind::Intra4(_) | Kind::Intra16(_)) && mb.transform8
        {
            return Err(Error::Invalid(crate::Fault::MacroblockParameters));
        }
        let is_intra = !matches!(mb.kind, Kind::Inter { .. });
        let availability = if is_intra {
            self.intra_availability(address, slice)
        } else {
            IntraAvailability::default()
        };
        let mut meta = Metadata {
            address,
            slice,
            intra: is_intra,
            transform8: mb.transform8,
            qp: mb.qp,
            coded_luma: coded_luma(mb),
            skip_internal: !is_intra
                && mb.cbp & 15 == 0
                && matches!(&mb.kind, Kind::Inter { parts } if parts.len() == 1),
            motion: [Motion::default(); 16],
        };
        let (mx, my) = (address % self.mb_width, address / self.mb_width);
        if let Kind::Inter { parts } = &mb.kind {
            if parts.is_empty() || parts.len() > 16 {
                return Err(Error::Invalid(crate::Fault::PartitionCount));
            }
            #[cfg(feature = "profile")]
            let _inter = crate::profile::Timer::new(2);
            let chroma = if FULL { Chroma::Yuv444 } else { Chroma::Yuv420 };
            let width = self.picture.planes[0].width;
            let height = self.picture.planes[0].height;
            let chroma_size = if !FULL { 8 } else { 16 };
            let [y_plane, u_plane, v_plane] = &mut self.picture.planes;
            let mut targets = [
                MacroblockPlane::new(y_plane.block_mut(mx * 16, my * 16, 16, 16)?)?,
                MacroblockPlane::new(u_plane.block_mut(
                    mx * chroma_size,
                    my * chroma_size,
                    chroma_size,
                    chroma_size,
                )?)?,
                MacroblockPlane::new(v_plane.block_mut(
                    mx * chroma_size,
                    my * chroma_size,
                    chroma_size,
                    chroma_size,
                )?)?,
            ];
            let mut coverage = 0u16;
            for part in *parts {
                let (x, y, w, h) = (
                    part.x as usize,
                    part.y as usize,
                    part.width as usize,
                    part.height as usize,
                );
                let geometry = PartitionGeometry::new(x, y, w, h)?;
                let mask = geometry.coverage();
                if coverage & mask != 0 {
                    return Err(Error::Invalid(crate::Fault::PartitionOverlap));
                }
                coverage |= mask;
                let prediction = part.prediction;
                let index = prediction
                    .reference
                    .ok_or(Error::Invalid(crate::Fault::PartitionWithoutReference))?;
                let reference = refs
                    .pictures
                    .get(index as usize)
                    .ok_or(Error::Invalid(crate::Fault::ReferenceIndex))?;
                let source = &reference.picture;
                if source.chroma != chroma
                    || source.planes[0].width != width
                    || source.planes[0].height != height
                {
                    return Err(Error::Invalid(crate::Fault::IncompatibleReferenceLayout));
                }
                let motion = Motion {
                    reference: reference.slot,
                    x: prediction.mv[0],
                    y: prediction.mv[1],
                };
                for by in y / 4..(y + h) / 4 {
                    meta.motion[by * 4 + x / 4..by * 4 + (x + w) / 4].fill(motion);
                }
                Self::inter_partition::<FULL>(&mut targets, source, mx, my, part, &geometry)?;
            }
            if coverage != u16::MAX {
                return Err(Error::Invalid(crate::Fault::IncompleteMacroblockPartitions));
            }
        }
        #[cfg(feature = "profile")]
        let planes_timer = crate::profile::Timer::new(3);
        for plane in 0..3 {
            let size = if plane > 0 && !FULL { 8 } else { 16 };
            let (px, py) = (mx * size, my * size);
            // FFmpeg gates the whole inter residual plane on CBP, rather than
            // visiting sixteen empty transform blocks after motion prediction.
            if !is_intra
                && (if size == 16 {
                    mb.cbp & 15 == 0
                } else {
                    mb.cbp & 48 == 0
                })
            {
                continue;
            }
            let mut visited = 0u16;
            if is_intra {
                if size == 8 {
                    let mode = mb.chroma_mode;
                    let edges = self.edges(plane, px, py, 8, availability, visited)?;
                    intra::chroma8(
                        &mut self.picture.planes[plane].block_mut(px, py, 8, 8)?,
                        &edges,
                        mode,
                    )?;
                } else if let Kind::Intra16(mode) = mb.kind {
                    let edges = self.edges(plane, px, py, 16, availability, visited)?;
                    intra::luma16(
                        &mut self.picture.planes[plane].block_mut(px, py, 16, 16)?,
                        &edges,
                        mode,
                    )?;
                }
            }
            let use8 = size == 16 && mb.transform8;
            let block_size = if use8 { 8 } else { 4 };
            let blocks = size * size / (block_size * block_size);
            if mb.bypass && is_intra && (size == 8 || matches!(mb.kind, Kind::Intra16(_))) {
                let mut residual = [0i16; 256];
                for index in 0..blocks {
                    let (bx, by) = if size == 16 {
                        (
                            (index / 4 % 2) * 8 + (index % 2) * 4,
                            (index / 8) * 8 + (index % 4 / 2) * 4,
                        )
                    } else {
                        (index % 2 * 4, index / 2 * 4)
                    };
                    for y in 0..4 {
                        for x in 0..4 {
                            residual[(by + y) * size + bx + x] =
                                mb.coefficients[plane * 256 + index * 16 + x * 4 + y];
                        }
                    }
                }
                let (vertical, horizontal) = if size == 8 {
                    (mb.chroma_mode == 2, mb.chroma_mode == 1)
                } else if let Kind::Intra16(mode) = mb.kind {
                    (mode == 0, mode == 1)
                } else {
                    (false, false)
                };
                transform::bypass(
                    &mut self.picture.planes[plane].block_mut(px, py, size, size)?,
                    &mut residual,
                    size,
                    vertical,
                    horizontal,
                )?;
                mb.coefficients[plane * 256..plane * 256 + size * size].fill(0);
                continue;
            }
            if !mb.bypass && (!is_intra || size == 8 || matches!(mb.kind, Kind::Intra16(_))) {
                transform::add_plane(
                    &mut self.picture.planes[plane].block_mut(px, py, size, size)?,
                    (&mut mb.coefficients[plane * 256..(plane + 1) * 256])
                        .try_into()
                        .unwrap(),
                    (&mb.nonzero[plane * 16..(plane + 1) * 16])
                        .try_into()
                        .unwrap(),
                    use8,
                    size == 8 || matches!(mb.kind, Kind::Intra16(_)),
                )?;
                continue;
            }
            for index in 0..blocks {
                let (bx, by) = if block_size == 8 {
                    (index % 2 * 8, index / 2 * 8)
                } else if size == 16 {
                    (
                        (index / 4 % 2) * 8 + (index % 2) * 4,
                        (index / 8) * 8 + (index % 4 / 2) * 4,
                    )
                } else {
                    (index % 2 * 4, index / 2 * 4)
                };
                let x = px + bx;
                let y = py + by;
                let mode = match &mb.kind {
                    Kind::Intra4(m) => Some(m[index]),
                    Kind::Intra8(m) => Some(m[index]),
                    _ => None,
                };
                if size == 16 {
                    if let Some(mode) = mode {
                        let edges = self.edges(plane, x, y, block_size, availability, visited)?;
                        intra::nxn(
                            &mut self.picture.planes[plane]
                                .block_mut(x, y, block_size, block_size)?,
                            &edges,
                            mode,
                        )?;
                    }
                }
                let offset = plane * 256 + index * block_size * block_size;
                let coeff = &mut mb.coefficients[offset..offset + block_size * block_size];
                let mut block =
                    self.picture.planes[plane].block_mut(x, y, block_size, block_size)?;
                if mb.bypass {
                    let mut residual = [0i16; 64];
                    for yy in 0..block_size {
                        for xx in 0..block_size {
                            residual[yy * block_size + xx] = coeff[xx * block_size + yy];
                        }
                    }
                    transform::bypass(
                        &mut block,
                        &mut residual,
                        block_size,
                        mode == Some(0),
                        mode == Some(1),
                    )?;
                    coeff.fill(0);
                } else if block_size == 8 {
                    let coeff: &mut [i16; 64] = coeff.try_into().unwrap();
                    let nonzero: u16 = mb.nonzero
                        [plane * 16 + index * 4..plane * 16 + index * 4 + 4]
                        .iter()
                        .map(|&n| n as u16)
                        .sum();
                    if nonzero <= 1 && coeff[0] != 0 {
                        transform::add_dc(&mut block, &mut coeff[0])?;
                    } else if nonzero != 0 {
                        transform::add8(&mut block, coeff)?;
                    }
                } else {
                    let coeff: &mut [i16; 16] = coeff.try_into().unwrap();
                    let nnz = mb.nonzero[plane * 16 + index];
                    // Intra16 and subsampled chroma count AC separately from
                    // their independently transformed DC. One AC plus DC must
                    // take the full transform, as FFmpeg add16intra/add8 do.
                    let separate_dc = size == 8 || matches!(mb.kind, Kind::Intra16(_));
                    if nnz == 0 || (!separate_dc && nnz == 1 && coeff[0] != 0) {
                        if coeff[0] != 0 {
                            transform::add_dc(&mut block, &mut coeff[0])?;
                        }
                    } else {
                        transform::add4(&mut block, coeff)?;
                    }
                }
                for yy in by / 4..(by + block_size) / 4 {
                    for xx in bx / 4..(bx + block_size) / 4 {
                        visited |= 1 << (yy * 4 + xx);
                    }
                }
            }
        }
        #[cfg(feature = "profile")]
        drop(planes_timer);
        self.put_metadata(address, meta);
        self.next += 1;
        // All samples in this row are reconstructed. Save originals before
        // filtering so next-row intra consumes unfiltered top neighbours.
        if self.next % self.mb_width == 0 {
            self.borders.save_before_filter(&self.picture, my)?;
            self.filter_row(my)?;
        }
        Ok(())
    }
    #[inline]
    fn inter_partition<const FULL: bool>(
        targets: &mut [MacroblockPlane<'_>; 3],
        source: &Picture,
        mx: usize,
        my: usize,
        p: &Partition,
        geometry: &PartitionGeometry,
    ) -> Result<()> {
        let (px, py, _, _) = geometry.coordinates();
        for (plane, target_plane) in targets.iter_mut().enumerate() {
            let sub = if plane > 0 && !FULL { 2 } else { 1 };
            let (x, y) = ((mx * 16 + px) / sub, (my * 16 + py) / sub);
            let mut target = target_plane.partition(geometry);
            let mv = p.prediction.mv;
            if sub == 1 {
                if let Some(w) = p.weights[plane] {
                    motion::weighted_luma(
                        &source.planes[plane],
                        (x * 4) as i32 + mv[0] as i32,
                        (y * 4) as i32 + mv[1] as i32,
                        &mut target,
                        w.denominator,
                        w.weight,
                        w.offset,
                    )?;
                    continue;
                }
            }
            if sub == 2 {
                motion::chroma(
                    &source.planes[plane],
                    (x * 8) as i32 + mv[0] as i32,
                    (y * 8) as i32 + mv[1] as i32,
                    &mut target,
                )?;
            } else {
                motion::luma(
                    &source.planes[plane],
                    (x * 4) as i32 + mv[0] as i32,
                    (y * 4) as i32 + mv[1] as i32,
                    &mut target,
                )?;
            }
            if let Some(weights) = p.weights[plane] {
                motion::weight(
                    &mut target,
                    weights.denominator,
                    weights.weight,
                    weights.offset,
                )?;
            }
        }
        Ok(())
    }
    fn intra_availability(&self, address: usize, slice: Slice) -> IntraAvailability {
        let (x, y) = (address % self.mb_width, address / self.mb_width);
        let available = |at| {
            self.metadata_at(at)
                .is_some_and(|m| m.slice.id == slice.id && (!slice.constrained_intra || m.intra))
        };
        IntraAvailability {
            left: x > 0 && available(address - 1),
            top: y > 0 && available(address - self.mb_width),
            top_left: x > 0 && y > 0 && available(address - self.mb_width - 1),
            top_right: x + 1 < self.mb_width && y > 0 && available(address - self.mb_width + 1),
        }
    }
    fn edges(
        &self,
        plane: usize,
        x: usize,
        y: usize,
        n: usize,
        availability: IntraAvailability,
        visited: u16,
    ) -> Result<Edges> {
        if plane > 0 && self.picture.chroma == Chroma::Yuv420 {
            self.edges_sized::<8>(plane, x, y, n, availability, visited)
        } else {
            self.edges_sized::<16>(plane, x, y, n, availability, visited)
        }
    }
    fn edges_sized<const SIZE: usize>(
        &self,
        plane: usize,
        x: usize,
        y: usize,
        n: usize,
        availability: IntraAvailability,
        visited: u16,
    ) -> Result<Edges> {
        let size = SIZE;
        let p = &self.picture.planes[plane];
        // Macroblock neighbour identity/slice constraints are resolved once,
        // shared by all planes. Interior queries use only the local 4x4 mask.
        let origin_x = x - x % size;
        let origin_y = y - y % size;
        let available = |xx: i32, yy: i32| {
            availability.available::<SIZE>(xx - origin_x as i32, yy - origin_y as i32, visited)
        };
        let read = |xx: usize, yy: usize| {
            if y % size == 0 && yy + 1 == y && self.borders.row == Some(y / size - 1) {
                self.borders.top(plane)[xx]
            } else {
                p.row(yy)[xx]
            }
        };
        let has_top = available(x as i32, y as i32 - 1);
        let has_left = available(x as i32 - 1, y as i32);
        let mut top = [0; 32];
        let mut left = [0; 16];
        if has_top {
            // Availability changes only at a 4x4 boundary. Load each complete
            // neighbour run once, as the FFmpeg top-right availability masks
            // do, instead of revisiting metadata for every individual pixel.
            let row = if y % size == 0 && self.borders.row == Some(y / size - 1) {
                self.borders.top(plane)
            } else {
                p.row(y - 1)
            };
            for (i, run) in top[..2 * n].chunks_exact_mut(4).enumerate() {
                let xx = x + i * 4;
                if available(xx as i32, y as i32 - 1) {
                    run.copy_from_slice(&row[xx..xx + 4]);
                } else {
                    run.fill(row[x + n - 1]);
                }
            }
        }
        if has_left {
            for (i, v) in left.iter_mut().enumerate().take(n) {
                *v = read(x - 1, y + i);
            }
        }
        let corner = available(x as i32 - 1, y as i32 - 1).then(|| read(x - 1, y - 1));
        Edges::new(
            n,
            if has_top { &top[..2 * n] } else { &[] },
            if has_left { &left[..n] } else { &[] },
            corner,
        )
    }
    fn filter_row(&mut self, row: usize) -> Result<()> {
        match self.picture.chroma {
            Chroma::Yuv444 => self.filter_row_sized::<true>(row),
            Chroma::Yuv420 => self.filter_row_sized::<false>(row),
        }
    }
    fn filter_row_sized<const FULL: bool>(&mut self, row: usize) -> Result<()> {
        #[cfg(feature = "profile")]
        let _filter = crate::profile::Timer::new(4);
        let metadata = &self.metadata;
        let base = self.base;
        let at = |address: usize| {
            let slot = if address >= base {
                address - base
            } else {
                address + metadata.len() - base
            };
            metadata
                .get(slot)
                .and_then(Option::as_ref)
                .filter(|m| m.address == address)
        };
        for mx in 0..self.mb_width {
            let address = row * self.mb_width + mx;
            let qmeta = at(address).unwrap();
            if qmeta.slice.deblock_idc == 1 {
                continue;
            }
            // FFmpeg fill_filter_caches excludes macroblocks whose internal
            // and external QPs cannot activate either threshold. Use actual
            // per-plane QPs, including differing chroma offsets and neighbours.
            let left = (mx != 0).then(|| at(address - 1).unwrap());
            let top = (row != 0).then(|| at(address - self.mb_width).unwrap());
            let inactive = |qp: i8| {
                qp as i16 + (qmeta.slice.alpha_offset as i16) < 16
                    || qp as i16 + (qmeta.slice.beta_offset as i16) < 16
            };
            let offsets_valid = (-12..=12).contains(&qmeta.slice.alpha_offset)
                && (-12..=12).contains(&qmeta.slice.beta_offset);
            if offsets_valid
                && (0..3).all(|plane| {
                    let qp = qmeta.qp[plane];
                    inactive(qp)
                        && [left, top]
                            .into_iter()
                            .flatten()
                            .all(|p| inactive(((qp as i16 + p.qp[plane] as i16 + 1) >> 1) as i8))
                })
            {
                continue;
            }
            for vertical in [true, false] {
                for edge in 0..4 {
                    if edge != 0 && qmeta.skip_internal {
                        continue;
                    }
                    if edge == 0 && (if vertical { mx == 0 } else { row == 0 }) {
                        continue;
                    }
                    let pmeta = if edge == 0 {
                        if vertical {
                            left.unwrap()
                        } else {
                            top.unwrap()
                        }
                    } else {
                        qmeta
                    };
                    if qmeta.slice.deblock_idc == 2 && pmeta.slice.id != qmeta.slice.id {
                        continue;
                    }
                    if edge % 2 != 0 && qmeta.transform8 {
                        continue;
                    }
                    let mut strengths = [0; 4];
                    for (segment, s) in strengths.iter_mut().enumerate() {
                        let q = if vertical {
                            segment * 4 + edge
                        } else {
                            edge * 4 + segment
                        };
                        let p = if vertical {
                            segment * 4 + if edge == 0 { 3 } else { edge - 1 }
                        } else {
                            if edge == 0 {
                                12 + segment
                            } else {
                                (edge - 1) * 4 + segment
                            }
                        };
                        *s = if pmeta.intra || qmeta.intra {
                            if edge == 0 { 4 } else { 3 }
                        } else {
                            filter::inter_strength(
                                pmeta.motion[p],
                                qmeta.motion[q],
                                pmeta.coded_luma & (1 << p) != 0
                                    || qmeta.coded_luma & (1 << q) != 0,
                                4,
                            )
                        };
                    }
                    if strengths == [0; 4] {
                        continue;
                    }
                    for plane in 0..3 {
                        let sub = if plane > 0 && !FULL { 2 } else { 1 };
                        if sub == 2 && edge % 2 != 0 {
                            continue;
                        }
                        let qp = (pmeta.qp[plane] as i32 + qmeta.qp[plane] as i32 + 1) >> 1;
                        let origin = Edge {
                            x: mx * 16 / sub + if vertical { edge * 4 / sub } else { 0 },
                            y: row * 16 / sub + if vertical { 0 } else { edge * 4 / sub },
                            vertical,
                            segment_len: 4 / sub,
                        };
                        self.picture.planes[plane].filter(
                            origin,
                            Filter {
                                strength: strengths,
                                qp,
                                alpha_offset: qmeta.slice.alpha_offset,
                                beta_offset: qmeta.slice.beta_offset,
                                subsampled_chroma: sub == 2,
                            },
                        )?;
                    }
                }
            }
        }
        Ok(())
    }
    pub fn finish(self) -> Result<Arc<Picture>> {
        if self.failed || self.next != self.total {
            return Err(Error::Invalid(crate::Fault::IncompleteOrFailedPicture));
        }
        Ok(self.picture.finish())
    }
}
