//! Shell drag-image metadata and local identity. No file contents or clipboard writes.
pub(crate) use crate::protocol::drag_drop::DragImage;
use anyhow::{Context, Result, ensure};
use crate::i18n::t;
use std::{
    mem::ManuallyDrop,
    os::windows::ffi::OsStrExt,
    path::{Path, PathBuf},
};
use windows::{
    Win32::{
        Foundation::*,
        Graphics::Gdi::*,
        System::{Com::*, DataExchange::RegisterClipboardFormatW, Memory::*, Ole::*},
        UI::Shell::*,
    },
    core::PCWSTR,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Identity {
    pub token: u64,
    pub drag: u64,
}
fn wide(path: &Path) -> Vec<u16> {
    path.as_os_str()
        .encode_wide()
        .map(|c| if c == b'/' as u16 { b'\\' as u16 } else { c })
        .chain(Some(0))
        .collect()
}
fn marker() -> u16 {
    unsafe { RegisterClipboardFormatW(windows::core::w!("Farwing.NativeDragIdentity.1")) as u16 }
}
fn format(id: u16) -> FORMATETC {
    FORMATETC {
        cfFormat: id,
        dwAspect: DVASPECT_CONTENT.0,
        lindex: -1,
        tymed: TYMED_HGLOBAL.0 as u32,
        ..Default::default()
    }
}
pub fn identity(data: &IDataObject) -> Option<Identity> {
    unsafe {
        let mut medium = data.GetData(&format(marker())).ok()?;
        let result = (|| {
            if medium.tymed != TYMED_HGLOBAL.0 as u32 || GlobalSize(medium.u.hGlobal) != 16 {
                return None;
            }
            let ptr = GlobalLock(medium.u.hGlobal);
            if ptr.is_null() {
                return None;
            }
            let bytes = std::slice::from_raw_parts(ptr.cast::<u8>(), 16);
            let value = Identity {
                token: u64::from_le_bytes(bytes[..8].try_into().ok()?),
                drag: u64::from_le_bytes(bytes[8..].try_into().ok()?),
            };
            let _ = GlobalUnlock(medium.u.hGlobal);
            (value.token != 0 && value.drag != 0).then_some(value)
        })();
        ReleaseStgMedium(&mut medium);
        result
    }
}
pub fn identify(data: &IDataObject, identity: Identity) -> Result<()> {
    unsafe {
        let memory = GlobalAlloc(GMEM_MOVEABLE, 16)?;
        let ptr = GlobalLock(memory);
        if ptr.is_null() {
            let _ = GlobalFree(Some(memory));
            anyhow::bail!("{}", t().system.drag_identity_memory_unavailable);
        }
        let bytes = std::slice::from_raw_parts_mut(ptr.cast::<u8>(), 16);
        bytes[..8].copy_from_slice(&identity.token.to_le_bytes());
        bytes[8..].copy_from_slice(&identity.drag.to_le_bytes());
        let _ = GlobalUnlock(memory);
        let mut medium = STGMEDIUM {
            tymed: TYMED_HGLOBAL.0 as u32,
            u: STGMEDIUM_0 { hGlobal: memory },
            pUnkForRelease: ManuallyDrop::new(None),
        };
        let result = data.SetData(&format(marker()), &medium, false);
        ReleaseStgMedium(&mut medium);
        result?;
    }
    Ok(())
}
struct Bitmap(HBITMAP);
impl Drop for Bitmap {
    fn drop(&mut self) {
        unsafe {
            let _ = DeleteObject(self.0.into());
        }
    }
}
struct Dc(HDC);
impl Drop for Dc {
    fn drop(&mut self) {
        unsafe {
            let _ = DeleteDC(self.0);
        }
    }
}
fn info(width: u32, height: u32) -> BITMAPINFO {
    BITMAPINFO {
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
    }
}
/// Called off the network/UI thread, only for the user's actual file selection.
pub fn selection(paths: &[PathBuf]) -> Result<DragImage> {
    ensure!(!paths.is_empty(), "文件选择为空");
    unsafe {
        OleInitialize(None)?;
    }
    struct Ole;
    impl Drop for Ole {
        fn drop(&mut self) {
            unsafe {
                OleUninitialize();
            }
        }
    }
    let _ole = Ole;
    let first = icon(&paths[0])?;
    if paths.len() == 1 {
        return Ok(first);
    }
    let mut icons = vec![first];
    for path in paths.iter().skip(1).take(2) {
        if let Ok(icon) = icon(path) {
            icons.push(icon);
        }
    }
    stack(&icons, paths.len())
}
fn icon(path: &Path) -> Result<DragImage> {
    let wide = wide(path);
    unsafe {
        let item: IShellItemImageFactory = SHCreateItemFromParsingName(PCWSTR(wide.as_ptr()), None)
            .context("获取选中文件的Shell图标接口")?;
        let bitmap = Bitmap(item.GetImage(
            SIZE { cx: 64, cy: 64 },
            SIIGBF_ICONONLY | SIIGBF_BIGGERSIZEOK,
        )?);
        let mut object = BITMAP::default();
        ensure!(
            GetObjectW(
                bitmap.0.into(),
                std::mem::size_of::<BITMAP>() as i32,
                Some((&mut object as *mut BITMAP).cast())
            ) != 0,
            "读取文件图标失败"
        );
        let (width, height) = (object.bmWidth as u32, object.bmHeight.unsigned_abs());
        ensure!(
            (1..=crate::protocol::drag_drop::MAX_IMAGE).contains(&width)
                && (1..=crate::protocol::drag_drop::MAX_IMAGE).contains(&height),
            "文件图标尺寸超限"
        );
        let dc = Dc(CreateCompatibleDC(None));
        ensure!(!dc.0.is_invalid(), "图标DC不可用");
        let mut pixels = vec![0u8; width as usize * height as usize * 4];
        ensure!(
            GetDIBits(
                dc.0,
                bitmap.0,
                0,
                height,
                Some(pixels.as_mut_ptr().cast()),
                &mut info(width, height),
                DIB_RGB_COLORS
            ) == height as i32,
            "读取图标像素失败"
        );
        // Shell image factories return premultiplied pixels; InitializeFromBitmap
        // requires straight alpha and performs its own multiplication.
        for p in pixels.chunks_exact_mut(4) {
            if p[3] > 0 && p[3] < 255 {
                for c in 0..3 {
                    p[c] = (u32::from(p[c]) * 255 / u32::from(p[3])).min(255) as u8;
                }
            }
        }
        Ok(DragImage {
            width,
            height,
            pixels,
        })
    }
}
fn stack(icons: &[DragImage], count: usize) -> Result<DragImage> {
    const SIZE: u32 = 80;
    let mut pixels = vec![0u8; SIZE as usize * SIZE as usize * 4];
    // Work in premultiplied alpha while resizing/compositing so transparent
    // icon edges do not gain dark halos. Convert once for the Shell helper.
    for (layer, icon) in icons.iter().enumerate().rev() {
        let mut source = icon.pixels.clone();
        for p in source.chunks_exact_mut(4) {
            for c in 0..3 {
                p[c] = (u32::from(p[c]) * u32::from(p[3]) / 255) as u8;
            }
        }
        let source = image::RgbaImage::from_raw(icon.width, icon.height, source)
            .ok_or_else(|| anyhow::anyhow!("多选图标无效"))?;
        let source =
            image::imageops::resize(&source, 64, 64, image::imageops::FilterType::Triangle);
        for y in 0..64usize {
            for x in 0..64usize {
                let p = source.get_pixel(x as u32, y as u32).0;
                let dx = x + layer * 7;
                let dy = y + layer * 5;
                let at = (dy * SIZE as usize + dx) * 4;
                for c in 0..4 {
                    pixels[at + c] = (u32::from(p[c])
                        + u32::from(pixels[at + c]) * (255 - u32::from(p[3])) / 255)
                        .min(255) as u8;
                }
            }
        }
    }
    let label = count.to_string();
    let width = (label.len() as i32 * 8 + 14).max(22);
    let left = SIZE as i32 - width - 2;
    let top = SIZE as i32 - 24;
    let right = SIZE as i32 - 2;
    let bottom = SIZE as i32 - 2;
    let inside = |x: i32, y: i32| {
        let cx = x.clamp(left + 4, right - 5);
        let cy = y.clamp(top + 4, bottom - 5);
        (x - cx) * (x - cx) + (y - cy) * (y - cy) <= 16
    };
    let color = unsafe { GetSysColor(COLOR_HIGHLIGHT) };
    for y in top..bottom {
        for x in left..right {
            if inside(x, y) {
                let p = &mut pixels[(y as usize * SIZE as usize + x as usize) * 4..][..4];
                p.copy_from_slice(&[
                    ((color >> 16) & 255) as u8,
                    ((color >> 8) & 255) as u8,
                    (color & 255) as u8,
                    255,
                ]);
            }
        }
    }
    unsafe {
        let mut bits = std::ptr::null_mut();
        let bitmap = Bitmap(CreateDIBSection(
            None,
            &info(SIZE, SIZE),
            DIB_RGB_COLORS,
            &mut bits,
            None,
            0,
        )?);
        ensure!(!bits.is_null(), "多选位图不可用");
        std::ptr::copy_nonoverlapping(pixels.as_ptr(), bits.cast(), pixels.len());
        let dc = Dc(CreateCompatibleDC(None));
        ensure!(!dc.0.is_invalid(), "多选图标DC不可用");
        let old = SelectObject(dc.0, bitmap.0.into());
        let font = SelectObject(dc.0, GetStockObject(DEFAULT_GUI_FONT));
        SetBkMode(dc.0, TRANSPARENT);
        SetTextColor(dc.0, COLORREF(GetSysColor(COLOR_HIGHLIGHTTEXT)));
        let mut text: Vec<u16> = label.encode_utf16().collect();
        let mut rect = RECT {
            left: left + 3,
            top,
            right: right - 3,
            bottom,
        };
        let drawn = DrawTextW(
            dc.0,
            &mut text,
            &mut rect,
            DT_CENTER | DT_VCENTER | DT_SINGLELINE | DT_NOPREFIX,
        );
        SelectObject(dc.0, font);
        SelectObject(dc.0, old);
        ensure!(drawn != 0, "绘制多选数量失败");
        std::ptr::copy_nonoverlapping(bits.cast::<u8>(), pixels.as_mut_ptr(), pixels.len());
    }
    for y in top..bottom {
        for x in left..right {
            if inside(x, y) {
                pixels[(y as usize * SIZE as usize + x as usize) * 4 + 3] = 255;
            }
        }
    }
    for p in pixels.chunks_exact_mut(4) {
        if p[3] > 0 && p[3] < 255 {
            for c in 0..3 {
                p[c] = (u32::from(p[c]) * 255 / u32::from(p[3])).min(255) as u8;
            }
        }
    }
    Ok(DragImage {
        width: SIZE,
        height: SIZE,
        pixels,
    })
}
pub fn decorate(data: &IDataObject, image: &DragImage) -> Result<()> {
    ensure!(image.valid(), "拖放图像无效");
    if image.pixels.is_empty() {
        return Ok(());
    }
    unsafe {
        let mut bits = std::ptr::null_mut();
        let bitmap = Bitmap(CreateDIBSection(
            None,
            &info(image.width, image.height),
            DIB_RGB_COLORS,
            &mut bits,
            None,
            0,
        )?);
        ensure!(!bits.is_null(), "拖放位图不可用");
        std::ptr::copy_nonoverlapping(image.pixels.as_ptr(), bits.cast(), image.pixels.len());
        let helper: IDragSourceHelper2 =
            CoCreateInstance(&CLSID_DragDropHelper, None, CLSCTX_INPROC_SERVER)?;
        helper.SetFlags(DSH_ALLOWDROPDESCRIPTIONTEXT.0 as u32)?;
        helper.InitializeFromBitmap(
            &SHDRAGIMAGE {
                sizeDragImage: SIZE {
                    cx: image.width as i32,
                    cy: image.height as i32,
                },
                ptOffset: POINT { x: 8, y: 8 },
                hbmpDragImage: bitmap.0,
                crColorKey: COLORREF(0xffff_ffff),
            },
            data,
        )?;
        // Successful initialization consumes the HBITMAP. The actual Shell
        // regression verifies it is already retired before helper Release.
        std::mem::forget(bitmap);
    }
    Ok(())
}
pub fn description(data: &IDataObject, effect: u32, text: Option<&str>) -> Result<()> {
    let mut message = [0u16; 260];
    if let Some(text) = text {
        for (out, value) in message.iter_mut().take(259).zip(text.encode_utf16()) {
            *out = value;
        }
    }
    let value = DROPDESCRIPTION {
        r#type: if text.is_none() {
            DROPIMAGE_INVALID
        } else if effect & 1 != 0 {
            DROPIMAGE_COPY
        } else {
            DROPIMAGE_NONE
        },
        szMessage: message,
        szInsert: [0; 260],
    };
    unsafe {
        let size = std::mem::size_of::<DROPDESCRIPTION>();
        let memory = GlobalAlloc(GMEM_MOVEABLE, size)?;
        let pointer = GlobalLock(memory);
        if pointer.is_null() {
            let _ = GlobalFree(Some(memory));
            anyhow::bail!("拖放提示内存不可用");
        }
        std::ptr::copy_nonoverlapping(
            (&value as *const DROPDESCRIPTION).cast::<u8>(),
            pointer.cast::<u8>(),
            size,
        );
        let _ = GlobalUnlock(memory);
        let mut medium = STGMEDIUM {
            tymed: TYMED_HGLOBAL.0 as u32,
            u: STGMEDIUM_0 { hGlobal: memory },
            pUnkForRelease: ManuallyDrop::new(None),
        };
        let result = data.SetData(
            &format(RegisterClipboardFormatW(CFSTR_DROPDESCRIPTION) as u16),
            &medium,
            false,
        );
        ReleaseStgMedium(&mut medium);
        result?;
    }
    Ok(())
}
