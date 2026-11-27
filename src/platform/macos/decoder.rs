//! macOS video decoding: the Rust H.264/AV1 software codecs only. VideoToolbox
//! hardware decoding is future work; the dmabuf frame type keeps the shape the
//! shared decoder plumbing compiles against but can never be constructed.
use crate::media::VideoCodec;
use crate::media::decode_api::{DecodeError, DecoderMode, DecoderNotification, VideoDecoderConfig};
use bytes::Bytes;
use std::collections::VecDeque;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CpuFormat {
    Nv12,
    I444,
}

pub struct CpuVideoFrame {
    pub pts: i64,
    pub width: u32,
    pub height: u32,
    pub format: CpuFormat,
    pub data: Bytes,
}

/// A zero-copy picture; macOS decoding is software-only, so no value of the
/// frame type can exist.
pub struct DmabufVideoFrame {
    pub pts: i64,
    pub width: u32,
    pub height: u32,
    pub frame: std::convert::Infallible,
}

/// No hardware decoder shares a device with the renderer.
pub(crate) type GpuDevice = std::convert::Infallible;

pub enum PlatformDecodedFrame {
    Cpu(CpuVideoFrame),
    /// Never constructed on macOS.
    Dmabuf(DmabufVideoFrame),
}

/// No hardware decoder yet.
pub fn probe_hardware(_codec: VideoCodec, _width: u32, _height: u32, _depth: u8, _chroma: u8) -> bool {
    false
}

/// The shared decoder plumbing names `LinuxVideoDecoder` on every
/// non-Windows platform; the type below is macOS's software-only decoder.
pub struct LinuxVideoDecoder {
    backend: openuuyc_codec::decoder::Decoder,
    pending: VecDeque<PlatformDecodedFrame>,
    notification: DecoderNotification,
}

fn software_error(error: openuuyc_codec::decoder::Error) -> DecodeError {
    use openuuyc_codec::decoder::Error;
    match error {
        Error::Closed => DecodeError::Closed,
        Error::Unsupported => DecodeError::Unsupported,
        Error::Allocation => DecodeError::Backend,
        Error::InvalidInput => DecodeError::InvalidInput,
        Error::NeedKeyframe => {
            tracing::debug!(%error, "software decoder input requires a new keyframe");
            DecodeError::NeedKeyframe
        }
    }
}

impl LinuxVideoDecoder {
    pub fn open(config: &VideoDecoderConfig) -> Result<Self, DecodeError> {
        let backend = match config.mode {
            DecoderMode::Hardware => return Err(DecodeError::Backend),
            DecoderMode::Software => {
                if config.width == 0
                    || config.height == 0
                    || config.width > 16384
                    || config.height > 16384
                {
                    return Err(DecodeError::InvalidInput);
                }
                openuuyc_codec::decoder::Decoder::new(config.codec, &config.extra_data)
                    .map_err(software_error)?
            }
        };
        Ok(Self {
            backend,
            pending: VecDeque::new(),
            notification: DecoderNotification::default(),
        })
    }

    pub fn probe_format(
        _device: GpuDevice,
        _codec: VideoCodec,
        _width: u32,
        _height: u32,
        _depth: u8,
        _chroma: u8,
    ) -> bool {
        false
    }

    pub fn poll_owned_frame(&mut self) -> Result<Option<PlatformDecodedFrame>, DecodeError> {
        if self.notification.is_cancelled() {
            self.pending.clear();
            return Err(DecodeError::Closed);
        }
        Ok(self.pending.pop_front())
    }

    pub fn poll_dropped_token(&mut self) -> Option<i64> {
        None
    }

    pub fn reset_for_keyframe(&mut self) -> Result<(), DecodeError> {
        self.pending.clear();
        self.backend.reset().map_err(software_error)
    }

    pub fn set_notification(&mut self, notification: DecoderNotification) {
        self.backend
            .set_cancellation(notification.shared_cancellation());
        self.notification = notification;
    }

    pub fn push_packet(&mut self, payload: &[u8], token: i64) -> Result<(), DecodeError> {
        if payload.len() > i32::MAX as usize {
            return Err(DecodeError::InvalidInput);
        }
        for frame in self
            .backend
            .push(payload, token)
            .map_err(software_error)?
        {
            if self.notification.is_cancelled() {
                self.pending.clear();
                return Err(DecodeError::Closed);
            }
            let format = match frame.format {
                openuuyc_codec::PixelFormat::Nv12 => CpuFormat::Nv12,
                openuuyc_codec::PixelFormat::I444 => CpuFormat::I444,
                _ => return Err(DecodeError::Unsupported),
            };
            self.pending
                .push_back(PlatformDecodedFrame::Cpu(CpuVideoFrame {
                    pts: frame.pts,
                    width: frame.coded_width,
                    height: frame.coded_height,
                    format,
                    data: frame.data,
                }));
        }
        Ok(())
    }
}

impl Drop for LinuxVideoDecoder {
    fn drop(&mut self) {
        self.pending.clear();
    }
}

/// The host encoder's VA-API capability check; no host encoding on macOS.
pub(crate) fn encoder_available() -> bool {
    false
}

/// The host encoder's shared display; no host encoding on macOS.
pub(crate) fn shared_display() -> anyhow::Result<std::convert::Infallible> {
    anyhow::bail!("macOS host role is not supported")
}
