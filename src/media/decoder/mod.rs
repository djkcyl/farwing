use crate::media::decode_api::{DecodeError, DecoderMode, VideoDecoderConfig};
use anyhow::{Context, Result, anyhow, bail};
use bytes::Bytes;
use windows::Win32::Graphics::Direct3D11::ID3D11Device;

use crate::media::{ConnectionMediaProfile, VideoCodec};
use farwing_protocol::session::Decoder;

/// The sizes a hardware decoder is probed at, largest first.
pub(crate) const PROBE_SIZES: [(u32, u32); 3] = [(3840, 2160), (2560, 1440), (1920, 1080)];

fn session_codec(codec: VideoCodec) -> farwing_protocol::session::Codec {
    match codec {
        VideoCodec::H264 => farwing_protocol::session::Codec::H264,
        VideoCodec::H265 => farwing_protocol::session::Codec::Hevc,
        VideoCodec::Av1 => farwing_protocol::session::Codec::Av1,
    }
}
use crate::transport::rtc::EncodedVideoFrame;

pub(crate) mod diagnostics;
pub(crate) mod software_slot;

use crate::platform::surface as windows_surface;

#[derive(Debug)]
pub(crate) struct DecodedFrame {
    pub pts: i64,
    pub width: u32,
    pub height: u32,
    pub surface: windows_surface::D3D11Surface,
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

/// What the presenter draws: every decoder's output is a D3D11 texture
/// (software pictures are uploaded on the decoder thread).
pub(crate) type RenderSurface = windows_surface::D3D11Surface;

pub(crate) struct NativeVideoDecoder {
    decoder: Box<WindowsVideoDecoder>,
    /// Wraps hardware output and uploads software pictures.
    writer: windows_surface::D3D11SurfaceWriter,
    candidate: DecoderCandidate,
    label: String,

    software_slot: Option<std::rc::Rc<software_slot::SoftwareSlot>>,
}

/// Local implementation choices, never serialized as invented decoder IDs.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum DecoderCandidate {
    WindowsD3d11 { adapter: u64 },

    Software,
}

impl DecoderCandidate {
    pub(crate) fn preferred(
        codec: VideoCodec,
        preference: crate::media::selection::DecoderPreference,
        preferred: Option<&windows_surface::D3D11SurfaceWriter>,
    ) -> Vec<Self> {
        let mut candidates = Vec::new();
        if preference.mode.hardware() {
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

        if let Some(adapter) = crate::media::selection::resolve_gpu(preference.gpu) {
            candidates.sort_by_key(|candidate| *candidate != Self::WindowsD3d11 { adapter });
        }
        if preference.mode.software() && matches!(codec, VideoCodec::H264 | VideoCodec::Av1) {
            candidates.push(Self::Software);
        }

        candidates
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
            DecoderCandidate::WindowsD3d11 { adapter } => {
                let supports = |writer: &windows_surface::D3D11SurfaceWriter| {
                    WindowsVideoDecoder::probe_format(
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
                if !(farwing_codec::Format {
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
                let decoder = WindowsVideoDecoder::open(&config)?;
                let writer =
                    surface_writer.map_or_else(windows_surface::D3D11SurfaceWriter::new, Ok)?;
                Ok(Self {
                    decoder: Box::new(decoder),
                    writer,
                    candidate,
                    label: crate::i18n::t().session.decoder_software.to_owned(),

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
            Some(crate::i18n::t().session.decoder_note_recovered)
        } else if preference.mode == ProcessingMode::Software {
            None
        } else if let Some(id) = preference.gpu {
            match resolve_gpu(Some(id)) {
                None => Some(crate::i18n::t().session.decoder_note_missing_gpu),
                Some(adapter) if self.candidate != (DecoderCandidate::WindowsD3d11 { adapter }) => {
                    Some(crate::i18n::t().session.selection_gpu_format)
                }
                _ => Some(crate::i18n::t().session.decoder_note_preferred_gpu),
            }
        } else {
            None
        };
        if let Some(note) = note {
            self.label.push_str(&(crate::i18n::t().session.decoder_note)(note));
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
        Some(self.writer.clone())
    }

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
        let backend = WindowsVideoDecoder::open(&config)?;
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
            writer: reader,
            candidate: DecoderCandidate::WindowsD3d11 { adapter },
            label: (crate::i18n::t().session.decoder_hardware)(
                &crate::platform::capture::encoding_adapters()
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
        let mut batch = poll_platform_decoder(&mut self.decoder, &self.writer);
        batch.input_error = input_error;
        batch
    }

    pub(crate) fn poll(&mut self) -> DecodedBatch {
        poll_platform_decoder(&mut self.decoder, &self.writer)
    }
}

fn poll_platform_decoder(
    decoder: &mut WindowsVideoDecoder,
    writer: &windows_surface::D3D11SurfaceWriter,
) -> DecodedBatch {
    let mut batch = DecodedBatch::default();

    use crate::platform::decoder::WindowsDecodedFrame;
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
            WindowsDecodedFrame::Gpu(frame) => (
                frame.pts(),
                frame.width(),
                frame.height(),
                writer.wrap_decoded_surface(frame),
            ),
            WindowsDecodedFrame::Cpu(frame) => {
                (frame.pts, frame.width, frame.height, writer.upload_cpu(&frame))
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

/// What this machine's GPUs decode, and which software formats their
/// texture upload takes. Probing opens a device per adapter and asks the
/// driver about every format (about 100 ms), so it is kept for the same set
/// of adapters (a driver update restarts an adapter under a new LUID) and is
/// started before a connection needs it.
#[derive(Default)]
struct DecoderInventory {
    adapters: Vec<u64>,
    /// Every codec, in advertising order.
    hardware: Option<Vec<(VideoCodec, Decoder)>>,
    software_av1: Option<Vec<Decoder>>,
}

static DECODER_INVENTORY: std::sync::Mutex<Option<DecoderInventory>> =
    std::sync::Mutex::new(None);

/// Probe in the background the hardware decoders a connection with this
/// decoding mode will advertise. (Software AV1 support is only probed when a
/// connection accepts AV1, as before.)
pub(crate) fn prepare_native_decoder_support(
    mode: crate::media::selection::ProcessingMode,
    delay: std::time::Duration,
) {
    if !mode.hardware() {
        return;
    }
    let spawned = std::thread::Builder::new()
        .name("Decoder probe".to_owned())
        .spawn(move || {
            std::thread::sleep(delay);
            let started = std::time::Instant::now();
            decoder_inventory(true, false, |_| ());
            tracing::debug!(
                elapsed_ms = started.elapsed().as_millis() as u64,
                "native decoder capabilities probed in advance"
            );
        });
    if let Err(error) = spawned {
        tracing::debug!(%error, "decoder probe thread unavailable");
    }
}

fn decoder_inventory<T>(
    hardware: bool,
    software_av1: bool,
    read: impl FnOnce(&DecoderInventory) -> T,
) -> T {
    let adapters = windows_surface::D3D11SurfaceWriter::adapter_ids().unwrap_or_default();
    // A connection waits here for a probe already running.
    let mut cached = DECODER_INVENTORY
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if cached.as_ref().is_none_or(|inventory| inventory.adapters != adapters) {
        *cached = Some(DecoderInventory {
            adapters,
            ..Default::default()
        });
    }
    let inventory = cached.as_mut().expect("decoder inventory");
    if hardware && inventory.hardware.is_none() {
        inventory.hardware = Some(probe_hardware_decoders());
    }
    if software_av1 && inventory.software_av1.is_none() {
        inventory.software_av1 = Some(probe_software_av1());
    }
    let value = read(inventory);
    // Nothing found may be a passing driver state: probe again next time.
    if inventory.hardware.as_ref().is_some_and(Vec::is_empty) {
        inventory.hardware = None;
    }
    if inventory.software_av1.as_ref().is_some_and(Vec::is_empty) {
        inventory.software_av1 = None;
    }
    value
}

fn probe_hardware_decoders() -> Vec<(VideoCodec, Decoder)> {
    let mut capabilities = Vec::new();
    let Ok(writers) = windows_surface::D3D11SurfaceWriter::available() else {
        return capabilities;
    };
    for codec in [VideoCodec::H264, VideoCodec::H265, VideoCodec::Av1] {
        for chroma in [1, 3] {
            for depth in [8, 10] {
                for (width, height) in PROBE_SIZES {
                    if writers.iter().any(|writer| {
                        WindowsVideoDecoder::probe_format(
                            writer.device_handle(),
                            codec,
                            width,
                            height,
                            depth,
                            chroma,
                        )
                    }) {
                        capabilities.push((
                            codec,
                            Decoder {
                                codec: session_codec(codec),
                                chroma444: chroma == 3,
                                bit10: depth == 10,
                                width,
                                height,
                                hardware: true,
                            },
                        ));
                        break;
                    }
                }
            }
        }
    }
    capabilities
}

fn probe_software_av1() -> Vec<Decoder> {
    let mut capabilities = Vec::new();
    if let Ok(writer) = windows_surface::D3D11SurfaceWriter::new() {
        for chroma in [1, 3] {
            for depth in [8, 10] {
                if writer.supports_software_av1(depth, chroma) {
                    capabilities.push(Decoder {
                        codec: farwing_protocol::session::Codec::Av1,
                        chroma444: chroma == 3,
                        bit10: depth == 10,
                        width: 1920,
                        height: 1080,
                        hardware: false,
                    });
                }
            }
        }
    }
    capabilities
}

/// The formats this controller decodes, as the session offers them.
pub(crate) fn detect_native_decoder_support(
    profile: ConnectionMediaProfile,
) -> Result<Vec<Decoder>> {
    std::thread::spawn(move || {
        let software_av1 =
            profile.decoder.mode.software() && profile.codec.accepts(VideoCodec::Av1);
        let mut capabilities = Vec::new();
        decoder_inventory(profile.decoder.mode.hardware(), software_av1, |inventory| {
            if profile.decoder.mode.hardware() {
                capabilities.extend(
                    inventory
                        .hardware
                        .iter()
                        .flatten()
                        .filter(|(codec, _)| profile.codec.accepts(*codec))
                        .map(|(_, capability)| capability.clone()),
                );
            }
            if profile.decoder.mode.software() && profile.codec.accepts(VideoCodec::H264) {
                // This is the advertised software ceiling, not
                // an arbitrary decoder rejection of a larger hardware-fallback AU.
                for chroma444 in [false, true] {
                    capabilities.push(Decoder {
                        codec: farwing_protocol::session::Codec::H264,
                        chroma444,
                        bit10: false,
                        width: 1920,
                        height: 1080,
                        hardware: false,
                    });
                }
            }
            if software_av1 {
                capabilities.extend(inventory.software_av1.iter().flatten().cloned());
            }
        });

        if capabilities.is_empty() {
            bail!("no decoder supports the selected codec and decoding mode");
        }
        Ok(capabilities)
    })
    .join()
    .map_err(|_| anyhow!("native capability probe thread panicked"))?
}

fn decoder_config(
    codec: VideoCodec,
    width: u32,
    height: u32,
    mode: DecoderMode,
    gpu_device: Option<ID3D11Device>,
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

use crate::platform::decoder::WindowsVideoDecoder;

fn platform_label() -> &'static str {
    "DXVA11"
}

