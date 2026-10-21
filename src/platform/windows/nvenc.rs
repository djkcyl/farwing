//! NVENC on the capture adapter. Raw desktop pixels stay in GPU memory.
mod cuda_input;
mod timing;
use super::{
    format::{Backend, Codec, Format, Rate},
    gpu_conversion::Conversion,
};
use crate::i18n::t;
use anyhow::{Context, Result, bail, ensure};
use farwing_nvenc_sys as nv;
use std::{
    ffi::{CStr, c_void},
    ptr,
};
use windows::{Win32::Graphics::Direct3D11::*, core::Interface};

/// NVENC API 13.0 (NVIDIA driver 570 or newer). Older drivers are not
/// supported: the session falls back to another encoder.
struct Api {
    functions: nv::NV_ENCODE_API_FUNCTION_LIST,
    _library: libloading::os::windows::Library,
}

impl Api {
    fn load() -> Result<Self> {
        unsafe {
            // Driver installation is the only DLL source; never search the working directory.
            let library =
                libloading::os::windows::Library::load_with_flags("nvEncodeAPI64.dll", 0x00000800)
                    .context(t().system.nvenc_driver_not_found)?;
            let maximum = library.get::<unsafe extern "C" fn(*mut u32) -> nv::NVENCSTATUS>(
                b"NvEncodeAPIGetMaxSupportedVersion\0",
            )?;
            let mut supported = 0;
            ensure!(
                maximum(&mut supported) == nv::NVENCSTATUS::NV_ENC_SUCCESS,
                "{}",
                t().system.nvenc_query_api_failed
            );
            // `supported` is (major << 4) | minor.
            ensure!(
                supported >= 0xd0,
                "{}",
                (t().system.nvenc_driver_too_old)(supported >> 4, supported & 15)
            );
            let create = library.get::<unsafe extern "C" fn(
                *mut nv::NV_ENCODE_API_FUNCTION_LIST,
            ) -> nv::NVENCSTATUS>(b"NvEncodeAPICreateInstance\0")?;
            let mut functions = nv::NV_ENCODE_API_FUNCTION_LIST::default();
            functions.version = nv::NV_ENCODE_API_FUNCTION_LIST_VER;
            ensure!(
                create(&mut functions) == nv::NVENCSTATUS::NV_ENC_SUCCESS,
                "{}",
                t().system.nvenc_create_api_failed
            );
            ensure!(
                functions.nvEncOpenEncodeSessionEx.is_some()
                    && functions.nvEncGetEncodePresetConfigEx.is_some()
                    && functions.nvEncGetEncodeCaps.is_some()
                    && functions.nvEncInitializeEncoder.is_some()
                    && functions.nvEncCreateBitstreamBuffer.is_some()
                    && functions.nvEncDestroyBitstreamBuffer.is_some()
                    && functions.nvEncRegisterResource.is_some()
                    && functions.nvEncUnregisterResource.is_some()
                    && functions.nvEncMapInputResource.is_some()
                    && functions.nvEncUnmapInputResource.is_some()
                    && functions.nvEncEncodePicture.is_some()
                    && functions.nvEncLockBitstream.is_some()
                    && functions.nvEncUnlockBitstream.is_some()
                    && functions.nvEncReconfigureEncoder.is_some()
                    && functions.nvEncDestroyEncoder.is_some(),
                "{}",
                t().system.nvenc_incomplete_driver
            );
            Ok(Self {
                functions,
                _library: library,
            })
        }
    }
    fn preset(&self, codec: Codec) -> (nv::GUID, nv::NV_ENC_TUNING_INFO) {
        // AV1 benefits from P3; AVC/HEVC retain P1's lower encode cost.
        // Quality tiers change rate/size, never switch to latency-tolerant HQ.
        (
            if codec == Codec::Av1 {
                nv::NV_ENC_PRESET_P3_GUID
            } else {
                nv::NV_ENC_PRESET_P1_GUID
            },
            nv::NV_ENC_TUNING_INFO::NV_ENC_TUNING_INFO_ULTRA_LOW_LATENCY,
        )
    }
    fn check(&self, session: *mut c_void, status: nv::NVENCSTATUS, operation: &str) -> Result<()> {
        if status == nv::NVENCSTATUS::NV_ENC_SUCCESS {
            return Ok(());
        }
        let detail = if session.is_null() {
            String::new()
        } else {
            self.functions
                .nvEncGetLastErrorString
                .and_then(|get| unsafe {
                    let p = get(session);
                    (!p.is_null()).then(|| CStr::from_ptr(p).to_string_lossy().into_owned())
                })
                .unwrap_or_default()
        };
        bail!(
            "{}",
            (t().system.nvenc_call_failed)(operation, &status.0.to_string(), &detail)
        )
    }
}

/// Build and drop a small encoder on this device. NVENC's first
/// initialization on a device costs about 25-30 ms, later ones a few: a
/// connecting session pays it here, before the device is handed to its
/// encoder.
pub(crate) fn warm_up(device: &ID3D11Device) -> Result<()> {
    let rate = Rate {
        target: 1_000_000,
        peak: 1_000_000,
        fps: 60,
        quality: 1,
        quality_target: super::format::QualityTarget {
            bitrate: 1_000_000,
            fps: 60,
        },
        smooth: false,
    };
    drop(Encoder::build(
        device,
        640,
        360,
        rate,
        Format::AVC,
        super::format::Color::Sdr,
    )?);
    Ok(())
}

pub(crate) struct Encoder {
    diagnostics: timing::Diagnostics,
    frame_rate: super::encoder_rate::Controller,
    api: Api,
    session: *mut c_void,
    bitstream: nv::NV_ENC_OUTPUT_PTR,
    registered: nv::NV_ENC_REGISTERED_PTR,
    config: nv::NV_ENC_CONFIG,
    init: nv::NV_ENC_INITIALIZE_PARAMS,
    conversion: Conversion,
    cuda: Option<cuda_input::Input>,
    index: u32,
    quality: i32,
    maximum: (u32, u32),
    format: Format,
    /// Long-term reference slots in use (H.264/HEVC only), 0 without.
    ltr_slots: u8,
}

/// Two long-term references, P frames from one reference
/// (`docs/protocol/rfi-ltr-brief.md` §6.7); `enabled = false` restores the
/// single-reference stream.
fn set_ltr(config: &mut nv::NV_ENC_CONFIG, codec: Codec, enabled: bool) {
    let (frames, references, l0) = if enabled {
        (2, 3, nv::NV_ENC_NUM_REF_FRAMES::NV_ENC_NUM_REF_FRAMES_1)
    } else {
        (0, 1, nv::NV_ENC_NUM_REF_FRAMES::NV_ENC_NUM_REF_FRAMES_AUTOSELECT)
    };
    unsafe {
        match codec {
            Codec::H264 => {
                let h264 = &mut config.encodeCodecConfig.h264Config;
                h264.set_enableLTR(u32::from(enabled));
                h264.ltrNumFrames = frames;
                h264.ltrTrustMode = 0;
                h264.maxNumRefFrames = references;
                h264.numRefL0 = l0;
            }
            Codec::H265 => {
                let hevc = &mut config.encodeCodecConfig.hevcConfig;
                hevc.set_enableLTR(u32::from(enabled));
                hevc.ltrNumFrames = frames;
                hevc.ltrTrustMode = 0;
                hevc.maxNumRefFramesInDPB = references;
                hevc.numRefL0 = l0;
            }
            Codec::Av1 => {
                // API 13.0 only (callers check). Frame ID numbers let the
                // decoder notice a reference it does not hold.
                let av1 = &mut config.encodeCodecConfig.av1Config;
                av1.set_enableLTR(u32::from(enabled));
                av1.set_enableFrameIdNumbers(u32::from(enabled));
                av1.ltrNumFrames = frames;
                av1.maxNumRefFramesInDPB = references;
            }
        }
    }
}

fn bit_depth(depth: u8) -> nv::NV_ENC_BIT_DEPTH {
    if depth == 10 {
        nv::NV_ENC_BIT_DEPTH::NV_ENC_BIT_DEPTH_10
    } else {
        nv::NV_ENC_BIT_DEPTH::NV_ENC_BIT_DEPTH_8
    }
}

#[derive(Debug, thiserror::Error)]
#[error("{}", input_rejected(.0))]
struct InputRejected(String);

fn input_rejected(error: &str) -> String {
    (t().system.nvenc_input_rejected)(error)
}

impl Encoder {
    pub(crate) fn new_format(
        device: &ID3D11Device,
        width: u32,
        height: u32,
        rate: Rate,
        format: Format,
        color: super::format::Color,
    ) -> Result<Self> {
        let encoder = Self::build(device, width, height, rate, format, color)?;
        tracing::info!(
            width,
            height,
            fps = rate.fps,
            bitrate = rate.target,
            backend = "NVENC",
            ?format,
            "host hardware encoder initialized"
        );
        Ok(encoder)
    }
    fn build(
        device: &ID3D11Device,
        width: u32,
        height: u32,
        rate: Rate,
        format: Format,
        color: super::format::Color,
    ) -> Result<Self> {
        if format.chroma == 3 && format.depth == 10 {
            return Self::create(
                device,
                width,
                height,
                rate,
                format,
                Conversion::planar_44410(device, (width, height), color)?,
            );
        }
        let conversion = Conversion::new(device, (width, height), format, color)?;
        let compute = conversion.is_compute();
        match Self::create(device, width, height, rate, format, conversion) {
            Err(error) if compute && error.downcast_ref::<InputRejected>().is_some() => {
                tracing::debug!(%error, "NVENC registered compute input rejected; trying pixel candidate");
                Self::create(
                    device,
                    width,
                    height,
                    rate,
                    format,
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
        conversion: Conversion,
    ) -> Result<Self> {
        let Rate {
            target: bitrate,
            fps,
            ..
        } = rate;
        ensure!(
            Backend::Nvidia.accepts(format),
            "{}",
            t().system.nvenc_format_unsupported
        );
        ensure!(
            width > 0 && height > 0 && width % 2 == 0 && height % 2 == 0,
            "{}",
            t().system.nvenc_size_not_even
        );
        ensure!(
            (1..=144).contains(&fps) && bitrate > 0,
            "{}",
            t().system.nvenc_invalid_parameters
        );
        let api = Api::load()?;
        // Query the actual codec-specific preset; changing only its GUID on
        // reconfigure would leave the old preset configuration in place.
        let (preset_guid, tuning) = api.preset(format.codec);
        let cuda = if format.chroma == 3 && format.depth == 10 {
            Some(cuda_input::Input::new(
                device,
                &conversion.output,
                (width, height),
            )?)
        } else {
            None
        };
        let mut encoder = Self {
            diagnostics: timing::Diagnostics::default(),
            frame_rate: super::encoder_rate::Controller::new(Rate { quality: 0, ..rate }),
            api,
            session: ptr::null_mut(),
            bitstream: ptr::null_mut(),
            registered: ptr::null_mut(),
            config: nv::NV_ENC_CONFIG::default(),
            init: nv::NV_ENC_INITIALIZE_PARAMS::default(),
            conversion,
            cuda,
            index: 0,
            quality: 0,
            maximum: (0, 0),
            format,
            ltr_slots: 0,
        };
        let _current = encoder
            .cuda
            .as_ref()
            .map(|input| input.enter())
            .transpose()?;
        unsafe {
            let mut open = nv::NV_ENC_OPEN_ENCODE_SESSION_EX_PARAMS::default();
            open.version = nv::NV_ENC_OPEN_ENCODE_SESSION_EX_PARAMS_VER;
            open.apiVersion = nv::NVENCAPI_VERSION;
            open.deviceType = nv::NV_ENC_DEVICE_TYPE::NV_ENC_DEVICE_TYPE_DIRECTX;
            open.device = device.as_raw();
            if let Some(input) = &encoder.cuda {
                open.deviceType = nv::NV_ENC_DEVICE_TYPE::NV_ENC_DEVICE_TYPE_CUDA;
                open.device = input.context;
            }
            let f = &encoder.api.functions;
            let status = f.nvEncOpenEncodeSessionEx.unwrap()(&mut open, &mut encoder.session);
            encoder
                .api
                .check(encoder.session, status, "OpenEncodeSessionEx")?;
            ensure!(
                !encoder.session.is_null(),
                "{}",
                t().system.nvenc_no_session
            );
            let dimension = |cap| -> Result<u32> {
                let mut query = nv::NV_ENC_CAPS_PARAM::default();
                query.version = nv::NV_ENC_CAPS_PARAM_VER;
                query.capsToQuery = cap;
                let mut value = 0;
                encoder.api.check(
                    encoder.session,
                    f.nvEncGetEncodeCaps.unwrap()(
                        encoder.session,
                        codec_guid(format),
                        &mut query,
                        &mut value,
                    ),
                    "GetEncodeCaps",
                )?;
                ensure!(
                    value > 0 && value <= 16384,
                    "{}",
                    t().system.nvenc_invalid_size_caps
                );
                Ok(value as u32)
            };
            encoder.maximum = (
                dimension(nv::NV_ENC_CAPS::NV_ENC_CAPS_WIDTH_MAX)?,
                dimension(nv::NV_ENC_CAPS::NV_ENC_CAPS_HEIGHT_MAX)?,
            );
            // Long-term references for loss recovery; AV1 needs SDK 13.
            let ltr_frames = {
                let mut query = nv::NV_ENC_CAPS_PARAM::default();
                query.version = nv::NV_ENC_CAPS_PARAM_VER;
                query.capsToQuery = nv::NV_ENC_CAPS::NV_ENC_CAPS_NUM_MAX_LTR_FRAMES;
                let mut value = 0;
                let status = f.nvEncGetEncodeCaps.unwrap()(
                    encoder.session,
                    codec_guid(format),
                    &mut query,
                    &mut value,
                );
                if status == nv::NVENCSTATUS::NV_ENC_SUCCESS { value } else { 0 }
            };
            let ltr = ltr_frames >= 2
                && std::env::var_os("FARWING_NVENC_LTR").is_none_or(|value| value != "0");
            if encoder.cuda.is_some() {
                encoder.maximum.1 = encoder.maximum.1.min(16384 / 3) & !1;
            }
            ensure!(
                width <= encoder.maximum.0 && height <= encoder.maximum.1,
                "{}",
                t().system.nvenc_size_exceeds_capability
            );
            let mut preset = nv::NV_ENC_PRESET_CONFIG::default();
            preset.version = nv::NV_ENC_PRESET_CONFIG_VER;
            preset.presetCfg.version = nv::NV_ENC_CONFIG_VER;
            let status = f.nvEncGetEncodePresetConfigEx.unwrap()(
                encoder.session,
                codec_guid(format),
                preset_guid,
                tuning,
                &mut preset,
            );
            encoder
                .api
                .check(encoder.session, status, "GetEncodePresetConfigEx")?;
            let mut config = preset.presetCfg;
            config.version = nv::NV_ENC_CONFIG_VER;
            // Retain the queried driver preset's profile for ordinary H264.
            // Chroma/depth use explicit profiles only where the codec requires it.
            if format.chroma == 3 {
                config.profileGUID = if format.codec == Codec::H264 {
                    nv::NV_ENC_H264_PROFILE_HIGH_444_GUID
                } else {
                    nv::NV_ENC_HEVC_PROFILE_FREXT_GUID
                };
            } else if format.codec == Codec::H265 {
                config.profileGUID = if format.depth == 10 {
                    nv::NV_ENC_HEVC_PROFILE_MAIN10_GUID
                } else {
                    nv::NV_ENC_HEVC_PROFILE_MAIN_GUID
                };
            }
            config.gopLength = u32::MAX;
            config.frameIntervalP = 1;
            config.frameFieldMode =
                nv::NV_ENC_PARAMS_FRAME_FIELD_MODE::NV_ENC_PARAMS_FRAME_FIELD_MODE_FRAME;
            config.mvPrecision = nv::NV_ENC_MV_PRECISION::NV_ENC_MV_PRECISION_QUARTER_PEL;
            config.rcParams.rateControlMode = nv::NV_ENC_PARAMS_RC_MODE::NV_ENC_PARAMS_RC_VBR;
            config.rcParams.set_enableAQ(1);
            config.rcParams.set_enableTemporalAQ(0);
            config.rcParams.set_enableLookahead(0);
            config.rcParams.set_disableIadapt(1);
            config.rcParams.set_zeroReorderDelay(1);
            config.rcParams.lookaheadDepth = 0;
            config.rcParams.multiPass = nv::NV_ENC_MULTI_PASS::NV_ENC_MULTI_PASS_DISABLED;
            config.rcParams.lowDelayKeyFrameScale = 1;
            set_rate(&mut config, format.codec, rate, fps, (width, height));
            if format.codec == Codec::Av1 {
                config.profileGUID = nv::NV_ENC_AV1_PROFILE_MAIN_GUID;
                let av1 = &mut config.encodeCodecConfig.av1Config;
                av1.idrPeriod = u32::MAX;
                av1.maxNumRefFramesInDPB = 1;
                av1.numFwdRefs = nv::NV_ENC_NUM_REF_FRAMES::NV_ENC_NUM_REF_FRAMES_1;
                av1.numBwdRefs = nv::NV_ENC_NUM_REF_FRAMES::NV_ENC_NUM_REF_FRAMES_1;
                av1.set_chromaFormatIDC(1);
                av1.inputBitDepth = bit_depth(format.depth);
                av1.outputBitDepth = bit_depth(format.depth);
                av1.set_repeatSeqHdr(1);
                av1.set_outputAnnexBFormat(0);
                av1.set_enableBitstreamPadding(0);
                // Captured desktop pixels are coded directly; synthetic grain
                // is outside the low-delay stream contract, including presets.
                av1.set_enableFilmGrainParams(0);
                av1.filmGrainParams = ptr::null_mut();
                av1.maxTemporalLayersMinus1 = 0;
                let color = encoder.conversion.color.space(None);
                av1.colorPrimaries = nv::NV_ENC_VUI_COLOR_PRIMARIES(color.primaries.into());
                av1.transferCharacteristics =
                    nv::NV_ENC_VUI_TRANSFER_CHARACTERISTIC(color.transfer.into());
                av1.matrixCoefficients = nv::NV_ENC_VUI_MATRIX_COEFFS(color.matrix.into());
                av1.colorRange = u32::from(color.range == 2);
            } else {
                let vui = if format.codec == Codec::H264 {
                    let h264 = &mut config.encodeCodecConfig.h264Config;
                    h264.idrPeriod = u32::MAX;
                    h264.maxNumRefFrames = 1;
                    h264.chromaFormatIDC = u32::from(format.chroma);
                    h264.sliceMode = 3;
                    h264.sliceModeData = 1;
                    h264.set_repeatSPSPPS(0);
                    h264.set_outputAUD(0);
                    h264.set_enableFillerDataInsertion(0);
                    // Without this, decoders may infer a full DPB of reorder
                    // delay even though frameIntervalP=1 disables B frames.
                    h264.h264VUIParameters.bitstreamRestrictionFlag = 1;
                    &mut h264.h264VUIParameters
                } else {
                    let hevc = &mut config.encodeCodecConfig.hevcConfig;
                    hevc.idrPeriod = u32::MAX;
                    hevc.maxNumRefFramesInDPB = 1;
                    hevc.set_chromaFormatIDC(u32::from(format.chroma));
                    hevc.inputBitDepth = bit_depth(format.depth);
                    hevc.outputBitDepth = bit_depth(format.depth);
                    hevc.sliceMode = 3;
                    hevc.sliceModeData = 1;
                    hevc.set_repeatSPSPPS(0);
                    hevc.set_outputAUD(0);
                    hevc.set_enableFillerDataInsertion(0);
                    &mut hevc.hevcVUIParameters
                };
                let color = encoder.conversion.color.space(None);
                vui.videoSignalTypePresentFlag = 1;
                vui.videoFormat = nv::NV_ENC_VUI_VIDEO_FORMAT::NV_ENC_VUI_VIDEO_FORMAT_UNSPECIFIED;
                vui.videoFullRangeFlag = u32::from(color.range == 2);
                vui.colourDescriptionPresentFlag = 1;
                vui.colourPrimaries = nv::NV_ENC_VUI_COLOR_PRIMARIES(color.primaries.into());
                vui.transferCharacteristics =
                    nv::NV_ENC_VUI_TRANSFER_CHARACTERISTIC(color.transfer.into());
                vui.colourMatrix = nv::NV_ENC_VUI_MATRIX_COEFFS(color.matrix.into());
            }
            set_ltr(&mut config, format.codec, ltr);
            encoder.config = config;
            let mut init = nv::NV_ENC_INITIALIZE_PARAMS::default();
            init.version = nv::NV_ENC_INITIALIZE_PARAMS_VER;
            init.encodeGUID = codec_guid(format);
            init.presetGUID = preset_guid;
            init.encodeWidth = width;
            init.encodeHeight = height;
            init.darWidth = width;
            init.darHeight = height;
            init.frameRateNum = fps;
            init.frameRateDen = 1;
            init.enablePTD = 1;
            init.enableEncodeAsync = 0;
            init.maxEncodeWidth = width;
            init.maxEncodeHeight = height;
            init.tuningInfo = tuning;
            init.encodeConfig = &mut encoder.config;
            let mut status = f.nvEncInitializeEncoder.unwrap()(encoder.session, &mut init);
            if status != nv::NVENCSTATUS::NV_ENC_SUCCESS && ltr {
                tracing::warn!(?status, "NVENC rejected long-term references; encoding without");
                set_ltr(&mut encoder.config, format.codec, false);
                status = f.nvEncInitializeEncoder.unwrap()(encoder.session, &mut init);
            } else if ltr {
                encoder.ltr_slots = 2;
            }
            encoder.api.check(encoder.session, status, "InitializeEncoder")?;
            init.encodeConfig = ptr::null_mut();
            encoder.init = init;
            let mut buffer = nv::NV_ENC_CREATE_BITSTREAM_BUFFER::default();
            buffer.version = nv::NV_ENC_CREATE_BITSTREAM_BUFFER_VER;
            encoder.api.check(
                encoder.session,
                f.nvEncCreateBitstreamBuffer.unwrap()(encoder.session, &mut buffer),
                "CreateBitstreamBuffer",
            )?;
            encoder.bitstream = buffer.bitstreamBuffer;
            ensure!(
                !encoder.bitstream.is_null(),
                "{}",
                t().system.nvenc_no_bitstream_buffer
            );
            let mut resource = nv::NV_ENC_REGISTER_RESOURCE::default();
            resource.version = nv::NV_ENC_REGISTER_RESOURCE_VER;
            resource.resourceType =
                nv::NV_ENC_INPUT_RESOURCE_TYPE::NV_ENC_INPUT_RESOURCE_TYPE_DIRECTX;
            resource.width = width;
            resource.height = height;
            resource.resourceToRegister = encoder.conversion.output.as_raw();
            resource.bufferFormat = match (format.chroma, format.depth) {
                (1, 10) => nv::NV_ENC_BUFFER_FORMAT::NV_ENC_BUFFER_FORMAT_YUV420_10BIT,
                (3, 8) => nv::NV_ENC_BUFFER_FORMAT::NV_ENC_BUFFER_FORMAT_AYUV,
                (3, 10) => nv::NV_ENC_BUFFER_FORMAT::NV_ENC_BUFFER_FORMAT_YUV444_10BIT,
                _ => nv::NV_ENC_BUFFER_FORMAT::NV_ENC_BUFFER_FORMAT_NV12,
            };
            if let Some(input) = &encoder.cuda {
                resource.resourceType =
                    nv::NV_ENC_INPUT_RESOURCE_TYPE::NV_ENC_INPUT_RESOURCE_TYPE_CUDADEVICEPTR;
                resource.resourceToRegister = input.pointer as usize as *mut c_void;
                resource.pitch = input.pitch;
            }
            resource.bufferUsage = nv::NV_ENC_BUFFER_USAGE::NV_ENC_INPUT_IMAGE;
            encoder
                .api
                .check(
                    encoder.session,
                    f.nvEncRegisterResource.unwrap()(encoder.session, &mut resource),
                    "RegisterResource",
                )
                .map_err(|error| InputRejected(format!("{error:#}")))?;
            encoder.registered = resource.registeredResource;
            if encoder.registered.is_null() {
                return Err(InputRejected(t().system.nvenc_no_registered_resource.into()).into());
            }
        }
        Ok(encoder)
    }
    pub(crate) fn maximum_size(&self) -> (u32, u32) {
        self.maximum
    }

    pub(crate) fn configure_rate(&mut self, rate: Rate) -> Result<bool> {
        let _current = self.cuda.as_ref().map(|input| input.enter()).transpose()?;
        ensure!(
            (1..=144).contains(&rate.fps) && rate.target > 0,
            "{}",
            t().system.nvenc_invalid_reconfigure
        );
        let Some(update) = self.frame_rate.decide(rate) else {
            return Ok(false);
        };
        let fps = update.rate.fps;
        let quality = update.rate.quality;
        let changed_quality = self.quality != quality;
        let mut config = self.config;
        set_rate(
            &mut config,
            self.format.codec,
            update.rate,
            update.buffer_fps,
            (self.init.encodeWidth, self.init.encodeHeight),
        );
        let mut params = nv::NV_ENC_RECONFIGURE_PARAMS::default();
        params.version = nv::NV_ENC_RECONFIGURE_PARAMS_VER;
        params.reInitEncodeParams = self.init;
        params.reInitEncodeParams.frameRateNum = fps;
        params.reInitEncodeParams.encodeConfig = &mut config;
        let result = unsafe {
            self.api.check(
                self.session,
                self.api.functions.nvEncReconfigureEncoder.unwrap()(self.session, &mut params),
                "ReconfigureEncoder",
            )
        };
        if let Err(error) = result {
            if update.control_changed {
                return Err(error);
            }
            // Rejected feedback leaves the committed configuration
            // intact; unlike failed frame controls it does not discard a frame.
            tracing::warn!(%error, "NVENC rate feedback rejected; retaining configured rate");
            return Ok(false);
        }
        self.config = config;
        self.init = params.reInitEncodeParams;
        self.init.encodeConfig = ptr::null_mut();
        self.quality = quality;
        self.frame_rate.commit(update);
        Ok(changed_quality)
    }

    pub(crate) fn ltr_slots(&self) -> u8 {
        self.ltr_slots
    }

    pub(crate) fn encode(
        &mut self,
        texture: &ID3D11Texture2D,
        timestamp_100ns: i64,
        control: super::encoder::FrameControl,
    ) -> Result<Vec<super::encoder::Encoded>> {
        let mut timing = timing::Frame::new();
        let result = self.encode_timed(texture, timestamp_100ns, control, &mut timing);
        self.diagnostics.record(timing, result.is_ok());
        result
    }

    fn encode_timed(
        &mut self,
        texture: &ID3D11Texture2D,
        timestamp_100ns: i64,
        control: super::encoder::FrameControl,
        timing: &mut timing::Frame,
    ) -> Result<Vec<super::encoder::Encoded>> {
        let keyframe = control.keyframe || (control.use_ltr.is_some() && self.ltr_slots == 0);
        self.frame_rate.input(timestamp_100ns);
        let converted = self.conversion.convert(texture);
        timing.stage(0);
        converted?;
        let _current = self.cuda.as_ref().map(|input| input.enter()).transpose()?;
        let copied = self.cuda.as_ref().map(|input| input.copy()).transpose();
        timing.stage(1);
        copied?;
        unsafe {
            let f = &self.api.functions;
            let mut map = nv::NV_ENC_MAP_INPUT_RESOURCE::default();
            map.version = nv::NV_ENC_MAP_INPUT_RESOURCE_VER;
            map.registeredResource = self.registered;
            let mapped = f.nvEncMapInputResource.unwrap()(self.session, &mut map);
            timing.stage(2);
            self.api.check(self.session, mapped, "MapInputResource")?;
            let _mapped = Mapped {
                api: &self.api,
                session: self.session,
                input: map.mappedResource,
            };
            let mut picture = nv::NV_ENC_PIC_PARAMS::default();
            picture.version = nv::NV_ENC_PIC_PARAMS_VER;
            picture.inputWidth = self.init.encodeWidth;
            picture.inputHeight = self.init.encodeHeight;
            if let Some(input) = &self.cuda {
                picture.inputPitch = input.pitch;
            }
            picture.inputBuffer = map.mappedResource;
            picture.bufferFmt = map.mappedBufferFmt;
            picture.outputBitstream = self.bitstream;
            picture.pictureStruct = nv::NV_ENC_PIC_STRUCT::NV_ENC_PIC_STRUCT_FRAME;
            picture.inputTimeStamp = timestamp_100ns.max(0) as u64;
            picture.inputDuration = 10_000_000 / u64::from(self.init.frameRateNum);
            picture.frameIdx = self.index;
            self.index = self.index.wrapping_add(1);
            if keyframe {
                picture.encodePicFlags = 6;
            }
            let mark = control.mark_ltr.filter(|slot| *slot < self.ltr_slots);
            let reference = control
                .use_ltr
                .filter(|slot| *slot < self.ltr_slots && !keyframe);
            match self.format.codec {
                Codec::H264 if self.ltr_slots > 0 => {
                    let h264 = &mut picture.codecPicParams.h264PicParams;
                    if let Some(slot) = mark {
                        h264.set_ltrMarkFrame(1);
                        h264.ltrMarkFrameIdx = u32::from(slot);
                    }
                    if let Some(slot) = reference {
                        h264.set_ltrUseFrames(1);
                        h264.ltrUseFrameBitmap = 1 << slot;
                    }
                }
                Codec::H265 if self.ltr_slots > 0 => {
                    let hevc = &mut picture.codecPicParams.hevcPicParams;
                    if let Some(slot) = mark {
                        hevc.set_ltrMarkFrame(1);
                        hevc.ltrMarkFrameIdx = u32::from(slot);
                    }
                    if let Some(slot) = reference {
                        hevc.set_ltrUseFrames(1);
                        hevc.ltrUseFrameBitmap = 1 << slot;
                    }
                }
                Codec::Av1 if self.ltr_slots > 0 => {
                    let av1 = &mut picture.codecPicParams.av1PicParams;
                    if let Some(slot) = mark {
                        av1.set_ltrMarkFrame(1);
                        av1.ltrMarkFrameIdx = u32::from(slot);
                    }
                    if let Some(slot) = reference {
                        av1.set_ltrUseFrames(1);
                        av1.ltrUseFrameBitmap = 1 << slot;
                    }
                }
                _ => {}
            }
            let status = f.nvEncEncodePicture.unwrap()(self.session, &mut picture);
            timing.stage(3);
            if status == nv::NVENCSTATUS::NV_ENC_ERR_NEED_MORE_INPUT {
                return Ok(Vec::new());
            }
            self.api.check(self.session, status, "EncodePicture")?;
            let mut bitstream = nv::NV_ENC_LOCK_BITSTREAM::default();
            bitstream.version = nv::NV_ENC_LOCK_BITSTREAM_VER;
            bitstream.outputBitstream = self.bitstream;
            let locked = f.nvEncLockBitstream.unwrap()(self.session, &mut bitstream);
            timing.stage(4);
            self.api.check(self.session, locked, "LockBitstream")?;
            let _locked = Locked {
                api: &self.api,
                session: self.session,
                output: self.bitstream,
            };
            ensure!(
                !bitstream.bitstreamBufferPtr.is_null()
                    && bitstream.bitstreamSizeInBytes > 0
                    && bitstream.bitstreamSizeInBytes <= 64 * 1024 * 1024,
                "{}",
                t().system.nvenc_invalid_bitstream
            );
            let bytes = std::slice::from_raw_parts(
                bitstream.bitstreamBufferPtr.cast::<u8>(),
                bitstream.bitstreamSizeInBytes as usize,
            )
            .to_vec();
            let idr = bitstream.pictureType == nv::NV_ENC_PIC_TYPE::NV_ENC_PIC_TYPE_IDR;
            let ltr = crate::media::encoding::LtrOutput {
                marked: (self.ltr_slots > 0 && bitstream.ltrFrame() == 1)
                    .then_some(bitstream.ltrFrameIdx as u8),
                used: reference.filter(|slot| !idr && bitstream.ltrFrameBitmap & (1 << slot) != 0),
            };

            if control.mark_ltr.is_some() || reference.is_some() {
                tracing::debug!(target: "farwing::recovery", ?control, ?ltr, idr,
                    bitmap = bitstream.ltrFrameBitmap, "NVENC long-term reference frame");
            }
            Ok(vec![super::encoder::Encoded {
                format: self.format,
                color: self.conversion.color.space(None),
                data: bytes,
                keyframe: idr,
                is_new: true,
                timing: None,
                meta: None,
                ltr,
                timestamp_100ns: bitstream
                    .outputTimeStamp
                    .try_into()
                    .context(t().system.nvenc_timestamp_overflow)?,
            }])
        }
    }
}
fn codec_guid(format: Format) -> nv::GUID {
    match format.codec {
        Codec::H264 => nv::NV_ENC_CODEC_H264_GUID,
        Codec::H265 => nv::NV_ENC_CODEC_HEVC_GUID,
        Codec::Av1 => nv::NV_ENC_CODEC_AV1_GUID,
    }
}
fn set_rate(
    config: &mut nv::NV_ENC_CONFIG,
    codec: Codec,
    rate: Rate,
    buffer_fps: u32,
    size: (u32, u32),
) {
    config.rcParams.averageBitRate = rate.target;
    config.rcParams.maxBitRate = rate.peak.max(rate.target);
    // VBV is a bit allocation window, not queued input frames. A time floor
    // preserves detail during IDR recovery at high FPS; retain the existing
    // larger allowance when input FPS is low. No startup buffering is added.
    let frames = if rate.smooth {
        // About one second: quality first for watching video.
        u64::from(buffer_fps.max(1))
    } else if codec == Codec::Av1 {
        1
    } else {
        5
    };
    config.rcParams.vbvBufferSize = (u64::from(rate.target) * frames / u64::from(buffer_fps.max(1)))
        .max(u64::from(rate.target) / 8)
        .min(u64::from(u32::MAX)) as u32;
    config.rcParams.vbvInitialDelay = 0;
    if codec == Codec::Av1 {
        let qp = av1_minimum_qp(rate.quality_target.bitrate, size, rate.quality_target.fps);
        config.rcParams.set_enableMinQP(u32::from(qp != 0));
        config.rcParams.minQP = nv::NV_ENC_QP {
            qpInterP: qp,
            qpInterB: qp,
            qpIntra: qp,
        };
    }
}
fn av1_minimum_qp(bitrate: u32, size: (u32, u32), fps: u32) -> u32 {
    // The selected ceiling/FPS stay stable through congestion and idle input.
    // One policy for every tier, custom rate and bit depth. Keep the measured
    // conservative floor at <=0.1 bits/pixel/frame, then smoothly release it
    // by 0.2 so a generous budget can still improve precision. This limits
    // refinement, never forces frames to spend the available bandwidth.
    let pixels_per_second =
        f64::from(size.0.max(1)) * f64::from(size.1.max(1)) * f64::from(fps.max(1));
    let density = f64::from(bitrate) / pixels_per_second;
    ((0.2 - density) * 400.0).round().clamp(0.0, 40.0) as u32
}
struct Mapped<'a> {
    api: &'a Api,
    session: *mut c_void,
    input: nv::NV_ENC_INPUT_PTR,
}
impl Drop for Mapped<'_> {
    fn drop(&mut self) {
        unsafe {
            (self.api.functions.nvEncUnmapInputResource.unwrap())(self.session, self.input);
        }
    }
}
struct Locked<'a> {
    api: &'a Api,
    session: *mut c_void,
    output: nv::NV_ENC_OUTPUT_PTR,
}
impl Drop for Locked<'_> {
    fn drop(&mut self) {
        unsafe {
            (self.api.functions.nvEncUnlockBitstream.unwrap())(self.session, self.output);
        }
    }
}
impl Drop for Encoder {
    fn drop(&mut self) {
        let _current = self.cuda.as_ref().and_then(|input| input.enter().ok());
        unsafe {
            let f = &self.api.functions;
            if !self.registered.is_null() {
                f.nvEncUnregisterResource.unwrap()(self.session, self.registered);
            }
            if !self.bitstream.is_null() {
                f.nvEncDestroyBitstreamBuffer.unwrap()(self.session, self.bitstream);
            }
            if !self.session.is_null() {
                f.nvEncDestroyEncoder.unwrap()(self.session);
            }
        }
    }
}
