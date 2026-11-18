//! Thread-affine Windows input operations. Ownership lives in the input engine.
use anyhow::{Result, ensure};
use std::time::{Duration, Instant};
use windows::Win32::{
    Foundation::{HANDLE, POINT, RECT},
    System::{StationsAndDesktops::*, Threading::GetCurrentThreadId},
    UI::{
        Input::{KeyboardAndMouse::*, Pointer::*},
        WindowsAndMessaging::*,
    },
};

pub(crate) const INPUT_MARKER: usize = 0x4f55_5549;
pub(crate) fn own_message() -> bool {
    unsafe { GetMessageExtraInfo().0 as usize == INPUT_MARKER }
}

/// A mouse message Windows promoted from a touch or pen contact: its extra
/// information carries `MI_WP_SIGNATURE` in the low 32 bits.
pub(crate) fn touch_message(extra: usize) -> bool {
    extra as u32 & 0xffff_ff00 == 0xff51_5700
}

/// The message being dispatched is one [`touch_message`] describes.
pub(crate) fn touch_promoted() -> bool {
    touch_message(unsafe { GetMessageExtraInfo().0 as usize })
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
    // Single physical transitions are submitted individually. Unicode batches have
    // an explicit caller-owned release obligation if the prefix is incomplete.
    ensure!(
        n == items.len(),
        "Windows输入提交不完整（{n}/{}）",
        items.len()
    );
    Ok(())
}
pub(crate) fn key_input(key: u16, down: bool) -> INPUT {
    let scan = super::keyboard::scan_code(key);
    let mut flags = if down {
        KEYBD_EVENT_FLAGS(0)
    } else {
        KEYEVENTF_KEYUP
    };
    if scan & 0xff00 == 0xe000 {
        flags |= KEYEVENTF_EXTENDEDKEY;
    }
    INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: VIRTUAL_KEY(key),
                wScan: scan & 0xff,
                dwFlags: flags,
                time: 0,
                dwExtraInfo: INPUT_MARKER,
            },
        },
    }
}
pub(crate) fn keyboard(key: u16, down: bool) -> Result<()> {
    send(&[key_input(key, down)])
}
pub(crate) fn toggle(key: u16) -> bool {
    unsafe { GetKeyState(i32::from(key)) & 1 != 0 }
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
pub(crate) fn text(text: &str, permitted: impl Fn() -> bool) -> Result<()> {
    for c in text.chars() {
        ensure!(permitted(), "文字输入已取消");
        let mut units = [0u16; 2];
        let units = c.encode_utf16(&mut units);
        let make = |unit, up| INPUT {
            r#type: INPUT_KEYBOARD,
            Anonymous: INPUT_0 {
                ki: KEYBDINPUT {
                    wVk: VIRTUAL_KEY(0),
                    wScan: unit,
                    dwFlags: if up {
                        KEYEVENTF_UNICODE | KEYEVENTF_KEYUP
                    } else {
                        KEYEVENTF_UNICODE
                    },
                    time: 0,
                    dwExtraInfo: INPUT_MARKER,
                },
            },
        };
        let mut items = Vec::with_capacity(units.len() * 2);
        for &unit in units.iter() {
            items.push(make(unit, false));
        }
        for &unit in units.iter() {
            items.push(make(unit, true));
        }
        let n = unsafe { SendInput(&items, std::mem::size_of::<INPUT>() as i32) } as usize;
        if n != items.len() {
            let error = windows::core::Error::from_thread();
            // Release only Unicode downs accepted in this batch; never repeat text.
            for (index, &unit) in units.iter().enumerate() {
                if index < n && units.len() + index >= n {
                    let _ = send(&[make(unit, true)]);
                }
            }
            anyhow::bail!("Windows文字输入提交不完整（{n}/{}）：{error}", items.len());
        }
    }
    Ok(())
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

pub(crate) struct Touch {
    active: std::collections::BTreeMap<u32, POINTER_TOUCH_INFO>,
    ready: bool,
    last: Instant,
}
impl Default for Touch {
    fn default() -> Self {
        Self {
            active: Default::default(),
            ready: false,
            last: Instant::now(),
        }
    }
}
impl Touch {
    fn inject(&mut self, items: &[POINTER_TOUCH_INFO]) -> Result<()> {
        if items.is_empty() {
            return Ok(());
        }
        if !self.ready {
            unsafe {
                InitializeTouchInjection(10, TOUCH_FEEDBACK_DEFAULT)?;
            }
            self.ready = true;
        }
        // The API rejects submissions inside the same 0.1ms time quantum. Retry
        // only ERROR_NOT_READY; invalid sequences must never be replayed.
        for attempt in 0..3 {
            match unsafe { InjectTouchInput(items) } {
                Ok(()) => {
                    self.last = Instant::now();
                    return Ok(());
                }
                Err(e)
                    if e.code() == windows::Win32::Foundation::ERROR_NOT_READY.to_hresult()
                        && attempt < 2 =>
                {
                    std::thread::sleep(Duration::from_millis(1))
                }
                Err(e) => {
                    // Invalid frames cancel the complete injection sequence in
                    // Windows. Do not keep trying to release obsolete contacts.
                    if e.code() == windows::Win32::Foundation::ERROR_INVALID_PARAMETER.to_hresult()
                    {
                        self.active.clear();
                    }
                    return Err(e.into());
                }
            }
        }
        unreachable!()
    }
    pub fn update(
        &mut self,
        kind: u8,
        points: &[crate::features::host::input::wire::Point],
        rect: RECT,
    ) -> Result<()> {
        if kind == 1 {
            self.cancel()?;
        }
        if kind >= 3 {
            return self.end(kind == 4);
        }
        // End disappearing contacts before reusing their Windows IDs. Include
        // retained contacts in the frame so a full ten-finger replacement fits.
        if self
            .active
            .keys()
            .any(|id| !points.iter().any(|p| p.id == *id))
        {
            let frame: Vec<_> = self
                .active
                .iter()
                .map(|(id, t)| {
                    let mut t = *t;
                    if !points.iter().any(|p| p.id == *id) {
                        t.pointerInfo.pointerFlags = POINTER_FLAG_UP;
                    }
                    t
                })
                .collect();
            self.inject(&frame)?;
            self.active
                .retain(|id, _| points.iter().any(|p| p.id == *id));
        }
        let mut next = std::collections::BTreeMap::new();
        let mut frame = Vec::new();
        for p in points {
            let mut t = POINTER_TOUCH_INFO::default();
            t.pointerInfo.pointerType = PT_TOUCH;
            // UU ids remain keys in the ownership map. Windows IDs are stable
            // across each contact but cannot be zero or overflow on id+1.
            t.pointerInfo.pointerId = self
                .active
                .get(&p.id)
                .map(|t| t.pointerInfo.pointerId)
                .unwrap_or_else(|| {
                    (1..=10)
                        .find(|id| {
                            !self.active.values().any(|t| t.pointerInfo.pointerId == *id)
                                && !next
                                    .values()
                                    .any(|t: &POINTER_TOUCH_INFO| t.pointerInfo.pointerId == *id)
                        })
                        .unwrap_or(0)
                });
            ensure!(t.pointerInfo.pointerId != 0, "触点名额已占用");
            t.pointerInfo.pointerFlags = if self.active.contains_key(&p.id) {
                POINTER_FLAG_UPDATE
            } else {
                POINTER_FLAG_DOWN
            } | POINTER_FLAG_INRANGE
                | POINTER_FLAG_INCONTACT;
            let x = (rect.left + ((rect.right - rect.left) as f32 * p.x as f32).round() as i32)
                .clamp(rect.left, rect.right - 1);
            let y = (rect.top + ((rect.bottom - rect.top) as f32 * p.y as f32).round() as i32)
                .clamp(rect.top, rect.bottom - 1);
            t.pointerInfo.ptPixelLocation = POINT { x, y };
            t.touchMask = TOUCH_MASK_CONTACTAREA | TOUCH_MASK_ORIENTATION | TOUCH_MASK_PRESSURE;
            t.rcContact = RECT {
                left: (x as f64 - p.radius_x).max(rect.left as f64) as i32,
                top: (y as f64 - p.radius_y).max(rect.top as f64) as i32,
                right: (x as f64 + p.radius_x).min((rect.right - 1) as f64) as i32,
                bottom: (y as f64 + p.radius_y).min((rect.bottom - 1) as f64) as i32,
            };
            t.orientation = (p.angle as u32) % 360;
            // S5C9500/S5C8750 zero the whole structure before S5C7710.
            // Therefore an unspecified/zero pressure stays zero, not 512.
            t.pressure = (p.pressure as f32 * 1024.0) as u32;
            frame.push(t);
            t.pointerInfo.pointerFlags =
                POINTER_FLAG_UPDATE | POINTER_FLAG_INRANGE | POINTER_FLAG_INCONTACT;
            next.insert(p.id, t);
        }
        if let Err(error) = self.inject(&frame) {
            let _ = self.cancel();
            return Err(error);
        }
        self.active = next;
        Ok(())
    }
    fn end(&mut self, cancel: bool) -> Result<()> {
        let frame: Vec<_> = self
            .active
            .values()
            .map(|t| {
                let mut t = *t;
                t.pointerInfo.pointerFlags = if cancel {
                    POINTER_FLAG_UP | POINTER_FLAG_CANCELED
                } else {
                    POINTER_FLAG_UP
                };
                t
            })
            .collect();
        let result = self.inject(&frame);
        if result.is_err() && !self.active.is_empty() {
            return result;
        }
        self.active.clear();
        Ok(())
    }
    pub fn cancel(&mut self) -> Result<()> {
        self.end(true)
    }
    pub fn tick(&mut self) -> Result<()> {
        if !self.active.is_empty() && self.last.elapsed() >= Duration::from_millis(100) {
            self.inject(&self.active.values().copied().collect::<Vec<_>>())?;
        }
        Ok(())
    }
}
impl Drop for Touch {
    fn drop(&mut self) {
        let _ = self.cancel();
    }
}
