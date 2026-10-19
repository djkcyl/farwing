/// Local format identity; no protocol implementation numbers are encoded here.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Codec {
    H264,
    H265,
    Av1,
}
impl std::str::FromStr for Codec {
    type Err = anyhow::Error;
    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value.to_ascii_lowercase().as_str() {
            "h264" | "avc" => Ok(Self::H264),
            "h265" | "hevc" => Ok(Self::H265),
            "av1" => Ok(Self::Av1),
            _ => anyhow::bail!("unsupported video codec: {value}"),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Format {
    pub codec: Codec,
    /// 1 = 4:2:0, 3 = 4:4:4, matching the bitstream chroma values.
    pub chroma: u8,
    pub depth: u8,
}
impl Format {
    pub const fn can_decode(self) -> bool {
        matches!(self.chroma, 1 | 3)
            && match self.codec {
                Codec::H264 => self.depth == 8,
                Codec::Av1 => {
                    cfg!(any(target_arch = "x86", target_arch = "x86_64"))
                        && matches!(self.depth, 8 | 10)
                }
                Codec::H265 => false,
            }
    }
    pub const fn can_encode(self) -> bool {
        matches!(self.codec, Codec::H264) && self.chroma == 1 && self.depth == 8
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PixelFormat {
    Bgra,
    Nv12,
    I444,
    P010,
    Ayuv,
    Y410,
}
impl PixelFormat {
    /// Layout of a nonempty packed allocation, including chroma rows.
    /// NV12/P010 use even coded dimensions; reject an unpadded visible size.
    pub fn layout(self, width: usize, height: usize) -> Option<(usize, usize)> {
        if width == 0
            || height == 0
            || (matches!(self, Self::Nv12 | Self::P010) && (width % 2 != 0 || height % 2 != 0))
        {
            return None;
        }
        let row = width.checked_mul(match self {
            Self::Nv12 | Self::I444 => 1,
            Self::P010 => 2,
            _ => 4,
        })?;
        let rows = match self {
            Self::Nv12 | Self::P010 => height.checked_add(height / 2)?,
            Self::I444 => height.checked_mul(3)?,
            _ => height,
        };
        Some((row, rows))
    }
}
