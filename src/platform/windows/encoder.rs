//! Hardware first; negotiated Rust software codecs share the capture lifecycle.
use anyhow::{Result, ensure};
use windows::{
    Win32::{Graphics::Direct3D11::*, System::Com::*},
    core::Interface,
};

mod probe;
pub(crate) use probe::ProbeCache;

// Driver busy paths use the SDK's bounded spin/yield/backoff schedule.
pub(super) fn retry_pause(attempt: usize) {
    match attempt {
        0..=3 => std::hint::spin_loop(),
        4..=7 => std::thread::yield_now(),
        _ => std::thread::sleep(std::time::Duration::from_millis(
            1u64 << (attempt - 8).min(3),
        )),
    }
}

pub(crate) struct Runtime {
    _scheduling: Option<super::gpu_scheduling::Lease>,
}
#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub(crate) struct SwitchCandidate(pub String);
impl Runtime {
    pub(crate) fn new() -> Result<Self> {
        unsafe {
            CoInitializeEx(None, COINIT_MULTITHREADED).ok()?;
        }
        Ok(Self { _scheduling: None })
    }
    /// Keep scheduling through capture replacement and encoder teardown.
    pub(crate) fn with_gpu_compatibility(compatible: bool) -> Result<Self> {
        let mut runtime = Self::new()?;
        runtime.begin_gpu_work(compatible);
        Ok(runtime)
    }
    pub(crate) fn begin_gpu_work(&mut self, compatible: bool) {
        self._scheduling
            .get_or_insert_with(|| super::gpu_scheduling::Lease::with_compatibility(compatible));
    }
    /// Call only after the worker has released its capture/encoder resources.
    pub(crate) fn end_gpu_work(&mut self) {
        self._scheduling = None;
    }
}
impl Drop for Runtime {
    fn drop(&mut self) {
        self.end_gpu_work();
        unsafe {
            CoUninitialize();
        }
    }
}

pub(crate) use crate::media::encoding::{Encoded, FrameTiming};

pub(crate) enum Encoder {
    Nvidia(super::nvenc::Encoder),
    Amd(super::amf::Encoder),
    Intel(super::qsv::Encoder),
    Software(Box<super::software_encoder::Encoder>),
}
impl Encoder {
    pub(crate) fn reconfigure_stream(
        &mut self,
        device: &ID3D11Device,
        size: (u32, u32),
        format: super::format::Format,
        rate: super::format::Rate,
        color: super::format::Color,
    ) -> Result<bool> {
        match self {
            Self::Intel(encoder) => encoder.reconfigure_stream(device, size, format, rate, color),
            _ => Ok(false),
        }
    }
    pub(crate) fn hardware_format(
        device: &ID3D11Device,
        size: (u32, u32),
        format: super::format::Format,
        rate: super::format::Rate,
        color: super::format::Color,
    ) -> Result<Self> {
        unsafe {
            let dxgi: windows::Win32::Graphics::Dxgi::IDXGIDevice = device.cast()?;
            let desc = dxgi.GetAdapter()?.GetDesc()?;
            match desc.VendorId {
                0x10de => Ok(Self::Nvidia(super::nvenc::Encoder::new_format(
                    device, size.0, size.1, rate, format, color,
                )?)),
                0x1002 => Ok(Self::Amd(super::amf::Encoder::new(
                    device, size, format, rate, color,
                )?)),
                0x8086 => Ok(Self::Intel(super::qsv::Encoder::new(
                    device, size, format, rate, color,
                )?)),
                _ => anyhow::bail!("当前采集适配器没有可用的硬件编码候选"),
            }
        }
    }
    pub(crate) fn software_format(
        device: &ID3D11Device,
        size: (u32, u32),
        format: super::format::Format,
        rate: super::format::Rate,
    ) -> Result<Self> {
        Ok(Self::Software(Box::new(
            super::software_encoder::Encoder::new(device, size, format, rate)?,
        )))
    }
    pub(crate) fn implementation(&self) -> i32 {
        match self {
            Self::Nvidia(_) => 0,
            Self::Amd(_) => 1,
            Self::Intel(_) => 2,
            Self::Software(_) => 5,
        }
    }
    pub(crate) fn maximum_size(&self) -> (u32, u32) {
        match self {
            Self::Nvidia(e) => e.maximum_size(),
            Self::Amd(e) => e.maximum_size(),
            Self::Intel(e) => e.maximum_size(),
            Self::Software(encoder) => encoder.maximum_size(),
        }
    }
    pub(crate) fn configure_rate(&mut self, rate: super::format::Rate) -> Result<bool> {
        match self {
            Self::Nvidia(e) => e.configure_rate(rate),
            Self::Amd(e) => e.configure(rate),
            Self::Intel(e) => e.configure(rate),
            Self::Software(encoder) => encoder.configure(rate),
        }
    }
    pub(crate) fn encode(
        &mut self,
        texture: &ID3D11Texture2D,
        timestamp: i64,
        keyframe: bool,
    ) -> Result<Vec<Encoded>> {
        self.encode_cancellable(
            texture,
            timestamp,
            keyframe,
            &std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
        )
    }
    pub(crate) fn encode_cancellable(
        &mut self,
        texture: &ID3D11Texture2D,
        timestamp: i64,
        keyframe: bool,
        cancel: &std::sync::Arc<std::sync::atomic::AtomicBool>,
    ) -> Result<Vec<Encoded>> {
        if keyframe && let Self::Software(e) = self {
            e.request_keyframe();
        }
        ensure!(
            !cancel.load(std::sync::atomic::Ordering::Acquire),
            "编码已取消"
        );
        match self {
            Self::Software(e) => e.encode(texture, timestamp, keyframe, cancel),
            Self::Nvidia(e) => e.encode(texture, timestamp, keyframe),
            Self::Amd(e) => e.encode(texture, timestamp, keyframe, cancel),
            Self::Intel(e) => e.encode(texture, timestamp, keyframe, cancel),
        }
    }
}
