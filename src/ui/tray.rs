//! Notification area integration on the existing Windows event-loop thread.
use anyhow::{Result, ensure};
use windows::{
    Win32::{
        Foundation::*,
        System::LibraryLoader::GetModuleHandleW,
        UI::{Shell::*, WindowsAndMessaging::*},
    },
    core::{PCWSTR, w},
};
const CALLBACK: u32 = WM_APP + 43;
pub(super) struct Tray {
    window: HWND,
}
fn data(window: HWND) -> NOTIFYICONDATAW {
    let mut data = NOTIFYICONDATAW {
        cbSize: size_of::<NOTIFYICONDATAW>() as u32,
        hWnd: window,
        uID: 1,
        uFlags: NIF_MESSAGE | NIF_ICON | NIF_TIP,
        uCallbackMessage: CALLBACK,
        ..Default::default()
    };
    data.hIcon = unsafe {
        LoadIconW(
            GetModuleHandleW(None).ok().map(|m| HINSTANCE(m.0)),
            PCWSTR(1usize as *const u16),
        )
    }
    .unwrap_or_default();
    // Non-default profiles show their name so concurrent instances are distinguishable.
    let tip = crate::product::display_name();
    let limit = data.szTip.len() - 1;
    for (to, from) in data.szTip[..limit].iter_mut().zip(tip.encode_utf16()) {
        *to = from;
    }
    data
}
fn add(window: HWND) -> bool {
    unsafe { Shell_NotifyIconW(NIM_ADD, &data(window)).as_bool() }
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
            ensure!(add(window), "无法创建托盘图标");
            Ok(tray)
        }
    }
}
impl Drop for Tray {
    fn drop(&mut self) {
        unsafe {
            let _ = Shell_NotifyIconW(NIM_DELETE, &data(self.window));
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
    if message == CALLBACK {
        match lparam.0 as u32 {
            WM_LBUTTONUP | WM_LBUTTONDBLCLK => {
                let _ = super::window_manager::send(super::window_manager::Request::ShowMain);
            }
            WM_RBUTTONUP | WM_CONTEXTMENU => unsafe {
                if let Ok(menu) = CreatePopupMenu() {
                    let _ = AppendMenuW(menu, MF_STRING, 1, w!("打开 Farwing"));
                    let _ = AppendMenuW(menu, MF_SEPARATOR, 0, None);
                    let _ = AppendMenuW(menu, MF_STRING, 2, w!("退出"));
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
