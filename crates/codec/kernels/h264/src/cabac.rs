// SPDX-License-Identifier: LGPL-2.1-or-later
// FFmpeg 8.0.3 cabac.c / cabac_functions.h, Copyright (c) 2003 Michael Niedermayer.
// Rust port: 16-bit refill arithmetic, packed contexts, bounded owned padding.
use crate::{Error, Result, tables};
const MASK: u32 = 65535;
#[cfg(all(target_arch = "x86_64", not(feature = "portable-cabac")))]
#[allow(unsafe_code)]
mod x86;

#[derive(Clone, Copy)]
pub struct Context(u8);
impl Context {
    pub fn raw(self) -> u8 {
        self.0
    }
}
pub struct Contexts(pub [Context; 1024]);
impl Contexts {
    pub fn new(qp: u8, cabac_init_idc: Option<u8>) -> Result<Self> {
        if qp > 51 || cabac_init_idc.is_some_and(|v| v > 2) {
            return Err(Error::Invalid(crate::Fault::CabacInitialization));
        }
        let table = match cabac_init_idc {
            None => &tables::INIT_I[..],
            Some(v) => &tables::INIT_PB[v as usize * 2048..(v as usize + 1) * 2048],
        };
        let mut out = [Context(0); 1024];
        for (i, state) in out.iter_mut().enumerate() {
            let pre =
                (((table[2 * i] as i32 * qp as i32) >> 4) + table[2 * i + 1] as i32).clamp(1, 126);
            *state = Context(if pre <= 63 {
                ((63 - pre) * 2) as u8
            } else {
                ((pre - 64) * 2 + 1) as u8
            });
        }
        Ok(Self(out))
    }
}

/// Owns the explicit decoder padding, unlike FFmpeg's unchecked caller contract.
/// Logical EOF is tracked independently; padding permits speculative refill,
/// not successful decoding after arbitrary truncation.
pub struct Cabac {
    bytes: Vec<u8>,
    size: usize,
    position: usize,
    low: u32,
    range: u32,
    finished: bool,
    failure: Option<Error>,
}
impl Cabac {
    pub fn new(data: &[u8]) -> Result<Self> {
        if data.len() < 2 {
            return Err(Error::Truncated);
        }
        let mut bytes = Vec::new();
        bytes
            .try_reserve_exact(data.len() + 8)
            .map_err(|_| Error::Allocation)?;
        bytes.extend_from_slice(data);
        bytes.resize(data.len() + 8, 0);
        // Use the aligned two-byte initialization. Byte alignment in Rust's
        // allocation does not affect syntax, state or the I_PCM cursor.
        let low = ((data[0] as u32) << 18) + ((data[1] as u32) << 10) + 512;
        if low > (510 << 17) {
            return Err(Error::Invalid(crate::Fault::CabacInitialOffset));
        }
        Ok(Self {
            bytes,
            size: data.len(),
            position: 2,
            low,
            range: 510,
            finished: false,
            failure: None,
        })
    }
    /// Arithmetic bins stay in registers. Bounds/terminal failures are sticky
    /// and checked at residual/macroblock boundaries before picture commit.
    #[inline]
    pub fn check(&self) -> Result<()> {
        self.failure.map_or(Ok(()), Err)
    }
    #[inline]
    fn refill(&mut self, shift: u32) {
        if shift > 7 || self.position > self.size + 2 {
            self.failure = Some(Error::Truncated);
            return;
        }
        let Some(bytes) = self.bytes.get(self.position..self.position + 2) else {
            self.failure = Some(Error::Truncated);
            return;
        };
        let word = ((bytes[0] as u32) << 9) + ((bytes[1] as u32) << 1);
        self.low = self.low.wrapping_add(word.wrapping_sub(MASK) << shift);
        self.position += 2;
        if self.position > self.size + 2 {
            self.failure = Some(Error::Truncated);
        }
    }
    #[inline(always)]
    pub fn decision(&mut self, state: &mut Context) -> u8 {
        // Refill itself is bounded and records a sticky failure. Syntax loops
        // have explicit coefficient/reference/prefix limits; their residual or
        // macroblock exit checks the failure before a picture can be committed.
        // Arithmetic invariants remain valid even after a failed refill, so
        // there is no need to reload the failure flag for every individual bin.
        #[cfg(all(target_arch = "x86_64", not(feature = "portable-cabac")))]
        {
            let bit = x86::decision(self, state);
            if self.low & MASK == 0 {
                self.refill(self.low.trailing_zeros().saturating_sub(16));
            }
            return bit;
        }
        #[cfg(not(all(target_arch = "x86_64", not(feature = "portable-cabac"))))]
        {
            // Context's private constructor and the transition table preserve 0..127.
            // The mask additionally gives the optimizer the array-index range.
            let s = (state.0 & 127) as usize;
            let lps = tables::CABAC[512 + 2 * (self.range as usize & 192) + s] as u32;
            let remaining = self.range - lps;
            let scaled = remaining << 17;
            let outcome = u32::from(self.low >= scaled);
            let mask = 0u32.wrapping_sub(outcome);
            self.low -= scaled & mask;
            self.range = remaining.wrapping_add(lps.wrapping_sub(remaining) & mask);
            let next = if outcome != 0 { 127 - s } else { 128 + s };
            state.0 = tables::CABAC[1024 + next];
            let shift = tables::CABAC[self.range as usize & 511] as u32;
            self.range <<= shift;
            self.low <<= shift;
            if self.low & MASK == 0 {
                self.refill(self.low.trailing_zeros().saturating_sub(16));
            }
            (s as u8 & 1) ^ outcome as u8
        }
    }
    #[inline]
    pub fn bypass(&mut self) -> u8 {
        self.low = self.low.wrapping_add(self.low);
        if self.low & MASK == 0 {
            self.refill(0);
        }
        let bit = u32::from(self.low >= self.range << 17);
        self.low -= (self.range << 17) & 0u32.wrapping_sub(bit);
        bit as u8
    }
    pub fn terminate(&mut self) -> bool {
        if self.failure.is_some() {
            return false;
        }
        if self.finished {
            self.failure = Some(Error::Invalid(crate::Fault::CabacAlreadyTerminated));
            return false;
        }
        self.range -= 2;
        if self.low >= self.range << 17 {
            self.finished = true;
            return true;
        }
        let shift = u32::from(self.range < 256);
        self.range <<= shift;
        self.low <<= shift;
        if self.low & MASK == 0 {
            self.refill(0);
        }
        false
    }
    /// Byte position after CABAC's outstanding lookahead, as in skip_bytes.
    /// Used to read I_PCM and reinitialize arithmetic decoding after samples.
    pub fn byte_cursor(&self) -> usize {
        self.position - usize::from(self.low & 1 != 0) - usize::from(self.low & 511 != 0)
    }
    pub fn pcm_and_restart(&mut self, count: usize) -> Result<Vec<u8>> {
        self.check()?;
        let at = self.byte_cursor();
        let end = at.checked_add(count).ok_or(Error::Truncated)?;
        if end > self.size {
            return Err(Error::Truncated);
        }
        let pcm = self.bytes[at..end].to_vec();
        let tail = &self.bytes[end..self.size];
        let next = Self::new(tail)?;
        *self = next;
        Ok(pcm)
    }
}
