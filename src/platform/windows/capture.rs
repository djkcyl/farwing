//! Desktop Duplication ownership and top-down BGRA frames.
use crate::i18n::t;
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
use duplication::Duplication;
mod duplication;
mod inventory;
mod power;
mod recovery;
mod session;
mod warmup;
pub(crate) use warmup::{keep_for_reconnect, warm_up};

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
                    "{}",
                    t().system.capture_invalid_screen_size
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
    if let Some(device) = warmup::take(luid) {
        let context = unsafe { device.GetImmediateContext() }?;
        return Ok((device, context));
    }
    new_device(luid)
}

fn new_device(luid: u64) -> Result<(ID3D11Device, ID3D11DeviceContext)> {
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
        let adapter = selected.context(t().system.capture_adapter_removed)?;
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
        let device = device.context(t().system.capture_create_device_failed)?;
        super::gpu_scheduling::configure_device(&device);
        Ok((
            device,
            context.context(t().system.capture_create_context_failed)?,
        ))
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
    /// User-mode driver version; 0 when unknown.
    #[serde(default)]
    pub driver: u64,
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
                    driver: adapter
                        .CheckInterfaceSupport(&IDXGIDevice::IID)
                        .map_or(0, |version| version as u64),
                });
            }
        }
        Ok(result)
    }
}

#[derive(Clone)]
pub(crate) struct Frame {
    pub width: u32,
    pub height: u32,
    pub texture: ID3D11Texture2D,
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
        let power = power::Request::new()?;
        let screen = refresh(selected)?;
        let backend = match Duplication::open(&screen.device_name) {
            Ok(capture) => {
                ensure!(
                    capture.screen.identity == screen.identity
                        && capture.screen.adapter == screen.adapter,
                    "{}",
                    t().system.capture_screen_changed_preparing
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
                    "{}",
                    t().system.capture_screen_changed_recovering
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
                // A non-timeout acquisition failure disables that candidate and
                // selects GDI. Creating another duplication successfully does
                // not prove it can acquire frames. The existing delayed probe
                // requires a real frame before switching back, avoiding an
                // endless ACCESS_LOST rebuild loop.
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

