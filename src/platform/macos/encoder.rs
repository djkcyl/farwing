//! Host encoding, viewer-only: macOS has no host role in this port, so the
//! probe never offers a candidate and an encoder can never be constructed.
use super::capture::{Desktop, Device, Image};
use crate::media::encoding::{Backend, Capability, Color, Format, Rate};
use anyhow::{Result, bail};

pub(crate) use crate::media::encoding::{Encoded, FrameTiming};

const UNSUPPORTED: &str = "macOS host role is not supported";

pub(crate) fn probe(
    _desktop: &mut Desktop,
    _is_active: impl Fn() -> bool,
) -> Result<Vec<Capability>> {
    bail!(UNSUPPORTED)
}

pub(crate) const GPU_SCHEDULING: bool = false;

/// Per-thread encoder runtime. No encoder needs setup.
pub(crate) struct Runtime;
#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub(crate) struct SwitchCandidate(pub String);
impl Runtime {
    pub(crate) fn new() -> Result<Self> {
        Ok(Self)
    }
    pub(crate) fn with_gpu_compatibility(_compatible: bool) -> Result<Self> {
        Self::new()
    }
    pub(crate) fn begin_gpu_work(&mut self, _compatible: bool) {}
    pub(crate) fn end_gpu_work(&mut self) {}
}

/// Never constructed; both factory entry points refuse.
pub(crate) enum Encoder {}
impl Encoder {
    pub(crate) fn reconfigure_stream(
        &mut self,
        _device: &Device,
        _size: (u32, u32),
        _format: Format,
        _rate: Rate,
        _color: Color,
    ) -> Result<bool> {
        match *self {}
    }

    pub(crate) fn hardware_format(
        _device: &Device,
        _backend: Backend,
        _size: (u32, u32),
        _format: Format,
        _rate: Rate,
        _color: Color,
    ) -> Result<Self> {
        bail!(UNSUPPORTED)
    }
    pub(crate) fn software_format(
        _device: &Device,
        _size: (u32, u32),
        _format: Format,
        _rate: Rate,
    ) -> Result<Self> {
        bail!(UNSUPPORTED)
    }
    pub(crate) fn implementation(&self) -> i32 {
        match *self {}
    }
    pub(crate) fn maximum_size(&self) -> (u32, u32) {
        match *self {}
    }
    pub(crate) fn configure_rate(&mut self, _rate: Rate) -> Result<bool> {
        match *self {}
    }
    pub(crate) fn encode(
        &mut self,
        _image: &Image,
        _timestamp: i64,
        _keyframe: bool,
    ) -> Result<Vec<Encoded>> {
        match *self {}
    }
    pub(crate) fn encode_cancellable(
        &mut self,
        _image: &Image,
        _timestamp: i64,
        _keyframe: bool,
        _cancel: &std::sync::Arc<std::sync::atomic::AtomicBool>,
    ) -> Result<Vec<Encoded>> {
        match *self {}
    }
}
