//! Participant pointers on the host's own screens (`docs/protocol/v2-design.md`
//! §4.3): one small click-through layered window per pointer or click ring.
//! Every window is excluded from capture; remote participants draw each
//! other's pointers themselves. All handles belong to the drawing thread.
use anyhow::{Context, Result, ensure};
use std::{
    collections::{BTreeMap, btree_map::Entry},
    ptr::null_mut,
};
use windows::{
    Win32::{
        Foundation::*,
        Graphics::{Gdi::*, GdiPlus::*},
        System::LibraryLoader::GetModuleHandleW,
        UI::{HiDpi::*, WindowsAndMessaging::*},
    },
    core::{PCWSTR, w},
};

thread_local! {
    static DISPLAY_REVISION: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

const CLASS: PCWSTR = w!("Farwing.Participant.Overlay");
/// 32bpp premultiplied ARGB, as `UpdateLayeredWindow` expects.
const PARGB: i32 = 0x000e200b;

// Geometry in DIPs, scaled by the screen's DPI.
/// Arrow glyph with its tip at the origin.
const ARROW: [(f32, f32); 7] = [
    (0.0, 0.0),
    (0.0, 16.5),
    (4.2, 12.6),
    (6.9, 18.6),
    (9.6, 17.4),
    (7.0, 11.6),
    (12.0, 11.6),
];
const ARROW_SIZE: (f32, f32) = (12.0, 18.6);
/// White outline around the arrow and the label.
const OUTLINE: f32 = 1.5;
/// Room for the outline and the drop shadow around the content.
const MARGIN: f32 = 3.0;
const SHADOW: u32 = 0x4D00_0000;
const FONT: f32 = 12.0;
const PADDING: (f32, f32) = (7.0, 3.0);
const RADIUS: f32 = 6.0;
const MAX_LABEL: f32 = 200.0;
/// Label offset from the arrow tip, and from the real cursor's hotspot
/// (clear of the system arrow).
const BESIDE_ARROW: (f32, f32) = (11.0, 15.0);
const BESIDE_CURSOR: (f32, f32) = (14.0, 20.0);
/// Gap when the label flips to the left of or above the pointer.
const FLIPPED_GAP: f32 = 4.0;
/// Click ring radius from the click to its end, and its stroke.
const RING_RADIUS: (f32, f32) = (4.0, 20.0);
const RING_STROKE: f32 = 3.0;

fn check(status: Status) -> Result<()> {
    ensure!(status == Ok, "参与者指针绘制失败（GDI+ {}）", status.0);
    Result::Ok(())
}

fn argb(alpha: u8, rgb: u32) -> u32 {
    (u32::from(alpha) << 24) | (rgb & 0x00FF_FFFF)
}

/// Window-relative geometry of a pointer window, in pixels.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Layout {
    pub size: (i32, i32),
    /// The window pixel placed on the pointer.
    pub hotspot: (i32, i32),
    /// The arrow tip, when the arrow is drawn.
    pub arrow: Option<(f32, f32)>,
    /// The label: x, y, width, height.
    pub label: [f32; 4],
}

/// Lay out a pointer window. `text` is the measured label text in pixels;
/// `flip` moves the label left of and/or above the pointer.
pub(crate) fn layout(text: (f32, f32), scale: f32, arrow: bool, flip: (bool, bool)) -> Layout {
    let s = scale.clamp(0.5, 4.0);
    let width = (text.0 + 2.0 * PADDING.0 * s).min(MAX_LABEL * s).ceil();
    let height = (text.1 + 2.0 * PADDING.1 * s).ceil();
    let beside = if arrow { BESIDE_ARROW } else { BESIDE_CURSOR };
    // Relative to the pointer.
    let x = if flip.0 {
        -width - FLIPPED_GAP * s
    } else {
        beside.0 * s
    };
    let y = if flip.1 {
        -height - FLIPPED_GAP * s
    } else {
        beside.1 * s
    };
    let (mut right, mut bottom) = ((x + width).max(0.0), (y + height).max(0.0));
    if arrow {
        right = right.max(ARROW_SIZE.0 * s);
        bottom = bottom.max(ARROW_SIZE.1 * s);
    }
    let margin = MARGIN * s;
    let left = (x.min(0.0) - margin).floor();
    let top = (y.min(0.0) - margin).floor();
    let hotspot = (-left as i32, -top as i32);
    Layout {
        size: (
            (right + margin - left).ceil() as i32,
            (bottom + margin - top).ceil() as i32,
        ),
        hotspot,
        arrow: arrow.then_some((hotspot.0 as f32, hotspot.1 as f32)),
        label: [x - left, y - top, width, height],
    }
}

/// Keep the label on the pointer's screen (`left, top, right, bottom`):
/// flip it left near the right edge and up near the bottom edge. `plain` is
/// the unflipped layout.
pub(crate) fn flips(at: (i32, i32), bounds: [i32; 4], plain: &Layout) -> (bool, bool) {
    (
        at.0 - plain.hotspot.0 + plain.size.0 > bounds[2],
        at.1 - plain.hotspot.1 + plain.size.1 > bounds[3],
    )
}

/// Ring radius, stroke and opacity at `progress`: 0 at the click, 1 when gone.
pub(crate) fn ring(progress: f32, scale: f32) -> (f32, f32, f32) {
    let t = if progress.is_finite() {
        progress.clamp(0.0, 1.0)
    } else {
        1.0
    };
    let eased = 1.0 - (1.0 - t).powi(3);
    let s = scale.clamp(0.5, 4.0);
    (
        (RING_RADIUS.0 + (RING_RADIUS.1 - RING_RADIUS.0) * eased) * s,
        RING_STROKE * s,
        1.0 - t,
    )
}

/// Side of a click ring window, in pixels.
pub(crate) fn ring_size(scale: f32) -> i32 {
    // The white halo is two DIPs wider than the coloured stroke.
    ((RING_RADIUS.1 + RING_STROKE / 2.0 + 1.0 + MARGIN) * 2.0 * scale.clamp(0.5, 4.0)).ceil()
        as i32
}

fn alpha(opacity: f32) -> u8 {
    if opacity.is_finite() {
        (opacity.clamp(0.0, 1.0) * 255.0).round() as u8
    } else {
        0
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum Key {
    /// A participant's pointer.
    Pointer(u64),
    /// A participant's click, by its click counter.
    Click(u64, u32),
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Shape {
    /// The arrow (unless the pointer is the real cursor) and a name label.
    Pointer {
        label: String,
        /// 0xRRGGBB.
        color: u32,
        arrow: bool,
    },
    /// An expanding ring; `progress` runs from 0 at the click to 1.
    Click { color: u32, progress: f32 },
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Sprite {
    pub key: Key,
    pub shape: Shape,
    /// The pointer tip or click centre, in desktop pixels.
    pub at: (i32, i32),
    /// The screen it is on: left, top, right, bottom.
    pub bounds: [i32; 4],
    /// The screen's DPI scale, 1.0 at 100 %.
    pub scale: f32,
    /// 0..=1.
    pub opacity: f32,
}

struct Runtime {
    token: usize,
    dpi: DPI_AWARENESS_CONTEXT,
}
impl Runtime {
    fn new() -> Result<Self> {
        let mut token = 0;
        let input = GdiplusStartupInput {
            GdiplusVersion: 1,
            ..Default::default()
        };
        unsafe {
            check(GdiplusStartup(&mut token, &input, null_mut()))?;
        }
        // Desktop pixels, like the capture's screen rectangles.
        let dpi =
            unsafe { SetThreadDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) };
        Result::Ok(Self { token, dpi })
    }
}
impl Drop for Runtime {
    fn drop(&mut self) {
        unsafe {
            GdiplusShutdown(self.token);
            if !self.dpi.0.is_null() {
                SetThreadDpiAwarenessContext(self.dpi);
            }
        }
    }
}

pub(crate) struct Overlay {
    surfaces: BTreeMap<Key, Surface>,
    text: Text,
    foreground: HWND,
    display_revision: u64,
    _runtime: Runtime,
}
impl Overlay {
    pub fn new() -> Result<Self> {
        ensure!(
            capture_exclusion(),
            "当前系统版本无法从远程画面中排除参与者指针"
        );
        let runtime = Runtime::new()?;
        static CLASS_REGISTERED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        ensure!(
            *CLASS_REGISTERED.get_or_init(|| unsafe {
                let class = WNDCLASSW {
                    lpfnWndProc: Some(window_proc),
                    hInstance: GetModuleHandleW(None).unwrap_or_default().into(),
                    lpszClassName: CLASS,
                    ..Default::default()
                };
                RegisterClassW(&class) != 0
            }),
            "无法注册参与者指针窗口"
        );
        Result::Ok(Self {
            surfaces: BTreeMap::new(),
            text: Text::new()?,
            foreground: HWND::default(),
            display_revision: DISPLAY_REVISION.get(),
            _runtime: runtime,
        })
    }

    /// Handle this thread's window messages and stay above other topmost
    /// windows. True once the displays changed since the last call.
    pub fn maintain(&mut self) -> Result<bool> {
        unsafe {
            let mut message = MSG::default();
            for _ in 0..64 {
                if !PeekMessageW(&mut message, None, 0, 0, PM_REMOVE).as_bool() {
                    break;
                }
                let _ = TranslateMessage(&message);
                DispatchMessageW(&message);
            }
        }
        let foreground = unsafe { GetForegroundWindow() };
        if foreground != self.foreground {
            self.foreground = foreground;
            for surface in self.surfaces.values().filter(|s| s.shown) {
                raise(surface.hwnd)?;
            }
        }
        let revision = DISPLAY_REVISION.get();
        let changed = revision != self.display_revision;
        self.display_revision = revision;
        Result::Ok(changed)
    }

    /// Show exactly these sprites; other windows are destroyed. A window
    /// only moves when nothing but its position changed.
    pub fn show(&mut self, sprites: &[Sprite]) -> Result<()> {
        let mut shown = Vec::with_capacity(sprites.len());
        for sprite in sprites {
            let scale = if sprite.scale.is_finite() {
                sprite.scale.clamp(0.5, 4.0)
            } else {
                1.0
            };
            let opacity = match sprite.shape {
                Shape::Click { progress, .. } => sprite.opacity * ring(progress, scale).2,
                Shape::Pointer { .. } => sprite.opacity,
            };
            let alpha = alpha(opacity);
            if alpha == 0 {
                continue;
            }
            let surface = match self.surfaces.entry(sprite.key) {
                Entry::Occupied(entry) => entry.into_mut(),
                Entry::Vacant(entry) => entry.insert(Surface::new(sprite.at)?),
            };
            match sprite.shape {
                Shape::Pointer { .. } => surface.pointer(&mut self.text, sprite, scale, alpha)?,
                Shape::Click { color, progress } => {
                    surface.click(sprite.at, color, progress, scale, alpha)?
                }
            }
            shown.push(sprite.key);
        }
        self.surfaces.retain(|key, _| shown.contains(key));
        Result::Ok(())
    }
}

unsafe extern "system" fn window_proc(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match message {
        WM_DISPLAYCHANGE | WM_DPICHANGED => {
            DISPLAY_REVISION.set(DISPLAY_REVISION.get().wrapping_add(1));
            LRESULT(0)
        }
        WM_PAINT => {
            let mut paint = PAINTSTRUCT::default();
            unsafe {
                BeginPaint(hwnd, &mut paint);
                let _ = EndPaint(hwnd, &paint);
            }
            LRESULT(0)
        }
        WM_NCHITTEST => LRESULT(HTTRANSPARENT as isize),
        WM_MOUSEACTIVATE => LRESULT(MA_NOACTIVATE as isize),
        WM_ERASEBKGND => LRESULT(1),
        _ => unsafe { DefWindowProcW(hwnd, message, wparam, lparam) },
    }
}

fn raise(hwnd: HWND) -> Result<()> {
    unsafe {
        SetWindowPos(
            hwnd,
            Some(HWND_TOPMOST),
            0,
            0,
            0,
            0,
            SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE | SWP_SHOWWINDOW,
        )?;
    }
    Result::Ok(())
}

/// What a pointer window's bitmap holds.
#[derive(PartialEq)]
struct Drawn {
    label: String,
    color: u32,
    arrow: bool,
    scale: u32,
    flip: (bool, bool),
}

struct Surface {
    hwnd: HWND,
    dc: HDC,
    bitmap: HBITMAP,
    old: HGDIOBJ,
    image: *mut GpBitmap,
    graphics: *mut GpGraphics,
    size: (i32, i32),
    drawn: Option<Drawn>,
    /// Label, scale key and measured text size.
    measured: Option<(String, u32, (f32, f32))>,
    origin: (i32, i32),
    alpha: u8,
    shown: bool,
}
impl Surface {
    fn new(at: (i32, i32)) -> Result<Self> {
        let mut surface = Self {
            hwnd: HWND::default(),
            dc: HDC::default(),
            bitmap: HBITMAP::default(),
            old: HGDIOBJ::default(),
            image: null_mut(),
            graphics: null_mut(),
            size: (0, 0),
            drawn: None,
            measured: None,
            origin: at,
            alpha: 0,
            shown: false,
        };
        unsafe {
            surface.hwnd = CreateWindowExW(
                WS_EX_LAYERED
                    | WS_EX_TRANSPARENT
                    | WS_EX_NOACTIVATE
                    | WS_EX_TOOLWINDOW
                    | WS_EX_TOPMOST,
                CLASS,
                w!("Farwing 参与者指针"),
                WS_POPUP,
                at.0,
                at.1,
                1,
                1,
                None,
                None,
                Some(GetModuleHandleW(None)?.into()),
                None,
            )?;
            // Before the window is ever shown: the host's own video must not
            // carry it, or participants would see these pointers twice.
            SetWindowDisplayAffinity(surface.hwnd, WDA_EXCLUDEFROMCAPTURE)
                .context("无法从远程画面中排除参与者指针")?;
            surface.dc = CreateCompatibleDC(None);
            ensure!(!surface.dc.0.is_null(), "创建参与者指针DC失败");
        }
        Result::Ok(surface)
    }

    fn resize(&mut self, size: (i32, i32)) -> Result<()> {
        ensure!(
            (1..=2048).contains(&size.0) && (1..=2048).contains(&size.1),
            "参与者指针尺寸无效"
        );
        if size == self.size && !self.graphics.is_null() {
            return Result::Ok(());
        }
        self.release_bitmap();
        self.drawn = None;
        unsafe {
            let info = BITMAPINFO {
                bmiHeader: BITMAPINFOHEADER {
                    biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                    biWidth: size.0,
                    biHeight: -size.1,
                    biPlanes: 1,
                    biBitCount: 32,
                    biCompression: BI_RGB.0,
                    ..Default::default()
                },
                ..Default::default()
            };
            let mut pixels = null_mut();
            self.bitmap =
                CreateDIBSection(Some(self.dc), &info, DIB_RGB_COLORS, &mut pixels, None, 0)?;
            ensure!(!pixels.is_null(), "创建参与者指针像素失败");
            self.old = SelectObject(self.dc, self.bitmap.into());
            ensure!(
                !self.old.0.is_null() && self.old.0 as isize != -1,
                "选择参与者指针表面失败"
            );
            check(GdipCreateBitmapFromScan0(
                size.0,
                size.1,
                size.0 * 4,
                PARGB,
                Some(pixels.cast()),
                &mut self.image,
            ))?;
            check(GdipGetImageGraphicsContext(
                self.image.cast(),
                &mut self.graphics,
            ))?;
            check(GdipSetSmoothingMode(self.graphics, SmoothingModeAntiAlias))?;
            check(GdipSetPixelOffsetMode(self.graphics, PixelOffsetModeHalf))?;
            check(GdipSetCompositingQuality(
                self.graphics,
                CompositingQualityHighQuality,
            ))?;
            check(GdipSetTextRenderingHint(
                self.graphics,
                TextRenderingHintAntiAlias,
            ))?;
        }
        self.size = size;
        Result::Ok(())
    }

    fn release_bitmap(&mut self) {
        unsafe {
            if !self.graphics.is_null() {
                GdipDeleteGraphics(self.graphics);
                self.graphics = null_mut();
            }
            if !self.image.is_null() {
                GdipDisposeImage(self.image.cast());
                self.image = null_mut();
            }
            if !self.old.0.is_null() && self.old.0 as isize != -1 {
                SelectObject(self.dc, self.old);
                self.old = HGDIOBJ::default();
            }
            if !self.bitmap.0.is_null() {
                let _ = DeleteObject(self.bitmap.into());
                self.bitmap = HBITMAP::default();
            }
        }
        self.size = (0, 0);
    }

    fn pointer(&mut self, text: &mut Text, sprite: &Sprite, scale: f32, alpha: u8) -> Result<()> {
        let Shape::Pointer {
            label,
            color,
            arrow,
        } = &sprite.shape
        else {
            return Result::Ok(());
        };
        let (color, arrow) = (*color, *arrow);
        let scale_key = (scale * 100.0).round() as u32;
        let measured = match &self.measured {
            Some((text, key, size)) if text == label && *key == scale_key => *size,
            _ => {
                let size = text.measure(label, scale)?;
                self.measured = Some((label.to_owned(), scale_key, size));
                size
            }
        };
        let plain = layout(measured, scale, arrow, (false, false));
        let flip = flips(sprite.at, sprite.bounds, &plain);
        let layout = if flip == (false, false) {
            plain
        } else {
            layout(measured, scale, arrow, flip)
        };
        let origin = (sprite.at.0 - layout.hotspot.0, sprite.at.1 - layout.hotspot.1);
        let drawn = Drawn {
            label: label.to_owned(),
            color,
            arrow,
            scale: scale_key,
            flip,
        };
        if self.drawn.as_ref() != Some(&drawn) {
            self.resize(layout.size)?;
            draw_pointer(self.graphics, text, &layout, scale, color, label)?;
            self.drawn = Some(drawn);
            self.present(origin, alpha)
        } else if alpha != self.alpha {
            self.present(origin, alpha)
        } else if origin != self.origin {
            unsafe {
                SetWindowPos(
                    self.hwnd,
                    None,
                    origin.0,
                    origin.1,
                    0,
                    0,
                    SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE,
                )?;
            }
            self.origin = origin;
            Result::Ok(())
        } else {
            Result::Ok(())
        }
    }

    fn click(
        &mut self,
        at: (i32, i32),
        color: u32,
        progress: f32,
        scale: f32,
        alpha: u8,
    ) -> Result<()> {
        let side = ring_size(scale);
        let (radius, stroke, _) = ring(progress, scale);
        self.resize((side, side))?;
        draw_click(self.graphics, side as f32 / 2.0, radius, stroke, scale, color)?;
        self.present((at.0 - side / 2, at.1 - side / 2), alpha)
    }

    fn present(&mut self, origin: (i32, i32), alpha: u8) -> Result<()> {
        unsafe {
            check(GdipFlush(self.graphics, FlushIntentionSync))?;
            let destination = POINT {
                x: origin.0,
                y: origin.1,
            };
            let size = SIZE {
                cx: self.size.0,
                cy: self.size.1,
            };
            let blend = BLENDFUNCTION {
                BlendOp: AC_SRC_OVER as u8,
                BlendFlags: 0,
                SourceConstantAlpha: alpha,
                AlphaFormat: AC_SRC_ALPHA as u8,
            };
            UpdateLayeredWindow(
                self.hwnd,
                None,
                Some(&destination),
                Some(&size),
                Some(self.dc),
                Some(&POINT::default()),
                COLORREF(0),
                Some(&blend),
                ULW_ALPHA,
            )?;
        }
        self.origin = origin;
        self.alpha = alpha;
        if !self.shown {
            raise(self.hwnd)?;
            self.shown = true;
        }
        Result::Ok(())
    }
}
impl Drop for Surface {
    fn drop(&mut self) {
        self.release_bitmap();
        unsafe {
            if !self.dc.0.is_null() {
                let _ = DeleteDC(self.dc);
            }
            if !self.hwnd.0.is_null() {
                let _ = DestroyWindow(self.hwnd);
            }
        }
    }
}

/// Label font, format and a 1×1 surface to measure text on.
struct Text {
    family: *mut GpFontFamily,
    fonts: BTreeMap<u32, *mut GpFont>,
    format: *mut GpStringFormat,
    image: *mut GpBitmap,
    graphics: *mut GpGraphics,
}
impl Text {
    fn new() -> Result<Self> {
        let mut text = Self {
            family: null_mut(),
            fonts: BTreeMap::new(),
            format: null_mut(),
            image: null_mut(),
            graphics: null_mut(),
        };
        unsafe {
            // Nicknames are often Chinese; Segoe UI has no CJK glyphs.
            if GdipCreateFontFamilyFromName(w!("Microsoft YaHei UI"), null_mut(), &mut text.family)
                != Ok
            {
                text.family = null_mut();
                check(GdipCreateFontFamilyFromName(
                    w!("Segoe UI"),
                    null_mut(),
                    &mut text.family,
                ))?;
            }
            check(GdipCreateStringFormat(
                StringFormatFlagsNoWrap.0,
                0,
                &mut text.format,
            ))?;
            check(GdipSetStringFormatTrimming(
                text.format,
                StringTrimmingEllipsisCharacter,
            ))?;
            check(GdipSetStringFormatLineAlign(
                text.format,
                StringAlignmentCenter,
            ))?;
            check(GdipCreateBitmapFromScan0(
                1,
                1,
                4,
                PARGB,
                None,
                &mut text.image,
            ))?;
            check(GdipGetImageGraphicsContext(
                text.image.cast(),
                &mut text.graphics,
            ))?;
            check(GdipSetTextRenderingHint(
                text.graphics,
                TextRenderingHintAntiAlias,
            ))?;
        }
        Result::Ok(text)
    }

    fn font(&mut self, scale: f32) -> Result<*mut GpFont> {
        let key = (scale * 100.0).round() as u32;
        if let Some(font) = self.fonts.get(&key) {
            return Result::Ok(*font);
        }
        let mut font = null_mut();
        unsafe {
            check(GdipCreateFont(
                self.family,
                FONT * scale,
                FontStyleRegular.0,
                UnitPixel,
                &mut font,
            ))?;
        }
        self.fonts.insert(key, font);
        Result::Ok(font)
    }

    /// Size of `label` in pixels.
    fn measure(&mut self, label: &str, scale: f32) -> Result<(f32, f32)> {
        let font = self.font(scale)?;
        let text: Vec<u16> = label.encode_utf16().collect();
        let mut bounds = RectF::default();
        unsafe {
            check(GdipMeasureString(
                self.graphics,
                PCWSTR(text.as_ptr()),
                text.len() as i32,
                font,
                &RectF {
                    X: 0.0,
                    Y: 0.0,
                    Width: 4096.0,
                    Height: 1024.0,
                },
                self.format,
                &mut bounds,
                null_mut(),
                null_mut(),
            ))?;
        }
        Result::Ok((bounds.Width.max(1.0), bounds.Height.max(FONT * scale)))
    }
}
impl Drop for Text {
    fn drop(&mut self) {
        unsafe {
            for font in self.fonts.values() {
                GdipDeleteFont(*font);
            }
            if !self.format.is_null() {
                GdipDeleteStringFormat(self.format);
            }
            if !self.family.is_null() {
                GdipDeleteFontFamily(self.family);
            }
            if !self.graphics.is_null() {
                GdipDeleteGraphics(self.graphics);
            }
            if !self.image.is_null() {
                GdipDisposeImage(self.image.cast());
            }
        }
    }
}

struct Objects {
    pen: *mut GpPen,
    brush: *mut GpSolidFill,
    path: *mut GpPath,
}
impl Objects {
    fn new() -> Self {
        Self {
            pen: null_mut(),
            brush: null_mut(),
            path: null_mut(),
        }
    }
}
impl Drop for Objects {
    fn drop(&mut self) {
        unsafe {
            if !self.path.is_null() {
                GdipDeletePath(self.path);
            }
            if !self.brush.is_null() {
                GdipDeleteBrush(self.brush.cast());
            }
            if !self.pen.is_null() {
                GdipDeletePen(self.pen);
            }
        }
    }
}

fn clear(g: *mut GpGraphics) -> Result<()> {
    unsafe {
        check(GdipSetCompositingMode(g, CompositingModeSourceCopy))?;
        check(GdipGraphicsClear(g, 0))?;
        check(GdipSetCompositingMode(g, CompositingModeSourceOver))
    }
}

/// Fill `path` with a shadow offset by `offset`, then outline it in white
/// and fill it with `color`.
unsafe fn outlined(
    g: *mut GpGraphics,
    path: *mut GpPath,
    offset: f32,
    outline: f32,
    color: u32,
) -> Result<()> {
    let mut objects = Objects::new();
    unsafe {
        check(GdipCreateSolidFill(SHADOW, &mut objects.brush))?;
        check(GdipTranslateWorldTransform(g, 0.0, offset, MatrixOrderPrepend))?;
        let shadow = GdipFillPath(g, objects.brush.cast(), path);
        check(GdipResetWorldTransform(g))?;
        check(shadow)?;
        check(GdipCreatePen1(argb(255, 0xFFFFFF), outline, UnitPixel, &mut objects.pen))?;
        check(GdipSetPenLineJoin(objects.pen, LineJoinRound))?;
        check(GdipDrawPath(g, objects.pen, path))?;
        check(GdipSetSolidFillColor(objects.brush, argb(255, color)))?;
        check(GdipFillPath(g, objects.brush.cast(), path))
    }
}

fn draw_pointer(
    g: *mut GpGraphics,
    text: &mut Text,
    layout: &Layout,
    scale: f32,
    color: u32,
    label: &str,
) -> Result<()> {
    clear(g)?;
    let font = text.font(scale)?;
    let format = text.format;
    // A centred stroke twice the outline width leaves the outline outside the fill.
    let outline = 2.0 * OUTLINE * scale;
    unsafe {
        if let Some((x, y)) = layout.arrow {
            let points: Vec<_> = ARROW
                .iter()
                .map(|(px, py)| PointF {
                    X: x + px * scale,
                    Y: y + py * scale,
                })
                .collect();
            let mut objects = Objects::new();
            check(GdipCreatePath(FillModeWinding, &mut objects.path))?;
            check(GdipAddPathPolygon(
                objects.path,
                points.as_ptr(),
                points.len() as i32,
            ))?;
            outlined(g, objects.path, scale, outline, color)?;
        }
        let [x, y, width, height] = layout.label;
        let radius = (RADIUS * scale).min(height / 2.0);
        let d = radius * 2.0;
        let mut objects = Objects::new();
        check(GdipCreatePath(FillModeWinding, &mut objects.path))?;
        check(GdipAddPathArc(objects.path, x, y, d, d, 180.0, 90.0))?;
        check(GdipAddPathArc(objects.path, x + width - d, y, d, d, 270.0, 90.0))?;
        check(GdipAddPathArc(
            objects.path,
            x + width - d,
            y + height - d,
            d,
            d,
            0.0,
            90.0,
        ))?;
        check(GdipAddPathArc(objects.path, x, y + height - d, d, d, 90.0, 90.0))?;
        check(GdipClosePathFigure(objects.path))?;
        outlined(g, objects.path, scale, outline * 0.5, color)?;
        check(GdipCreateSolidFill(argb(255, 0xFFFFFF), &mut objects.brush))?;
        let text: Vec<u16> = label.encode_utf16().collect();
        check(GdipDrawString(
            g,
            PCWSTR(text.as_ptr()),
            text.len() as i32,
            font,
            &RectF {
                X: x + PADDING.0 * scale,
                Y: y,
                Width: (width - 2.0 * PADDING.0 * scale).max(1.0),
                Height: height,
            },
            format,
            objects.brush.cast(),
        ))?;
    }
    Result::Ok(())
}

fn draw_click(
    g: *mut GpGraphics,
    center: f32,
    radius: f32,
    stroke: f32,
    scale: f32,
    color: u32,
) -> Result<()> {
    clear(g)?;
    let mut objects = Objects::new();
    unsafe {
        // A white halo keeps the ring visible on its own colour.
        for (color, width) in [
            (argb(0xB0, 0xFFFFFF), stroke + 2.0 * scale),
            (argb(255, color), stroke),
        ] {
            if !objects.pen.is_null() {
                GdipDeletePen(objects.pen);
                objects.pen = null_mut();
            }
            check(GdipCreatePen1(color, width, UnitPixel, &mut objects.pen))?;
            check(GdipDrawEllipse(
                g,
                objects.pen,
                center - radius,
                center - radius,
                radius * 2.0,
                radius * 2.0,
            ))?;
        }
    }
    Result::Ok(())
}

/// The real cursor's position in desktop pixels, whatever this thread's DPI
/// awareness.
pub(crate) fn cursor() -> Option<(i32, i32)> {
    let mut point = POINT::default();
    unsafe { GetPhysicalCursorPos(&mut point) }
        .ok()
        .map(|_| (point.x, point.y))
}

/// `WDA_EXCLUDEFROMCAPTURE` needs Windows 10 2004; older systems would
/// black the windows out of the captured video instead.
pub(crate) fn capture_exclusion() -> bool {
    static SUPPORTED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *SUPPORTED.get_or_init(|| {
        super::device_profile::registry_string(
            windows::Win32::System::Registry::HKEY_LOCAL_MACHINE,
            r"SOFTWARE\Microsoft\Windows NT\CurrentVersion",
            "CurrentBuildNumber",
        )
        .ok()
        .and_then(|build| build.trim().parse::<u32>().ok())
        .is_some_and(|build| build >= 19041)
    })
}


#[cfg(test)]
mod tests {
    use super::*;

    fn contains(layout: &Layout, [x, y, w, h]: [f32; 4]) -> bool {
        x >= 0.0 && y >= 0.0 && x + w <= layout.size.0 as f32 && y + h <= layout.size.1 as f32
    }

    #[test]
    fn pointer_layout_keeps_label_beside_the_arrow() {
        let plain = layout((60.0, 16.0), 1.0, true, (false, false));
        let (hx, hy) = (plain.hotspot.0 as f32, plain.hotspot.1 as f32);
        assert_eq!(plain.arrow, Some((hx, hy)));
        assert!(contains(&plain, plain.label));
        assert!(contains(
            &plain,
            [hx - OUTLINE, hy - OUTLINE, ARROW_SIZE.0 + 2.0 * OUTLINE, ARROW_SIZE.1 + 2.0 * OUTLINE]
        ));
        // Right of and below the tip, clear of the arrow's tail.
        let [x, y, w, h] = plain.label;
        assert!(x > hx + 9.6 && y > hy);
        assert_eq!((w, h), (74.0, 22.0));

        let flipped = layout((60.0, 16.0), 1.0, true, (true, true));
        let [x, y, w, h] = flipped.label;
        let (hx, hy) = (flipped.hotspot.0 as f32, flipped.hotspot.1 as f32);
        assert!(x + w < hx && y + h < hy);
        assert!(contains(&flipped, flipped.label));
    }

    #[test]
    fn layout_scales_and_caps_the_label() {
        let small = layout((40.0, 16.0), 1.0, false, (false, false));
        let large = layout((80.0, 32.0), 2.0, false, (false, false));
        assert!(small.arrow.is_none() && large.arrow.is_none());
        assert_eq!(large.label[2], small.label[2] * 2.0);
        assert!(large.size.0 >= small.size.0 * 2 - 2);
        let long = layout((1000.0, 16.0), 1.5, false, (false, false));
        assert_eq!(long.label[2], MAX_LABEL * 1.5);
    }

    #[test]
    fn labels_flip_at_the_screen_edges() {
        let bounds = [-1920, 0, 0, 1080];
        let plain = layout((60.0, 16.0), 1.0, true, (false, false));
        assert_eq!(flips((-960, 540), bounds, &plain), (false, false));
        assert_eq!(flips((-20, 540), bounds, &plain), (true, false));
        assert_eq!(flips((-960, 1070), bounds, &plain), (false, true));
        assert_eq!(flips((-1, 1079), bounds, &plain), (true, true));
    }

    #[test]
    fn click_ring_expands_and_fades_inside_its_window() {
        let side = ring_size(1.25) as f32;
        let mut last = 0.0;
        for step in 0..=10 {
            let (radius, stroke, opacity) = ring(step as f32 / 10.0, 1.25);
            assert!(radius >= last);
            assert!(radius + stroke / 2.0 + 1.25 <= side / 2.0);
            assert!((opacity - (1.0 - step as f32 / 10.0)).abs() < 1e-6);
            last = radius;
        }
        assert_eq!(ring(f32::NAN, 1.0).2, 0.0);
        assert_eq!(alpha(0.5), 128);
        assert_eq!(alpha(f32::NAN), 0);
    }
}
