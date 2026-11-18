//! Notification area integration on the existing Windows event-loop thread.
use anyhow::{Result, ensure};
use std::sync::{
    Mutex,
    atomic::{AtomicBool, Ordering},
};
use windows::{
    Win32::{
        Foundation::*,
        System::LibraryLoader::GetModuleHandleW,
        UI::{Shell::*, WindowsAndMessaging::*},
    },
    core::{PCWSTR, w},
};
const CALLBACK: u32 = WM_APP + 43;
/// 托盘只有一个实例，而窗口过程是自由回调、拿不到 [`Tray`]，所以"是否被控"
/// 与角标图标都放在模块级。HICON 是裸指针，按句柄值存以绕开 `Sync` 约束。
static CONTROLLED: AtomicBool = AtomicBool::new(false);
static BADGE: Mutex<Option<isize>> = Mutex::new(None);
pub(super) struct Tray {
    window: HWND,
}
/// 程序自带的那份图标（EXE 资源 ID 1）。
fn resource_icon() -> HICON {
    unsafe {
        LoadIconW(
            GetModuleHandleW(None).ok().map(|m| HINSTANCE(m.0)),
            PCWSTR(1usize as *const u16),
        )
    }
    .unwrap_or_default()
}
/// 被控角标那份：合成好的位图编码成 PNG 交给 shell 装载（Vista 起支持），
/// 尺寸按系统的小图标尺寸取，免得 shell 自己缩放。
fn badge_icon() -> Result<HICON> {
    let mut cached = BADGE.lock().unwrap_or_else(|error| error.into_inner());
    if let Some(handle) = *cached {
        return Ok(HICON(handle as *mut core::ffi::c_void));
    }
    let size = match unsafe { GetSystemMetrics(SM_CXSMICON) } {
        size if size > 0 => size as u32,
        _ => 16,
    };
    let image = crate::ui::branding::tray_pixmap(size, true);
    let mut png = std::io::Cursor::new(Vec::new());
    image.write_to(&mut png, image::ImageFormat::Png)?;
    let icon = unsafe {
        CreateIconFromResourceEx(
            png.get_ref(),
            true,
            0x0003_0000,
            size as i32,
            size as i32,
            LR_DEFAULTCOLOR,
        )
    }?;
    *cached = Some(icon.0 as isize);
    Ok(icon)
}
fn current_icon() -> HICON {
    if CONTROLLED.load(Ordering::Relaxed) {
        if let Ok(icon) = badge_icon() {
            return icon;
        }
    }
    resource_icon()
}
fn data(window: HWND) -> NOTIFYICONDATAW {
    let mut data = NOTIFYICONDATAW {
        cbSize: size_of::<NOTIFYICONDATAW>() as u32,
        hWnd: window,
        uID: 1,
        uFlags: NIF_MESSAGE | NIF_ICON | NIF_TIP,
        uCallbackMessage: CALLBACK,
        hIcon: current_icon(),
        ..Default::default()
    };
    for (to, from) in data.szTip.iter_mut().zip("OpenUUYC".encode_utf16()) {
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
                lpszClassName: w!("OpenUUYC.Tray.v1"),
                ..Default::default()
            };
            RegisterClassW(&class);
            // A hidden top-level window receives Explorer's TaskbarCreated broadcast.
            let window = CreateWindowExW(
                WS_EX_TOOLWINDOW,
                class.lpszClassName,
                w!("OpenUUYC Tray"),
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
    /// 被控角标：这台机器正被别人控制时，托盘图标右上角多一个红三角。
    pub(super) fn set_controlled(&self, controlled: bool) {
        if CONTROLLED.load(Ordering::Relaxed) == controlled {
            return;
        }
        let icon = if controlled {
            match badge_icon() {
                Ok(icon) => icon,
                Err(error) => {
                    tracing::warn!(error = %format!("{error:#}"), "托盘角标合成失败");
                    return;
                }
            }
        } else {
            resource_icon()
        };
        let mut update = data(self.window);
        update.hIcon = icon;
        // Cache only the state accepted by the shell. A failed update must
        // remain pending for the existing UI refresh to try again.
        if unsafe { Shell_NotifyIconW(NIM_MODIFY, &update) }.as_bool() {
            CONTROLLED.store(controlled, Ordering::Relaxed);
        } else {
            tracing::debug!(controlled, "tray icon update was not accepted");
        }
    }
}
impl Drop for Tray {
    fn drop(&mut self) {
        unsafe {
            let _ = Shell_NotifyIconW(NIM_DELETE, &data(self.window));
            let cached = BADGE
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .take();
            if let Some(handle) = cached {
                let _ = DestroyIcon(HICON(handle as *mut core::ffi::c_void));
            }
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
                    let _ = AppendMenuW(menu, MF_STRING, 1, w!("打开 OpenUUYC"));
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
                        2 => {
                            tracing::info!("托盘菜单选择了「退出」");
                            Some(super::window_manager::Request::Exit)
                        }
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
