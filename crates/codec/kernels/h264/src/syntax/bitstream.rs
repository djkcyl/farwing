// SPDX-License-Identifier: MIT
// Derived from oxideav-h264 0.1.8, Copyright (c) 2026 Karpelès Lab Inc.
// Retained syntax/metadata primitives used by this client; see COPYING.OxideAV in this crate.

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum BitError {
    #[error("attempted to read past end of bitstream")]
    Eof,
    #[error("Exp-Golomb leading-zero run exceeded 31 bits")]
    ExpGolombOverflow,
    #[error("u(n)/i(n) requested {0} bits, max supported is 32")]
    TooManyBits(u32),
    #[error("malformed rbsp_trailing_bits(): expected stop_one_bit then zeros")]
    BadTrailingBits,
}

pub type BitResult<T> = Result<T, BitError>;

pub struct BitReader<'a> {
    data: &'a [u8],
    byte_pos: usize,
    bit_pos: u8,
}

impl<'a> BitReader<'a> {
    pub fn new(data: &'a [u8]) -> Self {
        Self {
            data,
            byte_pos: 0,
            bit_pos: 0,
        }
    }

    pub fn position(&self) -> (usize, u8) {
        (self.byte_pos, self.bit_pos)
    }

    pub fn bits_remaining(&self) -> usize {
        if self.byte_pos >= self.data.len() {
            0
        } else {
            (self.data.len() - self.byte_pos) * 8 - self.bit_pos as usize
        }
    }

    pub fn byte_aligned(&self) -> bool {
        self.bit_pos == 0
    }

    pub fn u(&mut self, bits: u32) -> BitResult<u32> {
        if bits > 32 {
            return Err(BitError::TooManyBits(bits));
        }
        if self.bits_remaining() < bits as usize {
            return Err(BitError::Eof);
        }
        let mut value: u32 = 0;
        for _ in 0..bits {
            value = (value << 1) | self.read_bit_unchecked() as u32;
        }
        Ok(value)
    }

    pub fn i(&mut self, bits: u32) -> BitResult<i32> {
        let raw = self.u(bits)?;
        if bits == 0 {
            return Ok(0);
        }
        let sign_bit = 1u32 << (bits - 1);
        Ok(if bits == 32 {
            raw as i32
        } else if raw & sign_bit != 0 {
            (raw | !((1u32 << bits) - 1)) as i32
        } else {
            raw as i32
        })
    }

    pub fn f(&mut self, bits: u32) -> BitResult<u32> {
        self.u(bits)
    }

    pub fn ue(&mut self) -> BitResult<u32> {
        self.read_codenum()
    }

    pub fn se(&mut self) -> BitResult<i32> {
        let k = self.read_codenum()?;
        Ok(if k & 1 == 1 {
            k.div_ceil(2) as i32
        } else {
            -((k / 2) as i32)
        })
    }

    pub fn te(&mut self, x_max: u32) -> BitResult<u32> {
        if x_max == 0 {
            Ok(0)
        } else if x_max == 1 {
            Ok(if self.u(1)? == 0 { 1 } else { 0 })
        } else {
            self.ue()
        }
    }

    pub fn more_rbsp_data(&self) -> bool {
        if self.bits_remaining() == 0 {
            return false;
        }
        let mut last_one: Option<(usize, u8)> = None;
        for byte_idx in 0..self.data.len() {
            let b = self.data[byte_idx];
            if b == 0 {
                continue;
            }
            for bit_idx in 0..8u8 {
                if (b >> (7 - bit_idx)) & 1 == 1 {
                    last_one = Some((byte_idx, bit_idx));
                }
            }
        }
        match last_one {
            None => false,
            Some((b, bi)) => match self.byte_pos.cmp(&b) {
                core::cmp::Ordering::Less => true,
                core::cmp::Ordering::Equal => self.bit_pos < bi,
                core::cmp::Ordering::Greater => false,
            },
        }
    }

    pub fn rbsp_trailing_bits(&mut self) -> BitResult<()> {
        if self.u(1)? != 1 {
            return Err(BitError::BadTrailingBits);
        }
        while !self.byte_aligned() {
            if self.u(1)? != 0 {
                return Err(BitError::BadTrailingBits);
            }
        }
        Ok(())
    }

    fn read_codenum(&mut self) -> BitResult<u32> {
        let mut leading_zeros: u32 = 0;
        loop {
            if self.bits_remaining() == 0 {
                return Err(BitError::Eof);
            }
            if self.read_bit_unchecked() == 1 {
                break;
            }
            leading_zeros += 1;
            if leading_zeros > 31 {
                return Err(BitError::ExpGolombOverflow);
            }
        }
        let suffix = if leading_zeros == 0 {
            0
        } else {
            self.u(leading_zeros)?
        };
        Ok((1u32 << leading_zeros) - 1 + suffix)
    }

    fn read_bit_unchecked(&mut self) -> u8 {
        let bit = (self.data[self.byte_pos] >> (7 - self.bit_pos)) & 1;
        self.bit_pos += 1;
        if self.bit_pos == 8 {
            self.bit_pos = 0;
            self.byte_pos += 1;
        }
        bit
    }
}
