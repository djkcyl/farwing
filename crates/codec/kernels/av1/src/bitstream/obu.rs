//! Sized low-overhead OBUs (AV1 sections 5.3 and 5.4).
use crate::bitstream::{Error, Result};

pub const MAX_TEMPORAL_UNIT_BYTES: usize = 64 * 1024 * 1024;
const MAX_OBUS: usize = 1024;

pub fn leb128(data: &[u8], at: &mut usize) -> Result<usize> {
    let mut value = 0u64;
    for shift in (0..56).step_by(7) {
        let byte = *data.get(*at).ok_or(Error::Truncated)?;
        *at += 1;
        value |= u64::from(byte & 127) << shift;
        if byte & 128 == 0 {
            return u32::try_from(value)
                .map(|v| v as usize)
                .map_err(|_| Error::Limit);
        }
    }
    Err(Error::Limit)
}

#[derive(Clone, Copy, Debug)]
pub struct Obu<'a> {
    pub kind: u8,
    pub temporal_id: u8,
    pub spatial_id: u8,
    pub bytes: &'a [u8],
    pub payload: &'a [u8],
}

/// Borrowed iteration: no per-frame vector or OBU copy. Stops on the first error.
pub struct Units<'a> {
    remaining: &'a [u8],
    count: usize,
}

impl<'a> Units<'a> {
    pub fn new(data: &'a [u8]) -> Result<Self> {
        if data.len() > MAX_TEMPORAL_UNIT_BYTES {
            return Err(Error::Limit);
        }
        Ok(Self {
            remaining: data,
            count: 0,
        })
    }
}

impl<'a> Iterator for Units<'a> {
    type Item = Result<Obu<'a>>;
    fn next(&mut self) -> Option<Self::Item> {
        if self.remaining.is_empty() {
            return None;
        }
        let result = (|| {
            self.count += 1;
            if self.count > MAX_OBUS {
                return Err(Error::Limit);
            }
            let bytes = self.remaining;
            let header = bytes[0];
            if header & 0x83 != 2 {
                return Err(Error::Invalid);
            }
            let mut at = 1;
            let (temporal_id, spatial_id) = if header & 4 != 0 {
                let ext = *bytes.get(at).ok_or(Error::Truncated)?;
                at += 1;
                if ext & 7 != 0 {
                    return Err(Error::Invalid);
                }
                (ext >> 5, (ext >> 3) & 3)
            } else {
                (0, 0)
            };
            let size = leb128(bytes, &mut at)?;
            let end = at.checked_add(size).ok_or(Error::Limit)?;
            let payload = bytes.get(at..end).ok_or(Error::Truncated)?;
            self.remaining = &bytes[end..];
            Ok(Obu {
                kind: (header >> 3) & 15,
                temporal_id,
                spatial_id,
                bytes: &bytes[..end],
                payload,
            })
        })();
        if result.is_err() {
            self.remaining = &[];
        }
        Some(result)
    }
}
