//! Selected-monitor GDI fallback. DCs, bitmap and GPU upload stay on one thread.
use super::{
    capture::{Frame, Screen},
    preprocess::{Converter, Request},
};
use anyhow::{Context, Result, ensure};
use windows::{
    Win32::{
        Graphics::{Direct3D11::*, Dxgi::Common::*, Gdi::*},
        UI::WindowsAndMessaging::*,
    },
    core::{PCWSTR, w},
};
pub(super) struct Capture {
    pub device: ID3D11Device,
    context: ID3D11DeviceContext,
    pub screen: Screen,
    source: HDC,
    memory: HDC,
    bitmap: HBITMAP,
    previous: HGDIOBJ,
    bits: *mut std::ffi::c_void,
    texture: ID3D11Texture2D,
    converter: Converter,
}
impl Capture {
    pub fn new(selected: &Screen) -> Result<Self> {
        let screen = super::capture::refresh(selected)?;
        let (device, context) = super::capture::create_device(screen.adapter)?;
        unsafe {
            let converter = Converter::new(&device)?;
            let mut texture = None;
            device.CreateTexture2D(
                &D3D11_TEXTURE2D_DESC {
                    Width: screen.width,
                    Height: screen.height,
                    MipLevels: 1,
                    ArraySize: 1,
                    Format: DXGI_FORMAT_B8G8R8A8_UNORM,
                    SampleDesc: DXGI_SAMPLE_DESC {
                        Count: 1,
                        Quality: 0,
                    },
                    Usage: D3D11_USAGE_DEFAULT,
                    BindFlags: D3D11_BIND_SHADER_RESOURCE.0 as u32,
                    ..Default::default()
                },
                None,
                Some(&mut texture),
            )?;
            let mut result = Self {
                device,
                context,
                screen,
                source: HDC::default(),
                memory: HDC::default(),
                bitmap: HBITMAP::default(),
                previous: HGDIOBJ::default(),
                bits: std::ptr::null_mut(),
                texture: texture.context("创建GDI上传纹理")?,
                converter,
            };
            let name: Vec<_> = result
                .screen
                .device_name
                .encode_utf16()
                .chain(Some(0))
                .collect();
            result.source = CreateDCW(w!("DISPLAY"), PCWSTR(name.as_ptr()), PCWSTR::null(), None);
            ensure!(!result.source.0.is_null(), "无法取得所选屏幕GDI上下文");
            result.memory = CreateCompatibleDC(Some(result.source));
            ensure!(!result.memory.0.is_null(), "无法创建GDI采集上下文");
            let info = BITMAPINFO {
                bmiHeader: BITMAPINFOHEADER {
                    biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                    biWidth: result.screen.width as i32,
                    biHeight: -(result.screen.height as i32),
                    biPlanes: 1,
                    biBitCount: 32,
                    biCompression: BI_RGB.0,
                    ..Default::default()
                },
                ..Default::default()
            };
            result.bitmap = CreateDIBSection(
                Some(result.memory),
                &info,
                DIB_RGB_COLORS,
                &mut result.bits,
                None,
                0,
            )?;
            ensure!(!result.bits.is_null(), "GDI没有返回图像内存");
            result.previous = SelectObject(result.memory, result.bitmap.into());
            ensure!(
                !result.previous.0.is_null() && result.previous.0 as isize != -1,
                "GDI未绑定采集位图"
            );
            Ok(result)
        }
    }
    pub fn next(&mut self, request: Request, cursor: bool) -> Result<Frame> {
        // The DC belongs to this monitor, rather than a rectangle of the global
        // desktop. A topology change cannot accidentally capture its old neighbor.
        // Desktop owns the full topology/adapter refresh. The hot path still
        // checks this exact monitor's identity and physical mode before reading
        // its DC, without enumerating every GPU and DPI/HDR capability per frame.
        let name: Vec<_> = self
            .screen
            .device_name
            .encode_utf16()
            .chain(Some(0))
            .collect();
        if self.screen.identity.is_some() {
            ensure!(
                super::capture::display_identity(&self.screen.device_name) == self.screen.identity,
                "GDI所选屏幕身份变化，需要重建"
            );
        } else {
            let current = super::capture::refresh(&self.screen)?;
            ensure!(
                current.device_name == self.screen.device_name
                    && current.adapter == self.screen.adapter,
                "GDI所选源变化，需要重建"
            );
        }
        let mut mode = DEVMODEW {
            dmSize: std::mem::size_of::<DEVMODEW>() as u16,
            ..Default::default()
        };
        unsafe {
            ensure!(
                EnumDisplaySettingsExW(
                    PCWSTR(name.as_ptr()),
                    ENUM_CURRENT_SETTINGS,
                    &mut mode,
                    ENUM_DISPLAY_SETTINGS_FLAGS(0)
                )
                .as_bool()
                    && mode.dmPelsWidth == self.screen.width
                    && mode.dmPelsHeight == self.screen.height,
                "GDI所选源变化，需要重建"
            );
            BitBlt(
                self.memory,
                0,
                0,
                self.screen.width as i32,
                self.screen.height as i32,
                Some(self.source),
                0,
                0,
                SRCCOPY | CAPTUREBLT,
            )?;
            if cursor {
                let mut info = CURSORINFO {
                    cbSize: std::mem::size_of::<CURSORINFO>() as u32,
                    ..Default::default()
                };
                if GetCursorInfo(&mut info).is_ok() && info.flags == CURSOR_SHOWING {
                    let mut icon = ICONINFO::default();
                    if GetIconInfo(HICON(info.hCursor.0), &mut icon).is_ok() {
                        let _ = DrawIconEx(
                            self.memory,
                            info.ptScreenPos.x - self.screen.left - icon.xHotspot as i32,
                            info.ptScreenPos.y - self.screen.top - icon.yHotspot as i32,
                            HICON(info.hCursor.0),
                            0,
                            0,
                            0,
                            None,
                            DI_NORMAL,
                        );
                        if !icon.hbmColor.0.is_null() {
                            let _ = DeleteObject(icon.hbmColor.into());
                        }
                        if !icon.hbmMask.0.is_null() {
                            let _ = DeleteObject(icon.hbmMask.into());
                        }
                    }
                }
            }
            let _ = GdiFlush();
            self.context.UpdateSubresource(
                &self.texture,
                0,
                None,
                self.bits,
                self.screen.width * 4,
                0,
            );
            let mut desc = D3D11_TEXTURE2D_DESC::default();
            self.texture.GetDesc(&mut desc);
            let captured = std::time::Instant::now();
            let texture = self.converter.convert(
                &self.device,
                &self.context,
                &self.texture,
                desc,
                DXGI_MODE_ROTATION_IDENTITY,
                // BGRA -> SDR shader mode 0 does not consume SDR white level.
                if request.hdr {
                    crate::platform::display_hdr::sdr_white_scale_or(&self.screen.device_name, 3.75)
                } else {
                    1.
                },
                None,
                request,
                None,
            )?;
            texture.GetDesc(&mut desc);
            Ok(Frame {
                width: desc.Width,
                height: desc.Height,
                texture,
                captured,
                is_new: true,
                _storage: None,
                hdr_metadata: None,
            })
        }
    }
}
impl Drop for Capture {
    fn drop(&mut self) {
        unsafe {
            if !self.memory.0.is_null()
                && !self.previous.0.is_null()
                && self.previous.0 as isize != -1
            {
                SelectObject(self.memory, self.previous);
            }
            if !self.bitmap.0.is_null() {
                let _ = DeleteObject(self.bitmap.into());
            }
            if !self.memory.0.is_null() {
                let _ = DeleteDC(self.memory);
            }
            if !self.source.0.is_null() {
                let _ = DeleteDC(self.source);
            }
        }
    }
}
