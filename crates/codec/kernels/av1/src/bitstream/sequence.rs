//! Sequence metadata, parsed directly from AV1 section 5.5.
use crate::bitstream::{bits::Bits, obu::Obu, Error, Result};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Chroma {
    Monochrome,
    Yuv420,
    Yuv422,
    Yuv444,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Sequence {
    pub profile: u8,
    pub max_width: u32,
    pub max_height: u32,
    pub depth: u8,
    pub chroma: Chroma,
}

impl Sequence {
    /// Recognizes all AV1 profiles without declaring them playable. Metadata
    /// must distinguish unsupported 4:2:2, monochrome and 12-bit streams too.
    pub fn parse(obu: Obu<'_>) -> Result<Self> {
        if obu.kind != 1 || obu.temporal_id != 0 || obu.spatial_id != 0 {
            return Err(Error::Invalid);
        }
        let mut b = Bits::new(obu.payload);
        let profile = b.read(3)? as u8;
        if profile > 2 {
            return Err(Error::Invalid);
        }
        let still = b.flag()?;
        let reduced = b.flag()?;
        if reduced && !still {
            return Err(Error::Invalid);
        }
        if reduced {
            b.read(5)?;
        } else {
            let mut model_delay_bits = None;
            if b.flag()? {
                if b.read(32)? == 0 || b.read(32)? == 0 {
                    return Err(Error::Invalid);
                }
                if b.flag()? {
                    b.uvlc()?;
                }
                if b.flag()? {
                    model_delay_bits = Some(b.read(5)? + 1);
                    if b.read(32)? == 0 {
                        return Err(Error::Invalid);
                    }
                    b.read(5)?;
                    b.read(5)?;
                }
            }
            let initial_delay = b.flag()?;
            let count = b.read(5)? + 1;
            for _ in 0..count {
                b.read(12)?;
                if b.read(5)? > 7 {
                    b.flag()?;
                }
                if let Some(n) = model_delay_bits {
                    if b.flag()? {
                        b.read(n)?;
                        b.read(n)?;
                        b.flag()?;
                    }
                }
                if initial_delay && b.flag()? {
                    b.read(4)?;
                }
            }
        }
        let width_bits = b.read(4)? + 1;
        let height_bits = b.read(4)? + 1;
        let max_width = b.read(width_bits)? + 1;
        let max_height = b.read(height_bits)? + 1;
        if !reduced && b.flag()? {
            let delta = b.read(4)? + 2;
            let extra = b.read(3)? + 1;
            if delta + extra > 16 {
                return Err(Error::Invalid);
            }
        }
        b.read(3)?; // superblock size and intra filters
        if !reduced {
            b.read(4)?; // inter-intra, masked compound, warped motion, dual filter
            let order_hint = b.flag()?;
            if order_hint {
                b.read(2)?;
            }
            let screen_tools = if b.flag()? { 2 } else { b.read(1)? };
            if screen_tools > 0 && !b.flag()? {
                b.flag()?;
            }
            if order_hint {
                b.read(3)?;
            }
        }
        b.read(3)?; // super-resolution, CDEF, restoration
        let depth = if b.flag()? {
            if profile == 2 && b.flag()? {
                12
            } else {
                10
            }
        } else {
            8
        };
        let mono = profile != 1 && b.flag()?;
        let (primaries, transfer, matrix) = if b.flag()? {
            (b.read(8)?, b.read(8)?, b.read(8)?)
        } else {
            (2, 2, 2)
        };
        let chroma = if mono {
            b.flag()?; // range; monochrome has no separate_uv_delta_q bit
            Chroma::Monochrome
        } else {
            let sampling = if (primaries, transfer, matrix) == (1, 13, 0) {
                if profile != 1 && !(profile == 2 && depth == 12) {
                    return Err(Error::Invalid);
                }
                Chroma::Yuv444
            } else {
                b.flag()?; // range
                let (x, y) = match profile {
                    0 => (true, true),
                    1 => (false, false),
                    _ if depth == 12 => {
                        let x = b.flag()?;
                        (x, x && b.flag()?)
                    }
                    _ => (true, false),
                };
                if x && y {
                    if b.read(2)? == 3 {
                        return Err(Error::Invalid);
                    }
                    Chroma::Yuv420
                } else if x {
                    Chroma::Yuv422
                } else {
                    Chroma::Yuv444
                }
            };
            b.flag()?; // separate UV quantizer delta
            sampling
        };
        b.flag()?; // film grain present
        b.trailing()?;
        Ok(Self {
            profile,
            max_width,
            max_height,
            depth,
            chroma,
        })
    }
}
