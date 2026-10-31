//! Cameras through Media Foundation (`docs/protocol/camera-design.md` §3):
//! the list with each camera's modes, a capture that hands the encoder
//! pipeline BGRA textures ([`super::source::FrameSource`]), and the picture
//! adjustments. `FARWING_DEV_CAMERA=synthetic` adds a generated test camera
//! for machines without one.
mod controls;
mod reader;
mod synthetic;

use super::capture::{Frame, Source};
use crate::i18n::t;
use crate::media::camera::{Camera, Color, ControlState, Matrix, Mode, PixelFormat, Spec};
use anyhow::{Context, Result};
use std::sync::Arc;
use std::time::Instant;
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::Common::*;
use windows::Win32::Media::MediaFoundation::*;
use windows::core::{GUID, PWSTR};

pub(crate) use controls::{controls, set_control};

/// The ID of the generated test camera.
pub(crate) const SYNTHETIC: &str = "synthetic";

fn synthetic_enabled() -> bool {
    std::env::var("FARWING_DEV_CAMERA").is_ok_and(|v| v.eq_ignore_ascii_case(SYNTHETIC))
}

/// Media Foundation, started once for the process.
fn startup() -> Result<()> {
    static STARTED: std::sync::OnceLock<Result<(), String>> = std::sync::OnceLock::new();
    STARTED
        .get_or_init(|| unsafe { MFStartup(MF_VERSION, MFSTARTUP_FULL) }.map_err(|e| e.to_string()))
        .clone()
        .map_err(anyhow::Error::msg)
}

/// COM on this thread for as long as it lives (multithreaded; a thread
/// already in another apartment keeps it).
struct Apartment(bool);
impl Apartment {
    fn enter() -> Result<Self> {
        use windows::Win32::System::Com::{COINIT_MULTITHREADED, CoInitializeEx};
        let hr = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
        if hr.0 == 0x80010106u32 as i32 {
            return Ok(Self(false));
        }
        hr.ok()?;
        Ok(Self(true))
    }
}
impl Drop for Apartment {
    fn drop(&mut self) {
        if self.0 {
            unsafe { windows::Win32::System::Com::CoUninitialize() };
        }
    }
}

/// Why a camera could not be opened or read, in the user's words.
fn explain(error: &windows::core::Error) -> String {
    const IN_USE: [u32; 4] = [
        0xC00D3704, // MF_E_HW_MFT_FAILED_START_STREAMING
        0xC00D3EA3, // MF_E_VIDEO_RECORDING_DEVICE_PREEMPTED
        0x80070020, // sharing violation
        0x800700AA, // busy
    ];
    let code = error.code().0 as u32;
    if code == 0x80070005 {
        t().camera.privacy_denied.to_owned()
    } else if IN_USE.contains(&code) {
        t().camera.in_use.to_owned()
    } else if code == 0xC00D3EA2 || code == 0x80070015 || code == 0xC00D36FA {
        // Invalidated, not ready, or the device is gone.
        t().camera.removed.to_owned()
    } else {
        format!("{}: {error}", t().camera.open_failed)
    }
}

fn attributes(count: u32) -> Result<IMFAttributes> {
    let mut attributes = None;
    unsafe { MFCreateAttributes(&mut attributes, count) }?;
    attributes.context("MFCreateAttributes")
}

fn string(activate: &IMFActivate, key: &GUID) -> Result<String> {
    let mut value = PWSTR::null();
    let mut length = 0;
    unsafe {
        activate.GetAllocatedString(key, &mut value, &mut length)?;
        let text = value.to_string();
        windows::Win32::System::Com::CoTaskMemFree(Some(value.0.cast()));
        Ok(text?)
    }
}

/// The video capture devices, as activation objects with their link and name.
fn devices() -> Result<Vec<(IMFActivate, String, String)>> {
    startup()?;
    let filter = attributes(1)?;
    unsafe {
        filter.SetGUID(
            &MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE,
            &MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE_VIDCAP_GUID,
        )?;
        let mut list: *mut Option<IMFActivate> = std::ptr::null_mut();
        let mut count = 0u32;
        MFEnumDeviceSources(&filter, &mut list, &mut count)?;
        let mut devices = Vec::new();
        for index in 0..count as usize {
            // Take each element: dropping it releases the reference.
            let Some(activate) = std::ptr::read(list.add(index)) else {
                continue;
            };
            let link = string(&activate, &MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE_VIDCAP_SYMBOLIC_LINK);
            let name = string(&activate, &MF_DEVSOURCE_ATTRIBUTE_FRIENDLY_NAME);
            if let (Ok(link), Ok(name)) = (link, name) {
                devices.push((activate, link, name));
            }
        }
        windows::Win32::System::Com::CoTaskMemFree(Some(list.cast()));
        Ok(devices)
    }
}

fn split(value: u64) -> (u32, u32) {
    ((value >> 32) as u32, value as u32)
}

fn pixel_format(subtype: &GUID) -> Option<PixelFormat> {
    match *subtype {
        s if s == MFVideoFormat_NV12 => Some(PixelFormat::Nv12),
        s if s == MFVideoFormat_YUY2 => Some(PixelFormat::Yuy2),
        s if s == MFVideoFormat_MJPG => Some(PixelFormat::Mjpeg),
        s if s == MFVideoFormat_H264 => Some(PixelFormat::H264),
        _ => None,
    }
}

/// The mode a media type describes, if it is a format this capture reads.
fn mode_of(media_type: &IMFMediaType) -> Option<Mode> {
    unsafe {
        let format = pixel_format(&media_type.GetGUID(&MF_MT_SUBTYPE).ok()?)?;
        let (width, height) = split(media_type.GetUINT64(&MF_MT_FRAME_SIZE).ok()?);
        let (fps_numerator, fps_denominator) = split(media_type.GetUINT64(&MF_MT_FRAME_RATE).ok()?);
        (width > 0 && height > 0 && fps_numerator > 0 && fps_denominator > 0).then_some(Mode {
            width,
            height,
            fps_numerator,
            fps_denominator,
            format,
        })
    }
}

/// The native media types of a camera's first video stream.
fn native_types(reader: &IMFSourceReader) -> Vec<IMFMediaType> {
    (0..)
        .map_while(|index| unsafe {
            reader
                .GetNativeMediaType(MF_SOURCE_READER_FIRST_VIDEO_STREAM.0 as u32, index)
                .ok()
        })
        .collect()
}

fn modes_of(types: &[IMFMediaType]) -> Vec<Mode> {
    let mut modes: Vec<Mode> = types.iter().filter_map(mode_of).collect();
    modes.sort_by_key(|m| (std::cmp::Reverse(m.pixels()), std::cmp::Reverse(m.fps()), m.format));
    modes.dedup();
    modes
}

/// The cameras of this machine with their modes. A camera that cannot be
/// opened now (in use, not allowed) is still listed, without modes.
pub(crate) fn list() -> Result<Vec<Camera>> {
    let _apartment = Apartment::enter()?;
    let mut cameras = Vec::new();
    for (activate, id, name) in devices()? {
        let read = (|| -> windows::core::Result<(Vec<Mode>, Vec<ControlState>)> {
            let source: IMFMediaSource = unsafe { activate.ActivateObject() }?;
            let reader = unsafe { MFCreateSourceReaderFromMediaSource(&source, None) };
            let read = reader.map(|reader| (modes_of(&native_types(&reader)), controls::read_all(&source)));
            unsafe {
                let _ = source.Shutdown();
                let _ = activate.ShutdownObject();
            }
            read
        })();
        let (modes, controls, unavailable) = match read {
            Ok((modes, controls)) => (modes, controls, None),
            Err(error) => {
                tracing::info!(camera = %name, %error, "camera modes not read");
                (Vec::new(), Vec::new(), Some(explain(&error)))
            }
        };
        cameras.push(Camera {
            id,
            name,
            modes,
            unavailable,
            controls,
        });
    }
    if synthetic_enabled() {
        cameras.push(synthetic::camera());
    }
    Ok(cameras)
}

/// The adapter a camera's pictures go to when nothing else decides: the
/// first one (the one Windows renders the desktop on).
pub(crate) fn default_adapter() -> Result<u64> {
    use windows::Win32::Graphics::Dxgi::{CreateDXGIFactory1, IDXGIFactory1};
    unsafe {
        let factory: IDXGIFactory1 = CreateDXGIFactory1()?;
        let luid = factory.EnumAdapters1(0)?.GetDesc1()?.AdapterLuid;
        Ok((u64::from(luid.HighPart as u32) << 32) | u64::from(luid.LowPart))
    }
}

/// A picture from the camera in system memory: NV12, rows of `width`
/// bytes, the chroma rows right after the luma rows.
pub(super) struct Picture {
    pub data: Vec<u8>,
    pub width: u32,
    pub height: u32,
    pub captured: Instant,
    /// What the camera (or its decoder) reports.
    pub matrix: Matrix,
    pub full_range: bool,
}

/// Where pictures come from: the device, or the generator.
enum Feed {
    Device(reader::Reader),
    Synthetic(synthetic::Generator),
}

impl Feed {
    /// The next picture after `seen`, waiting at most `timeout` ms.
    fn next(&mut self, seen: u64, timeout: u32) -> Result<Option<(u64, Arc<Picture>)>> {
        match self {
            Self::Device(reader) => reader.next(seen, timeout),
            Self::Synthetic(generator) => Ok(generator.next(seen, timeout)),
        }
    }
}

/// A camera capture for an encoder pipeline: pictures uploaded to the
/// adapter and converted to BGRA.
pub(crate) struct Capture {
    spec: Spec,
    device: ID3D11Device,
    context: ID3D11DeviceContext,
    feed: Feed,
    seen: u64,
    converter: super::video_shader::VideoShaderRenderer,
    scaler: super::preprocess::Converter,
    output: Option<(ID3D11Texture2D, ID3D11RenderTargetView, (u32, u32))>,
    /// The last converted picture and the request it was scaled for.
    last: Option<(ID3D11Texture2D, Instant, super::preprocess::Request)>,
    _source: Option<controls::Registration>,
}

impl Capture {
    pub(crate) fn open(spec: &Spec) -> Result<Self> {
        let (device, context) = super::capture::create_device(spec.adapter)?;
        let (feed, registration) = if spec.camera == SYNTHETIC {
            (Feed::Synthetic(synthetic::Generator::start(spec.mode)), None)
        } else {
            let reader = reader::Reader::open(spec)?;
            let registration = controls::register(&spec.camera, reader.media_source());
            (Feed::Device(reader), Some(registration))
        };
        Ok(Self {
            converter: super::video_shader::VideoShaderRenderer::new(&device)?,
            scaler: super::preprocess::Converter::new(&device)?,
            spec: spec.clone(),
            device,
            context,
            feed,
            seen: 0,
            output: None,
            last: None,
            _source: registration,
        })
    }

    /// Upload `picture` and convert it to the BGRA output texture.
    fn convert(&mut self, picture: &Picture) -> Result<ID3D11Texture2D> {
        let (width, height) = (picture.width, picture.height);
        let desc = D3D11_TEXTURE2D_DESC {
            Width: width,
            Height: height,
            MipLevels: 1,
            ArraySize: 1,
            Format: DXGI_FORMAT_NV12,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Usage: D3D11_USAGE_DEFAULT,
            BindFlags: D3D11_BIND_SHADER_RESOURCE.0 as u32,
            ..Default::default()
        };
        let data = D3D11_SUBRESOURCE_DATA {
            pSysMem: picture.data.as_ptr().cast(),
            SysMemPitch: width,
            SysMemSlicePitch: 0,
        };
        let mut uploaded = None;
        unsafe { self.device.CreateTexture2D(&desc, Some(&data), Some(&mut uploaded)) }
            .context("upload camera picture")?;
        let uploaded = uploaded.context("upload camera picture")?;
        if self.output.as_ref().is_none_or(|(_, _, size)| *size != (width, height)) {
            let mut texture = None;
            let mut target = None;
            unsafe {
                self.device.CreateTexture2D(
                    &D3D11_TEXTURE2D_DESC {
                        Format: DXGI_FORMAT_B8G8R8A8_UNORM,
                        BindFlags: (D3D11_BIND_RENDER_TARGET.0 | D3D11_BIND_SHADER_RESOURCE.0) as u32,
                        ..desc
                    },
                    None,
                    Some(&mut texture),
                )?;
                let texture = texture.context("camera output texture")?;
                self.device.CreateRenderTargetView(&texture, None, Some(&mut target))?;
                self.output = Some((texture, target.context("camera output target")?, (width, height)));
            }
        }
        let (texture, target, _) = self.output.as_ref().expect("camera output");
        let color = crate::media::video_color::RenderColor {
            matrix: match self.spec.color.matrix.unwrap_or(picture.matrix) {
                Matrix::Bt601 => crate::media::video_color::ColorMatrix::Bt601,
                Matrix::Bt709 => crate::media::video_color::ColorMatrix::Bt709,
            },
            full_range: self.spec.color.full_range.unwrap_or(picture.full_range),
            hdr_peak_nits: None,
        };
        self.converter.draw(
            &self.device,
            &self.context,
            target,
            &uploaded,
            super::video_shader::VideoTextureView {
                color,
                array_slice: 0,
                input_format: DXGI_FORMAT_NV12,
                visible_x: 0,
                visible_y: 0,
                coded_width: width,
                coded_height: height,
                width,
                height,
            },
            winit::dpi::PhysicalSize::new(width, height),
            0,
        )?;
        Ok(texture.clone())
    }
}

impl super::source::FrameSource for Capture {
    fn device(&self) -> &ID3D11Device {
        &self.device
    }
    fn generation(&self) -> u64 {
        0
    }
    fn available(&self) -> bool {
        true
    }
    fn hdr_available(&self) -> bool {
        false
    }
    fn backend_name(&self) -> &'static str {
        "Media Foundation"
    }
    fn descriptor(&self) -> Source {
        Source::Camera(self.spec.clone())
    }
    fn next(
        &mut self,
        timeout: u32,
        tier: crate::media::geometry::Tier,
        _cursor: bool,
        _hdr: bool,
        maximum: (u32, u32),
    ) -> Result<Option<Frame>> {
        let request = super::preprocess::Request {
            tier,
            maximum,
            hdr: false,
        };
        let (texture, captured, is_new) = match self.feed.next(self.seen, timeout)? {
            Some((sequence, picture)) => {
                self.seen = sequence;
                (self.convert(&picture)?, picture.captured, true)
            }
            // The same picture again when only the request changed.
            None => match &self.last {
                Some((texture, captured, last)) if *last != request => (texture.clone(), *captured, false),
                _ => return Ok(None),
            },
        };
        let mut desc = D3D11_TEXTURE2D_DESC::default();
        unsafe { texture.GetDesc(&mut desc) };
        let scaled = self.scaler.convert(
            &self.device,
            &self.context,
            &texture,
            desc,
            DXGI_MODE_ROTATION_IDENTITY,
            1.0,
            None,
            request,
            None,
        )?;
        unsafe { self.context.Flush() };
        self.last = Some((texture, captured, request));
        let mut output = D3D11_TEXTURE2D_DESC::default();
        unsafe { scaled.GetDesc(&mut output) };
        Ok(Some(Frame {
            width: output.Width,
            height: output.Height,
            texture: scaled,
            captured,
            is_new,
            hdr_metadata: None,
            _storage: None,
        }))
    }
}

/// The error of a failed Media Foundation call on a camera.
pub(super) fn failure(error: windows::core::Error) -> anyhow::Error {
    anyhow::Error::msg(explain(&error))
}


/// Open `camera` in its default mode on the default adapter.
pub(crate) fn default_spec(camera: &Camera) -> Result<Spec> {
    if let Some(reason) = &camera.unavailable {
        anyhow::bail!("{reason}");
    }
    let mode = crate::media::camera::default_mode(&camera.modes).context(t().camera.no_mode)?;
    Ok(Spec::new(camera, mode, Color::default(), default_adapter()?))
}


/// A copy of a BGRA texture of `device` as an image (`dev camera probe`).
pub(crate) fn read_back(device: &ID3D11Device, texture: &ID3D11Texture2D) -> Result<image::RgbaImage> {
    let mut desc = D3D11_TEXTURE2D_DESC::default();
    unsafe { texture.GetDesc(&mut desc) };
    let mut staging = None;
    unsafe {
        device.CreateTexture2D(
            &D3D11_TEXTURE2D_DESC {
                Usage: D3D11_USAGE_STAGING,
                BindFlags: 0,
                CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
                MiscFlags: 0,
                ..desc
            },
            None,
            Some(&mut staging),
        )?;
    }
    let staging = staging.context("staging texture")?;
    let context = unsafe { device.GetImmediateContext()? };
    unsafe { context.CopyResource(&staging, texture) };
    let mapped = super::gpu_readback::map(&context, &staging, || true)?;
    let (width, height) = (desc.Width as usize, desc.Height as usize);
    let mut pixels = vec![0u8; width * height * 4];
    for y in 0..height {
        // SAFETY: the mapped texture has `height` rows of `RowPitch` bytes.
        let row = unsafe {
            std::slice::from_raw_parts(
                mapped.pData.cast::<u8>().add(y * mapped.RowPitch as usize),
                width * 4,
            )
        };
        for (out, bgra) in pixels[y * width * 4..(y + 1) * width * 4]
            .chunks_exact_mut(4)
            .zip(row.chunks_exact(4))
        {
            out.copy_from_slice(&[bgra[2], bgra[1], bgra[0], 255]);
        }
    }
    unsafe { context.Unmap(&staging, 0) };
    image::RgbaImage::from_raw(desc.Width, desc.Height, pixels).context("snapshot size")
}
