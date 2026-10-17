//! The decoded-surface types the shared decoder plumbing names, on a platform
//! that has no GPU surface to share between decoder and renderer.
//!
//! This is not a stand-in for a D3D11 texture: the types are uninhabited, so no
//! value of them can exist, every branch that would hold one is unreachable,
//! and every Linux frame takes the CPU path. A real zero-copy path here would
//! be a dmabuf export from VA-API, with its own ownership rules.
use anyhow::{Result, bail};

#[derive(Clone)]
pub(crate) enum D3D11SurfaceWriter {}

#[derive(Debug)]
pub(crate) enum D3D11Surface {}

impl D3D11SurfaceWriter {
    pub(crate) fn new() -> Result<Self> {
        bail!("当前平台没有 D3D11 零拷贝解码表面")
    }

    pub(crate) fn available() -> Result<Vec<Self>> {
        Ok(Vec::new())
    }

    pub(crate) fn device_handle(&self) -> super::decoder::GpuDevice {
        match *self {}
    }

    pub(crate) fn upload_cpu(
        &self,
        _frame: &super::decoder::CpuVideoFrame,
    ) -> Result<D3D11Surface> {
        match *self {}
    }

    pub(crate) fn wrap_decoded_surface(
        &self,
        _frame: std::convert::Infallible,
    ) -> Result<D3D11Surface> {
        match *self {}
    }
}

impl D3D11Surface {
    pub(crate) fn coded_size(&self) -> (u32, u32) {
        match *self {}
    }

    pub(crate) const fn visible_origin(&self) -> (u32, u32) {
        match *self {}
    }
}
