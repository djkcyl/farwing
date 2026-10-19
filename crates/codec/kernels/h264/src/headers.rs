// SPDX-License-Identifier: LGPL-2.1-or-later
//! Bounded, transactional parameter registry for the verified AVC base stream.
//! Reuses syntax parsers, without the upstream decoder's MVC/SVC state machine.
use crate::syntax::{nal::parse_nal_unit, pps::Pps, slice_header::SliceHeader, sps::Sps};
use crate::{Error, Result, bits::Bits};
use std::{borrow::Cow, sync::Arc};
#[derive(Clone)]
pub struct Headers {
    sps: [Option<Arc<Sps>>; 32],
    pps: [Option<Arc<Pps>>; 256],
}
pub struct Slice<'a> {
    pub header: SliceHeader,
    pub sps: Sps,
    pub pps: Pps,
    pub rbsp: Cow<'a, [u8]>,
    pub slice_data_cursor: (usize, u8),
    pub nal_unit_type: u8,
    pub nal_ref_idc: u8,
}
impl Headers {
    pub fn new() -> Self {
        Self {
            sps: std::array::from_fn(|_| None),
            pps: std::array::from_fn(|_| None),
        }
    }
    pub fn process<'a>(&mut self, nal: &'a [u8]) -> Result<Option<Slice<'a>>> {
        let byte = *nal.first().ok_or(Error::Truncated)?;
        let kind = byte & 31;
        if byte & 128 != 0 || kind == 0 || kind >= 24 {
            return Err(Error::Invalid(crate::Fault::NalHeader));
        }
        if matches!(kind,2..=4|19..=21) {
            return Err(Error::Unsupported(crate::Fault::PartitionedExtendedAvc));
        }
        if !matches!(kind, 1 | 5 | 7 | 8) {
            return Ok(None);
        }
        if matches!(kind, 7 | 8) && nal.len() > 65536 {
            return Err(Error::Invalid(crate::Fault::ParameterNalTooLarge));
        }
        let unit = parse_nal_unit(nal).map_err(|_| Error::Invalid(crate::Fault::Rbsp))?;
        match kind {
            7 => {
                let parsed =
                    Sps::parse(&unit.rbsp).map_err(|_| Error::Invalid(crate::Fault::Sps))?;
                let id = parsed.seq_parameter_set_id as usize;
                let slot = self
                    .sps
                    .get_mut(id)
                    .ok_or(Error::Invalid(crate::Fault::SpsId))?;
                *slot = Some(Arc::new(parsed));
                Ok(None)
            }
            8 => {
                let mut bits = Bits::new(&unit.rbsp);
                let id = bits.ue()? as usize;
                let sps_id = bits.ue()? as usize;
                let sps = self
                    .sps
                    .get(sps_id)
                    .and_then(Option::as_ref)
                    .ok_or(Error::NeedKeyframe)?;
                let parsed = Pps::parse_with_chroma_format(&unit.rbsp, sps.chroma_format_idc)
                    .map_err(|_| Error::Invalid(crate::Fault::Pps))?;
                let slot = self
                    .pps
                    .get_mut(id)
                    .ok_or(Error::Invalid(crate::Fault::PpsId))?;
                *slot = Some(Arc::new(parsed));
                Ok(None)
            }
            1 | 5 => {
                let mut bits = Bits::new(&unit.rbsp);
                bits.ue()?;
                bits.ue()?;
                let id = bits.ue()? as usize;
                let pps = self
                    .pps
                    .get(id)
                    .and_then(Option::as_ref)
                    .ok_or(Error::NeedKeyframe)?;
                let sps = self
                    .sps
                    .get(pps.seq_parameter_set_id as usize)
                    .and_then(Option::as_ref)
                    .ok_or(Error::NeedKeyframe)?;
                let (header, slice_data_cursor) =
                    SliceHeader::parse_and_tell(&unit.rbsp, sps, pps, &unit.header)
                        .map_err(|_| Error::Invalid(crate::Fault::SliceHeader))?;
                Ok(Some(Slice {
                    header,
                    sps: (**sps).clone(),
                    pps: (**pps).clone(),
                    rbsp: unit.rbsp,
                    slice_data_cursor,
                    nal_unit_type: kind,
                    nal_ref_idc: unit.header.nal_ref_idc,
                }))
            }
            _ => unreachable!(),
        }
    }
}
