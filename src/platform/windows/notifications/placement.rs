//! Keep an existing notification anchored as its display's work area changes.
use anyhow::{Result, anyhow};
use windows::Win32::{
    Foundation::{LPARAM, RECT},
    Graphics::Gdi::*,
    UI::WindowsAndMessaging::GetForegroundWindow,
};
use windows::core::BOOL;
use winit::{
    dpi::{PhysicalPosition, PhysicalSize},
    platform::windows::WindowExtWindows,
    window::{Window, WindowLevel},
};

pub(crate) struct Placement {
    monitor: HMONITOR,
    device: [u16; 32],
    failed: bool,
}

impl Placement {
    pub(crate) fn new(window: &Window) -> Result<Self> {
        let monitor = unsafe { MonitorFromWindow(GetForegroundWindow(), MONITOR_DEFAULTTOPRIMARY) };
        let info = monitor_info(monitor).ok_or_else(|| anyhow!("读取通知显示区域失败"))?;
        window.set_skip_taskbar(true);
        window.set_window_level(WindowLevel::AlwaysOnTop);
        let mut placement = Self {
            monitor,
            device: info.szDevice,
            failed: false,
        };
        placement.refresh(window);
        Ok(placement)
    }

    pub(crate) fn refresh(&mut self, window: &Window) {
        // This runs on the notification's existing UI tick. A resolution or
        // taskbar change need not produce a Resized event for this HWND.
        match self.position(window) {
            Ok(position) => {
                self.failed = false;
                if window.outer_position().ok() != Some(position) {
                    window.set_outer_position(position);
                }
            }
            Err(error) => {
                // Topology can be temporarily unavailable during a mode switch.
                // Keep the card alive and retry on its next normal UI refresh.
                if !self.failed {
                    tracing::warn!(%error, "notification placement unavailable");
                }
                self.failed = true;
            }
        }
    }

    fn position(&mut self, window: &Window) -> Result<PhysicalPosition<i32>> {
        let info = match monitor_info(self.monitor).filter(|info| info.szDevice == self.device) {
            Some(info) => info,
            None => {
                // HMONITORs may be replaced by a display mode change. Recover
                // the original device before using the window's old coordinates.
                let (monitor, info) = find_device(self.device)
                    .or_else(|| {
                        let hwnd = crate::platform::graphics::window_hwnd(window).ok()?;
                        let monitor = unsafe { MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST) };
                        Some((monitor, monitor_info(monitor)?))
                    })
                    .ok_or_else(|| anyhow!("当前没有可用的通知显示区域"))?;
                self.monitor = monitor;
                self.device = info.szDevice;
                info
            }
        };
        Ok(corner(
            info.monitorInfo.rcWork,
            window.outer_size(),
            window.scale_factor(),
        ))
    }
}

fn monitor_info(monitor: HMONITOR) -> Option<MONITORINFOEXW> {
    let mut info = MONITORINFOEXW::default();
    info.monitorInfo.cbSize = std::mem::size_of::<MONITORINFOEXW>() as u32;
    unsafe { GetMonitorInfoW(monitor, (&raw mut info).cast()) }
        .as_bool()
        .then_some(info)
}

fn find_device(device: [u16; 32]) -> Option<(HMONITOR, MONITORINFOEXW)> {
    struct Search {
        device: [u16; 32],
        found: Option<(HMONITOR, MONITORINFOEXW)>,
    }
    unsafe extern "system" fn visit(monitor: HMONITOR, _: HDC, _: *mut RECT, data: LPARAM) -> BOOL {
        let search = unsafe { &mut *(data.0 as *mut Search) };
        if let Some(info) = monitor_info(monitor).filter(|info| info.szDevice == search.device) {
            search.found = Some((monitor, info));
            return BOOL(0);
        }
        BOOL(1)
    }
    let mut search = Search {
        device,
        found: None,
    };
    // Returning FALSE from the callback means the requested monitor was found.
    let _ =
        unsafe { EnumDisplayMonitors(None, None, Some(visit), LPARAM((&raw mut search) as isize)) };
    search.found
}

fn corner(work: RECT, size: PhysicalSize<u32>, scale: f64) -> PhysicalPosition<i32> {
    let margin = (f64::from(crate::ui::theme::NOTIFICATION_MARGIN) * scale).round() as i32;
    PhysicalPosition::new(
        (work.right - size.width as i32 - margin).max(work.left),
        (work.bottom - size.height as i32 - margin).max(work.top),
    )
}
