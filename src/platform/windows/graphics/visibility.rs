//! Avoid submitting an ordinary background UI behind a fullscreen foreground.
//! Composition flip chains do not reliably report DXGI_STATUS_OCCLUDED.
use windows::Win32::{
    Foundation::{HWND, POINT, RECT},
    Graphics::Gdi::{
        ClientToScreen, GetMonitorInfoW, MONITOR_DEFAULTTONULL, MONITORINFO, MonitorFromWindow,
    },
    UI::WindowsAndMessaging::{
        GW_HWNDPREV, GWL_EXSTYLE, GetClientRect, GetForegroundWindow, GetWindow, GetWindowLongPtrW,
        GetWindowRect, IsIconic, IsWindowVisible, WS_EX_LAYERED, WS_EX_TOPMOST, WS_EX_TRANSPARENT,
    },
};

fn contains(outer: RECT, inner: RECT) -> bool {
    inner.right > inner.left
        && inner.bottom > inner.top
        && outer.left <= inner.left
        && outer.top <= inner.top
        && outer.right >= inner.right
        && outer.bottom >= inner.bottom
}

fn is_above(cover: HWND, mut window: HWND) -> bool {
    // The shell desktop can be foreground while remaining below visible apps.
    // Foreground alone therefore does not establish occlusion.
    for _ in 0..256 {
        let Ok(previous) = (unsafe { GetWindow(window, GW_HWNDPREV) }) else {
            return false;
        };
        if previous.is_invalid() {
            return false;
        }
        if previous == cover {
            return true;
        }
        window = previous;
    }
    false
}

pub(crate) fn covered_by_fullscreen(window: HWND) -> bool {
    unsafe {
        let foreground = GetForegroundWindow();
        if foreground.is_invalid()
            || foreground == window
            || !IsWindowVisible(foreground).as_bool()
            || IsIconic(foreground).as_bool()
            || GetWindowLongPtrW(window, GWL_EXSTYLE) as u32 & WS_EX_TOPMOST.0 != 0
            || GetWindowLongPtrW(foreground, GWL_EXSTYLE) as u32
                & (WS_EX_LAYERED.0 | WS_EX_TRANSPARENT.0)
                != 0
        {
            return false;
        }
        let monitor = MonitorFromWindow(foreground, MONITOR_DEFAULTTONULL);
        let mut info = MONITORINFO {
            cbSize: size_of::<MONITORINFO>() as u32,
            ..Default::default()
        };
        let mut client = RECT::default();
        let mut target = RECT::default();
        let mut origin = POINT::default();
        if monitor.is_invalid()
            || !GetMonitorInfoW(monitor, &mut info).as_bool()
            || GetClientRect(foreground, &mut client).is_err()
            || !ClientToScreen(foreground, &mut origin).as_bool()
            || GetWindowRect(window, &mut target).is_err()
        {
            return false;
        }
        client.left += origin.x;
        client.right += origin.x;
        client.top += origin.y;
        client.bottom += origin.y;
        // Another monitor or a partially visible window must continue drawing.
        // This is a visibility decision, not a game/process-name heuristic.
        contains(client, info.rcMonitor)
            && contains(client, target)
            && is_above(foreground, window)
            && GetForegroundWindow() == foreground
    }
}

pub(super) fn drawable(window: HWND) -> bool {
    (unsafe { IsWindowVisible(window).as_bool() && !IsIconic(window).as_bool() })
        && !covered_by_fullscreen(window)
}
