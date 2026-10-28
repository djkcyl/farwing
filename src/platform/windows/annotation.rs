//! Click-through layered desktop surfaces. All handles belong to the drawing thread.
use crate::{media::capture::Screen, protocol::annotation::Stroke};
use anyhow::{Result, ensure};
use std::{collections::BTreeMap, ptr::null_mut};
use windows::{
    Win32::{
        Foundation::*,
        Graphics::{Gdi::*, GdiPlus::*},
        System::LibraryLoader::GetModuleHandleW,
        UI::{HiDpi::*, WindowsAndMessaging::*},
    },
    core::{BOOL, w},
};

mod brand;

thread_local! {
    static DISPLAY_REVISION: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

fn check(status: Status) -> Result<()> {
    ensure!(status == Ok, "批注绘制失败（GDI+ {}）", status.0);
    Result::Ok(())
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
    layers: BTreeMap<i32, Layer>,
    sources: Vec<Screen>,
    live: Vec<Screen>,
    display_revision: u64,
    foreground: HWND,
    _runtime: Runtime,
}
impl Overlay {
    pub fn new() -> Result<Self> {
        let runtime = Runtime::new()?;
        let ui_access = unsafe {
            use windows::Win32::{Security::*, System::Threading::*};
            let mut token = HANDLE::default();
            if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token).is_ok() {
                let mut value = 0u32;
                let mut length = 0;
                let result = GetTokenInformation(
                    token,
                    TokenUIAccess,
                    Some((&mut value as *mut u32).cast()),
                    4,
                    &mut length,
                );
                let _ = CloseHandle(token);
                result.ok().map(|_| value != 0)
            } else {
                None
            }
        };
        tracing::info!(
            ?ui_access,
            "host annotation overlay initialized on interactive desktop"
        );
        static CLASS: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        ensure!(
            *CLASS.get_or_init(|| unsafe {
                let class = WNDCLASSW {
                    lpfnWndProc: Some(window_proc),
                    hInstance: GetModuleHandleW(None).unwrap_or_default().into(),
                    lpszClassName: w!("Farwing.Annotation.Overlay"),
                    ..Default::default()
                };
                RegisterClassW(&class) != 0
            }),
            "无法注册批注窗口"
        );
        Result::Ok(Self {
            layers: BTreeMap::new(),
            sources: Vec::new(),
            live: Vec::new(),
            display_revision: DISPLAY_REVISION.get(),
            foreground: HWND::default(),
            _runtime: runtime,
        })
    }
    pub fn pump(&self) {
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
    }
    pub fn hide(&self) {
        for layer in self.layers.values() {
            unsafe {
                let _ = ShowWindow(layer.hwnd, SW_HIDE);
            }
        }
    }
    pub fn needs_refresh(&self) -> bool {
        self.display_revision != DISPLAY_REVISION.get()
    }
    pub fn maintain_visibility(&mut self) -> Result<()> {
        let foreground = unsafe { GetForegroundWindow() };
        if foreground != self.foreground {
            self.foreground = foreground;
            for layer in self.layers.values() {
                layer.raise()?;
            }
        }
        Result::Ok(())
    }
    pub fn render(
        &mut self,
        screens: &[Screen],
        strokes: &BTreeMap<(i32, u32), std::sync::Arc<Stroke>>,
        boards: &BTreeMap<i32, u32>,
        dirty: &std::collections::BTreeSet<i32>,
        selected: Option<i32>,
    ) -> Result<()> {
        // Display notifications are independent of the media inventory timer.
        // Bind by source name AND monitor identity, never nearest-monitor fallback.
        let refresh = self.sources != screens || self.needs_refresh();
        if refresh {
            self.live = live_screens(screens)?;
            self.sources = screens.to_vec();
            self.display_revision = DISPLAY_REVISION.get();
        }
        let screens = &self.live;
        self.layers.retain(|id, layer| {
            screens.iter().any(|s| s.id == *id && layer.matches(s))
                && (selected == Some(*id)
                    || boards.contains_key(id)
                    || strokes.keys().any(|(screen, _)| screen == id))
        });
        let total: u64 = screens
            .iter()
            .filter(|s| {
                selected == Some(s.id)
                    || boards.contains_key(&s.id)
                    || strokes.keys().any(|(id, _)| *id == s.id)
            })
            .map(|s| u64::from(s.width) * u64::from(s.height))
            .sum();
        ensure!(total <= 64 * 1024 * 1024, "批注覆盖面积超限");
        for screen in screens {
            if selected != Some(screen.id)
                && !boards.contains_key(&screen.id)
                && !strokes.keys().any(|(id, _)| *id == screen.id)
            {
                continue;
            }
            if self.layers.contains_key(&screen.id) && !dirty.contains(&screen.id) && !refresh {
                continue;
            }
            if !self.layers.contains_key(&screen.id) {
                self.layers.insert(screen.id, Layer::new(screen)?);
            }
            self.layers.get_mut(&screen.id).unwrap().draw(
                boards.get(&screen.id).copied(),
                strokes
                    .range((screen.id, 0)..=(screen.id, u32::MAX))
                    .map(|(_, s)| s.as_ref()),
            )?;
        }
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
struct Layer {
    hwnd: HWND,
    dc: HDC,
    bitmap: HBITMAP,
    old: HGDIOBJ,
    image: *mut GpBitmap,
    graphics: *mut GpGraphics,
    screen: Screen,
    brand: Option<brand::Brand>,
}
impl Layer {
    fn raise(&self) -> Result<()> {
        unsafe {
            SetWindowPos(
                self.hwnd,
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
    fn matches(&self, s: &Screen) -> bool {
        self.screen.device_name == s.device_name
            && self.screen.identity == s.identity
            && self.screen.adapter == s.adapter
            && self.screen.width == s.width
            && self.screen.height == s.height
            && self.screen.left == s.left
            && self.screen.top == s.top
            && self.screen.dpi_scale == s.dpi_scale
    }
    fn new(screen: &Screen) -> Result<Self> {
        ensure!(
            screen.width > 0
                && screen.height > 0
                && screen.width <= 16384
                && screen.height <= 16384
                && u64::from(screen.width) * u64::from(screen.height) <= 32 * 1024 * 1024,
            "批注显示器尺寸无效"
        );
        let mut layer = Self {
            hwnd: HWND::default(),
            dc: HDC::default(),
            bitmap: HBITMAP::default(),
            old: HGDIOBJ::default(),
            image: null_mut(),
            graphics: null_mut(),
            screen: screen.clone(),
            brand: None,
        };
        unsafe {
            layer.hwnd = CreateWindowExW(
                WS_EX_LAYERED
                    | WS_EX_TRANSPARENT
                    | WS_EX_NOACTIVATE
                    | WS_EX_TOOLWINDOW
                    | WS_EX_TOPMOST,
                w!("Farwing.Annotation.Overlay"),
                w!("Farwing 批注"),
                WS_POPUP,
                screen.left,
                screen.top,
                screen.width as i32,
                screen.height as i32,
                None,
                None,
                Some(GetModuleHandleW(None)?.into()),
                None,
            )?;
            layer.dc = CreateCompatibleDC(None);
            ensure!(!layer.dc.0.is_null(), "创建批注DC失败");
            let info = BITMAPINFO {
                bmiHeader: BITMAPINFOHEADER {
                    biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                    biWidth: screen.width as i32,
                    biHeight: -(screen.height as i32),
                    biPlanes: 1,
                    biBitCount: 32,
                    biCompression: BI_RGB.0,
                    ..Default::default()
                },
                ..Default::default()
            };
            let mut pixels = null_mut();
            layer.bitmap =
                CreateDIBSection(Some(layer.dc), &info, DIB_RGB_COLORS, &mut pixels, None, 0)?;
            ensure!(!pixels.is_null(), "创建批注像素失败");
            layer.old = SelectObject(layer.dc, layer.bitmap.into());
            ensure!(
                !layer.old.0.is_null() && layer.old.0 as isize != -1,
                "选择批注表面失败"
            );
            check(GdipCreateBitmapFromScan0(
                screen.width as i32,
                screen.height as i32,
                screen.width as i32 * 4,
                0x000e200b,
                Some(pixels.cast()),
                &mut layer.image,
            ))?;
            check(GdipGetImageGraphicsContext(
                layer.image.cast(),
                &mut layer.graphics,
            ))?;
            check(GdipSetSmoothingMode(layer.graphics, SmoothingModeAntiAlias))?;
            check(GdipSetPixelOffsetMode(layer.graphics, PixelOffsetModeHalf))?;
            check(GdipSetCompositingQuality(
                layer.graphics,
                CompositingQualityHighQuality,
            ))?;
        }
        Result::Ok(layer)
    }
    fn draw_brand(&mut self, color: u32, scale: f32) -> Result<()> {
        if self.brand.is_none() {
            self.brand = Some(brand::Brand::new(&self.screen, scale)?);
        }
        self.brand
            .as_ref()
            .unwrap()
            .draw(self.graphics, color, self.screen.width)
    }
    fn draw<'a>(
        &mut self,
        board: Option<u32>,
        strokes: impl Iterator<Item = &'a Stroke>,
    ) -> Result<()> {
        unsafe {
            check(GdipSetCompositingMode(
                self.graphics,
                CompositingModeSourceCopy,
            ))?;
            check(GdipGraphicsClear(self.graphics, board.unwrap_or(0)))?;
            check(GdipSetCompositingMode(
                self.graphics,
                CompositingModeSourceOver,
            ))?;
            let scale = GetDpiForWindow(self.hwnd).max(96) as f32 / 96.0;
            if board.is_some() {
                self.draw_brand(board.unwrap(), scale)?;
            }
            for stroke in strokes {
                draw_stroke(
                    self.graphics,
                    stroke,
                    self.screen.width as f32,
                    self.screen.height as f32,
                    scale,
                )?;
            }
            check(GdipFlush(self.graphics, FlushIntentionSync))?;
            let destination = POINT {
                x: self.screen.left,
                y: self.screen.top,
            };
            let size = SIZE {
                cx: self.screen.width as i32,
                cy: self.screen.height as i32,
            };
            let blend = BLENDFUNCTION {
                BlendOp: AC_SRC_OVER as u8,
                BlendFlags: 0,
                SourceConstantAlpha: 255,
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
            self.raise()?;
        }
        Result::Ok(())
    }
}

fn live_screens(screens: &[Screen]) -> Result<Vec<Screen>> {
    unsafe extern "system" fn collect(
        monitor: HMONITOR,
        _: HDC,
        _: *mut RECT,
        data: LPARAM,
    ) -> BOOL {
        let monitors = unsafe { &mut *(data.0 as *mut Vec<(String, RECT, Option<u32>)>) };
        let mut info = MONITORINFOEXW::default();
        info.monitorInfo.cbSize = std::mem::size_of::<MONITORINFOEXW>() as u32;
        if unsafe { GetMonitorInfoW(monitor, &mut info.monitorInfo) }.as_bool() {
            let n = info
                .szDevice
                .iter()
                .position(|v| *v == 0)
                .unwrap_or(info.szDevice.len());
            let (mut x, mut y) = (0, 0);
            let dpi = unsafe { GetDpiForMonitor(monitor, MDT_EFFECTIVE_DPI, &mut x, &mut y) }
                .ok()
                .filter(|_| x > 0)
                .map(|_| (x * 100 + 48) / 96);
            monitors.push((
                String::from_utf16_lossy(&info.szDevice[..n]),
                info.monitorInfo.rcMonitor,
                dpi,
            ));
        }
        BOOL(1)
    }
    let mut monitors = Vec::<(String, RECT, Option<u32>)>::new();
    unsafe {
        EnumDisplayMonitors(
            None,
            None,
            Some(collect),
            LPARAM((&mut monitors as *mut Vec<(String, RECT, Option<u32>)>) as isize),
        )
    }
    .ok()?;
    let mut live = Vec::new();
    for source in screens {
        let Some((_, rect, dpi)) = monitors
            .iter()
            .find(|(name, _, _)| name.eq_ignore_ascii_case(&source.device_name))
        else {
            continue;
        };
        if source.identity.is_some()
            && super::capture::display_identity(&source.device_name) != source.identity
        {
            continue;
        }
        let mut screen = source.clone();
        screen.left = rect.left;
        screen.top = rect.top;
        screen.width = u32::try_from(rect.right - rect.left)?;
        screen.height = u32::try_from(rect.bottom - rect.top)?;
        screen.dpi_scale = dpi.or(screen.dpi_scale);
        live.push(screen);
    }
    Result::Ok(live)
}
impl Drop for Layer {
    fn drop(&mut self) {
        unsafe {
            if !self.graphics.is_null() {
                GdipDeleteGraphics(self.graphics);
            }
            if !self.image.is_null() {
                GdipDisposeImage(self.image.cast());
            }
            if !self.old.0.is_null() && self.old.0 as isize != -1 {
                SelectObject(self.dc, self.old);
            }
            if !self.bitmap.0.is_null() {
                let _ = DeleteObject(self.bitmap.into());
            }
            if !self.dc.0.is_null() {
                let _ = DeleteDC(self.dc);
            }
            if !self.hwnd.0.is_null() {
                let _ = DestroyWindow(self.hwnd);
            }
        }
    }
}
struct StrokeObjects {
    pen: *mut GpPen,
    brush: *mut GpSolidFill,
    path: *mut GpPath,
}
impl Drop for StrokeObjects {
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
fn draw_stroke(
    g: *mut GpGraphics,
    stroke: &Stroke,
    width: f32,
    height: f32,
    scale: f32,
) -> Result<()> {
    let points: Vec<_> = stroke
        .points
        .iter()
        .map(|p| [p.x * width, p.y * height])
        .collect();
    let Some(&first) = points.first() else {
        return Result::Ok(());
    };
    let line = (stroke.width * scale).max(1.0);
    let color = stroke.color;
    let mut objects = StrokeObjects {
        pen: null_mut(),
        brush: null_mut(),
        path: null_mut(),
    };
    unsafe {
        check(GdipCreatePen1(color, line, UnitPixel, &mut objects.pen))?;
        check(GdipSetPenLineJoin(objects.pen, LineJoinRound))?;
        check(GdipSetPenStartCap(objects.pen, LineCapRound))?;
        check(GdipSetPenEndCap(objects.pen, LineCapRound))?;
        check(GdipCreateSolidFill(color, &mut objects.brush))?;
        if points.len() == 1 {
            check(GdipFillEllipse(
                g,
                objects.brush.cast(),
                first[0] - line / 2.0,
                first[1] - line / 2.0,
                line,
                line,
            ))?;
        } else {
            check(GdipCreatePath(FillModeAlternate, &mut objects.path))?;
            let mut start = first;
            // Midpoint quadratic smoothing, expressed as cubic Beziers. The
            // last segment reaches the actual endpoint; batches don't add caps.
            for i in 1..points.len() {
                let control = points[i - 1];
                let end = if i == points.len() - 1 {
                    points[i]
                } else {
                    [
                        (control[0] + points[i][0]) * 0.5,
                        (control[1] + points[i][1]) * 0.5,
                    ]
                };
                let c1 = [
                    start[0] + (control[0] - start[0]) * 2.0 / 3.0,
                    start[1] + (control[1] - start[1]) * 2.0 / 3.0,
                ];
                let c2 = [
                    end[0] + (control[0] - end[0]) * 2.0 / 3.0,
                    end[1] + (control[1] - end[1]) * 2.0 / 3.0,
                ];
                check(GdipAddPathBezier(
                    objects.path,
                    start[0],
                    start[1],
                    c1[0],
                    c1[1],
                    c2[0],
                    c2[1],
                    end[0],
                    end[1],
                ))?;
                start = end;
            }
            check(GdipDrawPath(g, objects.pen, objects.path))?;
        }
    }
    Result::Ok(())
}
