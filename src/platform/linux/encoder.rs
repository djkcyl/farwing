//! Linux host encoding: NVENC on the GPU driving the display when the driver
//! offers it, then VA-API H.264 (AMD and Intel), the Rust H.264 software codec
//! otherwise. As on Windows, hardware comes first and every candidate is proven
//! on the selected desktop by encoding a frame and checking the SPS it
//! produced; VA-API output is also decoded, see [`vaapi_backend`].
use super::capture::{Desktop, Device, Image};
use super::cuda;
use crate::media::encoding::{Backend, Capability, Codec, Color, Format, QualityTarget, Rate};
use anyhow::{Context, Result, bail, ensure};

pub(crate) use crate::media::encoding::{Encoded, FrameTiming};

pub(crate) fn probe(
    desktop: &mut Desktop,
    is_active: impl Fn() -> bool,
) -> Result<Vec<Capability>> {
    let mut frame = None;
    for _ in 0..20 {
        if !is_active() {
            bail!("被控准备已取消");
        }
        if let Some(captured) = desktop.next(50, 1, false, false, (1280, 720))? {
            frame = Some(captured);
            break;
        }
    }
    let frame = frame.context("尚未取得所选桌面画面")?;
    let mut candidates = Vec::new();
    if crate::media::encoding::nvenc::Api::load().is_ok() {
        for codec in [Codec::H264, Codec::H265, Codec::Av1] {
            for chroma in [1, 3] {
                let format = Format {
                    codec,
                    chroma,
                    depth: 8,
                };
                if Backend::Nvidia.accepts(format) && super::nvenc::Encoder::accepts(format) {
                    candidates.push((Backend::Nvidia, format));
                }
            }
        }
    }
    // After NVENC: on a hybrid machine the default VA device is usually the
    // integrated GPU, while NvFBC frames already sit in the NVIDIA GPU's memory.
    let vaapi = vaapi_backend();
    if let Some(backend) = vaapi {
        candidates.push((backend, Format::AVC));
    }
    candidates.push((Backend::Software, Format::AVC));
    let rate = Rate {
        target: 2_000_000,
        peak: 2_000_000,
        fps: 30,
        quality: 1,
        quality_target: QualityTarget {
            bitrate: 2_000_000,
            fps: 30,
        },
    };
    let mut result = Vec::new();
    for (backend, format) in candidates {
        if !is_active() {
            bail!("被控准备已取消");
        }
        let probe = (|| -> Result<Capability> {
            let size = (frame.width, frame.height);
            let mut encoder = if backend == Backend::Software {
                Encoder::software_format(&desktop.device, size, format, rate)?
            } else {
                Encoder::hardware_format(&desktop.device, backend, size, format, rate, Color::Sdr)?
            };
            let mut output = None;
            for index in 0..20 {
                let encoded = encoder.encode(&frame.image, index * 333_333, true)?;
                output = encoded.iter().find_map(|encoded| {
                    crate::media::video_format::parse_stream_format(
                        format.codec.media(),
                        &encoded.data,
                    )
                });
                if output.is_some() {
                    if Some(backend) == vaapi {
                        decodes(&encoded, size)?;
                    }
                    break;
                }
            }
            let output = output.context("编码器未输出可验证的序列头")?;
            ensure!(
                output.chroma_format_idc == format.chroma
                    && output.bit_depth_luma == format.depth
                    && output.bit_depth_chroma == format.depth
                    && (output.visible_width, output.visible_height) == size,
                "编码器实际码流与请求格式不一致"
            );
            Ok(Capability {
                adapter: desktop.screen.adapter,
                backend,
                format,
                maximum: encoder.maximum_size(),
            })
        })();
        match probe {
            Ok(capability) => {
                tracing::debug!(?capability, "host encoder capability verified from SPS");
                result.push(capability);
            }
            Err(error) => {
                tracing::debug!(?backend, ?format, %error, "host encoder capability rejected")
            }
        }
    }
    ensure!(!result.is_empty(), "所选桌面没有可用的视频编码器");
    Ok(result)
}

/// The backend a VA-API H.264 encoder is reported as, if the driver has one.
///
/// The shared `Backend` names vendors, not APIs: the implementation code the
/// peer sees is AMD's or Intel's, as on Windows, even though the encoder here
/// is VA-API rather than AMF or QSV. Drivers that are neither (none known to
/// encode) count as Intel.
fn vaapi_backend() -> Option<Backend> {
    if !super::vaapi_encoder::Encoder::accepts(Format::AVC) || !super::decoder::encoder_available()
    {
        return None;
    }
    let vendor = super::decoder::shared_display()
        .ok()?
        .query_vendor_string()
        .unwrap_or_default();
    tracing::debug!(%vendor, "VA-API driver offers H.264 encoding");
    let amd = ["AMD", "Radeon", "radeonsi"]
        .iter()
        .any(|name| vendor.contains(name));
    Some(if amd { Backend::Amd } else { Backend::Intel })
}

/// The VA-API encoder submits SPS/PPS of our own making, so the parsed SPS
/// alone only proves our writer. Decoding the IDR proves the driver's slices
/// agree with those parameter sets.
fn decodes(encoded: &[Encoded], size: (u32, u32)) -> Result<()> {
    let mut decoder = openuuyc_h264::stream::Decoder::new();
    let mut pictures = Vec::new();
    for encoded in encoded {
        pictures.extend(decoder.submit(&encoded.data, 0)?);
    }
    pictures.extend(decoder.finish()?);
    let crop = pictures
        .first()
        .context("VA-API 码流未能解出画面")?
        .picture
        .crop;
    ensure!(
        (crop.width, crop.height) == (size.0 as usize, size.1 as usize),
        "VA-API 码流解出的画面尺寸不符"
    );
    Ok(())
}

/// Whether the host settings offer the GPU scheduling choice. Windows raises
/// the process GPU scheduling class (D3DKMT); Linux drivers have no
/// per-process equivalent, and Sunshine's Linux host sets none either.
pub(crate) const GPU_SCHEDULING: bool = false;

/// Per-thread encoder runtime. Neither encoder needs setup.
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
    /// No GPU scheduling class to hold; see [`GPU_SCHEDULING`].
    pub(crate) fn begin_gpu_work(&mut self, _compatible: bool) {}
    pub(crate) fn end_gpu_work(&mut self) {}
}

pub(crate) enum Encoder {
    Nvidia(super::nvenc::Encoder),
    /// With the vendor the VA driver reported, for the implementation code.
    Vaapi(Box<super::vaapi_encoder::Encoder>, Backend),
    Software(Box<super::software_encoder::Encoder>),
}
impl Encoder {
    /// Only Windows QSV changes size or format in place; the caller rebuilds
    /// every other encoder, as it does for Windows NVENC.
    pub(crate) fn reconfigure_stream(
        &mut self,
        _device: &Device,
        _size: (u32, u32),
        _format: Format,
        _rate: Rate,
        _color: Color,
    ) -> Result<bool> {
        Ok(false)
    }

    /// `backend` is the negotiated candidate's: one display can have both
    /// NVENC and a VA-API driver, so unlike Windows the adapter alone does not
    /// say which encoder the probe proved.
    pub(crate) fn hardware_format(
        _device: &Device,
        backend: Backend,
        size: (u32, u32),
        format: Format,
        rate: Rate,
        color: Color,
    ) -> Result<Self> {
        // The capture backends deliver 8-bit SDR BGRA only.
        ensure!(!color.is_hdr(), "Linux 版暂不支持 HDR 采集");
        match backend {
            Backend::Nvidia => {
                ensure!(
                    super::nvenc::Encoder::accepts(format),
                    "NVENC 的 CUDA 输入不支持该格式"
                );
                let context = cuda::display_context()?;
                Ok(Self::Nvidia(super::nvenc::Encoder::new(
                    &context, size, format, rate,
                )?))
            }
            Backend::Amd | Backend::Intel => {
                let display = super::decoder::shared_display()
                    .map_err(|error| anyhow::anyhow!("VA-API 显示不可用：{error}"))?;
                Ok(Self::Vaapi(
                    Box::new(super::vaapi_encoder::Encoder::new(
                        &display, size, format, rate,
                    )?),
                    backend,
                ))
            }
            Backend::Software => bail!("软件编码不是硬件编码候选"),
        }
    }
    pub(crate) fn software_format(
        _device: &Device,
        size: (u32, u32),
        format: Format,
        rate: Rate,
    ) -> Result<Self> {
        Ok(Self::Software(Box::new(
            super::software_encoder::Encoder::new(size, format, rate)?,
        )))
    }
    pub(crate) fn implementation(&self) -> i32 {
        match self {
            Self::Nvidia(_) => Backend::Nvidia.implementation(),
            Self::Vaapi(_, backend) => backend.implementation(),
            Self::Software(_) => Backend::Software.implementation(),
        }
    }
    pub(crate) fn maximum_size(&self) -> (u32, u32) {
        match self {
            Self::Nvidia(encoder) => encoder.maximum_size(),
            Self::Vaapi(encoder, _) => encoder.maximum_size(),
            Self::Software(encoder) => encoder.maximum_size(),
        }
    }
    pub(crate) fn configure_rate(&mut self, rate: Rate) -> Result<bool> {
        match self {
            Self::Nvidia(encoder) => encoder.configure_rate(rate),
            Self::Vaapi(encoder, _) => Ok(encoder.configure_rate(rate)?),
            Self::Software(encoder) => encoder.configure(rate),
        }
    }
    pub(crate) fn encode(
        &mut self,
        image: &Image,
        timestamp: i64,
        keyframe: bool,
    ) -> Result<Vec<Encoded>> {
        self.encode_cancellable(
            image,
            timestamp,
            keyframe,
            &std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
        )
    }
    pub(crate) fn encode_cancellable(
        &mut self,
        image: &Image,
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
            Self::Nvidia(encoder) => encoder.encode(image, timestamp, keyframe),
            Self::Vaapi(encoder, _) => {
                if let Image::Nv12(frame) = image {
                    return Ok(encoder.encode_nv12(frame, timestamp, keyframe)?);
                }
                let pixels = image.pixels()?;
                let pitch = image.size().0 as usize * 4;
                Ok(encoder.encode(&pixels, pitch, timestamp, keyframe)?)
            }
            Self::Software(encoder) => encoder.encode(image, timestamp, keyframe, cancel),
        }
    }
}

#[cfg(test)]
mod tests {
    /// Needs a running X session: `DISPLAY=:0 cargo test -- --ignored`.
    #[test]
    #[ignore]
    fn probe_captures_and_encodes_the_desktop() {
        let screens = super::super::capture::screens().unwrap();
        println!("{screens:#?}");
        let mut desktop = super::Desktop::open_selected(&screens[0]).unwrap();
        println!("backend {}", desktop.backend_name());
        let started = std::time::Instant::now();
        let caps = super::probe(&mut desktop, || true).unwrap();
        for cap in &caps {
            println!("{cap:?}");
        }
        println!("probed in {:?}", started.elapsed());
    }
}

#[cfg(test)]
mod throughput_tests {
    /// Needs a desktop and an NVIDIA GPU: capture and encode at full size, in
    /// every 4:2:0 format NVENC offers here.
    #[test]
    #[ignore]
    fn capture_and_encode_throughput() {
        use crate::media::encoding::{Codec, Color, Format, Rate};
        const QUALITY: i32 = 4;
        let screens = super::super::capture::screens().unwrap();
        let mut desktop = super::Desktop::open_selected(&screens[0]).unwrap();
        // The highest quality caps the stream at 3840x2160 like the official
        // client; a larger screen is scaled to fit, as in a session.
        let size =
            crate::media::geometry::output_size(screens[0].width, screens[0].height, QUALITY);
        let rate = Rate {
            target: 20_000_000,
            peak: 20_000_000,
            fps: 60,
            quality: QUALITY,
            quality_target: crate::media::encoding::QualityTarget {
                bitrate: 20_000_000,
                fps: 60,
            },
        };
        let mut measured = 0;
        for codec in [Codec::H264, Codec::H265, Codec::Av1] {
            let format = Format {
                codec,
                chroma: 1,
                depth: 8,
            };
            let mut encoder = match super::Encoder::hardware_format(
                &desktop.device,
                crate::media::encoding::Backend::Nvidia,
                size,
                format,
                rate,
                Color::Sdr,
            ) {
                Ok(encoder) => encoder,
                Err(error) => {
                    println!("{codec:?}: not offered here: {error:#}");
                    continue;
                }
            };
            let started = std::time::Instant::now();
            let (mut frames, mut capture, mut encode, mut bytes) = (
                0u32,
                std::time::Duration::ZERO,
                std::time::Duration::ZERO,
                0usize,
            );
            let mut cached = None;
            while started.elapsed() < std::time::Duration::from_secs(3) {
                let t = std::time::Instant::now();
                if let Some(frame) = desktop.next(16, QUALITY, false, false, size).unwrap() {
                    cached = Some(frame);
                }
                capture += t.elapsed();
                let Some(frame) = &cached else { continue };
                let t = std::time::Instant::now();
                for out in encoder
                    .encode(&frame.image, i64::from(frames) * 166_666, frames == 0)
                    .unwrap()
                {
                    bytes += out.data.len();
                }
                encode += t.elapsed();
                frames += 1;
            }
            println!(
                "{} {codec:?} {}x{}: {frames} frames in 3s ({:.0} fps), capture {:.2} ms/frame, encode {:.2} ms/frame, {} KiB",
                desktop.backend_name(),
                size.0,
                size.1,
                f64::from(frames) / 3.0,
                capture.as_secs_f64() * 1000.0 / f64::from(frames.max(1)),
                encode.as_secs_f64() * 1000.0 / f64::from(frames.max(1)),
                bytes / 1024
            );
            measured += 1;
        }
        assert!(measured > 0, "NVENC offered no format here");
    }
}
