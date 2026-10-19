use crate::media::LocalDisplayInfo;
use anyhow::{Context, Result, bail};
use windows::{
    Win32::{
        Foundation::{LPARAM, RECT},
        Graphics::Gdi::*,
        UI::HiDpi::*,
    },
    core::{BOOL, PCWSTR},
};

#[derive(Debug)]
pub(crate) struct ActiveDisplay {
    pub name: String,
    pub friendly_name: String,
    pub width: u32,
    pub height: u32,
    pub frequency: f64,
    pub scale_factor: f64,
    pub is_primary: bool,
}

pub(crate) fn active_displays() -> Result<Vec<ActiveDisplay>> {
    type Enumeration = Vec<(HMONITOR, MONITORINFOEXW)>;
    unsafe extern "system" fn collect(
        monitor: HMONITOR,
        _: HDC,
        _: *mut RECT,
        data: LPARAM,
    ) -> BOOL {
        let state = unsafe { &mut *(data.0 as *mut Enumeration) };
        let mut info = MONITORINFOEXW::default();
        info.monitorInfo.cbSize = std::mem::size_of::<MONITORINFOEXW>() as u32;
        if unsafe { GetMonitorInfoW(monitor, &mut info as *mut _ as *mut MONITORINFO) }.as_bool() {
            state.push((monitor, info));
        }
        BOOL(1)
    }
    let mut state = Enumeration::new();
    anyhow::ensure!(
        unsafe {
            EnumDisplayMonitors(
                None,
                None,
                Some(collect),
                LPARAM((&mut state as *mut Enumeration) as isize),
            )
        }
        .as_bool(),
        "failed to enumerate active monitors"
    );
    let topology = topology::Topology::query(true).ok();
    let targets = topology
        .as_ref()
        .and_then(|t| t.metadata().ok())
        .unwrap_or_default();
    let mut displays = Vec::with_capacity(state.len());
    for (monitor, info) in state {
        let length = info
            .szDevice
            .iter()
            .position(|&c| c == 0)
            .unwrap_or(info.szDevice.len());
        let name = String::from_utf16_lossy(&info.szDevice[..length]);
        let target = targets.iter().find(|t| t.source == name && t.active);
        let mut mode = DEVMODEW {
            dmSize: std::mem::size_of::<DEVMODEW>() as u16,
            ..Default::default()
        };
        // A monitor can disappear during hotplug; preserve the other monitors.
        if !unsafe {
            EnumDisplaySettingsExW(
                PCWSTR(info.szDevice.as_ptr()),
                ENUM_CURRENT_SETTINGS,
                &mut mode,
                ENUM_DISPLAY_SETTINGS_FLAGS(0),
            )
        }
        .as_bool()
        {
            continue;
        }
        let mut dpi_x = 96;
        let mut dpi_y = 96;
        // Source DPI is independent of the querying process's DPI-awareness mode.
        let scale = if let Some(dpi) = target.and_then(|t| t.dpi.as_ref()) {
            f64::from(dpi.current) / 100.0
        } else if unsafe { GetDpiForMonitor(monitor, MDT_EFFECTIVE_DPI, &mut dpi_x, &mut dpi_y) }
            .is_ok()
        {
            f64::from(dpi_x) / 96.0
        } else {
            1.0
        };
        let frequency = topology
            .as_ref()
            .and_then(|topology| {
                let target = target?;
                topology
                    .paths
                    .iter()
                    .find(|p| {
                        topology::luid(p.sourceInfo.adapterId) == target.source_adapter
                            && p.sourceInfo.id == target.source_id
                    })
                    .and_then(|p| {
                        (p.targetInfo.refreshRate.Denominator != 0).then(|| {
                            f64::from(p.targetInfo.refreshRate.Numerator)
                                / f64::from(p.targetInfo.refreshRate.Denominator)
                        })
                    })
            })
            .unwrap_or(f64::from(mode.dmDisplayFrequency));
        displays.push(ActiveDisplay {
            name,
            friendly_name: target.map(|t| t.name.clone()).unwrap_or_default(),
            width: mode.dmPelsWidth,
            height: mode.dmPelsHeight,
            frequency,
            scale_factor: scale,
            is_primary: info.monitorInfo.dwFlags
                & windows::Win32::UI::WindowsAndMessaging::MONITORINFOF_PRIMARY
                != 0,
        });
    }
    Ok(displays)
}

pub(crate) mod recovery;
pub(crate) mod topology;
pub(crate) mod virtual_driver;

/// A protocol source ID stays tied to the monitor interface for this process.
/// Enumeration order and Windows DISPLAYn names can change after a hotplug.
pub(crate) fn source_id(identity: &str) -> Result<i32> {
    use std::collections::HashMap;
    use std::sync::{Mutex, OnceLock};
    static IDS: OnceLock<Mutex<HashMap<String, i32>>> = OnceLock::new();
    let mut ids = IDS
        .get_or_init(Default::default)
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    if let Some(id) = ids.get(identity) {
        return Ok(*id);
    }
    let id = i32::try_from(ids.len()).context("显示器标识已耗尽")?;
    ids.insert(identity.to_owned(), id);
    Ok(id)
}

pub fn detect_local_display() -> Result<LocalDisplayInfo> {
    let displays = active_displays().context("failed to enumerate local displays")?;
    let display = displays
        .iter()
        .find(|display| display.is_primary)
        .or_else(|| {
            displays
                .iter()
                .max_by_key(|display| u64::from(display.width) * u64::from(display.height))
        })
        .context("no local display was detected")?;
    if display.width == 0 || display.height == 0 {
        bail!("local display reported an invalid resolution");
    }
    // Use the maximum refresh of active displays with a 30 Hz floor.
    // The primary display still supplies geometry; using
    // only its refresh would incorrectly limit viewing on a faster monitor.
    let refresh_hz = displays
        .iter()
        .filter(|display| display.frequency.is_finite() && display.frequency > 0.0)
        .map(|display| display.frequency.round() as u32)
        .max()
        .unwrap_or(30)
        .max(30);
    Ok(LocalDisplayInfo {
        width: display.width,
        height: display.height,
        refresh_hz,
    })
}

/// Current active display dimensions offered by the controller. This is
/// neither a list of supported physical modes nor a request to change one.
pub(crate) fn local_display_dimensions() -> Vec<(u32, u32)> {
    let mut modes = active_displays()
        .unwrap_or_default()
        .into_iter()
        .filter(|d| d.width != 0 && d.height != 0)
        .map(|d| (d.width, d.height))
        .collect::<Vec<_>>();
    modes.sort_unstable();
    modes.dedup();
    if modes.is_empty() {
        modes.push((1920, 1080));
    }
    modes
}

pub(crate) mod install;
