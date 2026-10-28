//! Intel hardware session selected by D3D11 LUID through the oneVPL dispatcher.
use super::{
    encoder::Encoded,
    format::{Codec, Format, Rate},
    gpu_conversion::Conversion,
    qsv_allocator::Allocator,
};
use crate::i18n::t;
use anyhow::{Context, Result, ensure};
use farwing_vpl_sys as v;
use std::{mem::size_of, ptr};
use windows::{
    Win32::Graphics::{Direct3D11::*, Dxgi::IDXGIDevice},
    core::Interface,
};

fn check(status: v::mfxStatus, operation: &str) -> Result<()> {
    ensure!(
        status >= 0,
        "{}",
        (t().system.qsv_call_failed)(operation, &status.to_string())
    );
    if status > 0 {
        tracing::debug!(status, operation, "QSV adjusted driver parameters");
    }
    Ok(())
}
fn header<T>(id: i32) -> v::mfxExtBuffer {
    v::mfxExtBuffer {
        BufferId: id as u32,
        BufferSz: size_of::<T>() as u32,
    }
}
fn codec(format: Format) -> u32 {
    (match format.codec {
        Codec::H264 => v::MFX_CODEC_AVC,
        Codec::H265 => v::MFX_CODEC_HEVC,
        Codec::Av1 => v::MFX_CODEC_AV1,
    }) as u32
}
fn fourcc(format: Format) -> u32 {
    (match (format.chroma, format.depth) {
        (1, 8) => v::MFX_FOURCC_NV12,
        (1, 10) => v::MFX_FOURCC_P010,
        (3, 8) => v::MFX_FOURCC_AYUV,
        (3, 10) => v::MFX_FOURCC_Y410,
        _ => 0,
    }) as u32
}
struct Session {
    loader: v::mfxLoader,
    session: v::mfxSession,
}
impl Session {
    fn new(device: &ID3D11Device) -> Result<Self> {
        let started = std::time::Instant::now();
        unsafe {
            // QSV may use the immediate context as soon as SetHandle binds it.
            // Preparing this in Allocator::new is too late: that constructor
            // runs only after the session has accepted the D3D11 device.
            let context = device.GetImmediateContext()?;
            let multithread: ID3D11Multithread = context.cast()?;
            let _ = multithread.SetMultithreadProtected(true);
            ensure!(
                multithread.GetMultithreadProtected().as_bool(),
                "{}",
                t().system.qsv_multithread_protection
            );
            let mut result = Self {
                loader: v::MFXLoad(),
                session: ptr::null_mut(),
            };
            ensure!(
                !result.loader.is_null(),
                "{}",
                t().system.qsv_dispatcher_failed
            );
            let dxgi: IDXGIDevice = device.cast()?;
            let mut luid = dxgi.GetAdapter()?.GetDesc()?.AdapterLuid;
            for (name, kind, data) in [
                (
                    b"mfxImplDescription.Impl\0".as_slice(),
                    5,
                    v::mfxVariant_data { U32: 2 },
                ),
                (
                    b"mfxImplDescription.AccelerationMode\0",
                    5,
                    v::mfxVariant_data { U32: 0x300 },
                ),
                (
                    b"mfxExtendedDeviceId.DeviceLUID\0",
                    11,
                    v::mfxVariant_data {
                        Ptr: ptr::from_mut(&mut luid).cast(),
                    },
                ),
            ] {
                let config = v::MFXCreateConfig(result.loader);
                ensure!(!config.is_null(), "{}", t().system.qsv_filter_failed);
                check(
                    v::MFXSetConfigFilterProperty(
                        config,
                        name.as_ptr(),
                        v::mfxVariant {
                            Version: v::mfxStructVersion { Version: 0x101 },
                            Type: kind,
                            Data: data,
                        },
                    ),
                    "SetConfigFilterProperty",
                )?;
            }
            check(
                v::MFXCreateSession(result.loader, 0, &mut result.session),
                "CreateSession",
            )?;
            ensure!(!result.session.is_null(), "{}", t().system.qsv_no_session);
            let status = v::MFXVideoCORE_SetHandle(result.session, 3, device.as_raw());
            if status < 0 {
                // Do not swallow -16 or give the allocator a different device.
                // Record whether the runtime already bound a device so a
                // driver/session ownership error remains distinguishable from
                // an unsupported codec. GetHandle returns an owned COM ref.
                let mut bound = ptr::null_mut();
                let query = v::MFXVideoCORE_GetHandle(result.session, 3, &mut bound);
                let existing =
                    (query >= 0 && !bound.is_null()).then(|| ID3D11Device::from_raw(bound));
                let existing_luid = existing.as_ref().and_then(|device| {
                    let adapter = device.cast::<IDXGIDevice>().ok()?.GetAdapter().ok()?;
                    let id = adapter.GetDesc().ok()?.AdapterLuid;
                    Some((u64::from(id.HighPart as u32) << 32) | u64::from(id.LowPart))
                });
                tracing::warn!(
                    status,
                    query,
                    ?existing_luid,
                    same_device = existing.as_ref().is_some_and(|bound| bound == device),
                    "QSV D3D11 device binding failed after enabling multithread protection"
                );
            }
            check(status, "SetD3D11Device")?;
            check(v::MFXSetPriority(result.session, 2), "SetPriority")?;
            tracing::info!(
                elapsed_ms = started.elapsed().as_secs_f64() * 1000.,
                "QSV dispatcher session created"
            );
            Ok(result)
        }
    }
}
// Session is dropped before callback state, including construction failures.
// Reuse this entire owner only after ENCODE_Close has quiesced the component.
struct Resources {
    session: Session,
    allocator: Box<Allocator>,
    _callbacks: Box<v::mfxFrameAllocator>,
}
impl Resources {
    fn new(device: &ID3D11Device, bind_flags: u32) -> Result<Self> {
        let session = Session::new(device)?;
        let mut allocator = Allocator::new(device, bind_flags)?;
        let callbacks = Box::new(allocator.callbacks());
        let mut resources = Self {
            session,
            allocator,
            _callbacks: callbacks,
        };
        check(
            unsafe {
                v::MFXVideoCORE_SetFrameAllocator(
                    resources.session.session,
                    &mut *resources._callbacks,
                )
            },
            "SetFrameAllocator",
        )?;
        Ok(resources)
    }
}
impl Drop for Session {
    fn drop(&mut self) {
        unsafe {
            if !self.session.is_null() {
                v::MFXClose(self.session);
            }
            if !self.loader.is_null() {
                v::MFXUnload(self.loader);
            }
        }
    }
}
struct Extensions {
    one: Box<v::mfxExtCodingOption>,
    two: Box<v::mfxExtCodingOption2>,
    three: Box<v::mfxExtCodingOption3>,
    color: Box<v::mfxExtVideoSignalInfo>,
    hevc: Box<v::mfxExtHEVCParam>,
    av1: Box<v::mfxExtAV1BitstreamParam>,
    av1_size: Box<v::mfxExtAV1ResolutionParam>,
    pointers: Vec<*mut v::mfxExtBuffer>,
}
impl Clone for Extensions {
    fn clone(&self) -> Self {
        let mut result = Self {
            one: self.one.clone(),
            two: self.two.clone(),
            three: self.three.clone(),
            color: self.color.clone(),
            hevc: self.hevc.clone(),
            av1: self.av1.clone(),
            av1_size: self.av1_size.clone(),
            pointers: Vec::new(),
        };
        for &pointer in &self.pointers {
            let id = unsafe { (*pointer).BufferId };
            for header in [
                &mut result.one.Header,
                &mut result.two.Header,
                &mut result.three.Header,
                &mut result.color.Header,
                &mut result.hevc.Header,
                &mut result.av1.Header,
                &mut result.av1_size.Header,
            ] {
                if header.BufferId == id {
                    result.pointers.push(ptr::from_mut(header));
                    break;
                }
            }
        }
        result
    }
}

#[derive(Debug, thiserror::Error)]
#[error("{}", encode_failure(.0))]
struct EncodeFailure(anyhow::Error);

/// `MFXVideoENCODE_Reset` refused a changed frame rate, quality or buffer
/// size (for example the one-second buffer of cinema mode).
#[derive(Debug, thiserror::Error)]
#[error("{}", reset_rejected(.0))]
struct ResetRejected(v::mfxStatus);

fn encode_failure(error: &anyhow::Error) -> String {
    (t().system.qsv_encode_stage_failed)(&format!("{error:#}"))
}

fn reset_rejected(status: &v::mfxStatus) -> String {
    (t().system.qsv_reset_failed)(&status.to_string())
}

#[derive(Debug, PartialEq, Eq)]
enum SubmitAction {
    Output,
    Grow,
    Retry,
    Failed,
}
fn submit_action(status: v::mfxStatus, has_sync: bool) -> SubmitAction {
    if status >= 0 && has_sync {
        SubmitAction::Output
    } else if status == v::mfxStatus_MFX_ERR_NOT_ENOUGH_BUFFER {
        SubmitAction::Grow
    } else if status > 0 {
        SubmitAction::Retry
    } else {
        SubmitAction::Failed
    }
}

pub(crate) struct Encoder {
    active: Option<Active>,
    device: ID3D11Device,
    size: (u32, u32),
    format: Format,
}
impl Encoder {
    pub fn new(
        device: &ID3D11Device,
        size: (u32, u32),
        format: Format,
        rate: Rate,
        color: super::format::Color,
    ) -> Result<Self> {
        Ok(Self {
            active: Some(Active::new(device, size, format, rate, color)?),
            device: device.clone(),
            size,
            format,
        })
    }
    pub fn maximum_size(&self) -> (u32, u32) {
        self.active.as_ref().map_or(self.size, Active::maximum_size)
    }
    pub fn configure(&mut self, rate: Rate) -> Result<bool> {
        let active = self
            .active
            .as_mut()
            .context(t().system.qsv_encoder_released)?;
        let color = active.conversion.color;
        match active.configure(rate) {
            // Initialize again on the same session, as for a stream change,
            // rather than give up the encoder; the next frame is a keyframe.
            Err(error) if error.is::<ResetRejected>() => {
                tracing::info!(%error, "QSV setting rejected by Reset; initializing again");
                let device = self.device.clone();
                if self.reconfigure_stream(&device, self.size, self.format, rate, color)? {
                    Ok(true)
                } else {
                    Err(error)
                }
            }
            result => result,
        }
    }
    pub fn reconfigure_stream(
        &mut self,
        device: &ID3D11Device,
        size: (u32, u32),
        format: Format,
        rate: Rate,
        color: super::format::Color,
    ) -> Result<bool> {
        let active = self
            .active
            .as_mut()
            .context(t().system.qsv_encoder_released)?;
        if device != &self.device || !active.initialized || active.surface.Data.Locked != 0 {
            return Ok(false);
        }
        let started = std::time::Instant::now();
        let previous = self.size;
        let previous_format = self.format;
        let previous_color = active.conversion.color;
        let maximum = (format == self.format).then_some(active.maximum);
        // A chroma/depth/codec change must select its own conversion path.
        // In particular 4:4:4 uses pixel targets rather than the 4:2:0 UAVs.
        let conversion = if format != self.format {
            Conversion::aligned(device, size, format, 16, color)?
        } else if active.conversion.is_compute() {
            Conversion::compute(device, size, format, 16, color)?
        } else {
            Conversion::pixel(device, size, format, 16, color)?
        };
        let closing = std::time::Instant::now();
        check(
            unsafe { v::MFXVideoENCODE_Close(active.raw()) },
            "Close before stream reconfiguration",
        )?;
        active.initialized = false;
        let close_ms = closing.elapsed().as_secs_f64() * 1000.;
        // The session and callback addresses stay valid while native surfaces
        // are freed. Never recycle a session after failed encoding/driver loss.
        let mut resources = active
            .resources
            .take()
            .context(t().system.qsv_session_closed)?;
        resources
            .allocator
            .prepare_reinitialize(conversion.bind_flags())?;
        drop(self.active.take());
        self.active = Some(Active::create(
            device,
            size,
            format,
            rate,
            conversion,
            None,
            Some((resources, maximum)),
        )?);
        self.size = size;
        self.format = format;
        tracing::info!(
            ?previous,
            ?size,
            ?previous_format,
            ?format,
            ?previous_color,
            ?color,
            close_ms,
            elapsed_ms = started.elapsed().as_secs_f64() * 1000.,
            "QSV stream reconfigured with retained session"
        );
        Ok(true)
    }
    pub fn ltr_slots(&self) -> u8 {
        self.active.as_ref().map_or(0, |active| active.ltr_slots)
    }
    pub fn encode(
        &mut self,
        texture: &ID3D11Texture2D,
        timestamp: i64,
        keyframe: bool,
        control: super::encoder::FrameControl,
        cancel: &std::sync::atomic::AtomicBool,
    ) -> Result<Vec<Encoded>> {
        ensure!(
            !cancel.load(std::sync::atomic::Ordering::Acquire),
            "{}",
            t().system.encode_cancelled
        );
        let active = self
            .active
            .as_mut()
            .context(t().system.qsv_encoder_released)?;
        let compute = active.conversion.is_compute();
        let result = active.encode(texture, timestamp, keyframe, control, cancel);
        match result {
            Err(error)
                if compute
                    && error.is::<EncodeFailure>()
                    && !cancel.load(std::sync::atomic::Ordering::Acquire) =>
            {
                let restored = (
                    active.params,
                    active.extensions.clone(),
                    active.frame_rate.clone(),
                );
                let rate = active.rate;
                let color = active.conversion.color;
                // Close the whole failed encoder BEFORE opening its
                // replacement; the old allocator/callbacks outlive that close.
                drop(self.active.take());
                tracing::debug!(%error, "QSV compute encode failed; rebuilding complete pixel path once");
                let replacement = (|| {
                    Active::create(
                        &self.device,
                        self.size,
                        self.format,
                        rate,
                        Conversion::pixel(&self.device, self.size, self.format, 16, color)?,
                        Some(restored),
                        None,
                    )
                })()
                .map_err(|error: anyhow::Error| {
                    super::encoder::SwitchCandidate((t().system.qsv_pixel_rebuild_failed)(
                        &format!("{error:#}"),
                    ))
                })?;
                self.active = Some(replacement);
                self.active
                    .as_mut()
                    .unwrap()
                    .encode(texture, timestamp, true, Default::default(), cancel)
                    .map_err(unwrap_encode_error)
            }
            result => result.map_err(unwrap_encode_error),
        }
    }
}
fn unwrap_encode_error(error: anyhow::Error) -> anyhow::Error {
    match error.downcast::<EncodeFailure>() {
        Ok(failure) => failure.0,
        Err(error) => error,
    }
}
impl Extensions {
    fn new(
        version: (u16, u16),
        size: (u32, u32),
        format: Format,
        low_power: bool,
        color: super::format::Color,
    ) -> Self {
        // Use the same VBR contract for SDR HEVC8/10. LowDelayBRC adds a strict
        // per-frame size constraint: on tested Iris Xe it undershoots Main10
        // and degrades HEVC8 text/IDR recovery. No-lookahead/no-reordering and
        // the negotiated target/peak remain unchanged; HDR keeps its contract.
        let low_delay_brc = match format.codec {
            Codec::Av1 => 16,
            Codec::H265 if color.is_hdr() && low_power => 16,
            Codec::H265 => 32,
            _ => 0,
        };
        let color = color.space(None);
        let mut result = Self {
            one: Box::new(v::mfxExtCodingOption {
                Header: header::<v::mfxExtCodingOption>(v::MFX_EXTBUFF_CODING_OPTION),
                NalHrdConformance: 32,
                PicTimingSEI: 32,
                VuiNalHrdParameters: 32,
                MaxDecFrameBuffering: 1,
                AUDelimiter: 32,
                ..Default::default()
            }),
            two: Box::new(v::mfxExtCodingOption2 {
                Header: header::<v::mfxExtCodingOption2>(v::MFX_EXTBUFF_CODING_OPTION2),
                MBBRC: 16,
                RepeatPPS: 32,
                LookAheadDepth: 0,
                ..Default::default()
            }),
            three: Box::new(v::mfxExtCodingOption3 {
                Header: header::<v::mfxExtCodingOption3>(v::MFX_EXTBUFF_CODING_OPTION3),
                ScenarioInfo: 8,
                LowDelayBRC: low_delay_brc,
                TargetChromaFormatPlus1: if format.chroma == 3 { 4 } else { 0 },
                TargetBitDepthLuma: format.depth.into(),
                TargetBitDepthChroma: format.depth.into(),
                ..Default::default()
            }),
            color: Box::new(v::mfxExtVideoSignalInfo {
                Header: header::<v::mfxExtVideoSignalInfo>(v::MFX_EXTBUFF_VIDEO_SIGNAL_INFO),
                VideoFormat: 5,
                VideoFullRange: u16::from(color.range == 2),
                ColourDescriptionPresent: 1,
                ColourPrimaries: color.primaries.into(),
                TransferCharacteristics: color.transfer.into(),
                MatrixCoefficients: color.matrix.into(),
            }),
            hevc: Box::new(v::mfxExtHEVCParam {
                Header: header::<v::mfxExtHEVCParam>(v::MFX_EXTBUFF_HEVC_PARAM),
                PicWidthInLumaSamples: size.0.div_ceil(16) as u16 * 16,
                PicHeightInLumaSamples: size.1.div_ceil(16) as u16 * 16,
                ..Default::default()
            }),
            av1: Box::new(v::mfxExtAV1BitstreamParam {
                Header: header::<v::mfxExtAV1BitstreamParam>(v::MFX_EXTBUFF_AV1_BITSTREAM_PARAM),
                WriteIVFHeaders: 32,
                ..Default::default()
            }),
            av1_size: Box::new(v::mfxExtAV1ResolutionParam {
                Header: header::<v::mfxExtAV1ResolutionParam>(v::MFX_EXTBUFF_AV1_RESOLUTION_PARAM),
                FrameWidth: size.0,
                FrameHeight: size.1,
                ..Default::default()
            }),
            pointers: Vec::new(),
        };
        if format.codec != Codec::Av1 {
            result.pointers.push(ptr::from_mut(&mut result.one.Header));
        }
        if format.codec == Codec::Av1 {
            result.pointers.push(ptr::from_mut(&mut result.av1.Header));
            result
                .pointers
                .push(ptr::from_mut(&mut result.av1_size.Header));
        }
        if version >= (1, 8) {
            result.pointers.push(ptr::from_mut(&mut result.two.Header));
        }
        if version >= (1, 16) {
            result
                .pointers
                .push(ptr::from_mut(&mut result.three.Header));
        }
        result
            .pointers
            .push(ptr::from_mut(&mut result.color.Header));
        if format.codec == Codec::H265 && (size.0 % 16 != 0 || size.1 % 16 != 0) {
            result.pointers.push(ptr::from_mut(&mut result.hevc.Header));
        }
        result
    }
    fn bind(&mut self, param: &mut v::mfxVideoParam) {
        param.ExtParam = self.pointers.as_mut_ptr();
        param.NumExtParam = self.pointers.len() as u16;
    }
}
struct Active {
    size: (u32, u32),
    resources: Option<Resources>,
    extensions: Extensions,
    params: v::mfxVideoParam,
    surface: Box<v::mfxFrameSurface1>,
    conversion: Conversion,
    buffer: Vec<u8>,
    rate: Rate,
    frame_rate: super::encoder_rate::Controller,
    maximum: (u32, u32),
    order: u32,
    initialized: bool,
    timestamps: std::collections::VecDeque<(u64, i64)>,
    format: Format,
    /// Long-term reference slots (0 without; `FARWING_QSV_LTR=1` enables
    /// them for H.264 / HEVC until verified on Intel hardware).
    ltr_slots: u8,
    /// Frame order of the picture in each long-term slot.
    long_term: [Option<u32>; 2],
}
impl Active {
    pub fn new(
        device: &ID3D11Device,
        size: (u32, u32),
        format: Format,
        rate: Rate,
        color: super::format::Color,
    ) -> Result<Self> {
        let conversion = Conversion::aligned(device, size, format, 16, color)?;
        let compute = conversion.is_compute();
        match Self::create(device, size, format, rate, conversion, None, None) {
            Ok(encoder) => Ok(encoder),
            Err(error) if compute => {
                tracing::debug!(%error,"QSV compute path rejected; trying pixel path");
                Self::create(
                    device,
                    size,
                    format,
                    rate,
                    Conversion::pixel(device, size, format, 16, color)?,
                    None,
                    None,
                )
            }
            Err(error) => Err(error),
        }
    }
    fn create(
        device: &ID3D11Device,
        size: (u32, u32),
        format: Format,
        rate: Rate,
        conversion: Conversion,
        restored: Option<(
            v::mfxVideoParam,
            Extensions,
            super::encoder_rate::Controller,
        )>,
        reuse: Option<(Resources, Option<(u32, u32)>)>,
    ) -> Result<Self> {
        let started = std::time::Instant::now();
        ensure!(
            format.valid() && size.0 > 0 && size.1 > 0 && size.0 <= 16384 && size.1 <= 16384,
            "{}",
            t().system.qsv_invalid_input_format
        );
        unsafe {
            let reused_session = reuse.is_some();
            let (resources, cached_maximum) = match reuse {
                Some((resources, maximum)) => (resources, maximum),
                None => (Resources::new(device, conversion.bind_flags())?, None),
            };
            let session = &resources.session;
            let mut version = v::mfxVersion::default();
            check(
                v::MFXQueryVersion(session.session, &mut version),
                "QueryVersion",
            )?;
            let version = (
                version.__bindgen_anon_1.Major,
                version.__bindgen_anon_1.Minor,
            );
            let mut params = v::mfxVideoParam {
                AsyncDepth: 1,
                IOPattern: 1,
                ..Default::default()
            };
            let mfx = &mut params.__bindgen_anon_1.mfx;
            mfx.CodecId = codec(format);
            if format.codec == Codec::Av1 {
                ensure!(version >= (2, 5), "{}", t().system.qsv_av1_requires_onevpl);
                mfx.CodecProfile = if format.chroma == 3 {
                    v::MFX_PROFILE_AV1_HIGH
                } else {
                    v::MFX_PROFILE_AV1_MAIN
                } as u16;
            }
            mfx.LowPower = if format.codec == Codec::H265 { 16 } else { 0 };
            mfx.FrameInfo = v::mfxFrameInfo {
                FourCC: fourcc(format),
                ChromaFormat: format.chroma.into(),
                BitDepthLuma: format.depth.into(),
                BitDepthChroma: format.depth.into(),
                Shift: u16::from(format.depth == 10),
                FrameRateExtN: rate.fps,
                FrameRateExtD: 1,
                PicStruct: 1,
                AspectRatioW: 1,
                AspectRatioH: 1,
                ..Default::default()
            };
            mfx.FrameInfo.__bindgen_anon_1.__bindgen_anon_1 =
                v::mfxFrameInfo__bindgen_ty_1__bindgen_ty_1 {
                    Width: size.0.div_ceil(16) as u16 * 16,
                    Height: size.1.div_ceil(16) as u16 * 16,
                    CropW: size.0 as u16,
                    CropH: size.1 as u16,
                    ..Default::default()
                };
            mfx.__bindgen_anon_1.__bindgen_anon_1 = v::mfxInfoMFX__bindgen_ty_1__bindgen_ty_1 {
                TargetUsage: 7,
                GopRefDist: 1,
                GopOptFlag: 1,
                RateControlMethod: 2,
                NumSlice: 1,
                ..Default::default()
            };
            // Long-term references: one short-term and two long-term frames
            // (rfi-ltr-brief §6.7).
            let wants_ltr = format.codec != Codec::Av1
                && std::env::var_os("FARWING_QSV_LTR").is_some_and(|value| value == "1");
            if wants_ltr {
                params.__bindgen_anon_1.mfx.__bindgen_anon_1.__bindgen_anon_1.NumRefFrame = 3;
            }
            set_rate(&mut params, rate);
            let mut extensions = Extensions::new(version, size, format, true, conversion.color);
            let mut frame_rate = super::encoder_rate::Controller::new(rate);
            if let Some((previous, previous_extensions, previous_rate)) = restored {
                params = previous;
                extensions = previous_extensions;
                frame_rate = previous_rate;
            }
            extensions.bind(&mut params);
            let mut queried = params;
            let mut status = v::MFXVideoENCODE_Query(session.session, &mut params, &mut queried);
            if (status < 0 || validate(&queried, size, format).is_err())
                && format.codec == Codec::H265
            {
                params.__bindgen_anon_1.mfx.LowPower = 32;
                extensions.three.LowDelayBRC = 32;
                queried = params;
                status = v::MFXVideoENCODE_Query(session.session, &mut params, &mut queried);
            }
            check(status, "Query")?;
            validate(&queried, size, format)?;
            params = queried;
            let ltr = wants_ltr
                && params.__bindgen_anon_1.mfx.__bindgen_anon_1.__bindgen_anon_1.NumRefFrame >= 3;
            if wants_ltr {
                tracing::info!(enabled = ltr, format = ?format,
                    "QSV long-term references (unverified hardware path)");
            }
            let mut request = v::mfxFrameAllocRequest::default();
            check(
                v::MFXVideoENCODE_QueryIOSurf(session.session, &mut params, &mut request),
                "QueryIOSurf",
            )?;
            ensure!(
                request.NumFrameSuggested > 0,
                "{}",
                t().system.qsv_no_surface_requirement
            );
            let mut surface = Box::<v::mfxFrameSurface1>::default();
            surface.Info = params.__bindgen_anon_1.mfx.FrameInfo;
            surface.Data.MemId = resources.allocator.register(conversion.output.clone());
            let mut this = Self {
                size,
                resources: Some(resources),
                extensions,
                params,
                surface,
                conversion,
                buffer: vec![0; output_capacity(size, format, &params)?],
                rate,
                frame_rate,
                maximum: size,
                order: 0,
                initialized: false,
                timestamps: Default::default(),
                ltr_slots: if ltr { 2 } else { 0 },
                long_term: [None, None],
                format,
            };
            let prepared_ms = started.elapsed().as_secs_f64() * 1000.;
            let init_started = std::time::Instant::now();
            let mut status = v::MFXVideoENCODE_Init(this.raw(), &mut this.params);
            if status < 0
                && format.codec == Codec::H265
                && this.params.__bindgen_anon_1.mfx.LowPower == 16
            {
                v::MFXVideoENCODE_Close(this.raw());
                this.params.__bindgen_anon_1.mfx.LowPower = 32;
                this.extensions.three.LowDelayBRC = 32;
                let mut queried = this.params;
                check(
                    v::MFXVideoENCODE_Query(this.raw(), &mut this.params, &mut queried),
                    "Query without LowPower",
                )?;
                validate(&queried, size, format)?;
                this.params = queried;
                check(
                    v::MFXVideoENCODE_QueryIOSurf(this.raw(), &mut this.params, &mut request),
                    "QueryIOSurf without LowPower",
                )?;
                this.surface.Info = this.params.__bindgen_anon_1.mfx.FrameInfo;
                status = v::MFXVideoENCODE_Init(this.raw(), &mut this.params);
            }
            check(status, "Init")?;
            this.initialized = true;
            check(
                v::MFXVideoENCODE_GetVideoParam(this.raw(), &mut this.params),
                "GetVideoParam",
            )?;
            validate(&this.params, size, format)?;
            this.buffer
                .resize(output_capacity(size, format, &this.params)?, 0);
            report_rate("initialized", &this.params, &this.extensions, rate);
            let init_ms = init_started.elapsed().as_secs_f64() * 1000.;
            let capabilities = std::time::Instant::now();
            this.maximum =
                cached_maximum.unwrap_or_else(|| this.maximum_for_format(format).unwrap_or(size));
            tracing::info!(
                ?size,
                reused_session,
                prepared_ms,
                init_ms,
                capabilities_ms = capabilities.elapsed().as_secs_f64() * 1000.,
                "QSV encoder component initialized"
            );
            Ok(this)
        }
    }
    fn raw(&self) -> v::mfxSession {
        self.resources.as_ref().unwrap().session.session
    }
    pub fn maximum_size(&self) -> (u32, u32) {
        self.maximum
    }
    fn maximum_for_format(&self, format: Format) -> Result<(u32, u32)> {
        unsafe {
            let loader = self.resources.as_ref().unwrap().session.loader;
            let mut description = ptr::null_mut();
            check(
                v::MFXEnumImplementations(loader, 0, 1, &mut description),
                "EnumImplementations",
            )?;
            ensure!(
                !description.is_null(),
                "{}",
                t().system.qsv_empty_caps_description
            );
            let result = (|| -> Result<(u32, u32)> {
                let description = &*description.cast::<v::mfxImplDescription>();
                let mut maximum = (0, 0);
                for entry in slice(description.Enc.Codecs, description.Enc.NumCodecs)? {
                    if entry.CodecID != codec(format) {
                        continue;
                    }
                    for profile in slice(entry.Profiles, entry.NumProfiles)? {
                        for memory in slice(profile.MemDesc, profile.NumMemTypes)? {
                            if memory.MemHandleType == v::mfxResourceType_MFX_RESOURCE_DX11_TEXTURE
                                && slice(memory.ColorFormats, memory.NumColorFormats)?
                                    .contains(&fourcc(format))
                            {
                                if u64::from(memory.Width.Max) * u64::from(memory.Height.Max)
                                    > u64::from(maximum.0) * u64::from(maximum.1)
                                {
                                    maximum = (memory.Width.Max, memory.Height.Max);
                                }
                            }
                        }
                    }
                }
                ensure!(
                    maximum.0 > 0 && maximum.1 > 0 && maximum.0 <= 16384 && maximum.1 <= 16384,
                    "{}",
                    t().system.qsv_no_size_limit
                );
                Ok(maximum)
            })();
            v::MFXDispReleaseImplDescription(loader, description);
            result
        }
    }
    pub fn configure(&mut self, rate: Rate) -> Result<bool> {
        ensure!(
            self.resources.is_some(),
            "{}",
            t().system.qsv_session_closed
        );
        let Some(update) = self.frame_rate.decide(rate) else {
            return Ok(false);
        };
        if self.surface.Data.Locked != 0 {
            return Ok(false);
        }
        let mut params = self.params;
        set_rate(&mut params, update.rate);
        self.extensions.bind(&mut params);
        unsafe {
            let status = v::MFXVideoENCODE_Reset(self.raw(), &mut params);
            if status != 0 && status != 5 {
                if update.control_changed {
                    return Err(ResetRejected(status).into());
                }
                tracing::warn!(
                    status,
                    "QSV rate feedback rejected; retaining configured rate"
                );
                return Ok(false);
            }
            let mut effective = params;
            let status = v::MFXVideoENCODE_GetVideoParam(self.raw(), &mut effective);
            if status == 0 {
                validate(&effective, self.size, self.format)?;
                params = effective;
            } else {
                tracing::debug!(
                    status,
                    "QSV rate query failed; keeping requested parameters"
                );
            }
        }
        let changed_quality = self.rate.tier != rate.tier;
        self.params = params;
        let capacity = output_capacity(self.size, self.format, &self.params)?;
        if capacity > self.buffer.len() {
            self.buffer.resize(capacity, 0);
        }
        report_rate("reconfigured", &self.params, &self.extensions, update.rate);
        self.surface.Info = unsafe { params.__bindgen_anon_1.mfx.FrameInfo };
        self.rate = rate;
        self.frame_rate.commit(update);
        Ok(changed_quality)
    }
    pub fn encode(
        &mut self,
        texture: &ID3D11Texture2D,
        timestamp: i64,
        keyframe: bool,
        control: super::encoder::FrameControl,
        cancel: &std::sync::atomic::AtomicBool,
    ) -> Result<Vec<Encoded>> {
        ensure!(
            self.resources.is_some(),
            "{}",
            t().system.qsv_session_closed
        );
        for attempt in 0..50 {
            ensure!(
                !cancel.load(std::sync::atomic::Ordering::Acquire),
                "{}",
                t().system.encode_cancelled
            );
            if self.surface.Data.Locked == 0 {
                break;
            }
            super::encoder::retry_pause(attempt);
        }
        if self.surface.Data.Locked != 0 {
            return Ok(Vec::new());
        }
        self.frame_rate.input(timestamp);
        self.conversion.convert(texture)?;
        self.submit(timestamp, keyframe, control, cancel)
            .map_err(|error| EncodeFailure(error).into())
    }
    fn submit(
        &mut self,
        timestamp: i64,
        keyframe: bool,
        control_request: super::encoder::FrameControl,
        cancel: &std::sync::atomic::AtomicBool,
    ) -> Result<Vec<Encoded>> {
        self.surface.Data.TimeStamp = (timestamp.max(0) as u64) * 9 / 1000;
        self.timestamps
            .push_back((self.surface.Data.TimeStamp, timestamp));
        while self.timestamps.len() > 128 {
            self.timestamps.pop_front();
        }
        let order = self.order;
        self.surface.Data.FrameOrder = order;
        self.order = self.order.wrapping_add(1);
        let mut control = v::mfxEncodeCtrl {
            FrameType: if keyframe {
                (v::MFX_FRAMETYPE_I | v::MFX_FRAMETYPE_IDR | v::MFX_FRAMETYPE_REF) as u16
            } else {
                0
            },
            ..Default::default()
        };
        // Long-term marks and recovery frames by frame order.
        let mark = control_request
            .mark_ltr
            .filter(|slot| *slot < self.ltr_slots);
        let reference = control_request
            .use_ltr
            .filter(|slot| *slot < self.ltr_slots && !keyframe)
            .and_then(|slot| self.long_term[usize::from(slot)].map(|order| (slot, order)));
        let unknown = v::MFX_FRAMEORDER_UNKNOWN as u32;
        let empty = v::mfxExtAVCRefListCtrl__bindgen_ty_1 {
            FrameOrder: unknown,
            ..Default::default()
        };
        let mut lists = v::mfxExtAVCRefListCtrl {
            Header: v::mfxExtBuffer {
                BufferId: v::MFX_EXTBUFF_AVC_REFLIST_CTRL as u32,
                BufferSz: std::mem::size_of::<v::mfxExtAVCRefListCtrl>() as u32,
            },
            NumRefIdxL0Active: 0,
            NumRefIdxL1Active: 0,
            PreferredRefList: [empty; 32],
            RejectedRefList: [empty; 16],
            LongTermRefList: [empty; 16],
            ApplyLongTermIdx: 0,
            reserved: [0; 15],
        };
        if let Some(slot) = mark {
            lists.LongTermRefList[0] = v::mfxExtAVCRefListCtrl__bindgen_ty_1 {
                FrameOrder: order,
                LongTermIdx: u16::from(slot),
                ..Default::default()
            };
            lists.ApplyLongTermIdx = 1;
        }
        if let Some((_, long_term)) = reference {
            lists.PreferredRefList[0].FrameOrder = long_term;
            lists.NumRefIdxL0Active = 1;
        }
        let mut list_pointer = [(&mut lists as *mut v::mfxExtAVCRefListCtrl).cast::<v::mfxExtBuffer>()];
        if mark.is_some() || reference.is_some() {
            control.NumExtParam = 1;
            control.ExtParam = list_pointer.as_mut_ptr();
        }
        // H.264 reports which references the frame actually used.
        let mut info = v::mfxExtAVCEncodedFrameInfo {
            Header: v::mfxExtBuffer {
                BufferId: v::MFX_EXTBUFF_ENCODED_FRAME_INFO as u32,
                BufferSz: std::mem::size_of::<v::mfxExtAVCEncodedFrameInfo>() as u32,
            },
            FrameOrder: unknown,
            PicStruct: 0,
            LongTermIdx: v::MFX_LONGTERM_IDX_NO_IDX as u16,
            MAD: 0,
            BRCPanicMode: 0,
            QP: 0,
            SecondFieldOffset: 0,
            reserved: [0; 2],
            UsedRefListL0: [Default::default(); 32],
            UsedRefListL1: [Default::default(); 32],
        };
        let mut info_pointer = [(&mut info as *mut v::mfxExtAVCEncodedFrameInfo).cast::<v::mfxExtBuffer>()];
        let report = self.ltr_slots > 0 && self.format.codec == Codec::H264;
        let mut stream = v::mfxBitstream::default();
        if report {
            stream.__bindgen_anon_1.__bindgen_anon_1.ExtParam = info_pointer.as_mut_ptr();
            stream.__bindgen_anon_1.__bindgen_anon_1.NumExtParam = 1;
        }
        let mut sync = ptr::null_mut();
        let mut accepted = false;
        unsafe {
            for attempt in 0..50 {
                ensure!(
                    !cancel.load(std::sync::atomic::Ordering::Acquire),
                    "{}",
                    t().system.encode_cancelled
                );
                stream.Data = self.buffer.as_mut_ptr();
                stream.MaxLength = self.buffer.len() as u32;
                let status = v::MFXVideoENCODE_EncodeFrameAsync(
                    self.raw(),
                    &mut control,
                    &mut *self.surface,
                    &mut stream,
                    &mut sync,
                );
                // A positive warning WITH a sync point owns real output.
                // Busy/not-ready without a sync point is bounded, not success.
                match submit_action(status, !sync.is_null()) {
                    SubmitAction::Output => {
                        accepted = true;
                        break;
                    }
                    SubmitAction::Grow => {
                        ensure!(
                            self.buffer.len() < 64 * 1024 * 1024,
                            "{}",
                            t().system.qsv_bitstream_too_large
                        );
                        self.buffer
                            .resize((self.buffer.len() * 2).min(64 * 1024 * 1024), 0);
                        continue;
                    }
                    SubmitAction::Retry => {
                        super::encoder::retry_pause(attempt);
                        continue;
                    }
                    SubmitAction::Failed => {}
                }
                return Err(
                    super::encoder::SwitchCandidate((t().system.qsv_submit_failed)(
                        &status.to_string(),
                    ))
                    .into(),
                );
            }
            if !accepted {
                return Err(super::encoder::SwitchCandidate(
                    t().system.qsv_submit_retries_exhausted.into(),
                )
                .into());
            }
            // Preserve the existing total timeout, but observe cancellation
            // between bounded waits. The stack bitstream stays alive until
            // either completion or the session is quiesced below.
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(8);
            let status = loop {
                if cancel.load(std::sync::atomic::Ordering::Acquire) {
                    break v::mfxStatus_MFX_ERR_ABORTED;
                }
                let remaining = deadline.saturating_duration_since(std::time::Instant::now());
                if remaining.is_zero() {
                    break v::mfxStatus_MFX_WRN_IN_EXECUTION;
                }
                let status = v::MFXVideoCORE_SyncOperation(
                    self.raw(),
                    sync,
                    remaining.as_millis().clamp(1, 20) as u32,
                );
                if status != v::mfxStatus_MFX_WRN_IN_EXECUTION {
                    break status;
                }
                std::thread::yield_now();
            };
            if status != 0 {
                // mfxBitstream is owned by the outstanding task. Quiesce the
                // session while this stack object and its buffer are still alive.
                v::MFXVideoENCODE_Close(self.raw());
                self.initialized = false;
                self.resources.take();
                return Err(
                    super::encoder::SwitchCandidate((t().system.qsv_sync_incomplete)(
                        &status.to_string(),
                    ))
                    .into(),
                );
            }
        }
        let start = stream.DataOffset as usize;
        let end = start
            .checked_add(stream.DataLength as usize)
            .context(t().system.qsv_bitstream_length_overflow)?;
        ensure!(
            end <= self.buffer.len() && start < end,
            "{}",
            t().system.qsv_invalid_bitstream
        );
        let output_time = self
            .timestamps
            .iter()
            .position(|(clock, _)| *clock == stream.TimeStamp)
            .and_then(|index| self.timestamps.remove(index))
            .map_or(timestamp, |(_, original)| original);
        let idr = stream.FrameType & v::MFX_FRAMETYPE_IDR as u16 != 0
            || (self.format.codec == Codec::Av1
                && crate::media::av1::parameters(&self.buffer[start..end]).is_some()
                && stream.FrameType & v::MFX_FRAMETYPE_I as u16 != 0);
        let mut ltr = crate::media::encoding::LtrOutput::default();
        if self.ltr_slots > 0 {
            if idr {
                self.long_term = [None, None];
            }
            if report {
                ltr.marked = (info.LongTermIdx != v::MFX_LONGTERM_IDX_NO_IDX as u16)
                    .then_some(info.LongTermIdx as u8)
                    .filter(|slot| *slot < self.ltr_slots);
                ltr.used = reference
                    .filter(|(_, long_term)| {
                        !idr && info.UsedRefListL0.iter().any(|r| r.FrameOrder == *long_term)
                    })
                    .map(|(slot, _)| slot);
            } else {
                // HEVC has no per-frame report: trust the request.
                ltr.marked = mark;
                ltr.used = reference.filter(|_| !idr).map(|(slot, _)| slot);
            }
            if let Some(slot) = ltr.marked {
                self.long_term[usize::from(slot)] = Some(order);
            }
            if mark.is_some() || reference.is_some() {
                tracing::debug!(target: "farwing::recovery", ?ltr, idr, "QSV long-term reference frame");
            }
        }
        Ok(vec![Encoded {
            data: self.buffer[start..end].to_vec(),
            keyframe: idr,
            timestamp_100ns: output_time,
            is_new: true,
            timing: None,
            meta: None,
            ltr,
            format: self.format,
            color: self.conversion.color.space(None),
        }])
    }
}
unsafe fn slice<'a, T>(data: *const T, count: u16) -> Result<&'a [T]> {
    unsafe {
        ensure!(
            count <= 1024 && (count == 0 || !data.is_null()),
            "{}",
            t().system.qsv_invalid_caps_array
        );
        Ok(if count == 0 {
            &[]
        } else {
            std::slice::from_raw_parts(data, count as usize)
        })
    }
}
fn output_capacity(size: (u32, u32), format: Format, params: &v::mfxVideoParam) -> Result<usize> {
    // Complex IDRs can exceed the original fixed 1MiB allocation. On the Intel
    // driver this can surface from SyncOperation, after the input was accepted,
    // too late for the unsubmitted-input grow/retry path. Reserve a whole raw
    // frame plus headers up front, and honor the driver's queried requirement.
    const LIMIT: usize = 64 * 1024 * 1024;
    let pixels = size.0.div_ceil(16) as usize * 16 * size.1.div_ceil(16) as usize * 16;
    let samples = if format.chroma == 3 {
        pixels * 3
    } else {
        pixels * 3 / 2
    };
    let raw = samples * if format.depth > 8 { 2 } else { 1 };
    let driver = unsafe {
        let mfx = params.__bindgen_anon_1.mfx;
        usize::from(mfx.__bindgen_anon_1.__bindgen_anon_1.BufferSizeInKB)
            * usize::from(mfx.BRCParamMultiplier).max(1)
            * 1000
    };
    ensure!(
        driver <= LIMIT,
        "{}",
        t().system.qsv_driver_buffer_too_large
    );
    Ok((raw + 64 * 1024).min(LIMIT).max(driver).max(1024 * 1024))
}
fn report_rate(stage: &str, params: &v::mfxVideoParam, extensions: &Extensions, requested: Rate) {
    unsafe {
        let mfx = params.__bindgen_anon_1.mfx;
        let encoding = mfx.__bindgen_anon_1.__bindgen_anon_1;
        let factor = u32::from(mfx.BRCParamMultiplier).max(1);
        tracing::debug!(
            stage,
            requested_bps = requested.target,
            effective_target_bps = u32::from(encoding.__bindgen_anon_2.TargetKbps) * factor * 1000,
            effective_peak_bps = u32::from(encoding.__bindgen_anon_3.MaxKbps) * factor * 1000,
            buffer_bytes = u32::from(encoding.BufferSizeInKB) * factor * 1000,
            rate_control = encoding.RateControlMethod,
            fps_n = mfx.FrameInfo.FrameRateExtN,
            fps_d = mfx.FrameInfo.FrameRateExtD,
            low_power = mfx.LowPower,
            profile = mfx.CodecProfile,
            level = mfx.CodecLevel,
            shift = mfx.FrameInfo.Shift,
            min_qpi = extensions.two.MinQPI,
            min_qpp = extensions.two.MinQPP,
            max_qpi = extensions.two.MaxQPI,
            max_qpp = extensions.two.MaxQPP,
            max_frame_bytes = extensions.two.MaxFrameSize,
            max_i_bytes = extensions.three.MaxFrameSizeI,
            max_p_bytes = extensions.three.MaxFrameSizeP,
            window_max_kbps = extensions.three.WinBRCMaxAvgKbps,
            window_frames = extensions.three.WinBRCSize,
            low_delay_brc = extensions.three.LowDelayBRC,
            gpb = extensions.three.GPB,
            scenario = extensions.three.ScenarioInfo,
            "QSV effective driver rate parameters"
        );
    }
}
fn set_rate(params: &mut v::mfxVideoParam, rate: Rate) {
    unsafe {
        let mfx = &mut params.__bindgen_anon_1.mfx;
        let peak = rate.peak.max(rate.target) / 1000;
        let factor = peak.div_ceil(u16::MAX as u32).max(1);
        mfx.BRCParamMultiplier = factor as u16;
        let encoding = &mut mfx.__bindgen_anon_1.__bindgen_anon_1;
        encoding.__bindgen_anon_2.TargetKbps = (rate.target / 1000).div_ceil(factor).max(1) as u16;
        encoding.__bindgen_anon_3.MaxKbps = peak.div_ceil(factor) as u16;
        let av1 = mfx.CodecId == v::MFX_CODEC_AV1 as u32;
        let plan = crate::media::encoding::plan::plan(
            rate,
            if av1 { super::format::Codec::Av1 } else { super::format::Codec::H264 },
            rate.fps,
            (0, 0),
            super::format::Backend::Intel,
        );
        if let Some(bits) = plan.buffer {
            encoding.BufferSizeInKB = ((bits / 8000) as u32)
                .div_ceil(factor)
                .max(1)
                .min(u16::MAX as u32) as u16;
            if av1 && !rate.smooth {
                encoding.__bindgen_anon_1.InitialDelayInKB = 0;
            }
        }
        mfx.FrameInfo.FrameRateExtN = rate.fps;
        mfx.FrameInfo.FrameRateExtD = 1;
    }
}
fn validate(params: &v::mfxVideoParam, size: (u32, u32), format: Format) -> Result<()> {
    unsafe {
        let mfx = params.__bindgen_anon_1.mfx;
        let info = mfx.FrameInfo;
        let dimensions = info.__bindgen_anon_1.__bindgen_anon_1;
        ensure!(
            params.AsyncDepth == 1
                && mfx.CodecId == codec(format)
                && (format.codec != Codec::Av1
                    || mfx.CodecProfile
                        == if format.chroma == 3 {
                            v::MFX_PROFILE_AV1_HIGH
                        } else {
                            v::MFX_PROFILE_AV1_MAIN
                        } as u16)
                && info.FourCC == fourcc(format)
                && info.ChromaFormat == u16::from(format.chroma)
                && (info.BitDepthLuma == u16::from(format.depth)
                    || (format.depth == 8 && info.BitDepthLuma == 0))
                && (info.BitDepthChroma == u16::from(format.depth)
                    || (format.depth == 8 && info.BitDepthChroma == 0))
                && (u32::from(dimensions.CropW), u32::from(dimensions.CropH)) == size
                && mfx.__bindgen_anon_1.__bindgen_anon_1.GopRefDist == 1,
            "{}",
            t().system.qsv_driver_changed_contract
        );
        Ok(())
    }
}
impl Drop for Active {
    fn drop(&mut self) {
        unsafe {
            if self.initialized {
                v::MFXVideoENCODE_Close(self.raw());
            }
            // Close the session before freeing callback state and its surface handles.
            self.resources.take();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::format::{Codec, Color, Format, QualityTarget, Rate};
    use windows::Win32::Graphics::{Direct3D11::*, Dxgi::Common::*};

    /// Needs an Intel GPU: `cargo test --release --lib qsv -- --ignored`.
    /// Control changes the driver's Reset refuses (such as cinema mode's
    /// buffer) keep the encoder and start again with a keyframe.
    #[test]
    #[ignore]
    fn rejected_settings_keep_the_encoder() {
        let _runtime = super::super::encoder::Runtime::new().unwrap();
        let Some(adapter) = super::super::capture::encoding_adapters()
            .unwrap()
            .into_iter()
            .find(|adapter| adapter.vendor == 0x8086)
        else {
            return;
        };
        let (device, _) = super::super::capture::create_device(adapter.luid).unwrap();
        let mut texture = None;
        unsafe {
            device
                .CreateTexture2D(
                    &D3D11_TEXTURE2D_DESC {
                        Width: 1280,
                        Height: 720,
                        MipLevels: 1,
                        ArraySize: 1,
                        Format: DXGI_FORMAT_B8G8R8A8_UNORM,
                        SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
                        Usage: D3D11_USAGE_DEFAULT,
                        BindFlags: (D3D11_BIND_SHADER_RESOURCE.0 | D3D11_BIND_RENDER_TARGET.0) as u32,
                        ..Default::default()
                    },
                    None,
                    Some(&mut texture),
                )
                .unwrap();
        }
        let texture = texture.unwrap();
        let rate = |fps: u32, smooth: bool| Rate {
            target: 4_000_000,
            peak: 4_000_000,
            fps,
            tier: crate::media::geometry::Tier::P1440,
            quality_target: QualityTarget {
                bitrate: 4_000_000,
                fps,
            },
            smooth,
        };
        let mut encoder = super::super::encoder::Encoder::hardware_format(
            &device,
            (1280, 720),
            Format {
                codec: Codec::H265,
                chroma: 1,
                depth: 8,
            },
            rate(60, false),
            Color::Sdr,
        )
        .unwrap();
        let mut timestamp = 0;
        let mut encode = |encoder: &mut super::super::encoder::Encoder, keyframe: bool| {
            let mut keyframes = 0;
            for i in 0..8 {
                timestamp += 166_666;
                for frame in encoder.encode(&texture, timestamp, keyframe && i == 0).unwrap() {
                    keyframes += usize::from(frame.keyframe);
                }
            }
            keyframes
        };
        assert!(encode(&mut encoder, true) > 0);
        for (fps, smooth) in [(60, true), (30, true), (60, false)] {
            let keyframe = encoder.configure_rate(rate(fps, smooth)).unwrap();
            let keyframes = encode(&mut encoder, keyframe);
            println!("fps={fps} smooth={smooth}: initialized again={keyframe} keyframes={keyframes}");
            assert!(!keyframe || keyframes > 0, "fps={fps} smooth={smooth}");
        }
    }
}
