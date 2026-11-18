//! The NVENC session shared by every platform: driver API loading, the preset
//! and rate-control configuration, reconfiguration, and encoding a registered
//! input. Platforms supply the device (D3D11 on Windows, a CUDA context on
//! Linux) and register their own input surface, so both encode with the same
//! parameters.
pub(crate) mod timing;

use super::{Codec, Color, Format, Rate};
use anyhow::{Context, Result, bail, ensure};
use openuuyc_nvenc_sys as nv;
use std::{
    ffi::{CStr, c_void},
    ptr,
};

pub(crate) use nv::{NV_ENC_BUFFER_FORMAT, NV_ENC_DEVICE_TYPE, NV_ENC_INPUT_RESOURCE_TYPE};

#[cfg(windows)]
type Library = libloading::os::windows::Library;
#[cfg(not(windows))]
type Library = libloading::Library;

pub(crate) struct Api {
    functions: nv::NV_ENCODE_API_FUNCTION_LIST,
    level: u32,
    version: u32,
    _library: Library,
}

impl Api {
    fn open_library() -> Result<(
        unsafe extern "C" fn(*mut u32) -> nv::NVENCSTATUS,
        unsafe extern "C" fn(*mut nv::NV_ENCODE_API_FUNCTION_LIST) -> nv::NVENCSTATUS,
        Library,
    )> {
        unsafe {
            // Driver installation is the only library source; never search the
            // working directory.
            #[cfg(windows)]
            let library =
                libloading::os::windows::Library::load_with_flags("nvEncodeAPI64.dll", 0x00000800)
                    .context("未找到 NVIDIA NVENC 驱动")?;
            #[cfg(not(windows))]
            let library = libloading::Library::new("libnvidia-encode.so.1")
                .context("未找到 NVIDIA NVENC 驱动")?;
            let maximum = *library.get::<unsafe extern "C" fn(*mut u32) -> nv::NVENCSTATUS>(
                b"NvEncodeAPIGetMaxSupportedVersion\0",
            )?;
            let create =
                *library.get::<unsafe extern "C" fn(
                    *mut nv::NV_ENCODE_API_FUNCTION_LIST,
                ) -> nv::NVENCSTATUS>(b"NvEncodeAPICreateInstance\0")?;
            Ok((maximum, create, library))
        }
    }
    pub fn load() -> Result<Self> {
        unsafe {
            let (maximum, create, library) = Self::open_library()?;
            let mut supported = 0;
            ensure!(
                maximum(&mut supported) == nv::NVENCSTATUS::NV_ENC_SUCCESS,
                "查询 NVENC API 失败"
            );
            ensure!(supported >= 0x80, "NVIDIA 驱动不支持NVENC API 8.0");
            let level = supported.min(0xc1);
            let version = (level >> 4) | ((level & 15) << 24);
            let mut functions = nv::NV_ENCODE_API_FUNCTION_LIST::default();
            functions.version = version | 0x70020000;
            ensure!(
                create(&mut functions) == nv::NVENCSTATUS::NV_ENC_SUCCESS,
                "创建 NVENC API 失败"
            );
            ensure!(
                functions.nvEncOpenEncodeSessionEx.is_some()
                    && (if level >= 0xa0 {
                        functions.nvEncGetEncodePresetConfigEx.is_some()
                    } else {
                        functions.nvEncGetEncodePresetConfig.is_some()
                    })
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
                "NVENC 驱动接口不完整"
            );
            Ok(Self {
                functions,
                level,
                version,
                _library: library,
            })
        }
    }
    fn structure(&self, revision: u32, high: bool) -> u32 {
        self.version | 0x70000000 | (revision << 16) | if high { 1 << 31 } else { 0 }
    }
    fn config_version(&self) -> u32 {
        self.structure(
            if self.level == 0x80 {
                6
            } else if self.level < 0xc0 {
                7
            } else {
                8
            },
            true,
        )
    }
    fn init_version(&self) -> u32 {
        self.structure(if self.level < 0xc1 { 5 } else { 6 }, true)
    }
    fn preset(&self, codec: Codec) -> (nv::GUID, nv::NV_ENC_TUNING_INFO) {
        if self.level < 0xa0 {
            (
                nv::GUID {
                    Data1: 0xc5f733b9,
                    Data2: 0xea97,
                    Data3: 0x4cf9,
                    Data4: [0xbe, 0xc2, 0xbf, 0x78, 0xa7, 0x4f, 0xd1, 0x05],
                },
                nv::NV_ENC_TUNING_INFO(0),
            )
        } else {
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
        bail!("NVENC {operation} 失败（{}）：{detail}", status.0)
    }
}

/// An input surface registered with a session, with its row pitch when the
/// input is a linear device buffer.
pub(crate) struct Registered(nv::NV_ENC_REGISTERED_PTR, u32);

/// An initialized NVENC session with its output bitstream buffer.
pub(crate) struct Session {
    diagnostics: timing::Diagnostics,
    frame_rate: super::rate::Controller,
    api: Api,
    session: *mut c_void,
    bitstream: nv::NV_ENC_OUTPUT_PTR,
    config: nv::NV_ENC_CONFIG,
    init: nv::NV_ENC_INITIALIZE_PARAMS,
    index: u32,
    quality: i32,
    maximum: (u32, u32),
    format: Format,
    color: Color,
}

// The session is owned and driven by one capture thread at a time.
unsafe impl Send for Session {}

impl Session {
    /// Open and initialize a session on `device` of `device_type`.
    pub fn open(
        device_type: nv::NV_ENC_DEVICE_TYPE,
        device: *mut c_void,
        width: u32,
        height: u32,
        rate: Rate,
        format: Format,
        color: Color,
    ) -> Result<Self> {
        let Rate {
            target: bitrate,
            fps,
            ..
        } = rate;
        ensure!(
            width > 0 && height > 0 && width % 2 == 0 && height % 2 == 0,
            "NVENC 尺寸必须为正偶数"
        );
        ensure!(
            (1..=144).contains(&fps) && bitrate > 0,
            "NVENC 编码参数无效"
        );
        let api = Api::load()?;
        ensure!(
            format.codec != Codec::Av1 || api.level >= 0xc0,
            "AV1需要NVENC API 12及支持的显卡驱动"
        );
        // Query the actual codec-specific preset; changing only its GUID on
        // reconfigure would leave the old preset configuration in place.
        let (preset_guid, tuning) = api.preset(format.codec);
        let mut encoder = Self {
            diagnostics: timing::Diagnostics::default(),
            frame_rate: super::rate::Controller::new(Rate { quality: 0, ..rate }),
            api,
            session: ptr::null_mut(),
            bitstream: ptr::null_mut(),
            config: nv::NV_ENC_CONFIG::default(),
            init: nv::NV_ENC_INITIALIZE_PARAMS::default(),
            index: 0,
            quality: 0,
            maximum: (0, 0),
            format,
            color,
        };
        unsafe {
            let mut open = nv::NV_ENC_OPEN_ENCODE_SESSION_EX_PARAMS::default();
            open.version = encoder.api.structure(1, false);
            open.apiVersion = encoder.api.version;
            open.deviceType = device_type;
            open.device = device;
            let f = &encoder.api.functions;
            let status = f.nvEncOpenEncodeSessionEx.unwrap()(&mut open, &mut encoder.session);
            encoder
                .api
                .check(encoder.session, status, "OpenEncodeSessionEx")?;
            ensure!(!encoder.session.is_null(), "NVENC 未返回会话");
            let dimension = |cap| -> Result<u32> {
                let mut query = nv::NV_ENC_CAPS_PARAM::default();
                query.version = encoder.api.structure(1, false);
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
                ensure!(value > 0 && value <= 16384, "NVENC 返回无效尺寸能力");
                Ok(value as u32)
            };
            encoder.maximum = (
                dimension(nv::NV_ENC_CAPS::NV_ENC_CAPS_WIDTH_MAX)?,
                dimension(nv::NV_ENC_CAPS::NV_ENC_CAPS_HEIGHT_MAX)?,
            );
            ensure!(
                width <= encoder.maximum.0 && height <= encoder.maximum.1,
                "所选画面超出 NVENC 尺寸能力"
            );
            let mut preset = nv::NV_ENC_PRESET_CONFIG::default();
            preset.version = encoder.api.structure(4, true);
            preset.presetCfg.version = encoder.api.config_version();
            let status = if encoder.api.level >= 0xa0 {
                f.nvEncGetEncodePresetConfigEx.unwrap()(
                    encoder.session,
                    codec_guid(format),
                    preset_guid,
                    tuning,
                    &mut preset,
                )
            } else {
                f.nvEncGetEncodePresetConfig.unwrap()(
                    encoder.session,
                    codec_guid(format),
                    preset_guid,
                    &mut preset,
                )
            };
            encoder
                .api
                .check(encoder.session, status, "GetEncodePresetConfigEx")?;
            let mut config = preset.presetCfg;
            config.version = encoder.api.config_version();
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
            config.rcParams.lowDelayKeyFrameScale = u8::from(encoder.api.level >= 0xa0);
            set_rate(&mut config, format.codec, rate, fps, (width, height));
            if format.codec == Codec::Av1 {
                config.profileGUID = nv::NV_ENC_AV1_PROFILE_MAIN_GUID;
                let av1 = &mut config.encodeCodecConfig.av1Config;
                av1.idrPeriod = u32::MAX;
                av1.maxNumRefFramesInDPB = 1;
                av1.numFwdRefs = nv::NV_ENC_NUM_REF_FRAMES::NV_ENC_NUM_REF_FRAMES_1;
                av1.numBwdRefs = nv::NV_ENC_NUM_REF_FRAMES::NV_ENC_NUM_REF_FRAMES_1;
                av1.set_chromaFormatIDC(1);
                av1.set_inputPixelBitDepthMinus8(u32::from(format.depth - 8));
                av1.set_pixelBitDepthMinus8(u32::from(format.depth - 8));
                av1.set_repeatSeqHdr(1);
                av1.set_outputAnnexBFormat(0);
                av1.set_enableBitstreamPadding(0);
                // Captured desktop pixels are coded directly; synthetic grain
                // is outside the low-delay stream contract, including presets.
                av1.set_enableFilmGrainParams(0);
                av1.filmGrainParams = ptr::null_mut();
                av1.maxTemporalLayersMinus1 = 0;
                let color = encoder.color.space(None);
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
                    hevc.set_pixelBitDepthMinus8(u32::from(format.depth - 8));
                    hevc.sliceMode = 3;
                    hevc.sliceModeData = 1;
                    hevc.set_repeatSPSPPS(0);
                    hevc.set_outputAUD(0);
                    hevc.set_enableFillerDataInsertion(0);
                    &mut hevc.hevcVUIParameters
                };
                let color = encoder.color.space(None);
                vui.videoSignalTypePresentFlag = 1;
                vui.videoFormat = nv::NV_ENC_VUI_VIDEO_FORMAT::NV_ENC_VUI_VIDEO_FORMAT_UNSPECIFIED;
                vui.videoFullRangeFlag = u32::from(color.range == 2);
                vui.colourDescriptionPresentFlag = 1;
                vui.colourPrimaries = nv::NV_ENC_VUI_COLOR_PRIMARIES(color.primaries.into());
                vui.transferCharacteristics =
                    nv::NV_ENC_VUI_TRANSFER_CHARACTERISTIC(color.transfer.into());
                vui.colourMatrix = nv::NV_ENC_VUI_MATRIX_COEFFS(color.matrix.into());
            }
            encoder.config = config;
            let mut init = nv::NV_ENC_INITIALIZE_PARAMS::default();
            init.version = encoder.api.init_version();
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
            encoder.api.check(
                encoder.session,
                f.nvEncInitializeEncoder.unwrap()(encoder.session, &mut init),
                "InitializeEncoder",
            )?;
            init.encodeConfig = ptr::null_mut();
            encoder.init = init;
            let mut buffer = nv::NV_ENC_CREATE_BITSTREAM_BUFFER::default();
            buffer.version = encoder.api.structure(1, false);
            encoder.api.check(
                encoder.session,
                f.nvEncCreateBitstreamBuffer.unwrap()(encoder.session, &mut buffer),
                "CreateBitstreamBuffer",
            )?;
            encoder.bitstream = buffer.bitstreamBuffer;
            ensure!(!encoder.bitstream.is_null(), "NVENC未返回码流缓冲");
        }
        tracing::info!(
            width,
            height,
            fps,
            bitrate,
            backend = "NVENC",
            ?format,
            api = encoder.api.level,
            "host hardware encoder initialized"
        );
        Ok(encoder)
    }

    /// Register an input surface of the session's size.
    pub fn register(
        &self,
        resource_type: nv::NV_ENC_INPUT_RESOURCE_TYPE,
        resource: *mut c_void,
        pitch: u32,
        buffer_format: nv::NV_ENC_BUFFER_FORMAT,
    ) -> Result<Registered> {
        unsafe {
            let mut registration = nv::NV_ENC_REGISTER_RESOURCE::default();
            registration.version = self
                .api
                .structure(if self.api.level < 0xc0 { 3 } else { 4 }, false);
            registration.resourceType = resource_type;
            registration.width = self.init.encodeWidth;
            registration.height = self.init.encodeHeight;
            registration.pitch = pitch;
            registration.resourceToRegister = resource;
            registration.bufferFormat = buffer_format;
            registration.bufferUsage = nv::NV_ENC_BUFFER_USAGE::NV_ENC_INPUT_IMAGE;
            self.api.check(
                self.session,
                self.api.functions.nvEncRegisterResource.unwrap()(self.session, &mut registration),
                "RegisterResource",
            )?;
            ensure!(
                !registration.registeredResource.is_null(),
                "驱动未返回注册资源"
            );
            Ok(Registered(registration.registeredResource, pitch))
        }
    }

    pub fn unregister(&self, registered: Registered) {
        unsafe {
            self.api.functions.nvEncUnregisterResource.unwrap()(self.session, registered.0);
        }
    }

    pub fn maximum_size(&self) -> (u32, u32) {
        self.maximum
    }

    /// Narrow the size capability to what the platform's input surface can
    /// hold, rejecting a session already larger than that.
    #[cfg_attr(
        not(windows),
        allow(dead_code, reason = "Only the Windows planar 4:4:4 input is limited.")
    )]
    pub fn restrict_maximum(&mut self, limit: (u32, u32)) -> Result<()> {
        self.maximum = (self.maximum.0.min(limit.0), self.maximum.1.min(limit.1));
        ensure!(
            self.init.encodeWidth <= self.maximum.0 && self.init.encodeHeight <= self.maximum.1,
            "所选画面超出 NVENC 尺寸能力"
        );
        Ok(())
    }

    pub fn configure_rate(&mut self, rate: Rate) -> Result<bool> {
        ensure!(
            (1..=144).contains(&rate.fps) && rate.target > 0,
            "NVENC 重配参数无效"
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
        params.version = self.api.structure(1, true);
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
            // T C37450: rejected feedback leaves the committed configuration
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

    /// Record a frame's stage times; slow frames are logged at most every 5 s.
    pub fn record(&mut self, timing: timing::Frame, success: bool) {
        self.diagnostics.record(timing, success);
    }

    /// Encode the picture currently in `input`.
    pub fn encode(
        &mut self,
        input: &Registered,
        timestamp_100ns: i64,
        keyframe: bool,
        timing: &mut timing::Frame,
    ) -> Result<Vec<super::Encoded>> {
        self.frame_rate.input(timestamp_100ns);
        unsafe {
            let f = &self.api.functions;
            let mut map = nv::NV_ENC_MAP_INPUT_RESOURCE::default();
            map.version = self.api.structure(4, false);
            map.registeredResource = input.0;
            let mapped = f.nvEncMapInputResource.unwrap()(self.session, &mut map);
            timing.stage(2);
            self.api.check(self.session, mapped, "MapInputResource")?;
            let _mapped = Mapped {
                api: &self.api,
                session: self.session,
                input: map.mappedResource,
            };
            let mut picture = nv::NV_ENC_PIC_PARAMS::default();
            picture.version = self
                .api
                .structure(if self.api.level < 0xc0 { 4 } else { 6 }, true);
            picture.inputWidth = self.init.encodeWidth;
            picture.inputHeight = self.init.encodeHeight;
            if input.1 != 0 {
                picture.inputPitch = input.1;
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
            let status = f.nvEncEncodePicture.unwrap()(self.session, &mut picture);
            timing.stage(3);
            if status == nv::NVENCSTATUS::NV_ENC_ERR_NEED_MORE_INPUT {
                return Ok(Vec::new());
            }
            self.api.check(self.session, status, "EncodePicture")?;
            let mut bitstream = nv::NV_ENC_LOCK_BITSTREAM::default();
            bitstream.version = self.api.structure(
                if self.api.level == 0xc0 { 2 } else { 1 },
                self.api.level >= 0xc1,
            );
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
                "NVENC 输出码流无效"
            );
            let bytes = std::slice::from_raw_parts(
                bitstream.bitstreamBufferPtr.cast::<u8>(),
                bitstream.bitstreamSizeInBytes as usize,
            )
            .to_vec();
            Ok(vec![super::Encoded {
                format: self.format,
                color: self.color.space(None),
                data: bytes,
                keyframe: bitstream.pictureType == nv::NV_ENC_PIC_TYPE::NV_ENC_PIC_TYPE_IDR,
                is_new: true,
                timing: None,
                timestamp_100ns: bitstream
                    .outputTimeStamp
                    .try_into()
                    .context("NVENC 时间戳溢出")?,
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
    let frames = if codec == Codec::Av1 { 1 } else { 5 };
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
impl Drop for Session {
    fn drop(&mut self) {
        unsafe {
            let f = &self.api.functions;
            if !self.bitstream.is_null() {
                f.nvEncDestroyBitstreamBuffer.unwrap()(self.session, self.bitstream);
            }
            if !self.session.is_null() {
                f.nvEncDestroyEncoder.unwrap()(self.session);
            }
        }
    }
}
