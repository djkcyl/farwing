//! Rust DXVA11 and software video decoding.
//! No MFT, native video bridge or implicit backend fallback.
#![cfg(windows)]
use crate::media::VideoCodec;
use crate::media::decode_api::{DecodeError, DecoderMode, DecoderNotification, VideoDecoderConfig};
use bytes::Bytes;
use std::collections::VecDeque;
use windows::Win32::Graphics::Direct3D11::{ID3D11Device, ID3D11Texture2D};
mod diagnostics;
mod rust_dxva;

/// Decoded pictures not shown because every display surface was in use
/// (diagnostics).
pub(crate) fn display_busy_drops() -> u64 {
    rust_dxva::DISPLAY_BUSY_DROPS.load(std::sync::atomic::Ordering::Relaxed)
}
pub struct WindowsGpuVideoFrame {
    texture: ID3D11Texture2D,
    subresource: u32,
    pts: i64,
    visible_x: u32,
    visible_y: u32,
    width: u32,
    height: u32,
    _lease: std::sync::Arc<rust_dxva::dxva::Lease>,
}
unsafe impl Send for WindowsGpuVideoFrame {}
impl WindowsGpuVideoFrame {
    pub fn texture(&self) -> &ID3D11Texture2D {
        &self.texture
    }

    pub const fn subresource(&self) -> u32 {
        self.subresource
    }
    pub const fn pts(&self) -> i64 {
        self.pts
    }

    pub const fn width(&self) -> u32 {
        self.width
    }
    pub const fn height(&self) -> u32 {
        self.height
    }
    pub const fn visible_x(&self) -> u32 {
        self.visible_x
    }
    pub const fn visible_y(&self) -> u32 {
        self.visible_y
    }
}
impl std::fmt::Debug for WindowsGpuVideoFrame {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WindowsGpuVideoFrame")
            .field("pts", &self.pts)
            .field("subresource", &self.subresource)
            .field("width", &self.width)
            .field("height", &self.height)
            .finish_non_exhaustive()
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WindowsCpuFormat {
    Nv12,
    P010,
    Ayuv,
    Y410,
}
pub struct WindowsCpuVideoFrame {
    pub pts: i64,
    pub width: u32,
    pub height: u32,
    pub coded_width: u32,
    pub coded_height: u32,
    pub format: WindowsCpuFormat,
    pub data: Bytes,
}
pub enum WindowsDecodedFrame {
    Gpu(WindowsGpuVideoFrame),
    Cpu(WindowsCpuVideoFrame),
}
pub struct WindowsVideoDecoder {
    backend: Backend,
    pending: VecDeque<WindowsDecodedFrame>,
    notification: DecoderNotification,
    dropped: VecDeque<i64>,
}
enum Backend {
    Hardware(Box<rust_dxva::Session>),
    Software(Box<farwing_codec::decoder::Decoder>),
}
fn software_error(error: farwing_codec::decoder::Error) -> DecodeError {
    use farwing_codec::decoder::Error;
    match error {
        Error::Closed => DecodeError::Closed,
        Error::Unsupported => DecodeError::Unsupported,
        Error::Allocation => DecodeError::Backend,
        Error::InvalidInput => DecodeError::InvalidInput,
        Error::NeedKeyframe => DecodeError::NeedKeyframe,
    }
}
fn software_frame(
    frame: farwing_codec::decoder::Frame,
) -> Result<WindowsDecodedFrame, DecodeError> {
    use farwing_codec::PixelFormat as F;
    Ok(WindowsDecodedFrame::Cpu(WindowsCpuVideoFrame {
        pts: frame.pts,
        width: frame.width,
        height: frame.height,
        coded_width: frame.coded_width,
        coded_height: frame.coded_height,
        data: frame.data,
        format: match frame.format {
            F::Nv12 => WindowsCpuFormat::Nv12,
            F::P010 => WindowsCpuFormat::P010,
            F::Ayuv => WindowsCpuFormat::Ayuv,
            F::Y410 => WindowsCpuFormat::Y410,
            F::Bgra => return Err(DecodeError::Unsupported),
        },
    }))
}

impl WindowsVideoDecoder {
    pub(crate) fn check_format(
        device: ID3D11Device,
        codec: VideoCodec,
        width: u32,
        height: u32,
        depth: u8,
        chroma: u8,
    ) -> anyhow::Result<()> {
        rust_dxva::Session::check_format(device, codec, width, height, depth, chroma)
    }
    pub fn open(config: &VideoDecoderConfig) -> Result<Self, DecodeError> {
        let backend = match config.mode {
            DecoderMode::Software => {
                if config.width == 0
                    || config.height == 0
                    || config.width > 16384
                    || config.height > 16384
                {
                    return Err(DecodeError::InvalidInput);
                }
                Backend::Software(Box::new(
                    farwing_codec::decoder::Decoder::new(config.codec, &config.extra_data)
                        .map_err(software_error)?,
                ))
            }

            DecoderMode::Hardware => Backend::Hardware(Box::new(rust_dxva::Session::open(config)?)),
        };
        let mut decoder = Self {
            backend,
            pending: VecDeque::new(),
            notification: DecoderNotification::default(),
            dropped: VecDeque::new(),
        };
        decoder.set_notification(decoder.notification.clone());
        Ok(decoder)
    }
    pub fn probe_format(
        device: ID3D11Device,
        codec: VideoCodec,
        width: u32,
        height: u32,
        depth: u8,
        chroma: u8,
    ) -> bool {
        rust_dxva::Session::probe(device, codec, width, height, depth, chroma)
    }
    pub fn poll_owned_frame(&mut self) -> Result<Option<WindowsDecodedFrame>, DecodeError> {
        if self.notification.is_cancelled() {
            self.pending.clear();
            return Err(DecodeError::Closed);
        }
        if let Some(frame) = self.pending.pop_front() {
            return Ok(Some(frame));
        }
        match &mut self.backend {
            Backend::Hardware(session) => session.poll(),
            Backend::Software(_) => Ok(None),
        }
    }
    pub fn poll_dropped_token(&mut self) -> Option<i64> {
        match &mut self.backend {
            Backend::Hardware(session) => session.poll_dropped(),
            Backend::Software(_) => self.dropped.pop_front(),
        }
    }
    pub fn reset_for_keyframe(&mut self) -> Result<(), DecodeError> {
        self.pending.clear();
        self.dropped.clear();
        match &mut self.backend {
            Backend::Hardware(session) => {
                session.reset();
                Ok(())
            }
            Backend::Software(session) => session.reset().map_err(software_error),
        }
    }
}
impl WindowsVideoDecoder {
    pub fn set_notification(&mut self, notification: DecoderNotification) {
        if let Backend::Software(session) = &mut self.backend {
            session.set_cancellation(notification.shared_cancellation());
        }
        self.notification = notification;
    }
    pub fn push_packet(&mut self, payload: &[u8], token: i64) -> Result<(), DecodeError> {
        match &mut self.backend {
            Backend::Hardware(session) => session.push(payload, token, &self.notification),
            Backend::Software(session) => {
                let frames = session.push(payload, token).map_err(software_error)?;
                if frames.is_empty() {
                    self.dropped.push_back(token);
                }
                for frame in frames {
                    self.pending.push_back(software_frame(frame)?);
                }
                Ok(())
            }
        }
    }
}
/// Decode one tiny H.264 picture on `device`. The first hardware decode on
/// a device costs about 30 ms of driver setup (later ones a few
/// milliseconds); a viewer pays it while it connects, not on its first
/// picture.
pub(crate) fn warm_up(device: ID3D11Device) -> anyhow::Result<()> {
    use anyhow::Context;
    const SIZE: u32 = 64;
    let mut encoder = farwing_codec::encoder::Encoder::new(farwing_codec::encoder::Config {
        width: SIZE,
        height: SIZE,
        format: farwing_codec::Format {
            codec: VideoCodec::H264,
            chroma: 1,
            depth: 8,
        },
        fps: 30,
        bitrate: 500_000,
    })?;
    let running = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    encoder.prepare(
        &[128; (SIZE * SIZE * 4) as usize],
        (SIZE * 4) as usize,
        &running,
    )?;
    let picture = encoder
        .encode(0, true, &running)?
        .context("warm-up picture not encoded")?;
    let mut decoder = WindowsVideoDecoder::open(&VideoDecoderConfig {
        codec: VideoCodec::H264,
        width: SIZE,
        height: SIZE,
        mode: DecoderMode::Hardware,
        gpu_device: Some(device),
        extra_data: Bytes::new(),
    })
    .map_err(|error| anyhow::anyhow!("{error:?}"))?;
    decoder
        .push_packet(&picture.data, 0)
        .map_err(|error| anyhow::anyhow!("{error:?}"))?;
    decoder
        .poll_owned_frame()
        .map_err(|error| anyhow::anyhow!("{error:?}"))?;
    Ok(())
}

impl Drop for WindowsVideoDecoder {
    fn drop(&mut self) {
        self.pending.clear();
    }
}
