//! Notification area integration on the existing Windows event-loop thread.
use crate::i18n::t;
use anyhow::{Result, ensure};
use std::sync::atomic::{AtomicIsize, AtomicU8, Ordering};
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
/// Exiting would also stop a background host: the menu offers leaving the
/// device center (the host keeps running) and quitting completely.
static FULL_EXIT: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
/// In a voice channel or call (`docs/protocol/chat-design.md` §7.7): bit 0
/// in voice, bit 1 muted, bit 2 deafened, bit 3 a call.
static VOICE: AtomicU8 = AtomicU8::new(0);
/// What the menu asked of the voice: 1 mute, 2 deafen, 3 leave (0: none).
static VOICE_ACTION: AtomicU8 = AtomicU8::new(0);
/// The icon's window, for updates from the voice.
static WINDOW: AtomicIsize = AtomicIsize::new(0);

/// The device center's voice, for the menu; the icon marks it.
pub(crate) fn set_voice(in_voice: bool, muted: bool, deafened: bool, call: bool) {
    let bits = u8::from(in_voice) | (u8::from(muted) << 1) | (u8::from(deafened) << 2) | (u8::from(call) << 3);
    let before = VOICE.swap(bits, Ordering::Relaxed);
    let window = WINDOW.load(Ordering::Relaxed);
    if (before & 0b1001) != (bits & 0b1001) && window != 0 {
        show(HWND(window as *mut _), NIM_MODIFY);
    }
}

/// `icon` with a green dot in its corner: in a voice channel or call.
fn with_dot(icon: HICON) -> HICON {
    use windows::Win32::Graphics::Gdi::*;
    // SAFETY: GDI objects created here are released here; `icon` is
    // replaced by the new one.
    unsafe {
        let mut info = ICONINFO::default();
        if GetIconInfo(icon, &mut info).is_err() || info.hbmColor.is_invalid() {
            return icon;
        }
        let mut bitmap = BITMAP::default();
        GetObjectW(info.hbmColor.into(), size_of::<BITMAP>() as i32, Some((&raw mut bitmap).cast()));
        let (w, h) = (bitmap.bmWidth, bitmap.bmHeight);
        let dc = CreateCompatibleDC(None);
        let mut header = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: w,
                biHeight: -h,
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            ..Default::default()
        };
        let mut pixels = vec![0u32; (w * h).max(0) as usize];
        GetDIBits(dc, info.hbmColor, 0, h as u32, Some(pixels.as_mut_ptr().cast()), &mut header, DIB_RGB_COLORS);
        let r = w as f32 * 0.24;
        let (cx, cy) = (w as f32 - r - 0.5, h as f32 - r - 0.5);
        for y in 0..h {
            for x in 0..w {
                let d = ((x as f32 + 0.5 - cx).powi(2) + (y as f32 + 0.5 - cy).powi(2)).sqrt();
                let i = (y * w + x) as usize;
                if d <= r {
                    pixels[i] = 0xFF4C_C38A;
                } else if d <= r + 1.2 {
                    pixels[i] = 0xFFFF_FFFF;
                }
            }
        }
        let mut bits = std::ptr::null_mut();
        let color = CreateDIBSection(Some(dc), &header, DIB_RGB_COLORS, &mut bits, None, 0);
        let result = match color {
            Ok(color) if !bits.is_null() => {
                std::ptr::copy_nonoverlapping(pixels.as_ptr(), bits.cast::<u32>(), pixels.len());
                let mask = CreateBitmap(w, h, 1, 1, None);
                let made = CreateIconIndirect(&ICONINFO {
                    fIcon: true.into(),
                    hbmMask: mask,
                    hbmColor: color,
                    ..Default::default()
                });
                let _ = DeleteObject(mask.into());
                let _ = DeleteObject(color.into());
                made.ok()
            }
            _ => None,
        };
        let _ = DeleteObject(info.hbmColor.into());
        let _ = DeleteObject(info.hbmMask.into());
        let _ = DeleteDC(dc);
        match result {
            Some(made) => {
                let _ = DestroyIcon(icon);
                made
            }
            None => icon,
        }
    }
}

/// The voice action chosen in the menu since last asked.
pub(crate) fn take_voice_action() -> u8 {
    VOICE_ACTION.swap(0, Ordering::Relaxed)
}

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
    let voice = VOICE.load(Ordering::Relaxed);
    if voice & 1 != 0 {
        let v = &t().voice;
        tip = format!("{tip} · {}", if voice & 8 != 0 { v.in_call } else { v.connected });
    }
    let limit = data.szTip.len() - 1;
    for (to, from) in data.szTip[..limit].iter_mut().zip(tip.encode_utf16()) {
        *to = from;
    }
    data
}

/// Add (or update) the icon for the current state and taskbar colour.
fn show(window: HWND, message: NOTIFY_ICON_MESSAGE) -> bool {
    let mut icon = icon(STATE.load(Ordering::Relaxed));
    if VOICE.load(Ordering::Relaxed) & 1 != 0 && !icon.is_invalid() {
        icon = with_dot(icon);
    }
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
            WINDOW.store(window.0 as isize, Ordering::Relaxed);
            ensure!(add(window), t().controls.tray_create_failed);
            Ok(tray)
        }
    }
    pub fn set_state(&self, state: TrayState, full_exit: bool) {
        FULL_EXIT.store(full_exit, Ordering::Relaxed);
        if STATE.swap(state as u8, Ordering::Relaxed) != state as u8 {
            show(self.window, NIM_MODIFY);
        }
    }
}
impl Drop for Tray {
    fn drop(&mut self) {
        unsafe {
            WINDOW.store(0, Ordering::Relaxed);
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
                    let _ = AppendMenuW(menu, MF_STRING, 1, &open);
                    let _ = AppendMenuW(menu, MF_SEPARATOR, 0, None);
                    let voice = VOICE.load(Ordering::Relaxed);
                    if voice & 1 != 0 {
                        let v = &t().voice;
                        let mute = HSTRING::from(if voice & 2 != 0 { v.unmute } else { v.mute });
                        let deafen = HSTRING::from(if voice & 4 != 0 { v.undeafen } else { v.deafen });
                        let leave = HSTRING::from(if voice & 8 != 0 { v.hang_up } else { v.disconnect });
                        let _ = AppendMenuW(menu, MF_STRING, 11, &mute);
                        let _ = AppendMenuW(menu, MF_STRING, 12, &deafen);
                        let _ = AppendMenuW(menu, MF_STRING, 13, &leave);
                        let _ = AppendMenuW(menu, MF_SEPARATOR, 0, None);
                    }
                    if FULL_EXIT.load(Ordering::Relaxed) {
                        let leave = HSTRING::from(t().controls.tray_leave);
                        let quit = HSTRING::from(t().controls.tray_quit);
                        let _ = AppendMenuW(menu, MF_STRING, 3, &leave);
                        let _ = AppendMenuW(menu, MF_STRING, 2, &quit);
                    } else {
                        let exit = HSTRING::from(t().controls.tray_exit);
                        let _ = AppendMenuW(menu, MF_STRING, 2, &exit);
                    }
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
                    if (11..=13).contains(&selected) {
                        VOICE_ACTION.store((selected - 10) as u8, Ordering::Relaxed);
                    }
                    let request = match selected {
                        1 => Some(super::window_manager::Request::ShowMain),
                        2 => Some(super::window_manager::Request::Exit),
                        3 => Some(super::window_manager::Request::Leave),
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
