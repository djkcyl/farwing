// SPDX-License-Identifier: MIT
// Derived from oxideav-h265 0.0.10, Copyright (c) 2026 Karpelès Lab Inc.
// Retained syntax/metadata primitives used by this client; see COPYING.OxideAV in this crate.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BitReaderError {
    EndOfBuffer,
    TooManyBits,
    ExpGolombOverflow,
}

impl core::fmt::Display for BitReaderError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::EndOfBuffer => f.write_str("read past end of RBSP buffer"),
            Self::TooManyBits => f.write_str("u(n) called with n > 32"),
            Self::ExpGolombOverflow => f.write_str("ue(v) leading-zero run exceeded 32"),
        }
    }
}

impl std::error::Error for BitReaderError {}

#[derive(Debug, Clone)]
pub struct BitReader<'a> {
    buf: &'a [u8],
    bit_pos: usize,
}

impl<'a> BitReader<'a> {
    pub fn new(buf: &'a [u8]) -> Self {
        Self { buf, bit_pos: 0 }
    }

    pub fn bits_left(&self) -> usize {
        self.buf
            .len()
            .saturating_mul(8)
            .saturating_sub(self.bit_pos)
    }

    pub fn u1(&mut self) -> Result<u8, BitReaderError> {
        if self.bit_pos >= self.buf.len() * 8 {
            return Err(BitReaderError::EndOfBuffer);
        }
        let byte = self.buf[self.bit_pos / 8];
        let bit = 7 - (self.bit_pos % 8);
        self.bit_pos += 1;
        Ok((byte >> bit) & 0x01)
    }

    pub fn u(&mut self, n: u8) -> Result<u32, BitReaderError> {
        if n > 32 {
            return Err(BitReaderError::TooManyBits);
        }
        if n == 0 {
            return Ok(0);
        }
        if self.bits_left() < n as usize {
            return Err(BitReaderError::EndOfBuffer);
        }
        let mut value: u32 = 0;
        for _ in 0..n {
            value = (value << 1) | (self.u1()? as u32);
        }
        Ok(value)
    }

    pub fn skip(&mut self, n: usize) -> Result<(), BitReaderError> {
        if self.bits_left() < n {
            return Err(BitReaderError::EndOfBuffer);
        }
        self.bit_pos += n;
        Ok(())
    }

    pub fn ue(&mut self) -> Result<u32, BitReaderError> {
        let mut leading_zero_bits: u32 = 0;
        loop {
            if leading_zero_bits > 32 {
                return Err(BitReaderError::ExpGolombOverflow);
            }
            let b = self.u1()?;
            if b == 1 {
                break;
            }
            leading_zero_bits += 1;
        }
        if leading_zero_bits == 0 {
            return Ok(0);
        }
        if leading_zero_bits == 32 {
            // 2^32 would overflow u32; the suffix must therefore be 0
            // to keep `codeNum` representable. Treat any other input
            // as an overflow.
            let suffix = self.u(32)?;
            return suffix
                .checked_sub(1)
                .and_then(|v| (1u64 << 32).checked_add(v as u64))
                .and_then(|big| u32::try_from(big).ok())
                .ok_or(BitReaderError::ExpGolombOverflow);
        }
        let suffix = self.u(leading_zero_bits as u8)?;
        Ok((1u32 << leading_zero_bits) - 1 + suffix)
    }

    pub fn se(&mut self) -> Result<i32, BitReaderError> {
        let code_num = self.ue()?;
        // Ceil(codeNum / 2) without overflow: (codeNum + 1) / 2.
        let magnitude = ((code_num as i64) + 1) / 2;
        let value = if code_num % 2 == 0 {
            // Even codeNum (0, 2, 4, …) maps to a non-positive value
            // (0, -1, -2, …).
            -magnitude
        } else {
            // Odd codeNum (1, 3, 5, …) maps to a positive value.
            magnitude
        };
        // `value` is bounded by ±2^31 because `ue()` caps `codeNum`
        // at `2^32 - 2`, whose mapped magnitude is `2^31 - 1`.
        i32::try_from(value).map_err(|_| BitReaderError::ExpGolombOverflow)
    }

    pub fn bit_pos(&self) -> usize {
        self.bit_pos
    }
}
