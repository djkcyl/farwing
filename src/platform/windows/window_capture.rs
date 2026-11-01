//! One window's pictures through Windows Graphics Capture
//! (`docs/protocol/chat-design.md` §8.2): captured while covered, the last
//! picture held while minimized, no yellow border on Windows 11.

use anyhow::{Context, Result, bail};
use std::sync::mpsc;
use std::time::Duration;
use windows::Graphics::Capture::{Direct3D11CaptureFramePool, GraphicsCaptureItem, GraphicsCaptureSession};
use windows::Graphics::DirectX::Direct3D11::IDirect3DDevice;
use windows::Graphics::DirectX::DirectXPixelFormat;
use windows::Graphics::SizeInt32;
use windows::Win32::Foundation::{CloseHandle, HWND, LPARAM, RECT};
use windows::Win32::Graphics::Direct3D11::{
    D3D11_BIND_SHADER_RESOURCE, D3D11_TEXTURE2D_DESC, D3D11_USAGE_DEFAULT, ID3D11Device, ID3D11DeviceContext,
    ID3D11Texture2D,
};
use windows::Win32::Graphics::Dwm::{DWMWA_CLOAKED, DwmGetWindowAttribute};
use windows::Win32::Graphics::Dxgi::IDXGIDevice;
use windows::Win32::Graphics::Gdi::{GetMonitorInfoW, MONITOR_DEFAULTTOPRIMARY, MONITORINFOEXW, MonitorFromWindow};
use windows::Win32::System::Threading::{
    OpenProcess, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION, QueryFullProcessImageNameW,
};
use windows::Win32::System::WinRT::Direct3D11::{CreateDirect3D11DeviceFromDXGIDevice, IDirect3DDxgiInterfaceAccess};
use windows::Win32::System::WinRT::Graphics::Capture::IGraphicsCaptureItemInterop;
use windows::Win32::UI::WindowsAndMessaging::{
    EnumWindows, GW_OWNER, GWL_EXSTYLE, GetWindow, GetWindowLongW, GetWindowRect, GetWindowTextLengthW, GetWindowTextW,
    GetWindowThreadProcessId, IsIconic, IsWindow, IsWindowVisible, WS_EX_TOOLWINDOW,
};
use windows::core::{BOOL, Interface};

/// A window that can be shared.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Window {
    pub hwnd: isize,
    /// The program it belongs to.
    pub pid: u32,
    pub title: String,
    /// The program's file name without its extension.
    pub program: String,
}

/// The windows on the desktop that can be shared, front to back (not this
/// program's own).
pub(crate) fn list() -> Vec<Window> {
    unsafe extern "system" fn each(hwnd: HWND, found: LPARAM) -> BOOL {
        // SAFETY: `found` is the vector passed below, alive for the call.
        let found = unsafe { &mut *(found.0 as *mut Vec<Window>) };
        if let Some(window) = shareable(hwnd) {
            found.push(window);
        }
        BOOL(1)
    }
    let mut found: Vec<Window> = Vec::new();
    // SAFETY: the callback only reads window properties.
    unsafe {
        let _ = EnumWindows(Some(each), LPARAM((&raw mut found) as isize));
    }
    found
}

fn shareable(hwnd: HWND) -> Option<Window> {
    // SAFETY: plain queries of a window's properties.
    unsafe {
        if !IsWindowVisible(hwnd).as_bool() || GetWindow(hwnd, GW_OWNER).is_ok_and(|o| !o.is_invalid()) {
            return None;
        }
        if GetWindowLongW(hwnd, GWL_EXSTYLE) as u32 & WS_EX_TOOLWINDOW.0 != 0 {
            return None;
        }
        let mut cloaked = 0u32;
        if DwmGetWindowAttribute(hwnd, DWMWA_CLOAKED, (&raw mut cloaked).cast(), 4).is_ok() && cloaked != 0 {
            return None;
        }
        let mut rect = RECT::default();
        if !IsIconic(hwnd).as_bool()
            && (GetWindowRect(hwnd, &mut rect).is_err() || rect.right - rect.left < 32 || rect.bottom - rect.top < 32)
        {
            return None;
        }
        let length = GetWindowTextLengthW(hwnd);
        if length <= 0 {
            return None;
        }
        let mut title = vec![0u16; length as usize + 1];
        let copied = GetWindowTextW(hwnd, &mut title);
        let title = String::from_utf16_lossy(&title[..copied.max(0) as usize]);
        let mut pid = 0u32;
        GetWindowThreadProcessId(hwnd, Some(&mut pid));
        if pid == 0 || pid == std::process::id() || title.trim().is_empty() {
            return None;
        }
        Some(Window {
            hwnd: hwnd.0 as isize,
            pid,
            title,
            program: program(pid).unwrap_or_default(),
        })
    }
}

fn program(pid: u32) -> Option<String> {
    // SAFETY: a limited query handle, closed below.
    unsafe {
        let process = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid).ok()?;
        let mut path = [0u16; 1024];
        let mut size = path.len() as u32;
        let result = QueryFullProcessImageNameW(
            process,
            PROCESS_NAME_WIN32,
            windows::core::PWSTR(path.as_mut_ptr()),
            &mut size,
        );
        let _ = CloseHandle(process);
        result.ok()?;
        let path = String::from_utf16_lossy(&path[..size as usize]);
        Some(std::path::Path::new(&path).file_stem()?.to_string_lossy().into_owned())
    }
}

/// The window still exists.
pub(crate) fn exists(hwnd: isize) -> bool {
    // SAFETY: a plain query.
    unsafe { IsWindow(Some(HWND(hwnd as *mut _))).as_bool() }
}

/// The window is minimized.
pub(crate) fn minimized(hwnd: isize) -> bool {
    // SAFETY: a plain query.
    unsafe { IsIconic(HWND(hwnd as *mut _)).as_bool() }
}

/// The adapter of the screen the window is mostly on (that screen's
/// capture device).
fn adapter(hwnd: HWND) -> Option<u64> {
    let screens = super::capture::screens().ok()?;
    // SAFETY: plain queries; the info's size is set.
    let device = unsafe {
        let monitor = MonitorFromWindow(hwnd, MONITOR_DEFAULTTOPRIMARY);
        let mut info = MONITORINFOEXW::default();
        info.monitorInfo.cbSize = size_of::<MONITORINFOEXW>() as u32;
        GetMonitorInfoW(monitor, (&raw mut info).cast()).ok().ok()?;
        let end = info.szDevice.iter().position(|c| *c == 0).unwrap_or(info.szDevice.len());
        String::from_utf16_lossy(&info.szDevice[..end])
    };
    let screen = screens
        .iter()
        .find(|s| s.device_name.eq_ignore_ascii_case(&device))
        .or_else(|| screens.iter().find(|s| s.primary))?;
    Some(screen.render_adapter.unwrap_or(screen.adapter))
}

/// A window being captured; stops when dropped.
pub(crate) struct Capture {
    device: ID3D11Device,
    context: ID3D11DeviceContext,
    winrt_device: IDirect3DDevice,
    pool: Direct3D11CaptureFramePool,
    session: GraphicsCaptureSession,
    size: SizeInt32,
    arrived: mpsc::Receiver<()>,
}

impl Drop for Capture {
    fn drop(&mut self) {
        let _ = self.session.Close();
        let _ = self.pool.Close();
    }
}

impl Capture {
    /// Starts capturing window `hwnd`.
    pub(crate) fn open(hwnd: isize) -> Result<Self> {
        let window = HWND(hwnd as *mut _);
        if !exists(hwnd) {
            bail!("the window is gone");
        }
        let (device, context) = super::capture::create_device(adapter(window).context("no screen for the window")?)?;
        // SAFETY: WinRT interop on a live device and window.
        unsafe {
            let dxgi: IDXGIDevice = device.cast()?;
            let winrt_device: IDirect3DDevice = CreateDirect3D11DeviceFromDXGIDevice(&dxgi)?.cast()?;
            let interop = windows::core::factory::<GraphicsCaptureItem, IGraphicsCaptureItemInterop>()?;
            let item: GraphicsCaptureItem = interop.CreateForWindow(window).context("capture the window")?;
            let size = item.Size()?;
            let pool = Direct3D11CaptureFramePool::CreateFreeThreaded(
                &winrt_device,
                DirectXPixelFormat::B8G8R8A8UIntNormalized,
                2,
                size,
            )?;
            let (tx, arrived) = mpsc::channel();
            pool.FrameArrived(&windows::Foundation::TypedEventHandler::new(move |_, _| {
                let _ = tx.send(());
                Ok(())
            }))?;
            let session = pool.CreateCaptureSession(&item)?;
            // Windows 11 only; the border stays elsewhere.
            let _ = session.SetIsBorderRequired(false);
            let _ = session.SetIsCursorCaptureEnabled(true);
            session.StartCapture()?;
            Ok(Self {
                device,
                context,
                winrt_device,
                pool,
                session,
                size,
                arrived,
            })
        }
    }

    pub(crate) fn device(&self) -> &ID3D11Device {
        &self.device
    }

    /// The next picture of the window (BGRA, its content's size), `None`
    /// when none came within `timeout` (a still or minimized window).
    pub(crate) fn next(&mut self, timeout: Duration) -> Result<Option<ID3D11Texture2D>> {
        let deadline = std::time::Instant::now() + timeout;
        loop {
            // The newest picture: older ones queued behind it are skipped.
            let mut frame = None;
            while let Ok(f) = self.pool.TryGetNextFrame() {
                frame = Some(f);
            }
            if let Some(frame) = frame {
                let content = frame.ContentSize()?;
                if content.Width <= 0 || content.Height <= 0 {
                    return Ok(None);
                }
                if content != self.size {
                    // Resized: the pool follows; the next picture has the size.
                    self.size = content;
                    self.pool.Recreate(&self.winrt_device, DirectXPixelFormat::B8G8R8A8UIntNormalized, 2, content)?;
                    continue;
                }
                let access: IDirect3DDxgiInterfaceAccess = frame.Surface()?.cast()?;
                // SAFETY: the frame's surface is a texture on our device.
                let surface: ID3D11Texture2D = unsafe { access.GetInterface()? };
                let copy = self.copy(&surface, content)?;
                let _ = frame.Close();
                return Ok(Some(copy));
            }
            let left = deadline.saturating_duration_since(std::time::Instant::now());
            if left.is_zero() {
                return Ok(None);
            }
            match self.arrived.recv_timeout(left) {
                Ok(()) | Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => bail!("the window capture ended"),
            }
        }
    }

    /// The frame's content as a texture of its own (the pool's buffer goes
    /// back for the next picture).
    fn copy(&self, surface: &ID3D11Texture2D, size: SizeInt32) -> Result<ID3D11Texture2D> {
        let mut desc = D3D11_TEXTURE2D_DESC::default();
        // SAFETY: a description query, a new texture and a copy on our device.
        unsafe {
            surface.GetDesc(&mut desc);
            desc.Width = size.Width as u32 & !1;
            desc.Height = size.Height as u32 & !1;
            desc.MipLevels = 1;
            desc.ArraySize = 1;
            desc.Usage = D3D11_USAGE_DEFAULT;
            desc.BindFlags = D3D11_BIND_SHADER_RESOURCE.0 as u32;
            desc.CPUAccessFlags = 0;
            desc.MiscFlags = 0;
            let mut texture = None;
            self.device.CreateTexture2D(&desc, None, Some(&mut texture))?;
            let texture = texture.context("window picture texture")?;
            let region = windows::Win32::Graphics::Direct3D11::D3D11_BOX {
                left: 0,
                top: 0,
                front: 0,
                right: desc.Width,
                bottom: desc.Height,
                back: 1,
            };
            self.context.CopySubresourceRegion(&texture, 0, 0, 0, 0, surface, 0, Some(&region));
            Ok(texture)
        }
    }
}
