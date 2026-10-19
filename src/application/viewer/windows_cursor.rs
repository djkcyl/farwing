//! Preserve native cursor masks/hotspots when a peer supplies a system ID only.
use std::cell::Cell;

use anyhow::{Context, Result, ensure};
use windows::Win32::Foundation::{HANDLE, HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::Graphics::Gdi::{BITMAP, DeleteObject, GetObjectW};
use windows::Win32::UI::Shell::{DefSubclassProc, RemoveWindowSubclass, SetWindowSubclass};
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::core::PCWSTR;

pub(super) struct SystemCursorSource {
    handle: HCURSOR, // LoadCursor returns a shared resource; never destroy it.
    pub size: [u32; 2],
}

impl SystemCursorSource {
    pub fn load(kind: i32, reported_size: [u32; 2]) -> Result<Self> {
        let id = if (1..=65535).contains(&kind) {
            kind as usize
        } else {
            32512
        };
        let handle = unsafe { LoadCursorW(None, PCWSTR(id as *const u16)) }
            .or_else(|_| unsafe { LoadCursorW(None, IDC_ARROW) })?;
        let actual = native_size(handle).unwrap_or_else(|_| unsafe {
            [
                GetSystemMetrics(SM_CXCURSOR).max(1) as u32,
                GetSystemMetrics(SM_CYCURSOR).max(1) as u32,
            ]
        });
        let size = [0, 1].map(|axis| {
            if reported_size[axis] > 0 {
                reported_size[axis]
            } else {
                actual[axis]
            }
        });
        Ok(Self { handle, size })
    }

    fn copy(&self, size: [u32; 2]) -> Result<OwnedCursor> {
        ensure!(
            size.into_iter().all(|n| (1..=2048).contains(&n)),
            "invalid native cursor size"
        );
        let handle = unsafe {
            CopyImage(
                HANDLE(self.handle.0),
                IMAGE_CURSOR,
                size[0] as i32,
                size[1] as i32,
                IMAGE_FLAGS(0),
            )
        }
        .context("scale system cursor")?;
        Ok(OwnedCursor(HCURSOR(handle.0)))
    }
}

struct OwnedCursor(HCURSOR);
impl Drop for OwnedCursor {
    fn drop(&mut self) {
        unsafe {
            let _ = DestroyCursor(self.0);
        }
    }
}

fn native_size(cursor: HCURSOR) -> Result<[u32; 2]> {
    let mut info = ICONINFO::default();
    unsafe { GetIconInfo(HICON(cursor.0), &mut info) }?;
    let bitmap = if info.hbmColor.0.is_null() {
        info.hbmMask
    } else {
        info.hbmColor
    };
    let mut data = BITMAP::default();
    let read = unsafe {
        GetObjectW(
            bitmap.into(),
            std::mem::size_of::<BITMAP>() as i32,
            Some((&mut data as *mut BITMAP).cast()),
        )
    };
    unsafe {
        if !info.hbmColor.0.is_null() {
            let _ = DeleteObject(info.hbmColor.into());
        }
        if !info.hbmMask.0.is_null() {
            let _ = DeleteObject(info.hbmMask.into());
        }
    }
    ensure!(
        read > 0 && data.bmWidth > 0 && data.bmHeight > 0,
        "read native cursor bitmap"
    );
    // A monochrome cursor stores AND and XOR masks vertically in one bitmap.
    Ok([
        data.bmWidth as u32,
        data.bmHeight as u32 / if info.hbmColor.0.is_null() { 2 } else { 1 },
    ])
}

struct Hook {
    cursor: HCURSOR,
    enabled: Cell<bool>,
    installed: Cell<bool>,
}

/// UI-thread owned. The stable Box is removed before destruction; WM_NCDESTROY
/// also retires the hook if the native window goes away before its Rust owner.
pub(super) struct SystemCursor {
    hwnd: HWND,
    hook: Option<Box<Hook>>,
    cursor: OwnedCursor,
}

impl SystemCursor {
    pub fn new(hwnd: HWND, source: &SystemCursorSource, size: [u32; 2]) -> Result<Self> {
        let cursor = source.copy(size)?;
        let hook = Box::new(Hook {
            cursor: cursor.0,
            enabled: Cell::new(false),
            installed: Cell::new(true),
        });
        let id = (&*hook as *const Hook) as usize;
        ensure!(
            unsafe { SetWindowSubclass(hwnd, Some(cursor_proc), id, id).as_bool() },
            "install system cursor handler"
        );
        Ok(Self {
            hwnd,
            hook: Some(hook),
            cursor,
        })
    }

    pub fn set_active(&self, active: bool) {
        let hook = self.hook.as_ref().expect("live cursor hook");
        hook.enabled.set(active);
        if active && hook.installed.get() {
            unsafe {
                SetCursor(Some(self.cursor.0));
            }
        }
    }
}

impl Drop for SystemCursor {
    fn drop(&mut self) {
        if let Some(hook) = self.hook.take() {
            hook.enabled.set(false);
            let id = (&*hook as *const Hook) as usize;
            if hook.installed.get()
                && !unsafe { RemoveWindowSubclass(self.hwnd, Some(cursor_proc), id).as_bool() }
            {
                // A disabled leaked userdata block is safer than a dangling
                // callback if the native removal fails. It owns no GDI handle.
                tracing::warn!("system cursor handler removal failed");
                Box::leak(hook);
            }
        }
        if unsafe { GetCursor() } == self.cursor.0 {
            unsafe {
                SetCursor(LoadCursorW(None, IDC_ARROW).ok());
            }
        }
    }
}

unsafe extern "system" fn cursor_proc(
    hwnd: HWND,
    msg: u32,
    wp: WPARAM,
    lp: LPARAM,
    id: usize,
    data: usize,
) -> LRESULT {
    // Installed on this window's UI thread with stable, owned userdata.
    let hook = unsafe { &*(data as *const Hook) };
    if msg == WM_NCDESTROY {
        hook.enabled.set(false);
        hook.installed.set(false);
        unsafe {
            let _ = RemoveWindowSubclass(hwnd, Some(cursor_proc), id);
        }
    } else if msg == WM_SETCURSOR
        && (lp.0 as u16) == HTCLIENT as u16
        && hook.enabled.get()
        && unsafe { GetForegroundWindow() } == hwnd
    {
        unsafe {
            SetCursor(Some(hook.cursor));
        }
        return LRESULT(1);
    }
    unsafe { DefSubclassProc(hwnd, msg, wp, lp) }
}
