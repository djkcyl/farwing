// SPDX-License-Identifier: LGPL-2.1-or-later
// FFmpeg h264_cabac.c / h264_cavlc.c intra syntax dispatch.
use crate::{
    Error, Result,
    bits::Bits,
    cabac::{Cabac, Contexts},
    cavlc, residual,
};

pub(crate) enum Entropy<'a> {
    Cavlc {
        bits: Bits<'a>,
        end: usize,
        skip_remaining: usize,
        run_read: bool,
    },
    Cabac {
        decoder: Cabac,
        contexts: Box<Contexts>,
        finished: bool,
        last_delta: bool,
    },
}
impl<'a> Entropy<'a> {
    pub fn new(
        rbsp: &'a [u8],
        cursor: (usize, u8),
        qp: u8,
        cabac: bool,
        init_idc: Option<u8>,
    ) -> Result<Self> {
        let mut bits = Bits::at(rbsp, cursor.0 * 8 + cursor.1 as usize)?;
        if cabac {
            bits.align_ones()?;
            Ok(Self::Cabac {
                decoder: Cabac::new(&rbsp[bits.position() / 8..])?,
                contexts: Box::new(Contexts::new(qp, init_idc)?),
                finished: false,
                last_delta: false,
            })
        } else {
            let end = rbsp
                .iter()
                .rposition(|&b| b != 0)
                .map(|i| i * 8 + 7 - rbsp[i].trailing_zeros() as usize)
                .ok_or(Error::Truncated)?;
            Ok(Self::Cavlc {
                bits,
                end,
                skip_remaining: 0,
                run_read: false,
            })
        }
    }
    #[inline]
    pub fn more(&self) -> bool {
        match self {
            Self::Cavlc {
                bits,
                end,
                skip_remaining,
                ..
            } => bits.position() < *end || *skip_remaining != 0,
            Self::Cabac { finished, .. } => !*finished,
        }
    }
    #[inline]
    pub fn end_mb(&mut self) -> Result<()> {
        if let Self::Cabac {
            decoder, finished, ..
        } = self
        {
            *finished = decoder.terminate();
            decoder.check()?;
        }
        Ok(())
    }
    pub fn finish(&self) -> Result<()> {
        match self {
            Self::Cavlc {
                bits,
                end,
                skip_remaining,
                ..
            } if bits.position() != *end || *skip_remaining != 0 => {
                Err(Error::Invalid(crate::Fault::SliceTrailingBits))
            }
            Self::Cabac {
                finished: false, ..
            } => Err(Error::Invalid(crate::Fault::UnterminatedSlice)),
            _ => Ok(()),
        }
    }
    pub fn skip_p(&mut self, left_not_skip: bool, top_not_skip: bool) -> Result<bool> {
        match self {
            Self::Cavlc {
                bits,
                skip_remaining,
                run_read,
                ..
            } => {
                if !*run_read {
                    *skip_remaining = bits.ue()? as usize;
                    *run_read = true;
                }
                if *skip_remaining != 0 {
                    *skip_remaining -= 1;
                    Ok(true)
                } else {
                    *run_read = false;
                    Ok(false)
                }
            }
            Self::Cabac {
                decoder, contexts, ..
            } => Ok(decoder.decision(
                &mut contexts.0[11 + usize::from(left_not_skip) + usize::from(top_not_skip)],
            ) != 0),
        }
    }
    /// Returns (intra, raw type within the I or P table).
    #[inline]
    pub fn p_type(&mut self) -> Result<(bool, u32)> {
        match self {
            Self::Cavlc { bits, .. } => {
                let raw = bits.ue()?;
                Ok(if raw >= 5 {
                    (true, raw - 5)
                } else {
                    (false, raw)
                })
            }
            Self::Cabac {
                decoder: c,
                contexts: s,
                ..
            } => {
                if c.decision(&mut s.0[14]) == 0 {
                    return Ok((
                        false,
                        if c.decision(&mut s.0[15]) == 0 {
                            3 * c.decision(&mut s.0[16]) as u32
                        } else {
                            2 - c.decision(&mut s.0[17]) as u32
                        },
                    ));
                }
                if c.decision(&mut s.0[17]) == 0 {
                    return Ok((true, 0));
                }
                if c.terminate() {
                    return Ok((true, 25));
                }
                let mut kind = 1 + 12 * c.decision(&mut s.0[18]) as u32;
                if c.decision(&mut s.0[19]) != 0 {
                    kind += 4 + 4 * c.decision(&mut s.0[19]) as u32;
                }
                kind += 2 * c.decision(&mut s.0[20]) as u32 + c.decision(&mut s.0[20]) as u32;
                Ok((true, kind))
            }
        }
    }
    pub fn p_subtype(&mut self) -> Result<u8> {
        match self {
            Self::Cavlc { bits, .. } => {
                let v = bits.ue()?;
                if v > 3 {
                    Err(Error::Invalid(crate::Fault::PSubpartitionType))
                } else {
                    Ok(v as u8)
                }
            }
            Self::Cabac {
                decoder: c,
                contexts: s,
                ..
            } => {
                if c.decision(&mut s.0[21]) != 0 {
                    Ok(0)
                } else if c.decision(&mut s.0[22]) == 0 {
                    Ok(1)
                } else if c.decision(&mut s.0[23]) != 0 {
                    Ok(2)
                } else {
                    Ok(3)
                }
            }
        }
    }
    #[inline]
    pub fn reference(&mut self, count: u32, left: i8, top: i8) -> Result<u8> {
        if count == 0 || count > 32 {
            return Err(Error::Invalid(crate::Fault::ActiveReferenceCount));
        }
        if count == 1 {
            return Ok(0);
        }
        let value = match self {
            Self::Cavlc { bits, .. } => {
                if count == 2 {
                    1 - bits.read(1)?
                } else {
                    bits.ue()?
                }
            }
            Self::Cabac {
                decoder: c,
                contexts: s,
                ..
            } => {
                let mut value = 0;
                let mut context = usize::from(left > 0) + 2 * usize::from(top > 0);
                while c.decision(&mut s.0[54 + context]) != 0 {
                    value += 1;
                    if value >= count {
                        return Err(Error::Invalid(crate::Fault::ReferenceIndex));
                    }
                    context = (context >> 2) + 4;
                }
                value
            }
        };
        if value >= count {
            Err(Error::Invalid(crate::Fault::ReferenceIndex))
        } else {
            Ok(value as u8)
        }
    }
    #[inline]
    pub fn mvd(&mut self, axis: usize, neighbour_abs: i32) -> Result<i32> {
        match self {
            Self::Cavlc { bits, .. } => bits.se(),
            Self::Cabac {
                decoder: c,
                contexts: s,
                ..
            } => {
                let base = if axis == 0 { 40 } else { 47 };
                if c.decision(
                    &mut s.0
                        [base + usize::from(neighbour_abs > 2) + usize::from(neighbour_abs > 32)],
                ) == 0
                {
                    return Ok(0);
                }
                let mut value = 1i32;
                let mut index = base + 3;
                while value < 9 && c.decision(&mut s.0[index]) != 0 {
                    if value < 4 {
                        index += 1;
                    }
                    value += 1;
                }
                if value == 9 {
                    let mut k = 3;
                    while c.bypass() != 0 {
                        value += 1 << k;
                        k += 1;
                        if k > 24 {
                            return Err(Error::Invalid(crate::Fault::MvdPrefix));
                        }
                    }
                    for bit in (0..k).rev() {
                        value += (c.bypass() as i32) << bit;
                    }
                }
                Ok(if c.bypass() != 0 { -value } else { value })
            }
        }
    }
    pub fn mb_type(&mut self, left_i16: bool, top_i16: bool) -> Result<u32> {
        match self {
            Self::Cavlc { bits, .. } => bits.ue(),
            Self::Cabac {
                decoder: c,
                contexts: s,
                ..
            } => {
                if c.decision(&mut s.0[3 + usize::from(left_i16) + usize::from(top_i16)]) == 0 {
                    return Ok(0);
                }
                if c.terminate() {
                    return Ok(25);
                }
                let mut kind = 1 + 12 * c.decision(&mut s.0[6]) as u32;
                if c.decision(&mut s.0[7]) != 0 {
                    kind += 4 + 4 * c.decision(&mut s.0[8]) as u32;
                }
                kind += 2 * c.decision(&mut s.0[9]) as u32 + c.decision(&mut s.0[10]) as u32;
                Ok(kind)
            }
        }
    }
    #[inline]
    pub fn transform8(&mut self, left: bool, top: bool) -> Result<bool> {
        match self {
            Self::Cavlc { bits, .. } => Ok(bits.read(1)? != 0),
            Self::Cabac {
                decoder, contexts, ..
            } => Ok(
                decoder.decision(&mut contexts.0[399 + usize::from(left) + usize::from(top)]) != 0,
            ),
        }
    }
    #[inline]
    pub fn mode(&mut self, prediction: u8) -> Result<u8> {
        let remainder = match self {
            Self::Cavlc { bits, .. } => {
                if bits.read(1)? != 0 {
                    return Ok(prediction);
                }
                bits.read(3)? as u8
            }
            Self::Cabac {
                decoder, contexts, ..
            } => {
                if decoder.decision(&mut contexts.0[68]) != 0 {
                    return Ok(prediction);
                }
                decoder.decision(&mut contexts.0[69])
                    + 2 * decoder.decision(&mut contexts.0[69])
                    + 4 * decoder.decision(&mut contexts.0[69])
            }
        };
        Ok(remainder + u8::from(remainder >= prediction))
    }
    #[inline]
    pub fn chroma_mode(&mut self, left: u8, top: u8) -> Result<u8> {
        match self {
            Self::Cavlc { bits, .. } => {
                let v = bits.ue()?;
                if v > 3 {
                    Err(Error::Invalid(crate::Fault::ChromaMode))
                } else {
                    Ok(v as u8)
                }
            }
            Self::Cabac {
                decoder: c,
                contexts: s,
                ..
            } => {
                if c.decision(&mut s.0[64 + usize::from(left != 0) + usize::from(top != 0)]) == 0 {
                    return Ok(0);
                }
                if c.decision(&mut s.0[67]) == 0 {
                    return Ok(1);
                }
                Ok(2 + c.decision(&mut s.0[67]))
            }
        }
    }
    #[inline]
    pub fn cbp(&mut self, left: u8, top: u8, chroma: bool, intra: bool) -> Result<u8> {
        if let Self::Cavlc { bits, .. } = self {
            return Ok(*match (chroma, intra) {
                (true, true) => crate::stream::CBP420.get(bits.ue()? as usize),
                (false, true) => crate::stream::CBP444.get(bits.ue()? as usize),
                (true, false) => [
                    0, 16, 1, 2, 4, 8, 32, 3, 5, 10, 12, 15, 47, 7, 11, 13, 14, 6, 9, 31, 35, 37,
                    42, 44, 33, 34, 36, 40, 39, 43, 45, 46, 17, 18, 20, 24, 19, 21, 26, 28, 23, 27,
                    29, 30, 22, 25, 38, 41,
                ]
                .get(bits.ue()? as usize),
                (false, false) => {
                    [0, 1, 2, 4, 8, 3, 5, 10, 12, 15, 7, 11, 13, 14, 6, 9].get(bits.ue()? as usize)
                }
            }
            .ok_or(Error::Invalid(crate::Fault::CbpCode))?);
        }
        let Self::Cabac {
            decoder: c,
            contexts: s,
            ..
        } = self
        else {
            unreachable!()
        };
        let mut cbp =
            c.decision(&mut s.0[73 + usize::from(left & 2 == 0) + 2 * usize::from(top & 4 == 0)]);
        cbp |= c.decision(&mut s.0[73 + usize::from(cbp & 1 == 0) + 2 * usize::from(top & 8 == 0)])
            << 1;
        cbp |= c
            .decision(&mut s.0[73 + usize::from(left & 8 == 0) + 2 * usize::from(cbp & 1 == 0)])
            << 2;
        cbp |= c.decision(&mut s.0[73 + usize::from(cbp & 4 == 0) + 2 * usize::from(cbp & 2 == 0)])
            << 3;
        if chroma {
            let (a, b) = (left >> 4, top >> 4);
            if c.decision(&mut s.0[77 + usize::from(a != 0) + 2 * usize::from(b != 0)]) != 0 {
                cbp |= (1 + c
                    .decision(&mut s.0[81 + usize::from(a == 2) + 2 * usize::from(b == 2)]))
                    << 4;
            }
        }
        Ok(cbp)
    }
    #[inline]
    pub fn delta(&mut self) -> Result<i32> {
        match self {
            Self::Cavlc { bits, .. } => bits.se(),
            Self::Cabac {
                decoder: c,
                contexts: s,
                last_delta,
                ..
            } => {
                if c.decision(&mut s.0[60 + usize::from(*last_delta)]) == 0 {
                    *last_delta = false;
                    return Ok(0);
                }
                let mut v = 1;
                let mut ctx = 62;
                while c.decision(&mut s.0[ctx]) != 0 {
                    ctx = 63;
                    v += 1;
                    if v > 102 {
                        return Err(Error::Invalid(crate::Fault::CabacQpDelta));
                    }
                }
                *last_delta = true;
                Ok(if v & 1 != 0 {
                    (v + 1) >> 1
                } else {
                    -((v + 1) >> 1)
                })
            }
        }
    }
    #[inline]
    pub fn no_delta(&mut self) {
        if let Self::Cabac { last_delta, .. } = self {
            *last_delta = false;
        }
    }
    pub fn pcm(&mut self, count: usize) -> Result<Vec<u8>> {
        match self {
            Self::Cavlc { bits, .. } => {
                while bits.position() % 8 != 0 {
                    if bits.read(1)? != 0 {
                        return Err(Error::Invalid(crate::Fault::PcmAlignment));
                    }
                }
                let mut out = vec![0; count];
                for v in &mut out {
                    *v = bits.read(8)? as u8;
                }
                Ok(out)
            }
            Self::Cabac {
                decoder,
                last_delta,
                ..
            } => {
                *last_delta = false;
                decoder.pcm_and_restart(count)
            }
        }
    }
    #[allow(clippy::too_many_arguments)]
    #[inline]
    pub fn residual(
        &mut self,
        nc: i32,
        category: usize,
        cbf: Option<(bool, bool)>,
        scan: &crate::scan::Scan,
        qmul: Option<&[i32]>,
        dst: &mut [i16],
    ) -> Result<u8> {
        match self {
            Self::Cavlc { bits, .. } => cavlc::residual(bits, nc, scan, qmul, dst),
            Self::Cabac {
                decoder, contexts, ..
            } => residual::cabac(decoder, contexts, category, cbf, scan, qmul, dst),
        }
    }
}
