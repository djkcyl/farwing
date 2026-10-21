//! Read-only pointer shape sampling, independent of desktop pixel capture.
use crate::i18n::t;
use anyhow::{Result, ensure};
use windows::Win32::{Graphics::Gdi::*, UI::WindowsAndMessaging::*};

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) struct Image {
    pub width: u32,
    pub height: u32,
    pub hotspot: [u32; 2],
    pub kind: i32,
    pub png: String,
}
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) struct Snapshot {
    pub showing: bool,
    pub x: i32,
    pub y: i32,
    pub image: Option<Image>,
}
#[derive(Default)]
pub(crate) struct Sampler {
    handle: usize,
    image: Option<Image>,
    sampled: Option<std::time::Instant>,
}
impl Sampler {
    pub fn sample(&mut self) -> Result<Snapshot> {
        use base64::Engine;
        use image::ImageEncoder;
        let pointer = pointer()?;
        if pointer.showing
            && (self.handle != pointer.handle
                || self.image.is_none()
                || self
                    .sampled
                    .is_none_or(|at| at.elapsed() >= std::time::Duration::from_millis(250)))
        {
            let shape = shape(pointer.handle)?;
            let mut png = Vec::new();
            image::codecs::png::PngEncoder::new(&mut png).write_image(
                &shape.rgba,
                shape.width,
                shape.height,
                image::ExtendedColorType::Rgba8,
            )?;
            ensure!(
                png.len() <= 4 * 1024 * 1024,
                "{}",
                t().system.cursor_image_too_large
            );
            self.image = Some(Image {
                width: shape.width,
                height: shape.height,
                hotspot: shape.hotspot,
                kind: shape.kind,
                png: base64::engine::general_purpose::STANDARD.encode(png),
            });
            self.handle = pointer.handle;
            self.sampled = Some(std::time::Instant::now());
        }
        // Touch and pen suppress the cursor; report the last shape so a
        // viewer does not take it for a hidden (game) cursor.
        let showing = pointer.showing || (pointer.suppressed && self.image.is_some());
        Ok(Snapshot {
            showing,
            x: pointer.x,
            y: pointer.y,
            image: if showing {
                self.image.clone()
            } else {
                None
            },
        })
    }
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct Pointer {
    pub handle: usize,
    pub showing: bool,
    /// Hidden by touch or pen input (`CURSOR_SUPPRESSED`), not by a program.
    pub suppressed: bool,
    pub x: i32,
    pub y: i32,
}
pub(crate) fn pointer() -> Result<Pointer> {
    let mut info = CURSORINFO {
        cbSize: std::mem::size_of::<CURSORINFO>() as u32,
        ..Default::default()
    };
    unsafe { GetCursorInfo(&mut info) }?;
    Ok(Pointer {
        handle: info.hCursor.0 as usize,
        showing: info.flags == CURSOR_SHOWING,
        suppressed: info.flags.0 & CURSOR_SUPPRESSED.0 != 0,
        x: info.ptScreenPos.x,
        y: info.ptScreenPos.y,
    })
}
pub(crate) struct Shape {
    pub width: u32,
    pub height: u32,
    pub hotspot: [u32; 2],
    pub rgba: Vec<u8>,
    pub kind: i32,
}
struct Icon(HICON);
impl Drop for Icon {
    fn drop(&mut self) {
        let _ = unsafe { DestroyIcon(self.0) };
    }
}
struct Info(ICONINFO);
impl Drop for Info {
    fn drop(&mut self) {
        unsafe {
            let _ = DeleteObject(self.0.hbmColor.into());
            let _ = DeleteObject(self.0.hbmMask.into());
        }
    }
}
struct Dib {
    dc: HDC,
    bitmap: HBITMAP,
    old: HGDIOBJ,
    bits: *mut u8,
    len: usize,
}
impl Drop for Dib {
    fn drop(&mut self) {
        unsafe {
            SelectObject(self.dc, self.old);
            let _ = DeleteObject(self.bitmap.into());
            let _ = DeleteDC(self.dc);
        }
    }
}
impl Dib {
    fn draw(icon: HICON, width: u32, height: u32, background: u8) -> Result<Vec<u8>> {
        let dc = unsafe { CreateCompatibleDC(None) };
        ensure!(!dc.0.is_null(), "{}", t().system.cursor_dc_failed);
        let mut dib = Self {
            dc,
            bitmap: HBITMAP::default(),
            old: HGDIOBJ::default(),
            bits: std::ptr::null_mut(),
            len: width as usize * height as usize * 4,
        };
        let info = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: width as i32,
                biHeight: -(height as i32),
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            ..Default::default()
        };
        unsafe {
            let mut bits = std::ptr::null_mut();
            dib.bitmap = CreateDIBSection(Some(dc), &info, DIB_RGB_COLORS, &mut bits, None, 0)?;
            dib.bits = bits.cast();
            ensure!(
                !dib.bits.is_null(),
                "{}",
                t().system.cursor_bitmap_memory_unavailable
            );
            dib.old = SelectObject(dc, dib.bitmap.into());
            ensure!(
                !dib.old.0.is_null() && dib.old.0 as isize != -1,
                "{}",
                t().system.cursor_select_bitmap_failed
            );
            let pixels = std::slice::from_raw_parts_mut(dib.bits, dib.len);
            for pixel in pixels.chunks_exact_mut(4) {
                pixel.copy_from_slice(&[background, background, background, 255]);
            }
            DrawIconEx(
                dc,
                0,
                0,
                icon,
                width as i32,
                height as i32,
                0,
                None,
                DI_NORMAL,
            )?;
            ensure!(
                GdiFlush().as_bool(),
                "{}",
                t().system.cursor_draw_incomplete
            );
            Ok(std::slice::from_raw_parts(dib.bits, dib.len).to_vec())
        }
    }
}
pub(crate) fn shape(handle: usize) -> Result<Shape> {
    let original = HICON(handle as *mut _);
    let icon = Icon(unsafe { CopyIcon(original) }?);
    let mut info = Info(ICONINFO::default());
    unsafe { GetIconInfo(icon.0, &mut info.0) }?;
    let bitmap = if info.0.hbmColor.0.is_null() {
        info.0.hbmMask
    } else {
        info.0.hbmColor
    };
    let mut size = BITMAP::default();
    ensure!(
        unsafe {
            GetObjectW(
                bitmap.into(),
                std::mem::size_of::<BITMAP>() as i32,
                Some((&mut size as *mut BITMAP).cast()),
            )
        } > 0,
        "{}",
        t().system.cursor_size_unreadable
    );
    let height = if info.0.hbmColor.0.is_null() {
        size.bmHeight / 2
    } else {
        size.bmHeight
    };
    ensure!(
        (1..=2048).contains(&size.bmWidth) && (1..=2048).contains(&height),
        "{}",
        t().system.cursor_invalid_size
    );
    let (width, height) = (size.bmWidth as u32, height as u32);
    ensure!(
        info.0.xHotspot < width && info.0.yHotspot < height,
        "{}",
        t().system.cursor_invalid_hotspot
    );
    let black = Dib::draw(icon.0, width, height, 0)?;
    let white = Dib::draw(icon.0, width, height, 255)?;
    let mut rgba = vec![0u8; black.len()];
    let mut inverted = false;
    for ((black, white), out) in black
        .chunks_exact(4)
        .zip(white.chunks_exact(4))
        .zip(rgba.chunks_exact_mut(4))
    {
        let differences = [0, 1, 2].map(|i| i32::from(white[i]) - i32::from(black[i]));
        if differences.iter().all(|d| *d <= -192) {
            out.fill(255);
            inverted = true;
            continue;
        }
        let alpha = (255 - differences.into_iter().max().unwrap_or(255)).clamp(0, 255);
        if alpha > 0 {
            for (i, source) in [2, 1, 0].into_iter().enumerate() {
                out[i] = ((alpha / 2 + 255 * i32::from(black[source])) / alpha).clamp(0, 255) as u8;
            }
            out[3] = alpha as u8;
        }
    }
    // Inversion cannot be represented by PNG alpha. As in the native sender,
    // keep a white foreground with a black four-neighbour outline.
    if inverted {
        let source = rgba.clone();
        for y in 0..height as usize {
            for x in 0..width as usize {
                let at = (y * width as usize + x) * 4;
                if source[at + 3] != 0 {
                    continue;
                }
                let near = [
                    x.checked_sub(1).map(|x| (x, y)),
                    (x + 1 < width as usize).then_some((x + 1, y)),
                    y.checked_sub(1).map(|y| (x, y)),
                    (y + 1 < height as usize).then_some((x, y + 1)),
                ];
                if near.into_iter().flatten().any(|(x, y)| {
                    let p = (y * width as usize + x) * 4;
                    source[p..p + 4] == [255, 255, 255, 255]
                }) {
                    rgba[at..at + 4].copy_from_slice(&[0, 0, 0, 255]);
                }
            }
        }
    }
    let mut kind = 0;
    for id in [
        IDC_ARROW,
        IDC_IBEAM,
        IDC_WAIT,
        IDC_CROSS,
        IDC_UPARROW,
        IDC_SIZE,
        IDC_ICON,
        IDC_SIZENWSE,
        IDC_SIZENESW,
        IDC_SIZEWE,
        IDC_SIZENS,
        IDC_SIZEALL,
        IDC_NO,
        IDC_HAND,
        IDC_APPSTARTING,
        IDC_HELP,
    ] {
        if unsafe { LoadCursorW(None, id) }.is_ok_and(|c| c.0 as usize == handle) {
            kind = id.0 as usize as i32;
            break;
        }
    }
    Ok(Shape {
        width,
        height,
        hotspot: [info.0.xHotspot, info.0.yHotspot],
        rgba,
        kind,
    })
}
