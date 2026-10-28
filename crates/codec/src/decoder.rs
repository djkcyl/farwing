//! One software decoder session for the supported low-delay video formats.
#[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
mod av1;
use crate::{Codec, PixelFormat};
use bytes::Bytes;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    InvalidInput,
    Unsupported,
    Allocation,
    NeedKeyframe,
    Closed,
}
impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "software decoder: {self:?}")
    }
}
impl std::error::Error for Error {}
pub type Result<T> = std::result::Result<T, Error>;

pub struct Frame {
    pub pts: i64,
    pub width: u32,
    pub height: u32,
    pub coded_width: u32,
    pub coded_height: u32,
    pub format: PixelFormat,
    pub data: Bytes,
}
enum Kernel {
    H264(Box<farwing_h264::stream::Decoder>),
    #[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
    Av1(Box<av1::Session>),
}
pub struct Decoder {
    kernel: Kernel,
    cancel: Arc<AtomicBool>,
}
impl Decoder {
    pub fn new(codec: Codec, extra: &[u8]) -> Result<Self> {
        let kernel = match codec {
            Codec::H264 => {
                let mut core = farwing_h264::stream::Decoder::new();
                core.seed(extra).map_err(h264_error)?;
                Kernel::H264(Box::new(core))
            }
            #[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
            Codec::Av1 => Kernel::Av1(Box::new(av1::Session::new(extra)?)),
            #[cfg(not(any(target_arch = "x86", target_arch = "x86_64")))]
            Codec::Av1 => return Err(Error::Unsupported),
            Codec::H265 => return Err(Error::Unsupported),
        };
        let mut session = Self {
            kernel,
            cancel: Arc::new(AtomicBool::new(false)),
        };
        session.set_cancellation(session.cancel.clone());
        Ok(session)
    }
    pub fn set_cancellation(&mut self, cancel: Arc<AtomicBool>) {
        #[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
        if let Kernel::Av1(core) = &mut self.kernel {
            core.set_cancellation(cancel.clone());
        }
        self.cancel = cancel;
    }
    pub fn reset(&mut self) -> Result<()> {
        match &mut self.kernel {
            Kernel::H264(core) => core.reset().map_err(h264_error),
            #[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
            Kernel::Av1(core) => core.reset(),
        }
    }
    pub fn push(&mut self, data: &[u8], token: i64) -> Result<Vec<Frame>> {
        if self.cancel.load(Ordering::Acquire) {
            return Err(Error::Closed);
        }
        if data.len() > i32::MAX as usize {
            return Err(Error::InvalidInput);
        }
        match &mut self.kernel {
            #[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
            Kernel::Av1(core) => Ok(core.push(data, token, &self.cancel)?.into_iter().collect()),
            Kernel::H264(core) => {
                let pictures = core
                    .submit_with_cancel(data, token as u64, &self.cancel)
                    .map_err(h264_error)?;
                let mut frames = Vec::with_capacity(pictures.len());
                for output in pictures {
                    let picture = output.picture;
                    let mut packed = Vec::new();
                    picture.pack_into(&mut packed).map_err(h264_error)?;
                    let yuv444 = picture.chroma == farwing_h264::picture::Chroma::Yuv444;
                    if yuv444 {
                        packed = planar_to_ayuv(
                            &packed,
                            picture.crop.width as usize * picture.crop.height as usize,
                        );
                    }
                    if self.cancel.load(Ordering::Acquire) {
                        core.reset().map_err(h264_error)?;
                        return Err(Error::Closed);
                    }
                    frames.push(Frame {
                        pts: output.token as i64,
                        width: picture.crop.width as u32,
                        height: picture.crop.height as u32,
                        coded_width: picture.crop.width as u32,
                        coded_height: picture.crop.height as u32,
                        format: if yuv444 { PixelFormat::Ayuv } else { PixelFormat::Nv12 },
                        data: Bytes::from(packed),
                    });
                }
                Ok(frames)
            }
        }
    }
}
/// Planar 4:4:4 (Y, U, V planes of `plane` samples) as packed AYUV, the
/// layout the renderer samples (V, U, Y, A per pixel).
fn planar_to_ayuv(planar: &[u8], plane: usize) -> Vec<u8> {
    let (y, chroma) = planar.split_at(plane);
    let (u, v) = chroma.split_at(plane);
    let mut packed = Vec::with_capacity(plane * 4);
    for ((y, u), v) in y.iter().zip(u).zip(&v[..plane]) {
        packed.extend_from_slice(&[*v, *u, *y, 255]);
    }
    packed
}

fn h264_error(error: farwing_h264::Error) -> Error {
    use farwing_h264::Error as E;
    match error {
        E::Cancelled | E::Closed => Error::Closed,
        E::Unsupported(_) => Error::Unsupported,
        E::Allocation => Error::Allocation,
        E::NeedKeyframe | E::Truncated | E::Invalid(_) => Error::NeedKeyframe,
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn planar_444_packs_as_vuya() {
        // Two pixels: Y 10 20, U 30 40, V 50 60.
        let packed = super::planar_to_ayuv(&[10, 20, 30, 40, 50, 60], 2);
        assert_eq!(packed, [50, 30, 10, 255, 60, 40, 20, 255]);
    }
}
