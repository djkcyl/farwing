//! Notification area integration on the existing Windows event-loop thread.
use crate::i18n::t;
use anyhow::{Result, ensure};
use std::sync::atomic::{AtomicU8, Ordering};
use windows::{
    Win32::{
        Foundation::*,
        System::{LibraryLoader::GetModuleHandleW, Registry::*},
        UI::{HiDpi::*, Shell::*, WindowsAndMessaging::*},
    },
    core::{HSTRING, PCWSTR, w},
};
const CALLBACK: u32 = WM_APP + 43;
pub(super) struct Tray {
    window: HWND,
}

/// What the notification icon shows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum TrayState {
    Idle = 0,
    /// Someone is connected to this device (a red dot).
    Controlled = 1,
    /// Connections to this device are switched off (grey).
    Disabled = 2,
}
static STATE: AtomicU8 = AtomicU8::new(TrayState::Idle as u8);

/// The taskbar is light (Windows' "default app mode" applies to apps only).
fn light_taskbar() -> bool {
    let mut value = 0u32;
    let mut size = size_of::<u32>() as u32;
    unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            w!(r"Software\Microsoft\Windows\CurrentVersion\Themes\Personalize"),
            w!("SystemUsesLightTheme"),
            RRF_RT_REG_DWORD,
            None,
            Some((&raw mut value).cast()),
            Some(&mut size),
        )
        .is_ok()
            && value != 0
    }
}

/// Resources 2–7 of `assets/windows.rc`: dark artwork for a light taskbar
/// (idle, controlled, disabled), then light artwork for a dark one.
fn icon(state: u8) -> HICON {
    let resource = 2 + usize::from(state) + if light_taskbar() { 0 } else { 3 };
    unsafe {
        let size = GetSystemMetricsForDpi(SM_CXSMICON, GetDpiForSystem());
        LoadImageW(
            GetModuleHandleW(None).ok().map(|m| HINSTANCE(m.0)),
            PCWSTR(resource as *const u16),
            IMAGE_ICON,
            size,
            size,
            LR_DEFAULTCOLOR,
        )
        .map(|handle| HICON(handle.0))
        .unwrap_or_default()
    }
}

fn data(window: HWND, icon: HICON) -> NOTIFYICONDATAW {
    let mut data = NOTIFYICONDATAW {
        cbSize: size_of::<NOTIFYICONDATAW>() as u32,
        hWnd: window,
        uID: 1,
        uFlags: NIF_MESSAGE | NIF_ICON | NIF_TIP,
        uCallbackMessage: CALLBACK,
        hIcon: icon,
        ..Default::default()
    };
    // Non-default profiles show their name so concurrent instances are distinguishable.
    let mut tip = crate::product::display_name();
    match STATE.load(Ordering::Relaxed) {
        1 => tip = format!("{tip} · {}", t().controls.tray_controlled),
        2 => tip = format!("{tip} · {}", t().controls.tray_connections_off),
        _ => {}
    }
    let limit = data.szTip.len() - 1;
    for (to, from) in data.szTip[..limit].iter_mut().zip(tip.encode_utf16()) {
        *to = from;
    }
    data
}

/// Add (or update) the icon for the current state and taskbar colour.
fn show(window: HWND, message: NOTIFY_ICON_MESSAGE) -> bool {
    let icon = icon(STATE.load(Ordering::Relaxed));
    let shown = unsafe { Shell_NotifyIconW(message, &data(window, icon)).as_bool() };
    if !icon.is_invalid() {
        // The shell keeps its own copy.
        let _ = unsafe { DestroyIcon(icon) };
    }
    shown
}
fn add(window: HWND) -> bool {
    show(window, NIM_ADD)
}
impl Tray {
    pub fn new() -> Result<Self> {
        unsafe {
            let instance = HINSTANCE(GetModuleHandleW(None)?.0);
            let class = WNDCLASSW {
                lpfnWndProc: Some(procedure),
                hInstance: instance,
                lpszClassName: w!("Farwing.Tray.v1"),
                ..Default::default()
            };
            RegisterClassW(&class);
            // A hidden top-level window receives Explorer's TaskbarCreated broadcast.
            let window = CreateWindowExW(
                WS_EX_TOOLWINDOW,
                class.lpszClassName,
                w!("Farwing Tray"),
                WS_POPUP,
                0,
                0,
                0,
                0,
                None,
                None,
                Some(instance),
                None,
            )?;
            let tray = Self { window };
            ensure!(add(window), t().controls.tray_create_failed);
            Ok(tray)
        }
    }
    pub fn set_state(&self, state: TrayState) {
        if STATE.swap(state as u8, Ordering::Relaxed) != state as u8 {
            show(self.window, NIM_MODIFY);
        }
    }
}
impl Drop for Tray {
    fn drop(&mut self) {
        unsafe {
            let _ = Shell_NotifyIconW(NIM_DELETE, &data(self.window, HICON::default()));
            let _ = DestroyWindow(self.window);
        }
    }
}
unsafe extern "system" fn procedure(
    window: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    if message == unsafe { RegisterWindowMessageW(w!("TaskbarCreated")) } {
        add(window);
        return LRESULT(0);
    }
    // The taskbar's colour or the display scale changed.
    if message == WM_SETTINGCHANGE || message == WM_DPICHANGED || message == WM_DISPLAYCHANGE {
        show(window, NIM_MODIFY);
    }
    if message == CALLBACK {
        match lparam.0 as u32 {
            WM_LBUTTONUP | WM_LBUTTONDBLCLK => {
                let _ = super::window_manager::send(super::window_manager::Request::ShowMain);
            }
            WM_RBUTTONUP | WM_CONTEXTMENU => unsafe {
                if let Ok(menu) = CreatePopupMenu() {
                    // In the language at the time the menu opens.
                    let open = HSTRING::from(t().controls.tray_open);
                    let exit = HSTRING::from(t().controls.tray_exit);
                    let _ = AppendMenuW(menu, MF_STRING, 1, &open);
                    let _ = AppendMenuW(menu, MF_SEPARATOR, 0, None);
                    let _ = AppendMenuW(menu, MF_STRING, 2, &exit);
                    let mut point = POINT::default();
                    let _ = GetCursorPos(&mut point);
                    let _ = SetForegroundWindow(window);
                    let selected = TrackPopupMenu(
                        menu,
                        TPM_RETURNCMD | TPM_RIGHTBUTTON,
                        point.x,
                        point.y,
                        None,
                        window,
                        None,
                    )
                    .0;
                    let _ = PostMessageW(Some(window), WM_NULL, WPARAM(0), LPARAM(0));
                    let _ = DestroyMenu(menu);
                    let request = match selected {
                        1 => Some(super::window_manager::Request::ShowMain),
                        2 => Some(super::window_manager::Request::Exit),
                        _ => None,
                    };
                    if let Some(request) = request {
                        let _ = super::window_manager::send(request);
                    }
                }
            },
            _ => (),
        }
        return LRESULT(0);
    }
    unsafe { DefWindowProcW(window, message, wparam, lparam) }
}
