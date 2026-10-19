// SPDX-License-Identifier: LGPL-2.1-or-later
// FFmpeg h264_refs.c reference-list and marking design, expressed with owning
// Rust pictures. Copyright (c) 2003 Michael Niedermayer; Rust port OpenUUYC.
use crate::syntax::{
    slice_header::{MmcoOp, RefPicListModificationOp as Reorder, SliceHeader},
    sps::Sps,
};
use crate::{
    Error, Result,
    picture::{Chroma, Crop, Picture},
    reconstruct::Reference,
};
use std::sync::Arc;

#[derive(Clone)]
struct Entry {
    number: u32,
    long: Option<u32>,
    identity: u64,
    picture: Arc<Picture>,
}
#[derive(Default)]
pub(crate) struct Dpb {
    frames: Vec<Entry>,
    next_identity: u64,
    recycled: Vec<Picture>,
}
impl Dpb {
    pub fn clear(&mut self) {
        let old = std::mem::take(&mut self.frames);
        for frame in old {
            self.recycle(frame.picture);
        }
    }
    fn recycle(&mut self, picture: Arc<Picture>) {
        if self.recycled.len() < 2 {
            if let Ok(picture) = Arc::try_unwrap(picture) {
                self.recycled.push(picture);
            }
        }
    }
    pub fn picture(
        &mut self,
        width: usize,
        height: usize,
        chroma: Chroma,
        crop: Crop,
    ) -> Result<Picture> {
        Picture::validate(width, height, chroma, crop)?;
        if let Some(index) = self.recycled.iter().position(|p| {
            p.chroma == chroma && p.planes[0].width == width && p.planes[0].height == height
        }) {
            let mut picture = self.recycled.swap_remove(index);
            picture.crop = crop;
            return Ok(picture);
        }
        // Drop incompatible sizes instead of accumulating dormant frame pools.
        self.recycled.clear();
        Picture::new(width, height, chroma, crop)
    }
    pub fn p_list(&self, header: &SliceHeader, sps: &Sps) -> Result<Vec<Reference>> {
        let max = 1u32 << (sps.log2_max_frame_num_minus4 + 4);
        let pic_num = |n: u32| {
            if n > header.frame_num {
                n as i64 - max as i64
            } else {
                n as i64
            }
        };
        let mut list: Vec<_> = self.frames.iter().filter(|p| p.long.is_none()).collect();
        list.sort_by_key(|p| std::cmp::Reverse(pic_num(p.number)));
        let mut long: Vec<_> = self.frames.iter().filter(|p| p.long.is_some()).collect();
        long.sort_by_key(|p| p.long);
        list.extend(long);
        let mut prediction = header.frame_num;
        let modifications = &header.ref_pic_list_modification.modifications_l0;
        for (position, op) in modifications.iter().enumerate() {
            let candidate = match *op {
                Reorder::Subtract(delta) | Reorder::Add(delta) => {
                    let difference = delta
                        .checked_add(1)
                        .filter(|&d| d <= max)
                        .ok_or(Error::Invalid(crate::Fault::ReferenceReorderingDelta))?;
                    prediction = if matches!(op, Reorder::Subtract(_)) {
                        (prediction + max - difference) % max
                    } else {
                        (prediction + difference) % max
                    };
                    self.frames
                        .iter()
                        .find(|p| p.long.is_none() && p.number == prediction)
                }
                Reorder::LongTerm(index) => self.frames.iter().find(|p| p.long == Some(index)),
            }
            .ok_or(Error::Invalid(crate::Fault::UnavailableReorderedReference))?;
            if position > list.len() {
                return Err(Error::Invalid(crate::Fault::ReferenceListHole));
            }
            list.insert(position, candidate);
            let mut i = position + 1;
            while i < list.len() {
                if list[i].identity == candidate.identity {
                    list.remove(i);
                } else {
                    i += 1;
                }
            }
        }
        let count = header.num_ref_idx_l0_active_minus1 as usize + 1;
        if list.len() < count {
            return Err(Error::Invalid(crate::Fault::MissingActiveReference));
        }
        Ok(list[..count]
            .iter()
            .map(|p| Reference {
                slot: self
                    .frames
                    .iter()
                    .position(|f| f.identity == p.identity)
                    .unwrap() as u8,
                picture: p.picture.clone(),
            })
            .collect())
    }
    pub fn commit(
        &mut self,
        picture: Arc<Picture>,
        header: &SliceHeader,
        sps: &Sps,
        idr: bool,
        is_reference: bool,
    ) -> Result<()> {
        if !is_reference {
            return Ok(());
        }
        let max = 1u32 << (sps.log2_max_frame_num_minus4 + 4);
        let mut frames = if idr { vec![] } else { self.frames.clone() };
        let mut long = None;
        let mut number = header.frame_num;
        let marking = header.dec_ref_pic_marking.as_ref();
        if idr {
            if marking.is_some_and(|m| m.long_term_reference_flag) {
                long = Some(0);
            }
        } else if let Some(ops) = marking.and_then(|m| m.adaptive_marking.as_ref()) {
            for op in ops {
                let short = |delta: u32| -> Result<u32> {
                    let d = delta
                        .checked_add(1)
                        .filter(|&d| d <= max)
                        .ok_or(Error::Invalid(crate::Fault::MmcoDifference))?;
                    Ok((header.frame_num + max - d) % max)
                };
                match *op {
                    MmcoOp::MarkShortTermUnused(delta) => {
                        let n = short(delta)?;
                        frames.retain(|p| !(p.long.is_none() && p.number == n));
                    }
                    MmcoOp::MarkLongTermUnused(index) => frames.retain(|p| p.long != Some(index)),
                    MmcoOp::AssignLongTerm(delta, index) => {
                        if index >= 16 {
                            return Err(Error::Invalid(crate::Fault::LongReferenceIndex));
                        }
                        let n = short(delta)?;
                        frames.retain(|p| p.long != Some(index));
                        let old = frames
                            .iter_mut()
                            .find(|p| p.long.is_none() && p.number == n)
                            .ok_or(Error::Invalid(crate::Fault::MmcoMissingShortReference))?;
                        old.long = Some(index);
                    }
                    MmcoOp::SetMaxLongTermIdx(plus_one) => {
                        frames.retain(|p| p.long.is_none_or(|i| i < plus_one))
                    }
                    MmcoOp::MarkAllUnused => {
                        frames.clear();
                        number = 0;
                        long = None;
                    }
                    MmcoOp::AssignCurrentLongTerm(index) => {
                        if index >= 16 {
                            return Err(Error::Invalid(crate::Fault::LongReferenceIndex));
                        }
                        frames.retain(|p| p.long != Some(index));
                        long = Some(index);
                    }
                }
            }
        } else if frames.len() >= sps.max_num_ref_frames.max(1) as usize {
            let oldest = frames
                .iter()
                .enumerate()
                .filter(|(_, p)| p.long.is_none())
                .min_by_key(|(_, p)| {
                    if p.number > header.frame_num {
                        p.number as i64 - max as i64
                    } else {
                        p.number as i64
                    }
                })
                .map(|(i, _)| i)
                .ok_or(Error::Invalid(crate::Fault::DpbHasNoSlidingReference))?;
            frames.remove(oldest);
        }
        if frames.len() >= 16 {
            return Err(Error::Invalid(crate::Fault::DpbCapacity));
        }
        self.next_identity = self
            .next_identity
            .checked_add(1)
            .ok_or(Error::Invalid(crate::Fault::ReferenceIdentityOverflow))?;
        frames.push(Entry {
            number,
            long,
            identity: self.next_identity,
            picture,
        });
        let old = std::mem::replace(&mut self.frames, frames);
        for frame in old {
            if !self.frames.iter().any(|p| p.identity == frame.identity) {
                self.recycle(frame.picture);
            }
        }
        Ok(())
    }
}
