//! Screen capture, viewer-only: macOS has no capture backend in this port, so
//! screens never enumerate and a desktop never opens. The types keep the
//! shape the shared host pipeline compiles against.
use anyhow::{Result, bail};
use std::time::Instant;

pub(crate) use crate::media::capture::{Screen, SourceGone};

const UNSUPPORTED: &str = "macOS host role is not supported";

/// A CPU-only capture path; see [`Device`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Device;

pub(crate) fn device_lost(_device: &Device) -> bool {
    false
}

pub(crate) fn create_device(_adapter: u64) -> Result<(Device, ())> {
    Ok((Device, ()))
}

/// GPU adapters a hardware encoder could be pinned to. None on macOS.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) struct EncodingAdapter {
    #[serde(default)]
    pub id: Option<crate::media::selection::GpuId>,
    pub luid: u64,
    pub vendor: u32,
    pub name: String,
}

pub(crate) fn encoding_adapters() -> Result<Vec<EncodingAdapter>> {
    Ok(Vec::new())
}

pub(crate) fn session_locked() -> Option<bool> {
    None
}

/// A captured picture's BGRA pixels. Only the CPU variant exists here; the
/// GPU-resident ones belong to backends this port does not have.
#[derive(Clone)]
pub(crate) enum Image {
    /// Tightly packed, `width * height * 4` bytes.
    Cpu {
        pixels: std::sync::Arc<Vec<u8>>,
        width: u32,
        height: u32,
    },
}
impl Image {
    pub fn size(&self) -> (u32, u32) {
        match self {
            Self::Cpu { width, height, .. } => (*width, *height),
        }
    }
    pub fn pixels(&self) -> Result<std::borrow::Cow<'_, [u8]>> {
        Ok(match self {
            Self::Cpu { pixels, .. } => std::borrow::Cow::Borrowed(pixels.as_slice()),
        })
    }
}

/// One captured picture, already scaled to the size the encoder was asked for.
#[derive(Clone)]
pub(crate) struct Frame {
    pub width: u32,
    pub height: u32,
    pub image: Image,
    pub captured: Instant,
    pub is_new: bool,
    pub hdr_metadata: Option<crate::media::video_color::HdrMetadata>,
}

/// The active outputs, in the protocol's terms.
pub(crate) fn screens() -> Result<Vec<Screen>> {
    bail!(UNSUPPORTED)
}

pub(crate) fn wayland_session() -> bool {
    false
}

pub(crate) fn refresh(_selected: &Screen) -> Result<Screen> {
    bail!(UNSUPPORTED)
}

pub(crate) fn needs_consent() -> bool {
    false
}

/// The Windows encode pipeline's shared texture ring; macOS has none, so one
/// is never made and `raw_desc` is always `None`.
pub(crate) struct RawCapture {
    pub captured: Instant,
    pub is_new: bool,
    _never: std::convert::Infallible,
}

/// A capture of one monitor, kept open between frames. Never constructed.
pub(crate) struct Desktop {
    pub device: Device,
    pub screen: Screen,
    pub generation: u64,
    pub available: bool,
    pub cursor: Option<super::cursor_shape::Snapshot>,
}

impl Desktop {
    pub fn raw_desc(&self) -> Option<(u32, u32, u32)> {
        None
    }

    pub fn open_selected(_selected: &Screen) -> Result<Self> {
        bail!(UNSUPPORTED)
    }

    pub fn backend_name(&self) -> &'static str {
        "unsupported"
    }

    pub fn waiting(&self) -> Option<String> {
        Some("macOS 被控尚未支持".to_owned())
    }

    pub fn hdr_available(&self) -> bool {
        false
    }

    pub fn next(
        &mut self,
        _timeout: u32,
        _quality: i32,
        _cursor: bool,
        _hdr: bool,
        _maximum: (u32, u32),
    ) -> Result<Option<Frame>> {
        bail!(UNSUPPORTED)
    }
}
