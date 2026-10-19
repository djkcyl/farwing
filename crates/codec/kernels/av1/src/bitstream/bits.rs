use crate::bitstream::{Error, Result};

#[derive(Clone)]
pub(crate) struct Bits<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl<'a> Bits<'a> {
    pub fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, pos: 0 }
    }

    pub fn read(&mut self, count: u32) -> Result<u32> {
        if count > 32 {
            return Err(Error::Invalid);
        }
        let end = self.pos.checked_add(count as usize).ok_or(Error::Limit)?;
        if end.div_ceil(8) > self.bytes.len() {
            return Err(Error::Truncated);
        }
        let mut left = count;
        let mut value = 0;
        while left > 0 {
            let available = 8 - (self.pos & 7) as u32;
            let take = available.min(left);
            let byte = u32::from(self.bytes[self.pos / 8]);
            value = (value << take) | ((byte >> (available - take)) & ((1 << take) - 1));
            self.pos += take as usize;
            left -= take;
        }
        Ok(value)
    }

    pub fn flag(&mut self) -> Result<bool> {
        Ok(self.read(1)? != 0)
    }

    pub fn position(&self) -> usize {
        self.pos
    }
    pub fn remaining(&self) -> usize {
        self.bytes.len().saturating_mul(8).saturating_sub(self.pos)
    }
    pub fn skip(&mut self, count: usize) -> Result<()> {
        let end = self.pos.checked_add(count).ok_or(Error::Limit)?;
        if end.div_ceil(8) > self.bytes.len() {
            return Err(Error::Truncated);
        }
        self.pos = end;
        Ok(())
    }

    pub fn uvlc(&mut self) -> Result<u32> {
        let mut leading = 0;
        while !self.flag()? {
            leading = (leading + 1).min(32);
        }
        if leading == 32 {
            return Ok(u32::MAX);
        }
        Ok(((1u32 << leading) - 1) + self.read(leading)?)
    }

    pub fn trailing(&mut self) -> Result<()> {
        if !self.flag()? {
            return Err(Error::Invalid);
        }
        while self.pos & 7 != 0 {
            if self.flag()? {
                return Err(Error::Invalid);
            }
        }
        if self.bytes[self.pos / 8..].iter().any(|byte| *byte != 0) {
            return Err(Error::Invalid);
        }
        Ok(())
    }
}
