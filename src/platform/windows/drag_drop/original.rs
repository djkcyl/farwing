//! Identity of the capture window belonging to an already-running OLE gesture.
//! Cancellation is addressed to that capture window, never to a new foreground app.
use anyhow::Result;
use windows::Win32::{
    Foundation::*,
    UI::{Input::KeyboardAndMouse::VK_ESCAPE, WindowsAndMessaging::*},
};
#[derive(Clone, Copy, Debug)]
pub(crate) struct Original {
    window: isize,
    thread: u32,
    process: u32,
}
impl Original {
    pub fn foreground() -> Option<Self> {
        let foreground = unsafe { GetForegroundWindow() };
        let thread = unsafe { GetWindowThreadProcessId(foreground, None) };
        Self::thread(thread)
    }
    pub fn thread(thread: u32) -> Option<Self> {
        if thread == 0 {
            return None;
        }
        let mut info = GUITHREADINFO {
            cbSize: std::mem::size_of::<GUITHREADINFO>() as u32,
            ..Default::default()
        };
        unsafe {
            GetGUIThreadInfo(thread, &mut info).ok()?;
        }
        if info.hwndCapture.is_invalid() {
            return None;
        }
        let mut process = 0;
        let actual = unsafe { GetWindowThreadProcessId(info.hwndCapture, Some(&mut process)) };
        (actual == thread && process != 0).then_some(Self {
            window: info.hwndCapture.0 as isize,
            thread,
            process,
        })
    }
    pub fn live(&self) -> bool {
        Self::thread(self.thread)
            .is_some_and(|v| v.window == self.window && v.process == self.process)
    }
    pub fn cancel(&self) -> Result<()> {
        if self.live() {
            unsafe {
                PostMessageW(
                    Some(HWND(self.window as _)),
                    WM_KEYDOWN,
                    WPARAM(VK_ESCAPE.0 as usize),
                    LPARAM(1),
                )?;
                PostMessageW(
                    Some(HWND(self.window as _)),
                    WM_KEYUP,
                    WPARAM(VK_ESCAPE.0 as usize),
                    LPARAM(0xc0000001u32 as isize),
                )?;
            }
        }
        Ok(())
    }
}
