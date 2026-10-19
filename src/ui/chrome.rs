//! Shared borderless window chrome and native fallback background.
use super::controls::{ViewerCaptionIcon as TitleIcon, viewer_caption_button as title_icon_button};
use super::d3d11::window_hwnd;
use anyhow::Result;
use std::time::{Duration, Instant};
use windows::Win32::Foundation::{COLORREF, HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows::Win32::Graphics::Dwm::{
    DWMWA_BORDER_COLOR, DWMWA_COLOR_NONE, DWMWA_USE_IMMERSIVE_DARK_MODE,
    DWMWA_WINDOW_CORNER_PREFERENCE, DWMWCP_DONOTROUND, DWMWCP_ROUNDSMALL, DwmSetWindowAttribute,
};
use windows::Win32::Graphics::Gdi::{
    DC_BRUSH, FillRect, GetDC, GetMonitorInfoW, GetStockObject, HBRUSH, HDC,
    MONITOR_DEFAULTTONEAREST, MONITORINFO, MonitorFromPoint, ReleaseDC, SetDCBrushColor,
};
use windows::Win32::UI::Input::KeyboardAndMouse::{ReleaseCapture, SetCapture};
use windows::Win32::UI::Shell::{DefSubclassProc, RemoveWindowSubclass, SetWindowSubclass};
use windows::Win32::UI::WindowsAndMessaging::GetCursorPos;
use windows::Win32::UI::WindowsAndMessaging::{
    GetClientRect, WM_ERASEBKGND, WM_MOVING, WM_NCCALCSIZE, WM_NCDESTROY,
};
use winit::dpi::{PhysicalPosition, PhysicalSize};
use winit::window::{Fullscreen, ResizeDirection, Window};

const BACKDROP_SUBCLASS: usize = 0x4f555542;

pub(crate) struct TitleBarAlert {
    pub source: String,
    pub duration: String,
}

pub(crate) fn cancel_pointer_operation(
    moving: &mut WindowMoveState,
    resizing: &mut WindowResizeState,
) {
    if moving.start.take().is_some() | resizing.start.take().is_some() {
        let _ = unsafe { ReleaseCapture() };
    }
}

fn title_bar_frame() -> egui::Frame {
    egui::Frame::new()
        .fill(crate::ui::theme::SIDEBAR)
        .inner_margin(crate::ui::theme::WINDOW_TITLE_MARGIN)
        .stroke(egui::Stroke::new(
            crate::ui::theme::WINDOW_TITLE_STROKE,
            crate::ui::theme::LINE,
        ))
}

pub(crate) fn title_bar_height() -> f32 {
    crate::ui::theme::WINDOW_TITLE_CONTENT_HEIGHT + title_bar_frame().total_margin().sum().y
}

pub(crate) fn title_bar_panel<R>(
    ui: &mut egui::Ui,
    id: &'static str,
    height: f32,
    contents: impl FnOnce(&mut egui::Ui) -> R,
) -> egui::InnerResponse<R> {
    title_bar_panel_alert(ui, id, height, false, contents)
}

pub(crate) fn title_bar_panel_alert<R>(
    ui: &mut egui::Ui,
    id: &'static str,
    height: f32,
    alert: bool,
    contents: impl FnOnce(&mut egui::Ui) -> R,
) -> egui::InnerResponse<R> {
    let clip = ui.clip_rect();
    let bottom = ui.available_rect_before_wrap().top() + height;
    // The native video child starts exactly at this boundary. Frame-edge
    // antialiasing must not escape into its first row at fractional DPI.
    ui.set_clip_rect(clip.intersect(egui::Rect::from_min_max(
        clip.min,
        egui::pos2(clip.right(), bottom),
    )));
    let result = egui::Panel::top(id)
        .frame(if alert {
            title_bar_frame()
                .fill(crate::ui::theme::CONTROLLED_TITLE_BG)
                .stroke(egui::Stroke::new(
                    crate::ui::theme::WINDOW_TITLE_STROKE,
                    crate::ui::theme::CONTROLLED_TITLE_LINE,
                ))
        } else {
            title_bar_frame()
        })
        .exact_size(height)
        .show(ui, contents);
    ui.set_clip_rect(clip);
    result
}

pub(crate) fn window_title_bar(
    ui: &mut egui::Ui,
    window: &Window,
    alias: &str,
    move_state: Option<&mut WindowMoveState>,
    tooltips: bool,
) -> bool {
    window_title_bar_alert(ui, window, alias, move_state, tooltips, None)
}

pub(crate) fn window_title_bar_alert(
    ui: &mut egui::Ui,
    window: &Window,
    alias: &str,
    move_state: Option<&mut WindowMoveState>,
    tooltips: bool,
    alert: Option<&TitleBarAlert>,
) -> bool {
    ui.set_min_height(crate::ui::theme::WINDOW_TITLE_CONTENT_HEIGHT);
    let rect = egui::Rect::from_min_size(
        ui.available_rect_before_wrap().min,
        egui::vec2(
            ui.available_width(),
            crate::ui::theme::WINDOW_TITLE_CONTENT_HEIGHT,
        ),
    );
    let (caption_rect, controls_rect) = title_regions(
        rect,
        window
            .enabled_buttons()
            .contains(winit::window::WindowButtons::MAXIMIZE),
    );
    let drag = ui.interact(
        caption_rect,
        ui.id().with("window-title-drag"),
        egui::Sense::click_and_drag(),
    );
    if let Some(state) = move_state {
        update_nonmodal_window_move(ui.ctx(), window, &drag, state);
    } else if handle_title_drag(window, &drag) {
        let _ = window.drag_window();
    }
    let brand_rect = if alert.is_some() {
        egui::Rect::from_min_max(
            caption_rect.min,
            egui::pos2(
                (caption_rect.left()
                    + crate::ui::theme::CONTROLLED_TITLE_BRAND_MAX.min(rect.width() * 0.25))
                .min(caption_rect.right()),
                caption_rect.bottom(),
            ),
        )
    } else {
        caption_rect
    };
    let mut caption = ui.new_child(
        egui::UiBuilder::new()
            .max_rect(brand_rect)
            .layout(egui::Layout::left_to_right(egui::Align::Center)),
    );
    caption.set_clip_rect(brand_rect);
    paint_brand_logo(&mut caption);
    caption.add(
        egui::Label::new(
            egui::RichText::new(alias)
                .size(crate::ui::theme::SMALL)
                .color(crate::ui::theme::TEXT),
        )
        .truncate(),
    );
    if let Some(alert) = alert {
        let margin = crate::ui::theme::WINDOW_TITLE_MARGIN;
        let centered = rect.translate(egui::vec2(
            (f32::from(margin.right) - f32::from(margin.left)) * 0.5,
            0.0,
        ));
        let inset = (brand_rect.right() - centered.left())
            .max(centered.right() - controls_rect.left())
            + crate::ui::theme::CONTROLLED_TITLE_GAP;
        paint_title_alert(ui, alert, centered.shrink2(egui::vec2(inset, 0.0)));
    }
    let mut controls = ui.new_child(
        egui::UiBuilder::new()
            .max_rect(controls_rect.shrink2(egui::vec2(4.0, 0.0)))
            .layout(egui::Layout::left_to_right(egui::Align::Center)),
    );
    controls.spacing_mut().item_spacing.x = 4.0;
    let close = window_buttons_impl(&mut controls, window, tooltips);
    ui.allocate_rect(rect, egui::Sense::hover());
    close
}

pub(crate) fn paint_title_alert(ui: &egui::Ui, alert: &TitleBarAlert, rect: egui::Rect) {
    if rect.width() <= 0.0 {
        return;
    }
    let color = crate::ui::theme::CONTROLLED_TITLE_TEXT;
    let font = egui::FontId::proportional(crate::ui::theme::COMPACT_TEXT);
    let suffix =
        ui.painter()
            .layout_no_wrap(format!("  ·  {}", alert.duration), font.clone(), color);
    let width = (rect.width() - suffix.size().x).max(0.0);
    let mut job = egui::text::LayoutJob::simple(alert.source.clone(), font, color, width);
    job.wrap.max_rows = 1;
    job.wrap.break_anywhere = true;
    let source = ui.painter().layout_job(job);
    let total = source.size().x + suffix.size().x;
    let x = rect.center().x - total / 2.0;
    let painter = ui.painter().with_clip_rect(rect.intersect(ui.clip_rect()));
    painter.galley(
        egui::pos2(x, rect.center().y - source.size().y / 2.0),
        source.clone(),
        color,
    );
    painter.galley(
        egui::pos2(x + source.size().x, rect.center().y - suffix.size().y / 2.0),
        suffix,
        color,
    );
}

fn title_regions(rect: egui::Rect, maximize: bool) -> (egui::Rect, egui::Rect) {
    let width = crate::ui::theme::WINDOW_CONTROLS_WIDTH
        - if maximize {
            0.0
        } else {
            crate::ui::theme::VIEWER_CAPTION_BUTTON + 4.0
        };
    let controls_left = (rect.right() - width).max(rect.left());
    (
        egui::Rect::from_min_max(rect.min, egui::pos2(controls_left, rect.bottom())),
        egui::Rect::from_min_max(egui::pos2(controls_left, rect.top()), rect.max),
    )
}

pub(crate) fn paint_brand_logo(ui: &mut egui::Ui) {
    let key = egui::Id::new("window-brand-logo");
    let texture = ui
        .ctx()
        .data(|data| data.get_temp::<egui::TextureHandle>(key))
        .unwrap_or_else(|| {
            let texture = crate::ui::branding::load_texture(ui.ctx());
            ui.ctx()
                .data_mut(|data| data.insert_temp(key, texture.clone()));
            texture
        });
    let (rect, _) = ui.allocate_exact_size(
        egui::Vec2::splat(crate::ui::theme::WINDOW_LOGO_SIZE),
        egui::Sense::hover(),
    );
    crate::ui::branding::paint(ui.painter(), rect, &texture);
}

pub(crate) fn handle_title_drag(window: &Window, response: &egui::Response) -> bool {
    if window.fullscreen().is_some() {
        return false;
    }
    if response.double_clicked() {
        if window.is_resizable()
            && window
                .enabled_buttons()
                .contains(winit::window::WindowButtons::MAXIMIZE)
        {
            window.set_maximized(!window.is_maximized());
        }
        false
    } else {
        response.drag_started()
    }
}

pub(crate) fn window_buttons(ui: &mut egui::Ui, window: &Window) -> bool {
    window_buttons_impl(ui, window, true)
}
fn window_buttons_impl(ui: &mut egui::Ui, window: &Window, tooltips: bool) -> bool {
    let expanded = window.is_maximized() || window.fullscreen().is_some();
    let minimize = title_icon_button(
        ui,
        TitleIcon::Minimize,
        false,
        if tooltips { "最小化" } else { "" },
    );
    if minimize.clicked() {
        window.set_minimized(true);
    }
    if window
        .enabled_buttons()
        .contains(winit::window::WindowButtons::MAXIMIZE)
    {
        let maximize = title_icon_button(
            ui,
            if expanded {
                TitleIcon::Restore
            } else {
                TitleIcon::Maximize
            },
            false,
            if !tooltips {
                ""
            } else if expanded {
                "还原"
            } else {
                "最大化"
            },
        );
        if maximize.clicked() {
            if window.fullscreen().is_some() {
                set_fullscreen(window, None);
            } else {
                if window.is_resizable()
                    && window
                        .enabled_buttons()
                        .contains(winit::window::WindowButtons::MAXIMIZE)
                {
                    window.set_maximized(!window.is_maximized());
                }
            }
        }
    }
    title_icon_button(
        ui,
        TitleIcon::Close,
        false,
        if tooltips { "关闭" } else { "" },
    )
    .clicked()
}

pub(crate) fn set_fullscreen(window: &Window, fullscreen: Option<Fullscreen>) {
    // Publish the target before winit synchronously changes styles/bounds.
    // Its undecorated WM_NCCALCSIZE path otherwise clamps an IsZoomed HWND
    // to rcWork even when the requested bounds cover the complete monitor.
    if let Err(error) = prepare_window_background(window, fullscreen.is_some()) {
        tracing::warn!(%error, "prepare fullscreen client area");
    }
    window.set_fullscreen(fullscreen);
    configure_dwm_window(window);
}

pub(crate) fn configure_dwm_window(window: &Window) {
    if let Err(error) = prepare_window_background(window, window.fullscreen().is_some()) {
        tracing::warn!(%error, "prepare window background");
    }
    let Ok(hwnd) = window_hwnd(window) else {
        return;
    };
    let dark_mode = 1_i32;
    let fullscreen = window.fullscreen().is_some();
    let color = crate::ui::theme::WINDOW_BORDER;
    let border_color = if fullscreen {
        DWMWA_COLOR_NONE
    } else {
        u32::from(color.r()) | (u32::from(color.g()) << 8) | (u32::from(color.b()) << 16)
    };
    let corner = if fullscreen {
        DWMWCP_DONOTROUND
    } else {
        DWMWCP_ROUNDSMALL
    };
    unsafe {
        let _ = DwmSetWindowAttribute(
            hwnd,
            DWMWA_USE_IMMERSIVE_DARK_MODE,
            (&raw const dark_mode).cast(),
            std::mem::size_of_val(&dark_mode) as u32,
        );
        let _ = DwmSetWindowAttribute(
            hwnd,
            DWMWA_BORDER_COLOR,
            (&raw const border_color).cast(),
            std::mem::size_of_val(&border_color) as u32,
        );
        let _ = DwmSetWindowAttribute(
            hwnd,
            DWMWA_WINDOW_CORNER_PREFERENCE,
            (&raw const corner).cast(),
            std::mem::size_of_val(&corner) as u32,
        );
    }
}

fn prepare_window_background(window: &Window, fullscreen: bool) -> Result<()> {
    anyhow::ensure!(
        unsafe {
            SetWindowSubclass(
                window_hwnd(window)?,
                Some(backdrop_proc),
                BACKDROP_SUBCLASS,
                usize::from(fullscreen),
            )
            .as_bool()
        },
        "install window background"
    );
    unsafe {
        let hwnd = window_hwnd(window)?;
        let dc = GetDC(Some(hwnd));
        paint_backdrop(hwnd, dc);
        ReleaseDC(Some(hwnd), dc);
    }
    Ok(())
}

unsafe extern "system" fn backdrop_proc(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
    id: usize,
    data: usize,
) -> LRESULT {
    unsafe {
        match message {
            WM_MOVING if data == 0 && lparam.0 != 0 => {
                let mut cursor = POINT::default();
                if GetCursorPos(&raw mut cursor).is_ok() {
                    let rect = &mut *(lparam.0 as *mut RECT);
                    let position =
                        constrain_drag_position(PhysicalPosition::new(rect.left, rect.top), cursor);
                    rect.bottom = rect
                        .bottom
                        .saturating_add(position.y.saturating_sub(rect.top));
                    rect.top = position.y;
                    return LRESULT(1);
                }
            }
            WM_NCCALCSIZE if data != 0 && wparam.0 != 0 => {
                // Keep the proposed full window rectangle as the client area.
                // Normal maximization still delegates to winit's rcWork logic.
                return LRESULT(0);
            }
            WM_ERASEBKGND => {
                paint_backdrop(hwnd, HDC(wparam.0 as _));
                return LRESULT(1);
            }
            WM_NCDESTROY => {
                let _ = RemoveWindowSubclass(hwnd, Some(backdrop_proc), id);
            }
            _ => {}
        }
        DefSubclassProc(hwnd, message, wparam, lparam)
    }
}

unsafe fn paint_backdrop(hwnd: HWND, dc: HDC) {
    unsafe {
        let mut rect = RECT::default();
        if GetClientRect(hwnd, &mut rect).is_ok() {
            let color = crate::ui::theme::BG;
            let old = SetDCBrushColor(
                dc,
                COLORREF(
                    u32::from(color.r())
                        | (u32::from(color.g()) << 8)
                        | (u32::from(color.b()) << 16),
                ),
            );
            FillRect(dc, &rect, HBRUSH(GetStockObject(DC_BRUSH).0));
            SetDCBrushColor(dc, old);
        }
    }
}

pub(crate) fn resize_regions(
    ui: &mut egui::Ui,
    window: &Window,
    mut handle: impl FnMut(&egui::Response, ResizeDirection),
) {
    if !window.is_resizable() || window.is_maximized() || window.fullscreen().is_some() {
        return;
    }
    let rect = ui.max_rect();
    // Page panels own the background layer. Keep the narrow resize handles
    // above them so a central panel cannot swallow border gestures.
    let resize_ui = ui.new_child(egui::UiBuilder::new().max_rect(rect).layer_id(
        egui::LayerId::new(egui::Order::Foreground, ui.id().with("window-resize")),
    ));
    let edge = 6.0;
    let corner = 12.0;
    let regions = [
        (
            egui::Rect::from_min_max(
                rect.min,
                egui::pos2(rect.min.x + corner, rect.min.y + corner),
            ),
            ResizeDirection::NorthWest,
            egui::CursorIcon::ResizeNwSe,
        ),
        (
            egui::Rect::from_min_max(
                egui::pos2(rect.max.x - corner, rect.min.y),
                egui::pos2(rect.max.x, rect.min.y + corner),
            ),
            ResizeDirection::NorthEast,
            egui::CursorIcon::ResizeNeSw,
        ),
        (
            egui::Rect::from_min_max(
                egui::pos2(rect.min.x, rect.max.y - corner),
                egui::pos2(rect.min.x + corner, rect.max.y),
            ),
            ResizeDirection::SouthWest,
            egui::CursorIcon::ResizeNeSw,
        ),
        (
            egui::Rect::from_min_max(
                egui::pos2(rect.max.x - corner, rect.max.y - corner),
                rect.max,
            ),
            ResizeDirection::SouthEast,
            egui::CursorIcon::ResizeNwSe,
        ),
        (
            egui::Rect::from_min_max(
                egui::pos2(rect.min.x + corner, rect.min.y),
                egui::pos2(rect.max.x - corner, rect.min.y + edge),
            ),
            ResizeDirection::North,
            egui::CursorIcon::ResizeVertical,
        ),
        (
            egui::Rect::from_min_max(
                egui::pos2(rect.min.x + corner, rect.max.y - edge),
                egui::pos2(rect.max.x - corner, rect.max.y),
            ),
            ResizeDirection::South,
            egui::CursorIcon::ResizeVertical,
        ),
        (
            egui::Rect::from_min_max(
                egui::pos2(rect.min.x, rect.min.y + corner),
                egui::pos2(rect.min.x + edge, rect.max.y - corner),
            ),
            ResizeDirection::West,
            egui::CursorIcon::ResizeHorizontal,
        ),
        (
            egui::Rect::from_min_max(
                egui::pos2(rect.max.x - edge, rect.min.y + corner),
                egui::pos2(rect.max.x, rect.max.y - corner),
            ),
            ResizeDirection::East,
            egui::CursorIcon::ResizeHorizontal,
        ),
    ];
    for (index, (region, direction, cursor)) in regions.into_iter().enumerate() {
        let response = resize_ui
            .interact(region, ui.id().with(("resize", index)), egui::Sense::drag())
            .on_hover_cursor(cursor);
        handle(&response, direction);
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct WindowMoveState {
    start: Option<(POINT, PhysicalPosition<i32>)>,
}

pub(crate) fn constrain_drag_position(
    position: PhysicalPosition<i32>,
    cursor: POINT,
) -> PhysicalPosition<i32> {
    // Follow the pointer's destination monitor, including monitors above the
    // primary display. Work-area coordinates also account for a top taskbar.
    let monitor = unsafe { MonitorFromPoint(cursor, MONITOR_DEFAULTTONEAREST) };
    let mut info = MONITORINFO {
        cbSize: std::mem::size_of::<MONITORINFO>() as u32,
        ..Default::default()
    };
    if unsafe { GetMonitorInfoW(monitor, &raw mut info) }.as_bool() {
        PhysicalPosition::new(position.x, position.y.max(info.rcWork.top))
    } else {
        position
    }
}

pub(crate) fn update_nonmodal_window_move(
    ctx: &egui::Context,
    window: &Window,
    response: &egui::Response,
    state: &mut WindowMoveState,
) {
    if window.fullscreen().is_some() {
        state.start = None;
        return;
    }
    if response.double_clicked() {
        if state.start.take().is_some() {
            let _ = unsafe { ReleaseCapture() };
        }
        if window.is_resizable()
            && window
                .enabled_buttons()
                .contains(winit::window::WindowButtons::MAXIMIZE)
        {
            window.set_maximized(!window.is_maximized());
        }
        return;
    }
    if response.drag_started() {
        let mut cursor = POINT::default();
        if unsafe { GetCursorPos(&raw mut cursor) }.is_ok()
            && let Ok(origin) = window.outer_position()
        {
            if window.is_maximized() {
                window.set_maximized(false);
            }
            state.start = Some((cursor, origin));
            tracing::debug!(x = origin.x, y = origin.y, "local title drag started");
            if let Ok(hwnd) = window_hwnd(window) {
                unsafe { SetCapture(hwnd) };
            }
        }
    }
    let primary_down = ctx.input(|input| input.pointer.primary_down());
    if primary_down && let Some((start_cursor, origin)) = state.start {
        let mut cursor = POINT::default();
        if unsafe { GetCursorPos(&raw mut cursor) }.is_ok() {
            let started = Instant::now();
            window.set_outer_position(constrain_drag_position(
                PhysicalPosition::new(
                    origin.x.saturating_add(cursor.x - start_cursor.x),
                    origin.y.saturating_add(cursor.y - start_cursor.y),
                ),
                cursor,
            ));
            let elapsed = started.elapsed();
            if elapsed >= Duration::from_millis(50) {
                tracing::warn!(
                    elapsed_ms = elapsed.as_secs_f64() * 1000.0,
                    "moving local viewer HWND blocked"
                );
            }
        }
    }
    if response.drag_stopped() || !primary_down {
        if state.start.is_some() {
            tracing::debug!("local title drag ended");
        }
        if state.start.take().is_some() {
            let _ = unsafe { ReleaseCapture() };
        }
    }
}

pub(crate) fn title_bar_height_pixels(window: &Window) -> u32 {
    if window.fullscreen().is_some() {
        0
    } else {
        // Include the egui frame's padding and stroke. Round outward so a
        // fractional-DPI border cannot paint over the first video row.
        title_bar_height_at_scale(window.scale_factor())
    }
}

fn title_bar_height_at_scale(scale: f64) -> u32 {
    (f64::from(title_bar_height()) * scale).ceil().max(1.0) as u32
}

#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct WindowResizeState {
    start: Option<WindowResizeStart>,
    pub(crate) min_size: Option<PhysicalSize<u32>>,
    pub(crate) requested_render_size: Option<PhysicalSize<u32>>,
}

#[derive(Clone, Copy, Debug)]
struct WindowResizeStart {
    cursor: POINT,
    origin: PhysicalPosition<i32>,
    size: PhysicalSize<u32>,
    direction: ResizeDirection,
}

pub(crate) fn update_nonmodal_window_resize(
    ctx: &egui::Context,
    window: &Window,
    response: &egui::Response,
    direction: ResizeDirection,
    state: &mut WindowResizeState,
    aspect: Option<(u32, u32)>,
) {
    if response.drag_started() {
        let mut cursor = POINT::default();
        if unsafe { GetCursorPos(&raw mut cursor) }.is_ok()
            && let Ok(origin) = window.outer_position()
        {
            state.start = Some(WindowResizeStart {
                cursor,
                origin,
                size: window.inner_size(),
                direction,
            });
            if let Ok(hwnd) = window_hwnd(window) {
                unsafe { SetCapture(hwnd) };
            }
        }
    }
    let primary_down = ctx.input(|input| input.pointer.primary_down());
    if primary_down && let Some(start) = state.start {
        let mut cursor = POINT::default();
        if unsafe { GetCursorPos(&raw mut cursor) }.is_ok() {
            state.requested_render_size = Some(apply_absolute_resize(
                window,
                start,
                cursor.x - start.cursor.x,
                cursor.y - start.cursor.y,
                aspect,
                state.min_size.unwrap_or(PhysicalSize::new(640, 400)),
            ));
        }
    }
    if response.drag_stopped() || !primary_down {
        if state.start.take().is_some() {
            let _ = unsafe { ReleaseCapture() };
        }
    }
}

fn apply_absolute_resize(
    window: &Window,
    start: WindowResizeStart,
    dx: i32,
    dy: i32,
    aspect: Option<(u32, u32)>,
    min_size: PhysicalSize<u32>,
) -> PhysicalSize<u32> {
    let west = matches!(
        start.direction,
        ResizeDirection::West | ResizeDirection::NorthWest | ResizeDirection::SouthWest
    );
    let east = matches!(
        start.direction,
        ResizeDirection::East | ResizeDirection::NorthEast | ResizeDirection::SouthEast
    );
    let north = matches!(
        start.direction,
        ResizeDirection::North | ResizeDirection::NorthEast | ResizeDirection::NorthWest
    );
    let south = matches!(
        start.direction,
        ResizeDirection::South | ResizeDirection::SouthEast | ResizeDirection::SouthWest
    );
    let mut width = start.size.width as i32
        + if east {
            dx
        } else if west {
            -dx
        } else {
            0
        };
    let mut height = start.size.height as i32
        + if south {
            dy
        } else if north {
            -dy
        } else {
            0
        };
    let min_width = min_size.width as i32;
    let min_height = min_size.height as i32;
    width = width.max(min_width);
    height = height.max(min_height);

    if let Some((video_width, video_height)) = aspect {
        let title_height = title_bar_height_pixels(window) as i32;
        let horizontal_only = (west || east) && !north && !south;
        let vertical_only = (north || south) && !west && !east;
        let width_drives = horizontal_only
            || (!vertical_only
                && i64::from(dx.abs()) * i64::from(video_height)
                    >= i64::from(dy.abs()) * i64::from(video_width));
        if width_drives {
            height = ((i64::from(width) * i64::from(video_height) + i64::from(video_width) / 2)
                / i64::from(video_width)) as i32
                + title_height;
            height = height.max(min_height);
        } else {
            let content_height = (height - title_height).max(1);
            width = ((i64::from(content_height) * i64::from(video_width)
                + i64::from(video_height) / 2)
                / i64::from(video_height)) as i32;
            width = width.max(min_width);
        }
    }

    let left = if west {
        start.origin.x + start.size.width as i32 - width
    } else {
        start.origin.x
    };
    let top = if north {
        start.origin.y + start.size.height as i32 - height
    } else {
        start.origin.y
    };
    window.set_outer_position(PhysicalPosition::new(left, top));
    let target = PhysicalSize::new(width as u32, height as u32);
    let _ = window.request_inner_size(target);
    target
}
