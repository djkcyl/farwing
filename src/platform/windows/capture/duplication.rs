//! Desktop Duplication shared by every capture of an output in this process.
//!
//! DXGI refuses a second duplication of an output within one process, so
//! participants viewing the same screen read from one duplication. Each
//! capture keeps its own conversion (size, HDR, embedded cursor); the shared
//! device is multithread protected because each capture encodes on its own
//! thread.
use super::{Frame, Screen, outputs};
use crate::platform::windows::{cursor::Cursor, preprocess};
use anyhow::{Context, Result, ensure};
use std::sync::{Arc, Mutex, MutexGuard, Weak};
use windows::{
    Win32::{
        Foundation::HMODULE,
        Graphics::{
            Direct3D::{D3D_DRIVER_TYPE_UNKNOWN, D3D_FEATURE_LEVEL_11_0},
            Direct3D11::*,
            Dxgi::{Common::*, *},
        },
    },
    core::Interface,
};

struct Source {
    device: ID3D11Device,
    context: ID3D11DeviceContext,
    duplication: Option<IDXGIOutputDuplication>,
    frame_acquired: bool,
    repaired_at: Option<std::time::Instant>,
    presented_since_repair: bool,
    rotation: DXGI_MODE_ROTATION,
    screen: Screen,
    sdr_white: f32,
    source_hdr: Option<crate::media::video_color::HdrMetadata>,
    color_space: Option<DXGI_COLOR_SPACE_TYPE>,
    cursor: Cursor,
    /// Owned copy of the latest desktop image.
    image: Option<(ID3D11Texture2D, D3D11_TEXTURE2D_DESC)>,
    /// Counts desktop images and pointer updates, so each capture can tell
    /// what it has not converted yet.
    presented: u64,
    pointer: u64,
    captured: std::time::Instant,
    /// Set when the duplication cannot be repaired; it is released at once so
    /// a replacement can be created while captures still hold this one.
    failed: bool,
}

// The COM objects are used under the source mutex only, on a multithread
// protected device.
unsafe impl Send for Source {}

type Shared = Arc<Mutex<Source>>;

static SOURCES: std::sync::LazyLock<Mutex<std::collections::HashMap<String, Weak<Mutex<Source>>>>> =
    std::sync::LazyLock::new(Default::default);

fn lock<T>(value: &Mutex<T>) -> MutexGuard<'_, T> {
    value.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

pub(super) struct Duplication {
    pub device: ID3D11Device,
    pub screen: Screen,
    pub source_hdr: Option<crate::media::video_color::HdrMetadata>,
    source: Shared,
    converter: preprocess::Converter,
    quality: Option<preprocess::Request>,
    cursor_capture: bool,
    presented: u64,
    pointer: u64,
}

impl Duplication {
    pub(crate) fn open(name: &str) -> Result<Self> {
        let mut sources = lock(&SOURCES);
        sources.retain(|_, source| source.strong_count() > 0);
        if let Some(source) = sources.get(name).and_then(Weak::upgrade) {
            let current = outputs()?
                .into_iter()
                .find(|(_, _, screen)| screen.device_name == name)
                .map(|(_, _, screen)| screen)
                .context("所选屏幕已断开")?;
            let mut shared = lock(&source);
            if !shared.failed && shared.screen == current {
                drop(shared);
                return Self::attach(source);
            }
            // A changed output needs a new duplication; release the old one
            // so DXGI accepts it.
            shared.fail();
        }
        let source = Arc::new(Mutex::new(Source::open(name)?));
        sources.insert(name.to_owned(), Arc::downgrade(&source));
        Self::attach(source)
    }

    fn attach(source: Shared) -> Result<Self> {
        let (device, screen, source_hdr) = {
            let shared = lock(&source);
            (
                shared.device.clone(),
                shared.screen.clone(),
                shared.source_hdr,
            )
        };
        Ok(Self {
            converter: preprocess::Converter::new(&device)?,
            device,
            screen,
            source_hdr,
            source,
            quality: None,
            cursor_capture: false,
            presented: 0,
            pointer: 0,
        })
    }

    /// A timeout means there is no new desktop image, not a lost capture source.
    pub(crate) fn next(
        &mut self,
        timeout_ms: u32,
        quality: i32,
        cursor_capture: bool,
        hdr: bool,
        maximum: (u32, u32),
    ) -> Result<Option<Frame>> {
        let request = preprocess::Request {
            quality,
            maximum,
            hdr,
        };
        let source = self.source.clone();
        let mut source = lock(&source);
        // Another capture may already have acquired an image this one has
        // not converted yet.
        if source.presented == self.presented && source.pointer == self.pointer {
            source.acquire(timeout_ms)?;
        }
        let presented = source.presented != self.presented;
        let pointer_changed = cursor_capture && source.pointer != self.pointer;
        self.presented = source.presented;
        self.pointer = source.pointer;
        // Quality/cursor changes must also work on an otherwise static desktop.
        // Before the first real image, there is nothing safe to compose.
        if !presented
            && self.quality == Some(request)
            && self.cursor_capture == cursor_capture
            && !pointer_changed
        {
            return Ok(None);
        }
        let Some((image, desc)) = source.image.clone() else {
            return Ok(None);
        };
        let texture = self.converter.convert(
            &source.device,
            &source.context,
            &image,
            desc,
            source.rotation,
            source.sdr_white,
            source.source_hdr,
            request,
            if cursor_capture {
                source.cursor.render(source.screen.width, source.screen.height)
            } else {
                None
            },
        )?;
        unsafe {
            source.context.Flush();
        }
        let mut output = D3D11_TEXTURE2D_DESC::default();
        unsafe {
            texture.GetDesc(&mut output);
        }
        self.quality = Some(request);
        self.cursor_capture = cursor_capture;
        Ok(Some(Frame {
            width: output.Width,
            height: output.Height,
            texture,
            captured: if presented {
                source.captured
            } else {
                std::time::Instant::now()
            },
            is_new: presented || pointer_changed,
            _storage: None,
            hdr_metadata: if hdr { source.source_hdr } else { None },
        }))
    }
}

impl Source {
    fn open(name: &str) -> Result<Self> {
        let (adapter, output, screen) = outputs()?
            .into_iter()
            .find(|(_, _, screen)| screen.device_name == name)
            .context("所选屏幕已断开")?;
        unsafe {
            let mut device = None;
            let mut context = None;
            D3D11CreateDevice(
                &adapter,
                D3D_DRIVER_TYPE_UNKNOWN,
                HMODULE::default(),
                D3D11_CREATE_DEVICE_BGRA_SUPPORT | D3D11_CREATE_DEVICE_VIDEO_SUPPORT,
                Some(&[D3D_FEATURE_LEVEL_11_0]),
                D3D11_SDK_VERSION,
                Some(&mut device),
                None,
                Some(&mut context),
            )?;
            let device = device.context("未创建采集设备")?;
            let context = context.context("未创建采集上下文")?;
            // Captures sharing this source convert and encode on their own
            // threads.
            if let Ok(multithread) = context.cast::<ID3D11Multithread>() {
                let _ = multithread.SetMultithreadProtected(true);
            }
            crate::platform::windows::gpu_scheduling::configure_device(&device);
            let advanced = output
                .cast::<IDXGIOutput6>()
                .ok()
                .and_then(|o| o.GetDesc1().ok());
            let use_float = advanced.as_ref().is_some_and(|d| {
                matches!(
                    d.ColorSpace,
                    DXGI_COLOR_SPACE_RGB_FULL_G2084_NONE_P2020
                        | DXGI_COLOR_SPACE_RGB_FULL_G10_NONE_P709
                )
            });
            let color_space = advanced.as_ref().map(|d| d.ColorSpace);
            let duplication = Self::duplicate(&output, &device, use_float)?;
            let source_hdr = advanced
                .filter(|d| {
                    d.ColorSpace == DXGI_COLOR_SPACE_RGB_FULL_G2084_NONE_P2020
                        && duplication.GetDesc().ModeDesc.Format == DXGI_FORMAT_R16G16B16A16_FLOAT
                })
                .map(|d| crate::media::video_color::HdrMetadata {
                    max_luminance: d.MaxLuminance.clamp(0., 20000.) as u16,
                    min_luminance: (d.MinLuminance * 10000.).clamp(0., 50000.) as u16,
                    chromaticity: [35400, 14600, 8500, 39850, 6550, 2300, 15635, 16450],
                    max_content_light_level: d.MaxLuminance.clamp(0., 20000.) as u16,
                    max_frame_average_light_level: d.MaxFullFrameLuminance.clamp(0., 20000.) as u16,
                });
            let rotation = duplication.GetDesc().Rotation;
            let sdr_white = crate::platform::display_hdr::sdr_white_scale_or(
                &screen.device_name,
                if source_hdr.is_some() { 3.75 } else { 1. },
            );
            Ok(Self {
                device,
                context,
                duplication: Some(duplication),
                frame_acquired: false,
                repaired_at: None,
                presented_since_repair: false,
                rotation,
                screen,
                sdr_white,
                source_hdr,
                color_space,
                cursor: Default::default(),
                image: None,
                presented: 0,
                pointer: 0,
                captured: std::time::Instant::now(),
                failed: false,
            })
        }
    }

    fn duplicate(
        output: &IDXGIOutput1,
        device: &ID3D11Device,
        use_float: bool,
    ) -> Result<IDXGIOutputDuplication> {
        unsafe {
            let duplication = if let Ok(output5) = output.cast::<IDXGIOutput5>() {
                let formats = if use_float {
                    vec![DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_FORMAT_R16G16B16A16_FLOAT]
                } else {
                    vec![DXGI_FORMAT_B8G8R8A8_UNORM]
                };
                let modern = output5.DuplicateOutput1(device, 0, &formats);
                let modern = if use_float {
                    modern.or_else(|_| {
                        output5.DuplicateOutput1(device, 0, &[DXGI_FORMAT_B8G8R8A8_UNORM])
                    })
                } else {
                    modern
                };
                modern.or_else(|modern_error| {
                    // Exposing IDXGIOutput5 does not guarantee DuplicateOutput1
                    // works for this output; some indirect display drivers
                    // only accept DuplicateOutput. Retain DXGI before
                    // considering a different capture backend.
                    output.DuplicateOutput(device).inspect(|_| {
                        tracing::debug!(%modern_error, "using compatible DXGI duplication interface");
                    })
                })
            } else {
                output.DuplicateOutput(device)
            }
            .context("无法采集当前桌面")?;
            Ok(duplication)
        }
    }

    /// Release the duplication of a source that cannot recover.
    fn fail(&mut self) {
        let _ = self.release_frame();
        self.duplication = None;
        self.failed = true;
    }

    fn acquire(&mut self, timeout_ms: u32) -> Result<()> {
        ensure!(!self.failed, "共享桌面采集已失效");
        let result = self.acquire_once(timeout_ms);
        let Err(error) = &result else { return result };
        let code = error
            .downcast_ref::<windows::core::Error>()
            .map(windows::core::Error::code);
        if code == Some(DXGI_ERROR_ACCESS_LOST)
            && self.repaired_at.is_none_or(|at| {
                self.presented_since_repair && at.elapsed() >= std::time::Duration::from_secs(1)
            })
        {
            // A display reset can invalidate only duplication. Keep the owned
            // textures/device and encoders when the source contract is
            // unchanged. A repair that never produces a frame, or fails again
            // immediately, must still reach the outer bounded backend fallback.
            self.repaired_at = Some(std::time::Instant::now());
            self.presented_since_repair = false;
            if let Err(error) = self.repair() {
                self.fail();
                return Err(error);
            }
            tracing::info!(device=%self.screen.device_name, "DXGI capture object restored on the existing graphics device");
            let result = self.acquire_once(timeout_ms);
            if result.is_err() {
                self.fail();
            }
            return result;
        }
        // Access denial and session disconnects are transient (secure desktop,
        // console switch); the duplication stays.
        if !matches!(code, Some(code) if code == windows::Win32::Foundation::E_ACCESSDENIED
            || code == DXGI_ERROR_SESSION_DISCONNECTED)
        {
            self.fail();
        }
        result
    }

    fn repair(&mut self) -> Result<()> {
        unsafe {
            self.device.GetDeviceRemovedReason()?;
        }
        let before = unsafe {
            self.duplication
                .as_ref()
                .context("DXGI采集对象缺失")?
                .GetDesc()
        };
        self.frame_acquired = false;
        self.duplication = None;
        let (_, output, screen) = outputs()?
            .into_iter()
            .find(|(_, _, s)| s.device_name == self.screen.device_name)
            .context("恢复期间所选屏幕已断开")?;
        ensure!(
            screen.identity == self.screen.identity
                && screen.adapter == self.screen.adapter
                && screen.width == self.screen.width
                && screen.height == self.screen.height
                && screen.hdr == self.screen.hdr,
            "恢复期间采集源发生变化"
        );
        let color_space = output
            .cast::<IDXGIOutput6>()
            .ok()
            .and_then(|output| unsafe { output.GetDesc1() }.ok())
            .map(|desc| desc.ColorSpace);
        ensure!(
            color_space == self.color_space,
            "恢复期间采集色彩空间发生变化"
        );
        let duplication = Self::duplicate(
            &output,
            &self.device,
            before.ModeDesc.Format == DXGI_FORMAT_R16G16B16A16_FLOAT,
        )?;
        let after = unsafe { duplication.GetDesc() };
        ensure!(
            after.ModeDesc.Width == before.ModeDesc.Width
                && after.ModeDesc.Height == before.ModeDesc.Height
                && after.ModeDesc.Format == before.ModeDesc.Format
                && after.Rotation == before.Rotation,
            "恢复期间采集格式发生变化"
        );
        self.duplication = Some(duplication);
        Ok(())
    }

    /// Wait for the next desktop image or pointer update and keep an owned
    /// copy, so the duplication frame is returned before anyone encodes.
    fn acquire_once(&mut self, timeout_ms: u32) -> Result<()> {
        unsafe {
            let mut info = DXGI_OUTDUPL_FRAME_INFO::default();
            let mut resource = None;
            // Also covers a frame held by a failed/interrupted prior call.
            self.release_frame()?;
            let duplication = self.duplication.as_ref().context("DXGI采集对象缺失")?;
            match duplication.AcquireNextFrame(timeout_ms.min(50), &mut info, &mut resource) {
                Ok(()) => self.frame_acquired = true,
                Err(error) if error.code() == DXGI_ERROR_WAIT_TIMEOUT => return Ok(()),
                Err(error) => return Err(error).context("桌面采集失效"),
            }
            let captured = std::time::Instant::now();
            let cursor_update = self.cursor.update(&self.device, duplication, &info);
            let result = (|| -> Result<()> {
                cursor_update?;
                if info.LastMouseUpdateTime != 0 {
                    self.pointer = self.pointer.wrapping_add(1);
                }
                // A successful acquisition can contain only a pointer update.
                // Its desktop surface is not a newly presented image (and can
                // be black immediately after duplication creation/recovery).
                if info.LastPresentTime == 0 {
                    return Ok(());
                }
                let texture: ID3D11Texture2D = resource.context("采集未返回纹理")?.cast()?;
                let mut desc = D3D11_TEXTURE2D_DESC::default();
                texture.GetDesc(&mut desc);
                ensure!(
                    matches!(
                        desc.Format,
                        DXGI_FORMAT_B8G8R8A8_UNORM
                            | DXGI_FORMAT_R8G8B8A8_UNORM
                            | DXGI_FORMAT_R16G16B16A16_FLOAT
                    ),
                    "不支持的采集像素格式：{}",
                    desc.Format.0
                );
                ensure!(
                    desc.Width > 0
                        && desc.Height > 0
                        && desc.Width <= 16384
                        && desc.Height <= 16384,
                    "采集纹理尺寸无效"
                );
                let reuse = self.image.as_ref().is_some_and(|(_, current)| {
                    (current.Width, current.Height, current.Format)
                        == (desc.Width, desc.Height, desc.Format)
                });
                if !reuse {
                    let owned = D3D11_TEXTURE2D_DESC {
                        MipLevels: 1,
                        ArraySize: 1,
                        SampleDesc: DXGI_SAMPLE_DESC {
                            Count: 1,
                            Quality: 0,
                        },
                        Usage: D3D11_USAGE_DEFAULT,
                        BindFlags: 0,
                        CPUAccessFlags: 0,
                        MiscFlags: 0,
                        ..desc
                    };
                    let mut image = None;
                    self.device.CreateTexture2D(&owned, None, Some(&mut image))?;
                    self.image = Some((image.context("采集副本纹理")?, owned));
                }
                let (image, _) = self.image.as_ref().context("采集副本缺失")?;
                self.context.CopyResource(image, &texture);
                self.captured = captured;
                self.presented = self.presented.wrapping_add(1);
                self.presented_since_repair = true;
                Ok(())
            })();
            // Submit the copy before returning the duplication lease. DWM can
            // then prepare the next image while captures convert and encode.
            self.context.Flush();
            let released = self.release_frame();
            if result.is_ok() {
                released?;
            }
            result
        }
    }

    fn release_frame(&mut self) -> Result<()> {
        if std::mem::take(&mut self.frame_acquired) {
            unsafe {
                self.duplication
                    .as_ref()
                    .context("DXGI采集对象缺失")?
                    .ReleaseFrame()
            }
            .context("释放采集帧失败")?;
        }
        Ok(())
    }
}

impl Drop for Source {
    fn drop(&mut self) {
        let _ = self.release_frame();
    }
}
