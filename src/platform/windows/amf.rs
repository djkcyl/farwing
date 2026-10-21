//! AMF's D3D11 hardware producer. All COM/driver calls stay on its owner thread.
use super::{
    encoder::Encoded,
    format::{Codec, Format, Rate},
    gpu_conversion::Conversion,
};
use crate::i18n::t;
use anyhow::{Context, Result, ensure};
use farwing_amf_sys as a;
use std::ptr::{self, NonNull};
use windows::{Win32::Graphics::Direct3D11::*, core::Interface};

fn check(result: a::AMF_RESULT, operation: &str) -> Result<()> {
    ensure!(
        result == a::AMF_RESULT_AMF_OK,
        "{}",
        (t().system.amf_call_failed)(operation, &result.to_string())
    );
    Ok(())
}
fn wide(name: &[u8]) -> Vec<u16> {
    name.iter().map(|&c| u16::from(c)).collect()
}
fn integer(value: i64) -> a::AMFVariantStruct {
    a::AMFVariantStruct {
        type_: 2,
        __bindgen_anon_1: a::AMFVariantStruct__bindgen_ty_1 { int64Value: value },
    }
}
fn boolean(value: bool) -> a::AMFVariantStruct {
    a::AMFVariantStruct {
        type_: 1,
        __bindgen_anon_1: a::AMFVariantStruct__bindgen_ty_1 {
            boolValue: u8::from(value),
        },
    }
}
fn frame_rate(value: u32) -> a::AMFVariantStruct {
    a::AMFVariantStruct {
        type_: 7,
        __bindgen_anon_1: a::AMFVariantStruct__bindgen_ty_1 {
            rateValue: a::AMFRate { num: value, den: 1 },
        },
    }
}
trait Release {
    unsafe fn release(p: *mut Self);
}
struct Owned<T: Release>(NonNull<T>);
impl<T: Release> Owned<T> {
    unsafe fn take(p: *mut T) -> Result<Self> {
        Ok(Self(
            NonNull::new(p).context(t().system.amf_null_interface)?,
        ))
    }
    fn raw(&self) -> *mut T {
        self.0.as_ptr()
    }
}
impl<T: Release> Drop for Owned<T> {
    fn drop(&mut self) {
        unsafe { T::release(self.raw()) }
    }
}
macro_rules! interface {
    ($t:ty,$vt:ty) => {
        impl Release for $t {
            unsafe fn release(p: *mut Self) {
                unsafe {
                    if let Some(release) = (*(*p).pVtbl).Release {
                        release(p);
                    }
                }
            }
        }
        impl Owned<$t> {
            fn vt(&self) -> &$vt {
                unsafe { &*self.0.as_ref().pVtbl }
            }
        }
    };
}
interface!(a::AMFContext, a::AMFContextVtbl);
interface!(a::AMFComponent, a::AMFComponentVtbl);
interface!(a::AMFSurface, a::AMFSurfaceVtbl);
interface!(a::AMFData, a::AMFDataVtbl);
interface!(a::AMFBuffer, a::AMFBufferVtbl);
interface!(a::AMFCaps, a::AMFCapsVtbl);
interface!(a::AMFIOCaps, a::AMFIOCapsVtbl);

pub(crate) struct Encoder {
    component: Option<Owned<a::AMFComponent>>,
    context: Option<Owned<a::AMFContext>>,
    pending: Option<(Owned<a::AMFSurface>, bool, super::encoder::FrameControl)>,
    conversion: Conversion,
    /// Long-term reference slots (0 without; `FARWING_AMF_LTR=1` enables
    /// them until verified on AMD hardware, `docs/protocol/rfi-ltr-brief.md`).
    ltr_slots: u8,
    format: Format,
    rate: Rate,
    maximum: (u32, u32),
    frame_rate: super::encoder_rate::Controller,
    configured_fps: u32,
    _library: libloading::os::windows::Library,
}
impl Encoder {
    pub fn new(
        device: &ID3D11Device,
        size: (u32, u32),
        format: Format,
        rate: Rate,
        color: super::format::Color,
    ) -> Result<Self> {
        ensure!(format.valid(), "{}", t().system.amf_invalid_input_format);
        let usages: &[i64] = if format.codec == Codec::H265 {
            &[1, 2]
        } else {
            &[2]
        };
        let paths: &[bool] = if format.chroma == 1 {
            &[true, false]
        } else {
            &[false]
        };
        let mut failure = None;
        for &usage in usages {
            for &compute in paths {
                let candidate = (|| {
                    let conversion = if compute {
                        Conversion::compute(device, size, format, 1, color)?
                    } else {
                        Conversion::pixel(device, size, format, 1, color)?
                    };
                    Self::create(device, size, format, rate, conversion, usage)
                })();
                match candidate {
                    Ok(encoder) => return Ok(encoder),
                    Err(error) => {
                        tracing::debug!(%error, usage, compute, "AMF input/usage candidate rejected");
                        failure = Some(error);
                    }
                }
            }
        }
        Err(failure.context(t().system.amf_no_input_candidate)?)
    }
    fn create(
        device: &ID3D11Device,
        size: (u32, u32),
        format: Format,
        rate: Rate,
        conversion: Conversion,
        usage: i64,
    ) -> Result<Self> {
        unsafe {
            let library = libloading::os::windows::Library::load_with_flags("amfrt64.dll", 0x800)?;
            let init = library
                .get::<unsafe extern "C" fn(u64, *mut *mut a::AMFFactory) -> a::AMF_RESULT>(
                    b"AMFInit\0",
                )?;
            let mut factory = ptr::null_mut();
            check(init(0x1_0005_0002_0000, &mut factory), "Init")?;
            ensure!(
                !factory.is_null() && !(*factory).pVtbl.is_null(),
                "{}",
                t().system.amf_invalid_factory
            );
            let factory_vt = &*(*factory).pVtbl;
            let mut context = ptr::null_mut();
            check(
                factory_vt
                    .CreateContext
                    .with_context(|| (t().system.amf_interface_missing)("CreateContext"))?(
                    factory,
                    &mut context,
                ),
                "CreateContext",
            )?;
            let context = Owned::<a::AMFContext>::take(context)?;
            check(
                context
                    .vt()
                    .InitDX11
                    .with_context(|| (t().system.amf_interface_missing)("InitDX11"))?(
                    context.raw(),
                    device.as_raw(),
                    110,
                ),
                "InitDX11",
            )?;
            let mut component = ptr::null_mut();
            let id = wide(if format.codec == Codec::Av1 {
                a::AMFVideoEncoder_AV1
            } else if format.codec == Codec::H264 {
                a::AMFVideoEncoderVCE_AVC
            } else {
                a::AMFVideoEncoder_HEVC
            });
            check(
                factory_vt
                    .CreateComponent
                    .with_context(|| (t().system.amf_interface_missing)("CreateComponent"))?(
                    factory,
                    context.raw(),
                    id.as_ptr(),
                    &mut component,
                ),
                "CreateComponent",
            )?;
            let component = Owned::<a::AMFComponent>::take(component)?;
            let mut this = Self {
                component: Some(component),
                context: Some(context),
                pending: None,
                ltr_slots: 0,
                conversion,
                format,
                rate,
                maximum: size,
                frame_rate: super::encoder_rate::Controller::new(rate),
                configured_fps: rate.fps,
                _library: library,
            };
            let rate = this.initial_rate(rate);
            this.rate = rate;
            this.frame_rate = super::encoder_rate::Controller::new(rate);
            if format.codec == Codec::Av1 {
                this.configure_av1(size, rate)?;
            } else {
                this.set_initial(b"Usage\0", b"HevcUsage\0", integer(usage))?;
                this.set_initial(
                    b"FrameSize\0",
                    b"HevcFrameSize\0",
                    a::AMFVariantStruct {
                        type_: 5,
                        __bindgen_anon_1: a::AMFVariantStruct__bindgen_ty_1 {
                            sizeValue: a::AMFSize {
                                width: size.0 as i32,
                                height: size.1 as i32,
                            },
                        },
                    },
                )?;
                this.set_initial(
                    b"QualityPreset\0",
                    b"HevcQualityPreset\0",
                    integer(if format.codec == Codec::H265 { 10 } else { 1 }),
                )?;
                this.set_initial(
                    b"RateControlMethod\0",
                    b"HevcRateControlMethod\0",
                    // AVC latency-constrained VBR reduces coded repeats during
                    // scene/IDR recovery at the same target and peak budget.
                    // Select it before Init; runtime property acceptance alone
                    // does not establish that this driver changed its RC mode.
                    integer(if format.codec == Codec::H264 { 3 } else { 2 }),
                )?;
                this.set_initial(b"FrameRate\0", b"HevcFrameRate\0", frame_rate(rate.fps))?;
                this.set_initial(
                    b"TargetBitrate\0",
                    b"HevcTargetBitrate\0",
                    integer(rate.target.into()),
                )?;
                this.set_initial(
                    b"PeakBitrate\0",
                    b"HevcPeakBitrate\0",
                    integer(rate.peak.max(rate.target).into()),
                )?;
                this.set_initial(
                    b"VBVBufferSize\0",
                    b"HevcVBVBufferSize\0",
                    integer(vbv(rate)),
                )?;
                this.set_initial(b"EnableVBAQ\0", b"HevcEnableVBAQ\0", boolean(true))?;
                this.set_initial(b"EnforceHRD\0", b"HevcEnforceHRD\0", boolean(false))?;
                this.set_initial(
                    b"HighMotionQualityBoostEnable\0",
                    b"HevcHighMotionQualityBoostEnable\0",
                    boolean(false),
                )?;
                this.set_initial(b"QueryTimeout\0", b"HevcQueryTimeout\0", integer(10))?;
                this.set_initial(
                    b"LowLatencyInternal\0",
                    b"LowLatencyInternal\0",
                    boolean(true),
                )?;
                // Keep overflow protection: disabling it can exceed the target
                // substantially on difficult AVC content. Runtime VBV headroom
                // reduces the repeated pictures caused by ordinary IDR bursts.
                this.set_initial(
                    b"RateControlSkipFrameEnable\0",
                    b"HevcRateControlSkipFrameEnable\0",
                    boolean(true),
                )?;
                this.set_initial(
                    b"FillerDataEnable\0",
                    b"HevcFillerDataEnable\0",
                    boolean(false),
                )?;
                if format.codec == Codec::H264 {
                    for (name, value) in [
                        (b"BPicturesPattern\0".as_slice(), integer(0)),
                        (b"IDRPeriod\0", integer(0)),
                        (b"SlicesPerFrame\0", integer(1)),
                        (b"CABACEnable\0", integer(1)),
                        (b"DeBlockingFilter\0", boolean(true)),
                        (b"EnableVBAQ\0", boolean(true)),
                    ] {
                        this.set_initial(name, name, value)?;
                    }
                } else {
                    for (name, value) in [
                        (b"HevcColorBitDepth\0".as_slice(), i64::from(format.depth)),
                        (b"HevcHeaderInsertionMode\0", 2),
                        (b"HevcOutputMode\0", 0),
                        (b"HevcGOPSize\0", 1_000_000),
                        (b"HevcSlicesPerFrame\0", 1),
                    ] {
                        this.set_initial(name, name, integer(value))?;
                    }
                }
                let color = this.conversion.color.space(None);
                // The GPU conversion already produced YUV. Explicitly describe
                // that matrix/range instead of AMF's size-dependent defaults.
                this.set_initial(
                    a::AMF_VIDEO_ENCODER_OUTPUT_COLOR_PROFILE,
                    a::AMF_VIDEO_ENCODER_HEVC_OUTPUT_COLOR_PROFILE,
                    integer(color_profile(this.conversion.color)),
                )?;
                if format.codec == Codec::H265 {
                    this.enable_multi_hw();
                }
                this.set_initial(
                    b"OutColorPrimaries\0",
                    b"HevcOutColorPrimaries\0",
                    integer(color.primaries.into()),
                )?;
                this.set_initial(
                    a::AMF_VIDEO_ENCODER_OUTPUT_TRANSFER_CHARACTERISTIC,
                    a::AMF_VIDEO_ENCODER_HEVC_OUTPUT_TRANSFER_CHARACTERISTIC,
                    integer(color.transfer.into()),
                )?;
                this.set_initial(
                    b"OutMatrixCoeff\0",
                    b"HevcOutMatrixCoeff\0",
                    integer(color.matrix.into()),
                )?;
                this.set_initial(
                    b"FullRangeColor\0",
                    b"HevcNominalRange\0",
                    boolean(color.range == 2),
                )?;
            }
            let native = match (format.chroma, format.depth) {
                (1, 8) => 1,
                (1, 10) => 10,
                (3, 8) => 15,
                (3, 10) => 16,
                _ => unreachable!(),
            };
            if std::env::var_os("FARWING_AMF_LTR").is_some_and(|value| value == "1") {
                this.enable_ltr();
            }
            let comp = this.component.as_ref().unwrap();
            check(
                comp.vt()
                    .Init
                    .with_context(|| (t().system.amf_interface_missing)("Init"))?(
                    comp.raw(),
                    native,
                    size.0 as i32,
                    size.1 as i32,
                ),
                "Encoder Init",
            )?;
            this.maximum = this.query_maximum(native).unwrap_or(size);
            this.report_parameters("initialized");
            ensure!(
                size.0 <= this.maximum.0 && size.1 <= this.maximum.1,
                "{}",
                t().system.amf_size_exceeds_capability
            );
            Ok(this)
        }
    }
    fn initial_rate(&self, mut rate: Rate) -> Rate {
        let maximum = (|| -> Result<u32> {
            unsafe {
                let comp = self
                    .component
                    .as_ref()
                    .context(t().system.amf_encoder_closed)?;
                let mut caps = ptr::null_mut();
                check(
                    comp.vt()
                        .GetCaps
                        .with_context(|| (t().system.amf_interface_missing)("GetCaps"))?(
                        comp.raw(),
                        &mut caps,
                    ),
                    "GetCaps",
                )?;
                let caps = Owned::<a::AMFCaps>::take(caps)?;
                let mut value = a::AMFVariantStruct::default();
                let name = if self.format.codec == Codec::Av1 {
                    a::AMF_VIDEO_ENCODER_AV1_CAP_MAX_BITRATE
                } else if self.format.codec == Codec::H265 {
                    b"HevcMaxBitrate\0".as_slice()
                } else {
                    b"MaxBitrate\0".as_slice()
                };
                check(
                    caps.vt()
                        .GetProperty
                        .with_context(|| (t().system.amf_interface_missing)("GetProperty"))?(
                        caps.raw(),
                        wide(name).as_ptr(),
                        &mut value,
                    ),
                    "MaxBitrate",
                )?;
                ensure!(
                    value.type_ == 2 && value.__bindgen_anon_1.int64Value > 0,
                    "{}",
                    t().system.amf_invalid_max_bitrate
                );
                Ok(u32::try_from(value.__bindgen_anon_1.int64Value).unwrap_or(u32::MAX))
            }
        })();
        if let Ok(maximum) = maximum {
            if rate.peak.max(rate.target) > maximum {
                rate.target = rate.target.min(maximum);
                rate.peak = maximum;
                tracing::debug!(
                    maximum,
                    "AMF initial bitrate limited by hardware capability"
                );
            }
        }
        rate
    }
    fn report_parameters(&self, stage: &str) {
        if !tracing::enabled!(tracing::Level::DEBUG) {
            return;
        }
        let Some(component) = self.component.as_ref() else {
            return;
        };
        let Some(get) = component.vt().GetProperty else {
            return;
        };
        let mut values = serde_json::Map::new();
        for (avc, hevc) in [
            ("FrameRate", "HevcFrameRate"),
            ("TargetBitrate", "HevcTargetBitrate"),
            ("PeakBitrate", "HevcPeakBitrate"),
            ("VBVBufferSize", "HevcVBVBufferSize"),
            ("Usage", "HevcUsage"),
            ("QualityPreset", "HevcQualityPreset"),
            ("RateControlMethod", "HevcRateControlMethod"),
            ("QueryTimeout", "HevcQueryTimeout"),
            ("LowLatencyInternal", "LowLatencyInternal"),
            ("EnablePreAnalysis", "HevcEnablePreAnalysis"),
            (
                "RateControlPreanalysisEnable",
                "HevcRateControlPreAnalysisEnable",
            ),
            (
                "RateControlSkipFrameEnable",
                "HevcRateControlSkipFrameEnable",
            ),
            ("EnforceHRD", "HevcEnforceHRD"),
            ("FillerDataEnable", "HevcFillerDataEnable"),
            ("MinQP", "HevcMinQP_I"),
            ("MaxQP", "HevcMaxQP_I"),
        ] {
            let name = match self.format.codec {
                Codec::H264 => avc.to_owned(),
                Codec::H265 => hevc.to_owned(),
                Codec::Av1 => format!("Av1{avc}"),
            };
            let mut value = a::AMFVariantStruct::default();
            unsafe {
                let result = get(
                    component.raw(),
                    wide(format!("{name}\0").as_bytes()).as_ptr(),
                    &mut value,
                );
                if result != a::AMF_RESULT_AMF_OK {
                    values.insert(name, serde_json::json!({"status":result}));
                    continue;
                }
                let field = match value.type_ {
                    1 => serde_json::json!(value.__bindgen_anon_1.boolValue != 0),
                    2 => serde_json::json!(value.__bindgen_anon_1.int64Value),
                    7 => {
                        let r = value.__bindgen_anon_1.rateValue;
                        serde_json::json!({"num":r.num,"den":r.den})
                    }
                    _ => serde_json::json!({"type":value.type_}),
                };
                values.insert(name, field);
            }
        }
        tracing::debug!(stage,?self.format,parameters=%serde_json::Value::Object(values),"AMF reported encoder parameters");
    }
    fn set_initial(&self, avc: &[u8], hevc: &[u8], value: a::AMFVariantStruct) -> Result<()> {
        let result = self.set(avc, hevc, value);
        if self.format.codec == Codec::H265
            && matches!(hevc, b"HevcUsage\0" | b"HevcColorBitDepth\0")
        {
            return result;
        }
        if let Err(error) = result {
            // Optional properties do not replace the actual Init
            // and output probe. Requested SPS/depth is still verified outside.
            tracing::debug!(%error, "AMF initial property rejected; continuing actual probe");
        }
        Ok(())
    }
    fn set(&self, avc: &[u8], hevc: &[u8], value: a::AMFVariantStruct) -> Result<()> {
        let av1;
        let name = wide(if self.format.codec == Codec::Av1 {
            if avc.starts_with(b"Av1") {
                avc
            } else {
                av1 = [b"Av1".as_slice(), avc].concat();
                &av1
            }
        } else if self.format.codec == Codec::H264 {
            avc
        } else {
            hevc
        });
        let comp = self
            .component
            .as_ref()
            .context(t().system.amf_encoder_closed)?;
        unsafe {
            check(
                comp.vt()
                    .SetProperty
                    .with_context(|| (t().system.amf_interface_missing)("SetProperty"))?(
                    comp.raw(),
                    name.as_ptr(),
                    value,
                ),
                &String::from_utf16_lossy(name.strip_suffix(&[0]).unwrap_or(&name)),
            )
        }
    }
    fn configure_av1(&self, size: (u32, u32), rate: Rate) -> Result<()> {
        let color = self.conversion.color.space(None);
        for (name, value) in [
            (a::AMF_VIDEO_ENCODER_AV1_USAGE, 2),
            (a::AMF_VIDEO_ENCODER_AV1_ENCODING_LATENCY_MODE, 3),
            (a::AMF_VIDEO_ENCODER_AV1_QUALITY_PRESET, 100),
            (a::AMF_VIDEO_ENCODER_AV1_PROFILE, 1),
            (
                a::AMF_VIDEO_ENCODER_AV1_OUTPUT_COLOR_PROFILE,
                color_profile(self.conversion.color),
            ),
            (
                a::AMF_VIDEO_ENCODER_AV1_COLOR_BIT_DEPTH,
                self.format.depth as i64,
            ),
            (a::AMF_VIDEO_ENCODER_AV1_RATE_CONTROL_METHOD, 1),
            (a::AMF_VIDEO_ENCODER_AV1_TARGET_BITRATE, rate.target as i64),
            (
                a::AMF_VIDEO_ENCODER_AV1_PEAK_BITRATE,
                rate.peak.max(rate.target) as i64,
            ),
            (
                a::AMF_VIDEO_ENCODER_AV1_VBV_BUFFER_SIZE,
                i64::from(rate.target / rate.fps.max(1)),
            ),
            (a::AMF_VIDEO_ENCODER_AV1_INITIAL_VBV_BUFFER_FULLNESS, 0),
            (a::AMF_VIDEO_ENCODER_AV1_INTRA_PERIOD, 0),
            (a::AMF_VIDEO_ENCODER_AV1_HEADER_INSERTION_MODE, 2),
            (a::AMF_VIDEO_ENCODER_AV1_NUM_TEMPORAL_LAYERS, 1),
            (a::AMF_VIDEO_ENCODER_AV1_QUERY_TIMEOUT, 10),
            (
                a::AMF_VIDEO_ENCODER_AV1_OUTPUT_COLOR_PRIMARIES,
                color.primaries as i64,
            ),
            (
                a::AMF_VIDEO_ENCODER_AV1_OUTPUT_TRANSFER_CHARACTERISTIC,
                color.transfer as i64,
            ),
            (
                a::AMF_VIDEO_ENCODER_AV1_OUTPUT_MATRIX_COEFF,
                color.matrix as i64,
            ),
        ] {
            self.set(name, name, integer(value))?;
        }
        for name in [
            a::AMF_VIDEO_ENCODER_AV1_PRE_ANALYSIS_ENABLE,
            a::AMF_VIDEO_ENCODER_AV1_RATE_CONTROL_PREENCODE,
            a::AMF_VIDEO_ENCODER_AV1_TILE_GROUP_OBU,
            a::AMF_VIDEO_ENCODER_AV1_ENFORCE_HRD,
            a::AMF_VIDEO_ENCODER_AV1_FILLER_DATA,
        ] {
            self.set(name, name, boolean(false))?;
        }
        // Older AV1 runtimes have no B-picture properties and only encode
        // forward frames. On runtimes exposing them, keep reordering disabled.
        for name in [
            a::AMF_VIDEO_ENCODER_AV1_MAX_CONSECUTIVE_BPICTURES,
            a::AMF_VIDEO_ENCODER_AV1_B_PIC_PATTERN,
        ] {
            self.set_initial(name, name, integer(0))?;
        }
        self.set(
            a::AMF_VIDEO_ENCODER_AV1_FRAMESIZE,
            a::AMF_VIDEO_ENCODER_AV1_FRAMESIZE,
            a::AMFVariantStruct {
                type_: 5,
                __bindgen_anon_1: a::AMFVariantStruct__bindgen_ty_1 {
                    sizeValue: a::AMFSize {
                        width: size.0 as i32,
                        height: size.1 as i32,
                    },
                },
            },
        )?;
        self.set(
            a::AMF_VIDEO_ENCODER_AV1_FRAMERATE,
            a::AMF_VIDEO_ENCODER_AV1_FRAMERATE,
            frame_rate(rate.fps),
        )
    }
    /// Name of a property for this codec.
    fn named(&self, avc: &'static [u8], hevc: &'static [u8], av1: &'static [u8]) -> &'static [u8] {
        match self.format.codec {
            Codec::H264 => avc,
            Codec::H265 => hevc,
            Codec::Av1 => av1,
        }
    }
    /// Two long-term references, one active reference (rfi-ltr-brief §6.7).
    fn enable_ltr(&mut self) {
        let settings = [
            (self.named(b"MaxOfLTRFrames\0", b"HevcMaxOfLTRFrames\0", b"Av1MaxNumLTRFrames\0"), 2),
            (self.named(b"LTRMode\0", b"HevcLTRMode\0", b"Av1LTRMode\0"), 1),
            (self.named(b"MaxNumRefFrames\0", b"HevcMaxNumRefFrames\0", b"Av1MaxNumRefFrames\0"), 1),
        ];
        let mut applied = true;
        for (name, value) in settings {
            if let Err(error) = self.set(name, name, integer(value)) {
                tracing::warn!(%error, "AMF rejected long-term references; encoding without");
                applied = false;
                break;
            }
        }
        if !applied {
            let name = self.named(b"MaxOfLTRFrames\0", b"HevcMaxOfLTRFrames\0", b"Av1MaxNumLTRFrames\0");
            let _ = self.set(name, name, integer(0));
            return;
        }
        self.ltr_slots = 2;
        tracing::info!(format = ?self.format, "AMF long-term references enabled (unverified hardware path)");
    }
    pub fn ltr_slots(&self) -> u8 {
        self.ltr_slots
    }
    fn enable_multi_hw(&self) {
        let result = (|| -> Result<()> {
            unsafe {
                let comp = self
                    .component
                    .as_ref()
                    .context(t().system.amf_encoder_closed)?;
                let mut caps = ptr::null_mut();
                check(
                    comp.vt()
                        .GetCaps
                        .with_context(|| (t().system.amf_interface_missing)("GetCaps"))?(
                        comp.raw(),
                        &mut caps,
                    ),
                    "GetCaps",
                )?;
                let caps = Owned::<a::AMFCaps>::take(caps)?;
                let mut count = a::AMFVariantStruct::default();
                check(
                    caps.vt()
                        .GetProperty
                        .with_context(|| (t().system.amf_interface_missing)("GetProperty"))?(
                        caps.raw(),
                        wide(a::AMF_VIDEO_ENCODER_HEVC_CAP_NUM_OF_HW_INSTANCES).as_ptr(),
                        &mut count,
                    ),
                    "HevcNumOfHwInstances",
                )?;
                if count.type_ == 2 && count.__bindgen_anon_1.int64Value > 1 {
                    self.set(
                        a::AMF_VIDEO_ENCODER_HEVC_MULTI_HW_INSTANCE_ENCODE,
                        a::AMF_VIDEO_ENCODER_HEVC_MULTI_HW_INSTANCE_ENCODE,
                        boolean(true),
                    )?;
                }
                Ok(())
            }
        })();
        if let Err(error) = result {
            tracing::debug!(%error,"AMF optional multi-instance configuration unavailable");
        }
    }
    fn query_maximum(&self, native: i32) -> Result<(u32, u32)> {
        unsafe {
            let comp = self.component.as_ref().unwrap();
            let mut caps = ptr::null_mut();
            check(
                comp.vt()
                    .GetCaps
                    .with_context(|| (t().system.amf_interface_missing)("GetCaps"))?(
                    comp.raw(),
                    &mut caps,
                ),
                "GetCaps",
            )?;
            let caps = Owned::<a::AMFCaps>::take(caps)?;
            let mut input = ptr::null_mut();
            check(
                caps.vt()
                    .GetInputCaps
                    .with_context(|| (t().system.amf_interface_missing)("GetInputCaps"))?(
                    caps.raw(),
                    &mut input,
                ),
                "GetInputCaps",
            )?;
            let input = Owned::<a::AMFIOCaps>::take(input)?;
            let vt = input.vt();
            let count = vt
                .GetNumOfFormats
                .with_context(|| (t().system.amf_interface_missing)("GetNumOfFormats"))?(
                input.raw(),
            );
            ensure!(
                (0..=128).contains(&count),
                "{}",
                t().system.amf_invalid_format_caps
            );
            let mut supported = false;
            for index in 0..count {
                let (mut format, mut is_native) = (0, 0u8);
                check(
                    vt.GetFormatAt
                        .with_context(|| (t().system.amf_interface_missing)("GetFormatAt"))?(
                        input.raw(),
                        index,
                        &mut format,
                        &mut is_native,
                    ),
                    "GetFormatAt",
                )?;
                supported |= format == native;
            }
            ensure!(supported, "{}", t().system.amf_format_unsupported);
            let (mut min_w, mut max_w, mut min_h, mut max_h) = (0, 0, 0, 0);
            vt.GetWidthRange
                .with_context(|| (t().system.amf_interface_missing)("GetWidthRange"))?(
                input.raw(),
                &mut min_w,
                &mut max_w,
            );
            vt.GetHeightRange
                .with_context(|| (t().system.amf_interface_missing)("GetHeightRange"))?(
                input.raw(),
                &mut min_h,
                &mut max_h,
            );
            ensure!(
                (1..=16384).contains(&max_w) && (1..=16384).contains(&max_h),
                "{}",
                t().system.amf_invalid_max_size
            );
            Ok((max_w as u32, max_h as u32))
        }
    }
    pub fn maximum_size(&self) -> (u32, u32) {
        self.maximum
    }
    pub fn configure(&mut self, rate: Rate) -> Result<bool> {
        let Some(update) = self.frame_rate.decide(rate) else {
            return Ok(false);
        };
        let requested = rate;
        let rate = update.rate;
        let changed_quality = self.rate.quality != rate.quality;
        // Runtime property failures are logged individually; other
        // accepted properties remain in force. No fictitious atomic rollback.
        for result in [
            self.set(
                b"FrameRate\0",
                b"HevcFrameRate\0",
                frame_rate(update.rate.fps),
            ),
            self.set(
                b"TargetBitrate\0",
                b"HevcTargetBitrate\0",
                integer(rate.target.into()),
            ),
            self.set(
                b"VBVBufferSize\0",
                b"HevcVBVBufferSize\0",
                integer(runtime_vbv(rate, self.format.codec, update.buffer_fps)),
            ),
            self.set(
                b"PeakBitrate\0",
                b"HevcPeakBitrate\0",
                integer(rate.peak.max(rate.target).into()),
            ),
        ] {
            if let Err(error) = result {
                tracing::warn!(%error,"AMF runtime property rejected");
            }
        }
        self.rate = requested;
        self.configured_fps = update.rate.fps;
        self.frame_rate.commit(update);
        self.report_parameters("reconfigured");
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
            !cancel.load(std::sync::atomic::Ordering::Acquire),
            "{}",
            t().system.encode_cancelled
        );
        if self.pending.is_some() {
            return self.output(cancel);
        }
        self.frame_rate.input(timestamp);
        self.conversion.convert(texture)?;
        unsafe {
            let context = self.context.as_ref().unwrap();
            let mut surface = ptr::null_mut();
            check(
                context
                    .vt()
                    .CreateSurfaceFromDX11Native
                    .with_context(|| (t().system.amf_interface_missing)("DX11Surface"))?(
                    context.raw(),
                    self.conversion.output.as_raw(),
                    &mut surface,
                    ptr::null_mut(),
                ),
                "CreateSurfaceFromDX11Native",
            )?;
            let surface = Owned::<a::AMFSurface>::take(surface)?;
            let vt = surface.vt();
            vt.SetPts
                .with_context(|| (t().system.amf_interface_missing)("SetPts"))?(
                surface.raw(),
                timestamp,
            );
            vt.SetDuration
                .with_context(|| (t().system.amf_interface_missing)("SetDuration"))?(
                surface.raw(),
                10_000_000 / i64::from(self.configured_fps.max(1)),
            );
            if keyframe {
                let settings: Vec<(&[u8], a::AMFVariantStruct)> =
                    if self.format.codec == Codec::H264 {
                        vec![
                            (b"ForcePictureType\0", integer(2)),
                            (b"InsertSPS\0", boolean(true)),
                            (b"InsertPPS\0", boolean(true)),
                        ]
                    } else if self.format.codec == Codec::Av1 {
                        vec![
                            (a::AMF_VIDEO_ENCODER_AV1_FORCE_FRAME_TYPE, integer(1)),
                            (
                                a::AMF_VIDEO_ENCODER_AV1_FORCE_INSERT_SEQUENCE_HEADER,
                                boolean(true),
                            ),
                        ]
                    } else {
                        vec![
                            (b"HevcForcePictureType\0", integer(2)),
                            (b"HevcInsertHeader\0", boolean(true)),
                        ]
                    };
                for (name, value) in settings {
                    check(
                        vt.SetProperty.with_context(|| {
                            (t().system.amf_interface_missing)("surface SetProperty")
                        })?(surface.raw(), wide(name).as_ptr(), value),
                        "force IDR",
                    )?;
                }
            }
            if self.ltr_slots > 0 {
                let mark = control.mark_ltr.filter(|slot| *slot < self.ltr_slots);
                let reference = control
                    .use_ltr
                    .filter(|slot| *slot < self.ltr_slots && !keyframe);
                let mut settings: Vec<(&[u8], a::AMFVariantStruct)> = Vec::new();
                if let Some(slot) = mark {
                    settings.push((
                        self.named(
                            b"MarkCurrentWithLTRIndex\0",
                            b"HevcMarkCurrentWithLTRIndex\0",
                            b"Av1MarkCurrentWithLTRIndex\0",
                        ),
                        integer(i64::from(slot)),
                    ));
                }
                if let Some(slot) = reference {
                    settings.push((
                        self.named(
                            b"ForceLTRReferenceBitfield\0",
                            b"HevcForceLTRReferenceBitfield\0",
                            b"Av1ForceLTRReferenceBitfield\0",
                        ),
                        integer(1 << slot),
                    ));
                }
                for (name, value) in settings {
                    check(
                        vt.SetProperty.with_context(|| {
                            (t().system.amf_interface_missing)("surface SetProperty")
                        })?(surface.raw(), wide(name).as_ptr(), value),
                        "long-term reference",
                    )?;
                }
            }
            let comp = self.component.as_ref().unwrap();
            let submit = comp
                .vt()
                .SubmitInput
                .with_context(|| (t().system.amf_interface_missing)("SubmitInput"))?;
            let mut accepted = false;
            for attempt in 0..500 {
                ensure!(
                    !cancel.load(std::sync::atomic::Ordering::Acquire),
                    "{}",
                    t().system.encode_cancelled
                );
                let status = submit(comp.raw(), surface.raw().cast());
                if status == a::AMF_RESULT_AMF_OK {
                    accepted = true;
                    break;
                }
                if (status as u32) <= 44 && (0x100000002042u64 >> status as u32) & 1 != 0 {
                    check(status, "SubmitInput")?;
                }
                super::encoder::retry_pause(attempt);
            }
            if !accepted {
                return Ok(Vec::new());
            }
            self.pending = Some((surface, keyframe, control));
        }
        self.output(cancel)
    }
    fn output(&mut self, cancel: &std::sync::atomic::AtomicBool) -> Result<Vec<Encoded>> {
        unsafe {
            let comp = self.component.as_ref().unwrap();
            let mut data = ptr::null_mut();
            for attempt in 0..500 {
                // Keep pending native input alive until Terminate in Drop.
                ensure!(
                    !cancel.load(std::sync::atomic::Ordering::Acquire),
                    "{}",
                    t().system.encode_cancelled
                );
                let status = comp
                    .vt()
                    .QueryOutput
                    .with_context(|| (t().system.amf_interface_missing)("QueryOutput"))?(
                    comp.raw(),
                    &mut data,
                );
                if status != a::AMF_RESULT_AMF_OK && status != a::AMF_RESULT_AMF_REPEAT {
                    // Keep the outstanding surface alive until the encoder is closed.
                    if !data.is_null() {
                        drop(Owned::<a::AMFData>::take(data)?);
                    }
                    check(status, "QueryOutput")?;
                }
                if !data.is_null() {
                    break;
                }
                super::encoder::retry_pause(attempt);
            }
            ensure!(
                !data.is_null(),
                "{}",
                t().system.amf_output_attempts_exceeded
            );
            let data = Owned::<a::AMFData>::take(data)?;
            let iid = a::AMFGuid {
                data1: 0xb04b7248,
                data2: 0xb6f0,
                data3: 0x4321,
                data41: 0xb6,
                data42: 0x91,
                data43: 0xba,
                data44: 0xa4,
                data45: 0x74,
                data46: 0x0f,
                data47: 0x9f,
                data48: 0xcb,
            };
            let mut buffer = ptr::null_mut();
            check(
                data.vt()
                    .QueryInterface
                    .with_context(|| (t().system.amf_interface_missing)("QueryInterface"))?(
                    data.raw(),
                    &iid,
                    &mut buffer,
                ),
                "Buffer interface",
            )?;
            let buffer = Owned::<a::AMFBuffer>::take(buffer.cast())?;
            let size = buffer
                .vt()
                .GetSize
                .with_context(|| (t().system.amf_interface_missing)("GetSize"))?(
                buffer.raw()
            );
            let bytes = buffer
                .vt()
                .GetNative
                .with_context(|| (t().system.amf_interface_missing)("GetNative"))?(
                buffer.raw()
            )
            .cast::<u8>();
            ensure!(
                !bytes.is_null() && size > 0 && size <= 64 * 1024 * 1024,
                "{}",
                t().system.amf_invalid_bitstream
            );
            let mut kind = a::AMFVariantStruct::default();
            let name = wide(if self.format.codec == Codec::Av1 {
                a::AMF_VIDEO_ENCODER_AV1_OUTPUT_FRAME_TYPE
            } else if self.format.codec == Codec::H264 {
                b"OutputDataType\0"
            } else {
                b"HevcOutputDataType\0"
            });
            let status = buffer
                .vt()
                .GetProperty
                .with_context(|| (t().system.amf_interface_missing)("GetProperty"))?(
                buffer.raw(),
                name.as_ptr(),
                &mut kind,
            );
            let keyframe = if status == a::AMF_RESULT_AMF_OK && kind.type_ == 2 {
                kind.__bindgen_anon_1.int64Value == 0
            } else {
                tracing::debug!(
                    status,
                    "AMF frame type unavailable; preserving requested type"
                );
                self.pending
                    .as_ref()
                    .is_some_and(|(_, requested, _)| *requested)
            };
            // What the encoder actually did with long-term references.
            let mut ltr = crate::media::encoding::LtrOutput::default();
            if self.ltr_slots > 0 {
                let property = |name: &[u8]| -> Option<i64> {
                    let mut value = a::AMFVariantStruct::default();
                    let name = wide(name);
                    let status = buffer.vt().GetProperty?(buffer.raw(), name.as_ptr(), &mut value);
                    (status == a::AMF_RESULT_AMF_OK && value.type_ == 2)
                        .then(|| value.__bindgen_anon_1.int64Value)
                };
                ltr.marked = property(self.named(
                    b"MarkedLTRIndex\0",
                    b"HevcMarkedLTRIndex\0",
                    b"Av1MarkedLTRIndex\0",
                ))
                .and_then(|slot| u8::try_from(slot).ok())
                .filter(|slot| *slot < self.ltr_slots);
                let requested = self.pending.as_ref().and_then(|(_, _, control)| control.use_ltr);
                let referenced = property(self.named(
                    b"ReferencedLTRIndexBitfield\0",
                    b"HevcReferencedLTRIndexBitfield\0",
                    b"Av1ReferencedLTRIndexBitfield\0",
                ))
                .unwrap_or(0);
                ltr.used = requested.filter(|slot| !keyframe && referenced & (1 << slot) != 0);
                if requested.is_some() || ltr.marked.is_some() {
                    tracing::debug!(target: "farwing::recovery", ?ltr, referenced, keyframe,
                        "AMF long-term reference frame");
                }
            }
            let frame = Encoded {
                data: std::slice::from_raw_parts(bytes, size).to_vec(),
                keyframe,
                timestamp_100ns: buffer
                    .vt()
                    .GetPts
                    .with_context(|| (t().system.amf_interface_missing)("GetPts"))?(
                    buffer.raw()
                ),
                is_new: true,
                timing: None,
                format: self.format,
                color: self.conversion.color.space(None),
                meta: None,
                ltr,
            };
            self.pending = None;
            Ok(vec![frame])
        }
    }
}
fn vbv(rate: Rate) -> i64 {
    i64::from(rate.peak.max(rate.target))
}
fn runtime_vbv(rate: Rate, codec: Codec, fps: u32) -> i64 {
    if rate.smooth {
        // About one second: quality first for watching video.
        return vbv(rate);
    }
    let frames = if codec != Codec::Av1 && rate.peak.max(rate.target) < 120_000_000 {
        5
    } else {
        1
    };
    let original = vbv(rate) * frames / i64::from(fps.max(1));
    if codec == Codec::Av1 {
        original
    } else {
        // AMF needs more transient allocation than NVENC to recover an IDR
        // without a run of duplicate pictures. Preserve lower-FPS headroom.
        original.max(vbv(rate) / 5)
    }
}
fn color_profile(color: super::format::Color) -> i64 {
    if color.is_hdr() {
        a::AMF_VIDEO_CONVERTER_COLOR_PROFILE_ENUM_AMF_VIDEO_CONVERTER_COLOR_PROFILE_FULL_2020.into()
    } else {
        a::AMF_VIDEO_CONVERTER_COLOR_PROFILE_ENUM_AMF_VIDEO_CONVERTER_COLOR_PROFILE_601.into()
    }
}
impl Drop for Encoder {
    fn drop(&mut self) {
        unsafe {
            if let Some(comp) = self.component.as_ref() {
                if let Some(terminate) = comp.vt().Terminate {
                    terminate(comp.raw());
                }
            }
            self.pending.take();
            self.component.take();
            if let Some(context) = self.context.as_ref() {
                if let Some(terminate) = context.vt().Terminate {
                    terminate(context.raw());
                }
            }
            self.context.take();
        }
    }
}
