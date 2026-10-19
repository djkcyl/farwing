use crate::bitstream::bits::Bits;

#[derive(Clone)]
pub(super) struct BitReader<'a> {
    bits: Bits<'a>,
}
impl<'a> BitReader<'a> {
    pub fn new(data: &'a [u8]) -> Self {
        Self {
            bits: Bits::new(data),
        }
    }
    pub fn read_bit(&mut self) -> Result<bool, String> {
        self.bits.flag().map_err(|e| e.to_string())
    }
    pub fn read_bits<T: TryFrom<u32>>(&mut self, count: usize) -> Result<T, String> {
        let count = u32::try_from(count).map_err(|_| "AV1 bit count overflow")?;
        let value = self.bits.read(count).map_err(|e| e.to_string())?;
        T::try_from(value).map_err(|_| "AV1 field does not fit output type".into())
    }
    pub fn read_le<T: TryFrom<u32>>(&mut self, bytes: usize) -> Result<T, String> {
        if bytes > 4 || self.position() & 7 != 0 {
            return Err("invalid AV1 little-endian field".into());
        }
        let mut value = 0u32;
        for i in 0..bytes {
            value |= self.read_bits::<u32>(8)? << (i * 8);
        }
        T::try_from(value).map_err(|_| "AV1 field does not fit output type".into())
    }
    pub fn skip_bits(&mut self, count: usize) -> Result<(), String> {
        self.bits.skip(count).map_err(|e| e.to_string())
    }
    pub fn position(&self) -> u64 {
        self.bits.position() as u64
    }
    pub fn num_bits_left(&self) -> usize {
        self.bits.remaining()
    }
}
