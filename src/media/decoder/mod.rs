use crate::media::decode_api::{DecodeError, DecoderMode, VideoDecoderConfig};
use anyhow::{Context, Result, anyhow, bail};
use bytes::Bytes;
use yuv::{YuvBiPlanarImage, YuvConversionMode, YuvRange, YuvStandardMatrix, yuv_nv12_to_rgba};

use crate::media::video_color::{ColorMatrix, RenderColor};
use crate::media::{ConnectionMediaProfile, VideoCodec};
use crate::protocol::capability::{CodecCapability, DeviceCapability, QUALITY_DIMENSIONS};
use crate::transport::rtc::EncodedVideoFrame;

pub(crate) mod diagnostics;
pub(crate) mod software_slot;

use crate::platform::surface as windows_surface;

#[derive(Debug)]
pub(crate) struct DecodedFrame {
    pub pts: i64,
    pub width: u32,
    pub height: u32,
    pub surface: DecodedSurface,
    pub ready_at: std::time::Instant,
}

#[derive(Default)]
pub(crate) struct DecodedBatch {
    pub frames: Vec<DecodedFrame>,
    pub input_error: Option<anyhow::Error>,
    pub output_issues: Vec<DecoderOutputIssue>,
}

pub(crate) enum DecoderOutputIssue {
    Dropped(i64),
    Failed {
        token: Option<i64>,
        error: anyhow::Error,
    },
}

#[derive(Debug)]
#[cfg_attr(
    not(windows),
    allow(dead_code, reason = "Only the Windows decoder produces GPU surfaces.")
)]
pub(crate) enum DecodedSurface {
    CpuNv12(Bytes),

    CpuI444(Bytes),

    /// A dmabuf the renderer imports instead of copying through the CPU.
    #[cfg(not(windows))]
    Dmabuf(crate::platform::linux::dmabuf::DmabufFrame),

    D3D11(windows_surface::D3D11Surface),
}

#[derive(Debug)]
#[cfg_attr(
    not(windows),
    allow(dead_code, reason = "Only the Windows presenter draws GPU surfaces.")
)]
pub(crate) enum RenderSurface {
    CpuRgba8(Vec<Rgba8>),

    /// Packed NV12 handed to the renderer as-is; the shader converts it.
    /// Doing that on the GPU saves a full-frame CPU conversion per picture.
    #[cfg(not(windows))]
    CpuNv12 {
        data: Bytes,
        color: RenderColor,
    },

    /// A dmabuf-backed NV12 picture; the renderer imports the planes directly.
    #[cfg(not(windows))]
    Dmabuf {
        frame: crate::platform::linux::dmabuf::DmabufFrame,
        color: RenderColor,
    },

    D3D11(windows_surface::D3D11Surface),
}

#[repr(C, align(4))]
#[derive(Clone, Copy, Debug, bytemuck::Pod, bytemuck::Zeroable)]
pub(crate) struct Rgba8(pub [u8; 4]);

impl DecodedSurface {
    // Associate the decoder token with its input frame before choosing color.
    // CPU conversion remains on the decoder worker, outside the frame-queue lock.
    pub(crate) fn prepare(
        self,
        width: u32,
        height: u32,
        color: RenderColor,
    ) -> Result<RenderSurface> {
        match self {
            Self::CpuI444(data) => {
                i444_to_rgba_pixels(width, height, &data, color).map(RenderSurface::CpuRgba8)
            }
            #[cfg(windows)]
            Self::CpuNv12(data) => {
                nv12_to_rgba_pixels(width, height, &data, color).map(RenderSurface::CpuRgba8)
            }
            // The Linux renderer samples NV12 directly; only validate the layout.
            #[cfg(not(windows))]
            Self::CpuNv12(data) => {
                nv12_layout(width, height, &data)?;
                Ok(RenderSurface::CpuNv12 { data, color })
            }

            // Already in the renderer's format: the shader samples it as NV12.
            #[cfg(not(windows))]
            Self::Dmabuf(frame) => Ok(RenderSurface::Dmabuf { frame, color }),

            Self::D3D11(surface) => Ok(RenderSurface::D3D11(surface)),
        }
    }
}

pub(crate) struct NativeVideoDecoder {
    decoder: Box<PlatformDecoder>,
    frame_reader: FrameReader,
    candidate: DecoderCandidate,
    label: String,

    software_slot: Option<std::rc::Rc<software_slot::SoftwareSlot>>,
}

/// Local implementation choices, never serialized as invented UU decoder IDs.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum DecoderCandidate {
    #[cfg(windows)]
    WindowsD3d11 {
        adapter: u64,
    },
    /// VA-API through the local driver: NVDEC, Intel or AMD.
    #[cfg(not(windows))]
    LinuxVaapi,

    Software,
}

impl DecoderCandidate {
    #[cfg_attr(
        not(windows),
        allow(unused_variables, reason = "Only D3D11 writers name an adapter.")
    )]
    pub(crate) fn preferred(
        codec: VideoCodec,
        preference: crate::media::selection::DecoderPreference,
        preferred: Option<&windows_surface::D3D11SurfaceWriter>,
    ) -> Vec<Self> {
        let mut candidates = Vec::new();
        if preference.mode.hardware() {
            // VA-API decodes on the driver's device; there is no adapter list.
            #[cfg(not(windows))]
            candidates.push(Self::LinuxVaapi);
            #[cfg(windows)]
            {
                if let Some(writer) = preferred {
                    candidates.push(Self::WindowsD3d11 {
                        adapter: writer.adapter_id(),
                    });
                }
                match windows_surface::D3D11SurfaceWriter::adapter_ids() {
                    Ok(adapters) => {
                        for adapter in adapters {
                            let candidate = Self::WindowsD3d11 { adapter };
                            if !candidates.contains(&candidate) {
                                candidates.push(candidate);
                            }
                        }
                    }
                    Err(error) => tracing::warn!(%error, "decoder adapter enumeration failed"),
                }
            }
        }

        if let Some(adapter) = crate::media::selection::resolve_gpu(preference.gpu) {
            candidates.sort_by_key(|candidate| candidate.adapter() != Some(adapter));
        }
        if preference.mode.software() && matches!(codec, VideoCodec::H264 | VideoCodec::Av1) {
            candidates.push(Self::Software);
        }

        candidates
    }

    /// The GPU adapter a hardware candidate decodes on, where the platform
    /// names one.
    pub(crate) const fn adapter(&self) -> Option<u64> {
        match self {
            #[cfg(windows)]
            Self::WindowsD3d11 { adapter } => Some(*adapter),
            _ => None,
        }
    }
}

impl NativeVideoDecoder {
    pub(crate) fn open(
        codec: VideoCodec,
        width: u32,
        height: u32,
        frame_rate: u32,
        preference: crate::media::selection::DecoderPreference,
        extra_data: Bytes,
    ) -> Result<Self> {
        Self::open_inner(
            codec, width, height, frame_rate, preference, extra_data, None,
        )
    }

    pub(crate) fn open_with_surface_writer(
        codec: VideoCodec,
        width: u32,
        height: u32,
        frame_rate: u32,
        preference: crate::media::selection::DecoderPreference,
        extra_data: Bytes,
        surface_writer: windows_surface::D3D11SurfaceWriter,
    ) -> Result<Self> {
        Self::open_inner(
            codec,
            width,
            height,
            frame_rate,
            preference,
            extra_data,
            Some(surface_writer),
        )
    }

    fn open_inner(
        codec: VideoCodec,
        width: u32,
        height: u32,
        frame_rate: u32,
        preference: crate::media::selection::DecoderPreference,
        extra_data: Bytes,
        surface_writer: Option<windows_surface::D3D11SurfaceWriter>,
    ) -> Result<Self> {
        let mut last_error = None;
        for candidate in DecoderCandidate::preferred(codec, preference, surface_writer.as_ref()) {
            match Self::open_candidate(
                candidate,
                codec,
                width,
                height,
                frame_rate,
                extra_data.clone(),
                surface_writer.clone(),
                None,
            ) {
                Ok(mut decoder) => {
                    decoder.describe_selection(preference, last_error.is_some());
                    return Ok(decoder);
                }
                Err(error) => {
                    tracing::debug!(?candidate, %error, "native decoder candidate unavailable");
                    last_error = Some(error);
                }
            }
        }
        Err(last_error.unwrap_or_else(|| DecodeError::NoBackend.into()))
    }

    #[allow(clippy::too_many_arguments)]
    #[cfg_attr(
        not(windows),
        allow(
            unused_variables,
            reason = "The D3D11 writer and its format probe are Windows-only."
        )
    )]
    pub(crate) fn open_candidate(
        candidate: DecoderCandidate,
        codec: VideoCodec,
        width: u32,
        height: u32,
        frame_rate: u32,
        extra_data: Bytes,
        surface_writer: Option<windows_surface::D3D11SurfaceWriter>,
        software_slot: Option<std::rc::Rc<software_slot::SoftwareSlot>>,
    ) -> Result<Self> {
        if width == 0 || height == 0 || frame_rate == 0 {
            return Err(DecodeError::InvalidInput.into());
        }
        let format = crate::media::video_format::parse_stream_format(codec, &extra_data);
        let depth = format.map_or(8, |f| f.bit_depth_luma);
        let chroma = format.map_or(1, |f| f.chroma_format_idc);
        match candidate {
            #[cfg(not(windows))]
            DecoderCandidate::LinuxVaapi => {
                let config = decoder_config(
                    codec,
                    width,
                    height,
                    DecoderMode::Hardware,
                    None,
                    extra_data,
                );
                let decoder = PlatformDecoder::open(&config)?;
                Ok(Self {
                    decoder: Box::new(decoder),
                    frame_reader: FrameReader::Cpu,
                    candidate,
                    label: format!("{} 硬解", platform_label()),
                    software_slot: None,
                })
            }
            #[cfg(windows)]
            DecoderCandidate::WindowsD3d11 { adapter } => {
                let supports = |writer: &windows_surface::D3D11SurfaceWriter| {
                    PlatformDecoder::probe_format(
                        writer.device_handle(),
                        codec,
                        width,
                        height,
                        depth,
                        chroma,
                    )
                };
                let writer = match surface_writer.filter(|writer| {
                    writer.adapter_id() == adapter
                        && unsafe { writer.device_handle().GetDeviceRemovedReason().is_ok() }
                }) {
                    Some(writer) => writer,
                    None => windows_surface::D3D11SurfaceWriter::for_adapter(adapter)?,
                };
                if !supports(&writer) {
                    return Err(DecodeError::Unsupported.into());
                }
                Self::open_windows_hardware(codec, width, height, writer, extra_data)
            }

            DecoderCandidate::Software => {
                if !(openuuyc_codec::Format {
                    codec,
                    chroma,
                    depth,
                })
                .can_decode()
                {
                    return Err(DecodeError::Unsupported.into());
                }
                let software_slot =
                    Some(software_slot.map_or_else(software_slot::SoftwareSlot::acquire, Ok)?);
                let config = decoder_config(
                    codec,
                    width,
                    height,
                    DecoderMode::Software,
                    None,
                    extra_data,
                );
                let decoder = PlatformDecoder::open(&config)?;
                // Windows uploads AV1's packed formats through its texture
                // writer. The Linux presenter draws every software format it
                // advertises straight from the CPU.
                #[cfg(windows)]
                let frame_reader = if codec == VideoCodec::Av1 {
                    FrameReader::Windows(
                        surface_writer.map_or_else(windows_surface::D3D11SurfaceWriter::new, Ok)?,
                    )
                } else {
                    FrameReader::Cpu
                };
                #[cfg(not(windows))]
                let frame_reader = {
                    let _ = surface_writer;
                    FrameReader::Cpu
                };
                Ok(Self {
                    decoder: Box::new(decoder),
                    frame_reader,
                    candidate,
                    label: "OpenUUYC 软件解码".to_owned(),

                    software_slot,
                })
            }
        }
    }

    pub(crate) const fn candidate(&self) -> DecoderCandidate {
        self.candidate
    }
    pub(crate) fn describe_selection(
        &mut self,
        preference: crate::media::selection::DecoderPreference,
        recovered: bool,
    ) {
        use crate::media::selection::{ProcessingMode, resolve_gpu};
        let note = if recovered {
            Some("原候选不可用，已回退")
        } else if preference.mode == ProcessingMode::Software {
            None
        } else if let Some(id) = preference.gpu {
            match resolve_gpu(Some(id)) {
                None => Some("首选显卡不可用，自动选择"),
                Some(adapter) if self.candidate.adapter() != Some(adapter) => {
                    Some("首选显卡不支持本次格式，已回退")
                }
                _ => Some("首选显卡"),
            }
        } else {
            None
        };
        if let Some(note) = note {
            self.label.push_str(&format!("（{note}）"));
        }
        tracing::info!(preference=?preference, actual=?self.candidate, reason=note.unwrap_or("selected"), "decoder preference resolved");
    }

    pub(crate) fn is_software(&self) -> bool {
        self.software_slot.is_some()
    }

    pub(crate) fn software_slot(&self) -> Option<std::rc::Rc<software_slot::SoftwareSlot>> {
        self.software_slot.clone()
    }

    pub(crate) fn surface_writer(&self) -> Option<windows_surface::D3D11SurfaceWriter> {
        match &self.frame_reader {
            FrameReader::Windows(writer) => Some(writer.clone()),
            FrameReader::Cpu => None,
        }
    }

    #[cfg(windows)]
    fn open_windows_hardware(
        codec: VideoCodec,
        width: u32,
        height: u32,
        reader: windows_surface::D3D11SurfaceWriter,
        extra_data: Bytes,
    ) -> Result<Self> {
        let config = decoder_config(
            codec,
            width,
            height,
            DecoderMode::Hardware,
            Some(reader.device_handle()),
            extra_data,
        );
        let backend = PlatformDecoder::open(&config)?;
        let adapter = reader.adapter_id();
        tracing::info!(
            adapter,
            ?codec,
            width,
            height,
            "opened hardware decoder on adapter"
        );
        Ok(Self {
            decoder: Box::new(backend),
            frame_reader: FrameReader::Windows(reader),
            candidate: DecoderCandidate::WindowsD3d11 { adapter },
            label: format!(
                "{} · DXVA11 硬解",
                crate::platform::capture::encoding_adapters()
                    .ok()
                    .and_then(|items| items.into_iter().find(|a| a.luid == adapter))
                    .map_or_else(|| platform_label().into(), |a| a.name)
            ),
            software_slot: None,
        })
    }

    pub(crate) fn label(&self) -> &str {
        &self.label
    }

    pub(crate) fn set_notification(
        &mut self,
        notification: crate::media::decode_api::DecoderNotification,
    ) {
        self.decoder.set_notification(notification);
    }

    pub(crate) fn reset_for_keyframe(&mut self) -> Result<()> {
        self.decoder
            .reset_for_keyframe()
            .with_context(|| format!("reset {} decoder for keyframe cutover", self.label))
    }

    pub(crate) fn push(&mut self, frame: EncodedVideoFrame, decode_token: i64) -> DecodedBatch {
        let input_error = self
            .decoder
            .push_packet(&frame.data, decode_token)
            .context("submit encoded video frame to native decoder")
            .err();
        let mut batch = poll_platform_decoder(&mut self.decoder, &mut self.frame_reader);
        batch.input_error = input_error;
        batch
    }

    pub(crate) fn poll(&mut self) -> DecodedBatch {
        poll_platform_decoder(&mut self.decoder, &mut self.frame_reader)
    }
}

#[cfg_attr(
    not(windows),
    allow(
        unused_variables,
        reason = "Only the GPU arm, which is Windows-only, reads the frame reader."
    )
)]
fn poll_platform_decoder(
    decoder: &mut PlatformDecoder,
    frame_reader: &mut FrameReader,
) -> DecodedBatch {
    let mut batch = DecodedBatch::default();

    #[cfg(not(windows))]
    use crate::platform::decoder::{CpuFormat, PlatformDecodedFrame};
    #[cfg(windows)]
    use crate::platform::decoder::{
        WindowsCpuFormat as CpuFormat, WindowsDecodedFrame as PlatformDecodedFrame,
    };
    loop {
        let frame = match decoder
            .poll_owned_frame()
            .context("poll owning Windows decoded frame")
        {
            Ok(Some(frame)) => frame,
            Ok(None) => break,
            Err(error) => {
                batch
                    .output_issues
                    .push(DecoderOutputIssue::Failed { token: None, error });
                break;
            }
        };
        let ready_at = std::time::Instant::now();
        let (pts, width, height, surface) = match frame {
            #[cfg(windows)]
            PlatformDecodedFrame::Gpu(frame) => (
                frame.pts(),
                frame.width(),
                frame.height(),
                match frame_reader {
                    FrameReader::Windows(writer) => writer
                        .wrap_decoded_surface(frame)
                        .map(DecodedSurface::D3D11),
                    _ => Err(anyhow!("GPU decoder output has no owning device")),
                },
            ),
            #[cfg(not(windows))]
            PlatformDecodedFrame::Dmabuf(frame) => {
                let surface = Ok(DecodedSurface::Dmabuf(frame.frame));
                (frame.pts, frame.width, frame.height, surface)
            }
            PlatformDecodedFrame::Cpu(frame) => {
                let surface = if let FrameReader::Windows(writer) = frame_reader {
                    writer.upload_cpu(&frame).map(DecodedSurface::D3D11)
                } else {
                    match frame.format {
                        CpuFormat::Nv12 => nv12_layout(frame.width, frame.height, &frame.data)
                            .map(|_| DecodedSurface::CpuNv12(frame.data)),
                        CpuFormat::I444 => Ok(DecodedSurface::CpuI444(frame.data)),
                        #[allow(unreachable_patterns, reason = "Linux decodes only these two.")]
                        _ => Err(anyhow!("software video format requires a texture uploader")),
                    }
                };
                (frame.pts, frame.width, frame.height, surface)
            }
        };
        match surface {
            Ok(surface) => batch.frames.push(DecodedFrame {
                pts,
                width,
                height,
                surface,
                ready_at,
            }),
            Err(error) => batch.output_issues.push(DecoderOutputIssue::Failed {
                token: Some(pts),
                error,
            }),
        }
    }
    while let Some(token) = decoder.poll_dropped_token() {
        batch.output_issues.push(DecoderOutputIssue::Dropped(token));
    }
    batch
}

pub(crate) fn detect_native_decoder_support(
    profile: ConnectionMediaProfile,
    stop: tokio_util::sync::CancellationToken,
) -> Result<DeviceCapability> {
    std::thread::spawn(move || {
        anyhow::ensure!(!stop.is_cancelled(), "native capability probe cancelled");
        let mut capabilities = Vec::new();
        if profile.decoder.mode.hardware()
            && let Ok(probe) = hardware_probe(stop.clone())
        {
            for (codec, id) in [
                (VideoCodec::H264, 1),
                (VideoCodec::H265, 2),
                (VideoCodec::Av1, 5),
            ] {
                if !profile.codec.accepts(codec) {
                    continue;
                }
                for chroma in [1, 3] {
                    for depth in [8, 10] {
                        for &(width, height) in QUALITY_DIMENSIONS[1..].iter().rev() {
                            anyhow::ensure!(
                                !stop.is_cancelled(),
                                "native capability probe cancelled"
                            );
                            if probe(codec, width as u32, height as u32, depth, chroma) {
                                capabilities.push(CodecCapability {
                                    video_codec: id,
                                    width,
                                    height,
                                    chroma_sampling: chroma,
                                    bit_depth: depth,
                                    codec_impl: 32,
                                });
                                break;
                            }
                        }
                    }
                }
            }
        }
        if profile.decoder.mode.software() && profile.codec.accepts(VideoCodec::H264) {
            // streamer 958290: this is the advertised software ceiling, not
            // an arbitrary decoder rejection of a larger hardware-fallback AU.
            for chroma_sampling in [1, 3] {
                capabilities.push(CodecCapability {
                    video_codec: 1,
                    width: 1920,
                    height: 1080,
                    chroma_sampling,
                    bit_depth: 8,
                    codec_impl: 37,
                });
            }
        }
        // The Linux presenter draws software frames as NV12, which the AV1
        // decoder produces for 8-bit 4:2:0 only.
        #[cfg(not(windows))]
        if profile.decoder.mode.software() && profile.codec.accepts(VideoCodec::Av1) {
            capabilities.push(CodecCapability {
                video_codec: 5,
                width: 1920,
                height: 1080,
                chroma_sampling: 1,
                bit_depth: 8,
                codec_impl: 37,
            });
        }
        #[cfg(windows)]
        if profile.decoder.mode.software() && profile.codec.accepts(VideoCodec::Av1) {
            if let Ok(writer) = windows_surface::D3D11SurfaceWriter::new() {
                for chroma in [1, 3] {
                    for depth in [8, 10] {
                        anyhow::ensure!(!stop.is_cancelled(), "native capability probe cancelled");
                        // Advertise support independently of other codecs. Only
                        // the common capabilities can establish a usable hardware
                        // alternative; DualCapability selects that after negotiation.
                        if writer.supports_software_av1(depth, chroma) {
                            capabilities.push(CodecCapability {
                                video_codec: 5,
                                width: 1920,
                                height: 1080,
                                chroma_sampling: chroma,
                                bit_depth: depth,
                                codec_impl: 37,
                            });
                        }
                    }
                }
            }
        }

        anyhow::ensure!(!stop.is_cancelled(), "native capability probe cancelled");
        if capabilities.is_empty() {
            bail!("no decoder supports the selected codec and decoding mode");
        }
        Ok(DeviceCapability {
            preferred_codec: profile.codec.preferred(),
            openuuyc_sdr_10bit: true,
            ice_id: String::new(),
            display_info: crate::platform::display_hdr::capabilities(
                profile.local_display.refresh_hz,
            ),
            video_codec_capability: capabilities,
        })
    })
    .join()
    .map_err(|_| anyhow!("native capability probe thread panicked"))?
}

/// A closure that answers whether the platform decodes a format in hardware.
/// On Windows every D3D11 device is asked; on Linux the VA-API driver is.
#[cfg(windows)]
fn hardware_probe(
    stop: tokio_util::sync::CancellationToken,
) -> Result<impl Fn(VideoCodec, u32, u32, u8, u8) -> bool> {
    let writers = windows_surface::D3D11SurfaceWriter::available()?;
    Ok(move |codec, width, height, depth, chroma| {
        writers.iter().any(|writer| {
            !stop.is_cancelled()
                && PlatformDecoder::probe_format(
                    writer.device_handle(),
                    codec,
                    width,
                    height,
                    depth,
                    chroma,
                )
        })
    })
}

#[cfg(not(windows))]
fn hardware_probe(
    stop: tokio_util::sync::CancellationToken,
) -> Result<impl Fn(VideoCodec, u32, u32, u8, u8) -> bool> {
    Ok(move |codec, width, height, depth, chroma| {
        !stop.is_cancelled()
            && crate::platform::decoder::probe_hardware(codec, width, height, depth, chroma)
    })
}

fn decoder_config(
    codec: VideoCodec,
    width: u32,
    height: u32,
    mode: DecoderMode,
    gpu_device: Option<crate::platform::decoder::GpuDevice>,
    extra_data: Bytes,
) -> VideoDecoderConfig {
    VideoDecoderConfig {
        codec,
        width,
        height,
        mode,
        gpu_device,
        extra_data,
    }
}

fn nv12_layout(width: u32, height: u32, nv12: &[u8]) -> Result<(usize, usize)> {
    if width == 0 || height == 0 || !width.is_multiple_of(2) || !height.is_multiple_of(2) {
        bail!("packed NV12 requires positive even dimensions, got {width}x{height}");
    }
    let width_usize = usize::try_from(width).context("decoded width does not fit usize")?;
    let height_usize = usize::try_from(height).context("decoded height does not fit usize")?;
    let luma_len = width_usize
        .checked_mul(height_usize)
        .context("decoded luma dimensions overflow")?;
    let expected_len = luma_len
        .checked_add(luma_len / 2)
        .context("decoded NV12 dimensions overflow")?;
    if nv12.len() < expected_len {
        bail!(
            "native decoder returned short NV12 frame: {} bytes for {}x{} (need {})",
            nv12.len(),
            width,
            height,
            expected_len
        );
    }
    Ok((luma_len, expected_len))
}

fn i444_to_rgba_pixels(
    width: u32,
    height: u32,
    data: &[u8],
    color: RenderColor,
) -> Result<Vec<Rgba8>> {
    let plane = (width as usize)
        .checked_mul(height as usize)
        .context("I444 dimensions overflow")?;
    if plane == 0 || plane.checked_mul(3) != Some(data.len()) {
        bail!(
            "invalid packed I444 frame {width}x{height}: {} bytes",
            data.len()
        );
    }
    let mut pixels = vec![Rgba8([0, 0, 0, 255]); plane];
    let image = yuv::YuvPlanarImage {
        y_plane: &data[..plane],
        y_stride: width,
        u_plane: &data[plane..2 * plane],
        u_stride: width,
        v_plane: &data[2 * plane..],
        v_stride: width,
        width,
        height,
    };
    yuv::yuv444_to_rgba(
        &image,
        bytemuck::cast_slice_mut(&mut pixels),
        width.checked_mul(4).context("RGBA stride overflow")?,
        if color.full_range {
            YuvRange::Full
        } else {
            YuvRange::Limited
        },
        match color.matrix {
            ColorMatrix::Bt601 => YuvStandardMatrix::Bt601,
            ColorMatrix::Bt709 => YuvStandardMatrix::Bt709,
            ColorMatrix::Bt2020 => YuvStandardMatrix::Bt2020,
        },
    )
    .map_err(|error| anyhow!("convert I444 to RGBA: {error}"))?;
    Ok(pixels)
}

#[cfg_attr(
    not(windows),
    allow(dead_code, reason = "The Linux renderer converts NV12 on the GPU.")
)]
fn nv12_to_rgba_pixels(
    width: u32,
    height: u32,
    nv12: &[u8],
    color: RenderColor,
) -> Result<Vec<Rgba8>> {
    let (luma_len, expected_len) = nv12_layout(width, height, nv12)?;
    let pixel_count = luma_len;
    let mut pixels = vec![Rgba8([0, 0, 0, 255]); pixel_count];
    let image = YuvBiPlanarImage {
        y_plane: &nv12[..luma_len],
        y_stride: width,
        uv_plane: &nv12[luma_len..expected_len],
        uv_stride: width,
        width,
        height,
    };
    yuv_nv12_to_rgba(
        &image,
        bytemuck::cast_slice_mut(&mut pixels),
        width
            .checked_mul(4)
            .context("decoded RGBA stride overflow")?,
        if color.full_range {
            YuvRange::Full
        } else {
            YuvRange::Limited
        },
        match color.matrix {
            ColorMatrix::Bt601 => YuvStandardMatrix::Bt601,
            ColorMatrix::Bt709 => YuvStandardMatrix::Bt709,
            ColorMatrix::Bt2020 => YuvStandardMatrix::Bt2020,
        },
        YuvConversionMode::Balanced,
    )
    .map_err(|error| anyhow!("convert native NV12 frame to RGBA: {error}"))?;
    Ok(pixels)
}

#[cfg(windows)]
type PlatformDecoder = crate::platform::decoder::WindowsVideoDecoder;
#[cfg(not(windows))]
type PlatformDecoder = crate::platform::decoder::LinuxVideoDecoder;

fn platform_label() -> &'static str {
    #[cfg(windows)]
    {
        "DXVA11"
    }
    #[cfg(not(windows))]
    {
        "VA-API"
    }
}

enum FrameReader {
    Cpu,
    #[cfg_attr(
        not(windows),
        allow(dead_code, reason = "Only the D3D11 decoder owns surfaces.")
    )]
    Windows(windows_surface::D3D11SurfaceWriter),
}
