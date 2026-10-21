//! Hardware first; negotiated Rust software codecs share the capture lifecycle.
use crate::i18n::t;
use anyhow::{Context, Result, ensure};
use windows::{
    Win32::{Graphics::Direct3D11::*, System::Com::*},
    core::Interface,
};

pub(crate) fn probe(
    desktop: &mut super::capture::Desktop,
    is_active: impl Fn() -> bool,
) -> Result<Vec<super::format::Capability>> {
    use super::format::{Backend, Capability, Codec, Format, Rate};
    let desc = unsafe {
        desktop
            .device
            .cast::<windows::Win32::Graphics::Dxgi::IDXGIDevice>()?
            .GetAdapter()?
            .GetDesc()?
    };
    let adapter =
        (u64::from(desc.AdapterLuid.HighPart as u32) << 32) | u64::from(desc.AdapterLuid.LowPart);
    let mut candidates = Vec::new();
    let mut adapters = super::capture::encoding_adapters()?;
    adapters.sort_by_key(|candidate| candidate.luid != adapter);
    for candidate in adapters {
        let luid = candidate.luid;
        let backend = match candidate.vendor {
            0x10de => Backend::Nvidia,
            0x1002 => Backend::Amd,
            0x8086 => Backend::Intel,
            _ => continue,
        };
        let device = if luid == adapter {
            desktop.device.clone()
        } else {
            match super::capture::create_device(luid) {
                Ok((device, _)) => device,
                Err(error) => {
                    tracing::debug!(%error,luid,"encoder adapter unavailable");
                    continue;
                }
            }
        };
        for codec in [Codec::H264, Codec::H265, Codec::Av1] {
            for chroma in [1, 3] {
                for depth in [8, 10] {
                    let format = Format {
                        codec,
                        chroma,
                        depth,
                    };
                    if backend.accepts(format) {
                        candidates.push((luid, device.clone(), backend, format));
                    }
                }
            }
        }
    }
    candidates.push((
        adapter,
        desktop.device.clone(),
        Backend::Software,
        Format::AVC,
    ));
    // Capability verification needs an input sample, not a new desktop present
    // for every candidate. Keep one owned converted sample per input format.
    let mut samples = [None::<super::capture::Frame>, None];
    let mut result = Vec::new();
    let mut last_failure = None;
    let mut acquired_frame = false;
    for (adapter, device, backend, format) in candidates {
        if !is_active() {
            anyhow::bail!("{}", t().system.encode_preparation_cancelled);
        }
        let probe = (|| -> Result<Capability> {
            let cached = &mut samples[usize::from(format.depth == 10)];
            for _ in 0..if cached.is_none() { 20 } else { 0 } {
                if !is_active() {
                    anyhow::bail!("{}", t().system.encode_preparation_cancelled);
                }
                if let Some(frame) = desktop.next(50, 1, false, format.depth == 10, (1280, 720))? {
                    acquired_frame = true;
                    *cached = Some(frame);
                    break;
                }
            }
            let frame = cached
                .as_ref()
                .context(t().system.encode_no_desktop_frame)?;
            let rate = Rate {
                target: 2_000_000,
                peak: 2_000_000,
                fps: 30,
                quality: 1,
                quality_target: super::format::QualityTarget {
                    bitrate: 2_000_000,
                    fps: 30,
                },
                smooth: false,
            };
            let sample_device: ID3D11Device = unsafe { frame.texture.GetDevice()? };
            let mut transfer = if device != sample_device {
                Some(super::transfer::Transfer::new(
                    &sample_device,
                    &device,
                    frame,
                )?)
            } else {
                None
            };
            let mut encoder = if backend == Backend::Software {
                Encoder::software_format(&device, (frame.width, frame.height), format, rate)?
            } else {
                Encoder::hardware_format(
                    &device,
                    (frame.width, frame.height),
                    format,
                    rate,
                    if format.depth == 10 {
                        super::format::Color::Hdr
                    } else {
                        super::format::Color::Sdr
                    },
                )?
            };
            let mut output = None;
            for i in 0..20 {
                if !is_active() {
                    anyhow::bail!("{}", t().system.encode_preparation_cancelled);
                }
                let delivery = if let Some(transfer) = transfer.as_mut() {
                    match transfer.copy(frame, &is_active)? {
                        Some(frame) => Some(frame),
                        None => {
                            std::thread::sleep(std::time::Duration::from_millis(1));
                            continue;
                        }
                    }
                } else {
                    None
                };
                let frames = encoder.encode(
                    delivery
                        .as_ref()
                        .map_or(&frame.texture, |d| &d.frame.texture),
                    i * 333333,
                    true,
                )?;
                drop(delivery);
                output = frames.iter().find_map(|frame| {
                    crate::media::video_format::parse_stream_format(
                        format.codec.media(),
                        &frame.data,
                    )
                });
                if output.is_some() {
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(1));
            }
            let output = output.context(t().system.encode_no_sequence_header)?;
            ensure!(
                output.chroma_format_idc == format.chroma
                    && output.bit_depth_luma == format.depth
                    && output.bit_depth_chroma == format.depth
                    && (output.visible_width, output.visible_height) == (frame.width, frame.height),
                "{}",
                t().system.encode_format_mismatch
            );
            Ok(Capability {
                adapter,
                backend,
                format,
                maximum: encoder.maximum_size(),
            })
        })();
        match probe {
            Ok(cap) => {
                tracing::debug!(
                    ?cap,
                    "host encoder capability verified from sequence header"
                );
                result.push(cap)
            }
            Err(error) => {
                tracing::debug!(?backend,?format,%error,"host encoder capability rejected");
                last_failure = Some(format!("{error:#}"));
            }
        }
    }
    ensure!(
        acquired_frame,
        "{}",
        (t().system.encode_desktop_no_frame)(
            last_failure
                .as_deref()
                .unwrap_or(t().system.encode_no_first_frame)
        )
    );
    ensure!(
        !result.is_empty(),
        "{}",
        (t().system.encode_no_encoder)(
            last_failure
                .as_deref()
                .unwrap_or(t().system.encode_no_candidate)
        )
    );
    Ok(result)
}

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

pub(crate) use crate::media::encoding::{Encoded, FrameControl, FrameTiming};

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
                _ => anyhow::bail!("{}", t().system.encode_no_hardware_candidate),
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
        self.encode_with(
            texture,
            timestamp,
            FrameControl {
                keyframe,
                ..Default::default()
            },
            cancel,
        )
    }
    /// Long-term reference slots this encoder marks and predicts from on
    /// request (0: keyframes only).
    pub(crate) fn ltr_slots(&self) -> u8 {
        match self {
            Self::Nvidia(e) => e.ltr_slots(),
            Self::Amd(e) => e.ltr_slots(),
            Self::Intel(e) => e.ltr_slots(),
            Self::Software(_) => 2,
        }
    }
    pub(crate) fn encode_with(
        &mut self,
        texture: &ID3D11Texture2D,
        timestamp: i64,
        control: FrameControl,
        cancel: &std::sync::Arc<std::sync::atomic::AtomicBool>,
    ) -> Result<Vec<Encoded>> {
        // Without long-term references a recovery frame is a keyframe.
        let keyframe = control.keyframe || (control.use_ltr.is_some() && self.ltr_slots() == 0);
        if keyframe && let Self::Software(e) = self {
            e.request_keyframe();
        }
        ensure!(
            !cancel.load(std::sync::atomic::Ordering::Acquire),
            "{}",
            t().system.encode_cancelled
        );
        match self {
            Self::Software(e) => e.encode(texture, timestamp, keyframe, control, cancel),
            Self::Nvidia(e) => e.encode(texture, timestamp, control),
            Self::Amd(e) => e.encode(texture, timestamp, keyframe, control, cancel),
            Self::Intel(e) => e.encode(texture, timestamp, keyframe, control, cancel),
        }
    }
}
