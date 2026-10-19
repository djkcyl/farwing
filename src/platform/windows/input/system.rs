//! Thread-affine Windows input operations. Ownership lives in the input engine.
use anyhow::{Result, ensure};
use std::time::{Duration, Instant};
use windows::Win32::{
    Foundation::HANDLE,
    System::{StationsAndDesktops::*, Threading::GetCurrentThreadId},
    UI::{Input::KeyboardAndMouse::*, WindowsAndMessaging::*},
};

// dwExtraInfo of input injected by this client ("FWUI"); distinct from other
// products so their injected input is never mistaken for ours.
pub(crate) const INPUT_MARKER: usize = 0x4657_5549;
pub(crate) fn own_message() -> bool {
    unsafe { GetMessageExtraInfo().0 as usize == INPUT_MARKER }
}

pub(crate) fn send(items: &[INPUT]) -> Result<()> {
    if items.is_empty() {
        return Ok(());
    }
    let started = Instant::now();
    let n = unsafe { SendInput(items, std::mem::size_of::<INPUT>() as i32) } as usize;
    if started.elapsed() >= Duration::from_millis(100) {
        tracing::warn!(
            elapsed_ms = started.elapsed().as_millis() as u64,
            requested = items.len(),
            accepted = n,
            "Windows input submission stalled"
        );
    }
    // Physical transitions are submitted individually, so an incomplete
    // submission never leaves an unrecorded press behind.
    ensure!(
        n == items.len(),
        "Windows输入提交不完整（{n}/{}）",
        items.len()
    );
    Ok(())
}
fn keyboard_input(vk: VIRTUAL_KEY, scan: u16, flags: KEYBD_EVENT_FLAGS, down: bool) -> INPUT {
    INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: vk,
                wScan: scan & 0xff,
                dwFlags: if down { flags } else { flags | KEYEVENTF_KEYUP },
                time: 0,
                dwExtraInfo: INPUT_MARKER,
            },
        },
    }
}
/// A physical key by its set-1 scan code (`0xE0xx` when extended); the
/// foreground layout turns it into a virtual key, so the host's own layout
/// and IME interpret it.
pub(crate) fn key(scan: u16, down: bool) -> Result<()> {
    let extended = if scan & 0xff00 == 0xe000 {
        KEYEVENTF_EXTENDEDKEY
    } else {
        KEYBD_EVENT_FLAGS(0)
    };
    send(&[match scan {
        // Pause is an E1 sequence and Num Lock shares its scan code; Hanja and
        // Hangul lie outside the layouts' scan tables. Their VKs are fixed.
        0x45 => keyboard_input(VK_PAUSE, scan, KEYBD_EVENT_FLAGS(0), down),
        0xe045 => keyboard_input(VK_NUMLOCK, scan, KEYEVENTF_EXTENDEDKEY, down),
        0xf1 => keyboard_input(VK_HANJA, scan, KEYBD_EVENT_FLAGS(0), down),
        0xf2 => keyboard_input(VK_HANGUL, scan, KEYBD_EVENT_FLAGS(0), down),
        _ => keyboard_input(VIRTUAL_KEY(0), scan, KEYEVENTF_SCANCODE | extended, down),
    }])
}
/// A logical key by virtual-key code, for host shortcuts that must not depend
/// on the layout (Win+D, Ctrl+Shift+Esc).
pub(crate) fn virtual_key(vk: u16, down: bool) -> Result<()> {
    let scan = super::keyboard::virtual_scan(vk);
    let flags = if scan & 0xff00 == 0xe000 {
        KEYEVENTF_EXTENDEDKEY
    } else {
        KEYBD_EVENT_FLAGS(0)
    };
    send(&[keyboard_input(VIRTUAL_KEY(vk), scan, flags, down)])
}
/// Caps Lock, Num Lock and Scroll Lock toggle state, in `LockState` order.
pub(crate) fn locks() -> [bool; 3] {
    [VK_CAPITAL, VK_NUMLOCK, VK_SCROLL].map(|vk| unsafe { GetKeyState(i32::from(vk.0)) } & 1 != 0)
}
pub(crate) fn mouse(x: i32, y: i32, data: u32, flags: MOUSE_EVENT_FLAGS) -> Result<()> {
    send(&[INPUT {
        r#type: INPUT_MOUSE,
        Anonymous: INPUT_0 {
            mi: MOUSEINPUT {
                dx: x,
                dy: y,
                mouseData: data,
                dwFlags: flags,
                time: 0,
                dwExtraInfo: INPUT_MARKER,
            },
        },
    }])
}
pub(crate) fn button(button: u8, down: bool) -> Result<()> {
    let (d, u, data) = match button {
        1 => (MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP, 0),
        2 => (MOUSEEVENTF_RIGHTDOWN, MOUSEEVENTF_RIGHTUP, 0),
        16 => (MOUSEEVENTF_MIDDLEDOWN, MOUSEEVENTF_MIDDLEUP, 0),
        32 => (MOUSEEVENTF_XDOWN, MOUSEEVENTF_XUP, 1),
        64 => (MOUSEEVENTF_XDOWN, MOUSEEVENTF_XUP, 2),
        _ => anyhow::bail!("无效鼠标按键"),
    };
    mouse(0, 0, data, if down { d } else { u })
}

pub(crate) struct Desktop {
    original: HDESK,
    owned: Option<HDESK>,
    name: String,
    dpi: windows::Win32::UI::HiDpi::DPI_AWARENESS_CONTEXT,
}
impl Desktop {
    pub fn ordinary(&self) -> bool {
        self.name.eq_ignore_ascii_case("Default")
    }
    pub fn new() -> Result<Self> {
        use windows::Win32::UI::HiDpi::*;
        let dpi =
            unsafe { SetThreadDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) };
        ensure!(!dpi.0.is_null(), "无法启用输入线程物理坐标");
        let original = match unsafe { GetThreadDesktop(GetCurrentThreadId()) } {
            Ok(desktop) => desktop,
            Err(error) => {
                unsafe {
                    SetThreadDpiAwarenessContext(dpi);
                }
                return Err(error.into());
            }
        };
        Ok(Self {
            original,
            owned: None,
            name: String::new(),
            dpi,
        })
    }
    /// Returns a new desktop handle without switching; callers release contacts first.
    pub fn changed(&self) -> Result<Option<(HDESK, String)>> {
        let desktop = unsafe {
            OpenInputDesktop(
                DESKTOP_CONTROL_FLAGS(0),
                false,
                DESKTOP_ACCESS_FLAGS(
                    DESKTOP_READOBJECTS.0
                        | DESKTOP_WRITEOBJECTS.0
                        | DESKTOP_CREATEWINDOW.0
                        | DESKTOP_JOURNALPLAYBACK.0,
                ),
            )?
        };
        let mut bytes = 0;
        let mut name = [0u16; 256];
        let result = unsafe {
            GetUserObjectInformationW(
                HANDLE(desktop.0),
                UOI_NAME,
                Some(name.as_mut_ptr().cast()),
                (name.len() * 2) as u32,
                Some(&mut bytes),
            )
        };
        if let Err(error) = result {
            unsafe {
                let _ = CloseDesktop(desktop);
            };
            return Err(error.into());
        }
        let name = String::from_utf16_lossy(
            &name[..name.iter().position(|&n| n == 0).unwrap_or(name.len())],
        );
        if name == self.name {
            unsafe {
                let _ = CloseDesktop(desktop);
            };
            Ok(None)
        } else {
            Ok(Some((desktop, name)))
        }
    }
    pub fn switch(&mut self, desktop: HDESK, name: String) -> Result<()> {
        if let Err(error) = unsafe { SetThreadDesktop(desktop) } {
            unsafe {
                let _ = CloseDesktop(desktop);
            };
            return Err(error.into());
        }
        if let Some(old) = self.owned.replace(desktop) {
            unsafe {
                let _ = CloseDesktop(old);
            }
        }
        self.name = name;
        Ok(())
    }
}
impl Drop for Desktop {
    fn drop(&mut self) {
        unsafe {
            let _ = SetThreadDesktop(self.original);
            if let Some(d) = self.owned.take() {
                let _ = CloseDesktop(d);
            }
            windows::Win32::UI::HiDpi::SetThreadDpiAwarenessContext(self.dpi);
        }
    }
}
