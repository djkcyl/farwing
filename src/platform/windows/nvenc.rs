//! NVENC on the capture adapter. Raw desktop pixels stay in GPU memory.
//! The session itself (presets, rate control, encoding) is shared with the
//! other platforms in `media::encoding::nvenc`.
mod cuda_input;
use super::{
    format::{Backend, Color, Format, Rate},
    gpu_conversion::Conversion,
};
use crate::media::encoding::nvenc::{
    NV_ENC_BUFFER_FORMAT, NV_ENC_DEVICE_TYPE, NV_ENC_INPUT_RESOURCE_TYPE, Registered, Session,
    timing,
};
use anyhow::{Result, ensure};
use std::ffi::c_void;
use windows::{Win32::Graphics::Direct3D11::*, core::Interface};

pub(crate) struct Encoder {
    /// Destroyed in `drop` while the CUDA context, if any, is current.
    session: std::mem::ManuallyDrop<Session>,
    registered: Option<Registered>,
    conversion: Conversion,
    /// Planar 4:4:4 10-bit reaches NVENC through a CUDA copy of the
    /// conversion output; every other format registers the texture itself.
    cuda: Option<cuda_input::Input>,
}

#[derive(Debug, thiserror::Error)]
#[error("NVENC输入候选被拒绝：{0}")]
struct InputRejected(String);

impl Encoder {
    pub(crate) fn new_format(
        device: &ID3D11Device,
        width: u32,
        height: u32,
        rate: Rate,
        format: Format,
        color: Color,
    ) -> Result<Self> {
        if format.chroma == 3 && format.depth == 10 {
            return Self::create(
                device,
                width,
                height,
                rate,
                format,
                color,
                Conversion::planar_44410(device, (width, height), color)?,
            );
        }
        let conversion = Conversion::new(device, (width, height), format, color)?;
        let compute = conversion.is_compute();
        match Self::create(device, width, height, rate, format, color, conversion) {
            Err(error) if compute && error.downcast_ref::<InputRejected>().is_some() => {
                tracing::debug!(%error, "NVENC registered compute input rejected; trying pixel candidate");
                Self::create(
                    device,
                    width,
                    height,
                    rate,
                    format,
                    color,
                    Conversion::pixel(device, (width, height), format, 1, color)?,
                )
            }
            result => result,
        }
    }
    fn create(
        device: &ID3D11Device,
        width: u32,
        height: u32,
        rate: Rate,
        format: Format,
        color: Color,
        conversion: Conversion,
    ) -> Result<Self> {
        ensure!(
            Backend::Nvidia.accepts(format),
            "NVENC当前D3D输入路径不支持该格式"
        );
        let cuda = if format.chroma == 3 && format.depth == 10 {
            Some(cuda_input::Input::new(
                device,
                &conversion.output,
                (width, height),
            )?)
        } else {
            None
        };
        let _current = cuda.as_ref().map(|input| input.enter()).transpose()?;
        let (device_type, raw) = match &cuda {
            Some(input) => (NV_ENC_DEVICE_TYPE::NV_ENC_DEVICE_TYPE_CUDA, input.context),
            None => (
                NV_ENC_DEVICE_TYPE::NV_ENC_DEVICE_TYPE_DIRECTX,
                device.as_raw(),
            ),
        };
        let mut session = Session::open(device_type, raw, width, height, rate, format, color)?;
        let buffer_format = match (format.chroma, format.depth) {
            (1, 10) => NV_ENC_BUFFER_FORMAT::NV_ENC_BUFFER_FORMAT_YUV420_10BIT,
            (3, 8) => NV_ENC_BUFFER_FORMAT::NV_ENC_BUFFER_FORMAT_AYUV,
            (3, 10) => NV_ENC_BUFFER_FORMAT::NV_ENC_BUFFER_FORMAT_YUV444_10BIT,
            _ => NV_ENC_BUFFER_FORMAT::NV_ENC_BUFFER_FORMAT_NV12,
        };
        let registered = match &cuda {
            Some(input) => {
                // The three planes are stacked in one pitched allocation.
                session.restrict_maximum((u32::MAX, (16384 / 3) & !1))?;
                session.register(
                    NV_ENC_INPUT_RESOURCE_TYPE::NV_ENC_INPUT_RESOURCE_TYPE_CUDADEVICEPTR,
                    input.pointer as usize as *mut c_void,
                    input.pitch,
                    buffer_format,
                )
            }
            None => session.register(
                NV_ENC_INPUT_RESOURCE_TYPE::NV_ENC_INPUT_RESOURCE_TYPE_DIRECTX,
                conversion.output.as_raw(),
                0,
                buffer_format,
            ),
        }
        .map_err(|error| InputRejected(format!("{error:#}")))?;
        drop(_current);
        Ok(Self {
            session: std::mem::ManuallyDrop::new(session),
            registered: Some(registered),
            conversion,
            cuda,
        })
    }
    pub(crate) fn maximum_size(&self) -> (u32, u32) {
        self.session.maximum_size()
    }

    pub(crate) fn configure_rate(&mut self, rate: Rate) -> Result<bool> {
        let _current = self.cuda.as_ref().map(|input| input.enter()).transpose()?;
        self.session.configure_rate(rate)
    }

    pub(crate) fn encode(
        &mut self,
        texture: &ID3D11Texture2D,
        timestamp_100ns: i64,
        keyframe: bool,
    ) -> Result<Vec<super::encoder::Encoded>> {
        let mut timing = timing::Frame::new();
        let result = self.encode_timed(texture, timestamp_100ns, keyframe, &mut timing);
        self.session.record(timing, result.is_ok());
        result
    }

    fn encode_timed(
        &mut self,
        texture: &ID3D11Texture2D,
        timestamp_100ns: i64,
        keyframe: bool,
        timing: &mut timing::Frame,
    ) -> Result<Vec<super::encoder::Encoded>> {
        let converted = self.conversion.convert(texture);
        timing.stage(0);
        converted?;
        let _current = self.cuda.as_ref().map(|input| input.enter()).transpose()?;
        let copied = self.cuda.as_ref().map(|input| input.copy()).transpose();
        timing.stage(1);
        copied?;
        let registered = self.registered.as_ref().expect("registered until drop");
        self.session
            .encode(registered, timestamp_100ns, keyframe, timing)
    }
}
impl Drop for Encoder {
    fn drop(&mut self) {
        let _current = self.cuda.as_ref().and_then(|input| input.enter().ok());
        // The input is unregistered before its session is destroyed, both
        // with the session's CUDA context current when it has one.
        if let Some(registered) = self.registered.take() {
            self.session.unregister(registered);
        }
        // SAFETY: the session is not used again.
        unsafe { std::mem::ManuallyDrop::drop(&mut self.session) };
    }
}
