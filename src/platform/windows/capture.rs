//! Desktop Duplication ownership and top-down BGRA frames.
use anyhow::{Context, Result, ensure};
use windows::{
    Win32::{
        Foundation::HMODULE,
        Graphics::{
            Direct3D::{D3D_DRIVER_TYPE_UNKNOWN, D3D_FEATURE_LEVEL_11_0},
            Direct3D11::*,
            Dxgi::{Common::*, *},
            Gdi::{
                DEVMODEW, DISPLAY_DEVICEW, ENUM_CURRENT_SETTINGS, ENUM_DISPLAY_SETTINGS_FLAGS,
                EnumDisplayDevicesW, EnumDisplaySettingsExW,
            },
        },
    },
    core::{Interface, PCWSTR},
};

pub(crate) use crate::media::capture::{Screen, SourceGone};
mod inventory;
use super::power_request as power;
mod recovery;
mod session;

/// Capture never asks the local user on Windows.
pub(crate) fn needs_consent() -> bool {
    false
}

/// Read the state of this process's interactive session. Unknown/failure is
/// not an unlocked session; callers retain their last confirmed report.
pub(crate) fn session_locked() -> Option<bool> {
    use windows::Win32::System::RemoteDesktop::*;
    let mut buffer = windows::core::PWSTR::null();
    let mut length = 0;
    unsafe {
        WTSQuerySessionInformationW(
            None,
            WTS_CURRENT_SESSION,
            WTSSessionInfoEx,
            &mut buffer,
            &mut length,
        )
        .ok()?;
        let locked = if !buffer.is_null() && length as usize >= std::mem::size_of::<WTSINFOEXW>() {
            let info = &*buffer.0.cast::<WTSINFOEXW>();
            if info.Level == 1 {
                match info.Data.WTSInfoExLevel1.SessionFlags as u32 {
                    WTS_SESSIONSTATE_LOCK => Some(true),
                    WTS_SESSIONSTATE_UNLOCK => Some(false),
                    _ => None,
                }
            } else {
                None
            }
        } else {
            None
        };
        if !buffer.is_null() {
            WTSFreeMemory(buffer.0.cast());
        }
        locked
    }
}

fn active_source_names(targets: Vec<super::display::topology::Target>) -> Option<Vec<String>> {
    if targets
        .iter()
        .any(|t| t.active && t.available && t.source.is_empty())
    {
        return None;
    }
    Some(
        targets
            .into_iter()
            .filter(|t| t.active && t.available)
            .map(|t| t.source)
            .collect(),
    )
}

fn outputs() -> Result<Vec<(IDXGIAdapter1, IDXGIOutput1, Screen)>> {
    let displays = super::display::active_displays().unwrap_or_default();
    // DXGI can retain a desktop output after the last monitor departs. Only
    // trust a complete DisplayConfig inventory; an unavailable query is not
    // evidence that a remote/session display should be removed.
    let active_sources = super::display::topology::Topology::query(true)
        // Liveness needs source identity only. Enumerating every supported mode
        // here stalls the capture thread on its once-per-second refresh.
        .and_then(|topology| topology.metadata())
        .ok()
        .and_then(active_source_names);
    unsafe {
        let factory: IDXGIFactory1 = CreateDXGIFactory1()?;
        let mut result = Vec::new();
        let mut adapter_index = 0;
        loop {
            let adapter = match factory.EnumAdapters1(adapter_index) {
                Ok(adapter) => adapter,
                Err(error) if error.code() == DXGI_ERROR_NOT_FOUND => break,
                Err(error) => return Err(error.into()),
            };
            adapter_index += 1;
            let mut output_index = 0;
            loop {
                let output = match adapter.EnumOutputs(output_index) {
                    Ok(output) => output,
                    Err(error) if error.code() == DXGI_ERROR_NOT_FOUND => break,
                    Err(error) => return Err(error.into()),
                };
                output_index += 1;
                let desc = output.GetDesc()?;
                if !desc.AttachedToDesktop.as_bool() {
                    continue;
                }
                let bounds = desc.DesktopCoordinates;
                let width = u32::try_from(bounds.right - bounds.left)?;
                let height = u32::try_from(bounds.bottom - bounds.top)?;
                ensure!(
                    width > 0 && height > 0 && width <= 16384 && height <= 16384,
                    "无效屏幕尺寸"
                );
                let length = desc
                    .DeviceName
                    .iter()
                    .position(|v| *v == 0)
                    .unwrap_or(desc.DeviceName.len());
                let name = String::from_utf16_lossy(&desc.DeviceName[..length]);
                if active_sources.as_ref().is_some_and(|sources| {
                    !sources
                        .iter()
                        .any(|source| source.eq_ignore_ascii_case(&name))
                }) {
                    tracing::debug!(device=%name,"ignoring DXGI output without an active display target");
                    continue;
                }
                let display = displays.iter().find(|d| d.name == name);
                let display_name = display
                    .map(|d| d.friendly_name.trim().to_owned())
                    .unwrap_or_default();
                let dpi_scale = display
                    .map(|d| d.scale_factor)
                    .filter(|scale| scale.is_finite() && (0.5..=10.0).contains(scale))
                    .map(|scale| (scale * 100.0).round() as u32);
                let mut mode = DEVMODEW {
                    dmSize: std::mem::size_of::<DEVMODEW>() as u16,
                    ..Default::default()
                };
                let fps = if EnumDisplaySettingsExW(
                    PCWSTR(desc.DeviceName.as_ptr()),
                    ENUM_CURRENT_SETTINGS,
                    &mut mode,
                    ENUM_DISPLAY_SETTINGS_FLAGS(0),
                )
                .as_bool()
                    && mode.dmDisplayFrequency > 1
                {
                    mode.dmDisplayFrequency
                } else {
                    60
                };
                let identity = display_identity(&name);
                let adapter_luid = adapter.GetDesc1()?.AdapterLuid;
                let adapter_key = (u64::from(adapter_luid.HighPart as u32) << 32)
                    | u64::from(adapter_luid.LowPart);
                let identity_key = identity
                    .clone()
                    .unwrap_or_else(|| format!("{adapter_key:016X}/{name}"));
                result.push((
                    adapter.clone(),
                    output.cast()?,
                    Screen {
                        id: super::display::source_id(&identity_key)?,
                        device_name: name.clone(),
                        display_name,
                        width,
                        height,
                        left: bounds.left,
                        top: bounds.top,
                        primary: bounds.left == 0 && bounds.top == 0,
                        fps,
                        dpi_scale,
                        hdr: output
                            .cast::<IDXGIOutput6>()
                            .ok()
                            .and_then(|o| o.GetDesc1().ok())
                            .is_some_and(|d| {
                                d.ColorSpace == DXGI_COLOR_SPACE_RGB_FULL_G2084_NONE_P2020
                            }),
                        adapter: adapter_key,
                        render_adapter: None,
                        identity,
                    },
                ));
            }
        }
        Ok(result)
    }
}

/// Windows 这一侧从来不是 Wayland 会话。共享代码用这个问题挑「摆窗口接取拖放」
/// 还是「走 OLE 那条路」，这里永远是后者（见 `features::drag_drop`）。
pub(crate) fn wayland_session() -> bool {
    false
}

pub(crate) fn screens() -> Result<Vec<Screen>> {
    Ok(outputs()?
        .into_iter()
        .map(|(_, _, screen)| screen)
        .collect())
}

pub(super) fn display_identity(name: &str) -> Option<String> {
    const EDD_GET_DEVICE_INTERFACE_NAME: u32 = 1;
    let name: Vec<_> = name.encode_utf16().chain(Some(0)).collect();
    let mut device = DISPLAY_DEVICEW {
        cb: std::mem::size_of::<DISPLAY_DEVICEW>() as u32,
        ..Default::default()
    };
    if !unsafe {
        EnumDisplayDevicesW(
            PCWSTR(name.as_ptr()),
            0,
            &mut device,
            EDD_GET_DEVICE_INTERFACE_NAME,
        )
    }
    .as_bool()
    {
        return None;
    }
    let end = device
        .DeviceID
        .iter()
        .position(|&c| c == 0)
        .unwrap_or(device.DeviceID.len());
    (end > 0).then(|| String::from_utf16_lossy(&device.DeviceID[..end]).to_ascii_lowercase())
}
pub(crate) fn refresh(selected: &Screen) -> Result<Screen> {
    let mut found = screens()?
        .into_iter()
        .find(|s| match &selected.identity {
            Some(identity) => s.identity.as_ref() == Some(identity),
            None => s.adapter == selected.adapter && s.device_name == selected.device_name,
        })
        .ok_or(SourceGone)?;
    found.id = selected.id;
    found.render_adapter = selected.render_adapter;
    Ok(found)
}
pub(crate) fn create_device(luid: u64) -> Result<(ID3D11Device, ID3D11DeviceContext)> {
    unsafe {
        let factory: IDXGIFactory1 = CreateDXGIFactory1()?;
        let mut selected = None;
        for index in 0..64 {
            let adapter = match factory.EnumAdapters1(index) {
                Ok(a) => a,
                Err(error) if error.code() == DXGI_ERROR_NOT_FOUND => break,
                Err(error) => return Err(error.into()),
            };
            let id = adapter.GetDesc1()?.AdapterLuid;
            if ((u64::from(id.HighPart as u32) << 32) | u64::from(id.LowPart)) == luid {
                selected = Some(adapter);
                break;
            }
        }
        let adapter = selected.context("采集适配器已移除")?;
        let (mut device, mut context) = (None, None);
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
        let device = device.context("创建图形设备失败")?;
        super::gpu_scheduling::configure_device(&device);
        Ok((device, context.context("创建图形上下文失败")?))
    }
}
use super::adapter_type;

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) struct EncodingAdapter {
    #[serde(default)]
    pub id: Option<crate::media::selection::GpuId>,
    pub luid: u64,
    pub vendor: u32,
    pub name: String,
}

/// Encoding inventory only. Display enumeration and source capture keep IDD outputs.
pub(crate) fn encoding_adapters() -> Result<Vec<EncodingAdapter>> {
    unsafe {
        let factory: IDXGIFactory1 = CreateDXGIFactory1()?;
        let mut result = Vec::new();
        for index in 0..64 {
            let adapter = match factory.EnumAdapters1(index) {
                Ok(a) => a,
                Err(error) if error.code() == DXGI_ERROR_NOT_FOUND => break,
                Err(error) => return Err(error.into()),
            };
            let desc = adapter.GetDesc1()?;
            if matches!(desc.VendorId, 0x10de | 0x1002 | 0x8086) {
                if adapter_type::indirect(desc.AdapterLuid) == Some(true) {
                    continue;
                }
                result.push(EncodingAdapter {
                    id: adapter_type::address(desc.AdapterLuid).map(|location| {
                        crate::media::selection::GpuId {
                            vendor: desc.VendorId,
                            device: desc.DeviceId,
                            subsystem: desc.SubSysId,
                            revision: desc.Revision,
                            location,
                        }
                    }),
                    luid: (u64::from(desc.AdapterLuid.HighPart as u32) << 32)
                        | u64::from(desc.AdapterLuid.LowPart),
                    vendor: desc.VendorId,
                    name: String::from_utf16_lossy(
                        &desc.Description[..desc
                            .Description
                            .iter()
                            .position(|c| *c == 0)
                            .unwrap_or(desc.Description.len())],
                    ),
                });
            }
        }
        Ok(result)
    }
}

/// The graphics device that captured frames live on.
pub(crate) type Device = ID3D11Device;

/// Whether `device` was removed (driver reset, adapter gone) and every
/// resource on it has to be recreated.
pub(crate) fn device_lost(device: &Device) -> bool {
    unsafe { device.GetDeviceRemovedReason() }.is_err()
}

#[derive(Clone)]
pub(crate) struct Frame {
    pub width: u32,
    pub height: u32,
    pub image: ID3D11Texture2D,
    pub captured: std::time::Instant,
    pub is_new: bool,
    pub hdr_metadata: Option<crate::media::video_color::HdrMetadata>,
    pub(crate) _storage: Option<std::sync::Arc<()>>,
}

enum Backend {
    Session(Box<session::Session>),
    Dxgi(Duplication),
    Gdi(super::gdi::Capture),
    Remote(Box<super::capture_service::Client>),
}
pub(crate) struct Desktop {
    pub device: ID3D11Device,
    pub screen: Screen,
    pub generation: u64,
    pub available: bool,
    backend: Backend,
    recovery: recovery::Recovery,
    recovery_probe: Option<Box<RecoveryProbe>>,
    refreshed: std::time::Instant,
    inventory: Option<inventory::Refresh>,
    sampler: super::cursor_shape::Sampler,
    pub cursor: Option<super::cursor_shape::Snapshot>,
    // The privileged capture agent owns this when using the remote backend.
    // Keep it until after the native capture resources have been destroyed.
    _power: Option<power::Request>,
    _gpu_scheduling: Option<super::gpu_scheduling::Lease>,
}
impl Desktop {
    pub fn open_selected(selected: &Screen) -> Result<Self> {
        if super::host_service::resident::is_owner() {
            let local = session::Session::new(selected)?;
            return Ok(Self {
                device: local.current().device.clone(),
                screen: local.current().screen.clone(),
                generation: 0,
                available: true,
                backend: Backend::Session(Box::new(local)),
                recovery: recovery::Recovery::new(true, std::time::Instant::now()),
                recovery_probe: None,
                refreshed: std::time::Instant::now(),
                inventory: None,
                sampler: Default::default(),
                cursor: None,
                _power: None,
                _gpu_scheduling: None,
            });
        }
        if let Some(remote) = super::capture_service::Client::connect(selected)? {
            return Ok(Self {
                device: remote.device.clone(),
                screen: remote.screen.clone(),
                generation: 0,
                available: true,
                backend: Backend::Remote(Box::new(remote)),
                recovery: recovery::Recovery::new(false, std::time::Instant::now()),
                recovery_probe: None,
                refreshed: std::time::Instant::now(),
                inventory: None,
                sampler: Default::default(),
                cursor: None,
                _power: None,
                _gpu_scheduling: Some(super::gpu_scheduling::Lease::new()),
            });
        }
        Self::open_local(selected)
    }
    pub(super) fn open_local(selected: &Screen) -> Result<Self> {
        let power = power::Request::capture()?;
        let screen = refresh(selected)?;
        let backend = match Duplication::open(&screen.device_name) {
            Ok(capture) => {
                ensure!(
                    capture.screen.identity == screen.identity
                        && capture.screen.adapter == screen.adapter,
                    "采集准备期间所选屏幕变化"
                );
                Backend::Dxgi(capture)
            }
            Err(error) => {
                tracing::info!(device=%screen.device_name, error=%format!("{error:#}"),
                    "desktop duplication unavailable; selecting GDI");
                Backend::Gdi(super::gdi::Capture::new(&screen)?)
            }
        };
        let device = match &backend {
            Backend::Session(s) => s.current().device.clone(),
            Backend::Dxgi(d) => d.device.clone(),
            Backend::Gdi(g) => g.device.clone(),
            Backend::Remote(_) => unreachable!(),
        };
        let recovery = recovery::Recovery::new(
            matches!(backend, Backend::Dxgi(_)),
            std::time::Instant::now(),
        );
        Ok(Self {
            device,
            screen,
            generation: 0,
            available: true,
            backend,
            recovery,
            recovery_probe: None,
            refreshed: std::time::Instant::now(),
            inventory: None,
            sampler: Default::default(),
            cursor: None,
            _power: Some(power),
            _gpu_scheduling: Some(super::gpu_scheduling::Lease::new()),
        })
    }
    pub fn backend_name(&self) -> &'static str {
        match &self.backend {
            Backend::Session(s) => s.backend,
            Backend::Dxgi(_) => "DXGI",
            Backend::Gdi(_) => "GDI",
            Backend::Remote(r) => r.backend_name(),
        }
    }
    pub fn hdr_available(&self) -> bool {
        match &self.backend {
            Backend::Session(s) => s.hdr,
            Backend::Dxgi(d) => d.source_hdr.is_some(),
            Backend::Gdi(_) => false,
            Backend::Remote(r) => r.hdr,
        }
    }
    fn replace(&mut self, backend: Backend) {
        self.recovery_probe = None;
        self.device = match &backend {
            Backend::Session(s) => s.current().device.clone(),
            Backend::Dxgi(d) => d.device.clone(),
            Backend::Gdi(g) => g.device.clone(),
            Backend::Remote(r) => r.device.clone(),
        };
        self.backend = backend;
        self.generation = self.generation.wrapping_add(1);
    }
    pub fn next(
        &mut self,
        timeout: u32,
        quality: i32,
        cursor: bool,
        hdr: bool,
        maximum: (u32, u32),
    ) -> Result<Option<Frame>> {
        if let Backend::Session(local) = &mut self.backend {
            let frame = local.next(timeout, quality, cursor, hdr, maximum)?;
            self.device = local.device.clone();
            self.screen = local.screen.clone();
            self.available = local.available;
            self.cursor = local.cursor.clone();
            self.generation = local.generation();
            return Ok(frame);
        }
        if let Backend::Remote(remote) = &mut self.backend {
            let result = remote.next(timeout, quality, cursor, hdr, maximum);
            self.device = remote.device.clone();
            self.screen = remote.screen.clone();
            self.available = remote.available;
            self.generation = remote.generation;
            self.cursor = remote.cursor.clone();
            return result;
        }
        self.cursor = match self.sampler.sample() {
            Ok(pointer) => Some(pointer),
            Err(error) => {
                tracing::debug!(%error,"host cursor sampling failed");
                None
            }
        };
        let request = super::preprocess::Request {
            quality,
            maximum,
            hdr,
        };
        if let Some(current) = self
            .inventory
            .as_mut()
            .map(inventory::Refresh::poll)
            .transpose()?
            .flatten()
        {
            let current = current?;
            // A presentation-only name change does not invalidate capture resources.
            self.screen.display_name.clone_from(&current.display_name);
            if current != self.screen {
                let mut replacement = Self::open_local(&current)?;
                replacement.generation = self.generation.wrapping_add(1);
                *self = replacement;
            }
        }
        if self.refreshed.elapsed() >= std::time::Duration::from_secs(1) {
            if self.inventory.is_none() {
                self.inventory = Some(inventory::Refresh::new()?);
            }
            self.inventory.as_mut().unwrap().request(&self.screen)?;
            self.refreshed = std::time::Instant::now();
        }
        if matches!(self.backend, Backend::Gdi(_))
            && self.recovery_probe.is_none()
            && self.recovery.due(std::time::Instant::now())
        {
            self.recovery.attempt(std::time::Instant::now());
            if let Ok(preferred) = Duplication::open(&self.screen.device_name) {
                ensure!(
                    preferred.screen.identity == self.screen.identity
                        && preferred.screen.adapter == self.screen.adapter,
                    "恢复期间所选屏幕变化"
                );
                self.recovery_probe = Some(Box::new(RecoveryProbe {
                    capture: preferred,
                    expires: std::time::Instant::now() + std::time::Duration::from_secs(1),
                }));
            }
        }
        if let Some(mut probe) = self.recovery_probe.take() {
            // Duplication's first successful acquisition can be pointer-only.
            // Keep the same candidate until a real image arrives; recreating it
            // on every probe can repeatedly discard that initial pointer frame.
            // Zero timeout keeps the GDI stream moving while the candidate waits.
            match probe.capture.next(0, quality, cursor, hdr, maximum) {
                Ok(Some(frame)) => {
                    self.replace(Backend::Dxgi(probe.capture));
                    self.recovery.promoted(std::time::Instant::now());
                    self.available = true;
                    tracing::info!(device=%self.screen.device_name, "DXGI capture resumed after fallback");
                    return Ok(Some(frame));
                }
                Ok(None) if std::time::Instant::now() < probe.expires => {
                    self.recovery_probe = Some(probe);
                }
                Ok(None) => {
                    tracing::debug!(device=%self.screen.device_name, "DXGI recovery candidate did not produce an image before its deadline")
                }
                Err(error) => {
                    tracing::debug!(device=%self.screen.device_name, error=%format!("{error:#}"), "DXGI recovery candidate failed")
                }
            }
        }
        let frame = match &mut self.backend {
            Backend::Dxgi(d) => d.next(timeout, quality, cursor, hdr, maximum),
            Backend::Gdi(g) => g.next(request, cursor).map(Some),
            Backend::Remote(_) | Backend::Session(_) => unreachable!(),
        };
        match frame {
            Ok(frame) => {
                if matches!(self.backend, Backend::Dxgi(_)) {
                    self.recovery.healthy(std::time::Instant::now());
                }
                self.available = true;
                Ok(frame)
            }
            Err(error) => {
                self.available = false;
                if error
                    .downcast_ref::<windows::core::Error>()
                    .is_some_and(|e| {
                        e.code() == windows::Win32::Foundation::E_ACCESSDENIED
                            || e.code() == DXGI_ERROR_SESSION_DISCONNECTED
                    })
                {
                    return Ok(None);
                }
                if self.screen.identity.is_none()
                    && error
                        .downcast_ref::<windows::core::Error>()
                        .is_some_and(|e| e.code() == DXGI_ERROR_ACCESS_LOST)
                {
                    return Err(SourceGone.into());
                }
                // Re-establish only the same physical output. Never reselect by
                // a reused enumeration index after unplug/replug or desktop loss.
                let current = refresh(&self.screen)?;
                self.screen.display_name.clone_from(&current.display_name);
                if current != self.screen {
                    // Do not consume a mode change by merely overwriting the
                    // inventory: the backend still owns the previous geometry.
                    let mut replacement = Self::open_local(&current)?;
                    replacement.generation = self.generation.wrapping_add(1);
                    tracing::info!(device=%current.device_name, width=current.width,
                        height=current.height, backend=replacement.backend_name(),
                        "capture source changed during failure; rebuilt selected source");
                    *self = replacement;
                    return Ok(None);
                }
                // T BC3830 maps a non-timeout acquisition failure to 4;
                // BAA520/BC80A0 disable that candidate and select GDI. Creating
                // another duplication successfully does not prove it can acquire
                // frames. The existing delayed probe requires a real frame before
                // switching back, avoiding an endless ACCESS_LOST rebuild loop.
                if self.recovery.failed(std::time::Instant::now()) {
                    tracing::info!("DXGI recovery was short-lived; backing off recovery probes");
                }
                let replacement = Backend::Gdi(super::gdi::Capture::new(&self.screen)?);
                tracing::info!(device=%self.screen.device_name,
                    error = %format!("{error:#}"), "selected capture failed; falling back to GDI");
                self.replace(replacement);
                Ok(None)
            }
        }
    }
}

struct RecoveryProbe {
    capture: Duplication,
    expires: std::time::Instant,
}

struct Duplication {
    pub device: ID3D11Device,
    context: ID3D11DeviceContext,
    duplication: Option<IDXGIOutputDuplication>,
    frame_acquired: bool,
    repaired_at: Option<std::time::Instant>,
    presented_since_repair: bool,
    rotation: DXGI_MODE_ROTATION,
    pub screen: Screen,
    converter: super::preprocess::Converter,
    sdr_white: f32,
    source_hdr: Option<crate::media::video_color::HdrMetadata>,
    color_space: Option<DXGI_COLOR_SPACE_TYPE>,
    quality: Option<super::preprocess::Request>,
    cursor: super::cursor::Cursor,
    cursor_capture: bool,
}

impl Duplication {
    pub(crate) fn open(name: &str) -> Result<Self> {
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
            super::gpu_scheduling::configure_device(&device);
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
            let converter = super::preprocess::Converter::new(&device)?;
            let sdr_white = crate::platform::display_hdr::sdr_white_scale_or(
                &screen.device_name,
                if source_hdr.is_some() { 3.75 } else { 1. },
            );
            Ok(Self {
                device,
                context: context.context("未创建采集上下文")?,
                duplication: Some(duplication),
                frame_acquired: false,
                repaired_at: None,
                presented_since_repair: false,
                rotation,
                screen,
                converter,
                sdr_white,
                source_hdr,
                color_space,
                quality: None,
                cursor: Default::default(),
                cursor_capture: false,
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
                    // works for this output. UU's IDD can require DuplicateOutput;
                    // retain DXGI before considering a different capture backend.
                    output.DuplicateOutput(device).inspect(|_| {
                        tracing::debug!(%modern_error, "using compatible DXGI duplication interface");
                    })
                })
            } else {
                output.DuplicateOutput(device)
            }
            .context("无法采集当前桌面；请检查屏幕、锁屏或其他采集程序")?;
            Ok(duplication)
        }
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
        let result = self.next_frame(timeout_ms, quality, cursor_capture, hdr, maximum);
        if let Err(error) = &result
            && error
                .downcast_ref::<windows::core::Error>()
                .is_some_and(|e| e.code() == DXGI_ERROR_ACCESS_LOST)
            && self.repaired_at.is_none_or(|at| {
                self.presented_since_repair && at.elapsed() >= std::time::Duration::from_secs(1)
            })
        {
            // A display reset can invalidate only duplication. Keep the owned
            // textures/device and encoder when the source contract is unchanged.
            // A repair that never produces a frame, or fails again immediately,
            // must still reach the outer bounded backend fallback.
            self.repaired_at = Some(std::time::Instant::now());
            self.presented_since_repair = false;
            self.repair()?;
            tracing::info!(device=%self.screen.device_name, "DXGI capture object restored on the existing graphics device");
            return self.next_frame(timeout_ms, quality, cursor_capture, hdr, maximum);
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

    fn next_frame(
        &mut self,
        timeout_ms: u32,
        quality: i32,
        cursor_capture: bool,
        hdr: bool,
        maximum: (u32, u32),
    ) -> Result<Option<Frame>> {
        let request = super::preprocess::Request {
            quality,
            maximum,
            hdr,
        };
        unsafe {
            let mut info = DXGI_OUTDUPL_FRAME_INFO::default();
            let mut resource = None;
            // Also covers a frame held by a failed/interrupted prior call.
            self.release_frame()?;
            let duplication = self.duplication.as_ref().context("DXGI采集对象缺失")?;
            match duplication.AcquireNextFrame(timeout_ms.min(50), &mut info, &mut resource) {
                Ok(()) => self.frame_acquired = true,
                Err(error) if error.code() == DXGI_ERROR_WAIT_TIMEOUT => {
                    return self.cached_frame(request, cursor_capture, false);
                }
                Err(error) => return Err(error).context("桌面采集失效，需要重建所选屏幕采集"),
            }
            // Conversion copies into our own texture; no borrowed duplication
            // surface escapes this call.
            let captured = std::time::Instant::now();
            let cursor_update = self.cursor.update(&self.device, duplication, &info);
            let result = (|| -> Result<Option<Frame>> {
                cursor_update?;
                // A successful acquisition can contain only a pointer update.
                // Its desktop surface is not a newly presented image (and can
                // be black immediately after duplication creation/recovery).
                // Keep our last owned desktop; composite the new pointer only
                // when the viewer requested an embedded cursor.
                if info.LastPresentTime == 0 {
                    return self.cached_frame(
                        request,
                        cursor_capture,
                        info.LastMouseUpdateTime != 0,
                    );
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
                let texture = self.converter.convert(
                    &self.device,
                    &self.context,
                    &texture,
                    desc,
                    self.rotation,
                    self.sdr_white,
                    self.source_hdr,
                    request,
                    if cursor_capture {
                        self.cursor.render(self.screen.width, self.screen.height)
                    } else {
                        None
                    },
                )?;
                texture.GetDesc(&mut desc);
                self.quality = Some(request);
                self.cursor_capture = cursor_capture;
                self.presented_since_repair = true;
                Ok(Some(Frame {
                    width: desc.Width,
                    height: desc.Height,
                    image: texture,
                    captured,
                    _storage: None,
                    is_new: true,
                    hdr_metadata: if hdr { self.source_hdr } else { None },
                }))
            })();
            // Submit the copy before returning the duplication lease. DWM can
            // then prepare the next image while our owned texture is encoded,
            // instead of adding that copy/wait after synchronous encoding.
            self.context.Flush();
            let released = self.release_frame();
            if result.is_ok() {
                released?;
            }
            result
        }
    }

    fn cached_frame(
        &mut self,
        request: super::preprocess::Request,
        cursor_capture: bool,
        pointer_changed: bool,
    ) -> Result<Option<Frame>> {
        let pointer_changed = cursor_capture && pointer_changed;
        if self.quality == Some(request)
            && self.cursor_capture == cursor_capture
            && !pointer_changed
        {
            return Ok(None);
        }
        // Quality/cursor changes must also work on an otherwise static desktop.
        // Before the first real image, there is nothing safe to compose.
        let Some((texture, desc)) = self.converter.last_input() else {
            return Ok(None);
        };
        let texture = self.converter.convert(
            &self.device,
            &self.context,
            &texture,
            desc,
            self.rotation,
            self.sdr_white,
            self.source_hdr,
            request,
            if cursor_capture {
                self.cursor.render(self.screen.width, self.screen.height)
            } else {
                None
            },
        )?;
        let mut output = D3D11_TEXTURE2D_DESC::default();
        unsafe {
            texture.GetDesc(&mut output);
        }
        self.quality = Some(request);
        self.cursor_capture = cursor_capture;
        Ok(Some(Frame {
            width: output.Width,
            height: output.Height,
            image: texture,
            captured: std::time::Instant::now(),
            is_new: pointer_changed,
            _storage: None,
            hdr_metadata: if request.hdr { self.source_hdr } else { None },
        }))
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

impl Drop for Duplication {
    fn drop(&mut self) {
        let _ = self.release_frame();
    }
}
