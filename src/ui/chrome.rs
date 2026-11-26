//! Shared borderless window chrome and native fallback background.
use super::controls::{ViewerCaptionIcon as TitleIcon, viewer_caption_button as title_icon_button};
#[cfg(windows)]
use super::gfx::window_hwnd;
use winit::window::{ResizeDirection, Window};

#[cfg(windows)]
#[path = "chrome_native_windows.rs"]
mod native;
#[cfg(not(windows))]
#[path = "chrome_native_linux.rs"]
mod native;

pub(crate) use native::{
    WindowMoveState, WindowResizeState, cancel_pointer_operation, configure_dwm_window,
    set_fullscreen, update_nonmodal_window_move, update_nonmodal_window_resize,
};
// Windows positions borderless windows itself and keeps them below the work
// area's top; X11 and Wayland window managers do that on their own.
#[cfg(windows)]
pub(crate) use native::constrain_drag_position;

pub(crate) struct TitleBarAlert {
    pub source: String,
    pub duration: String,
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

/// 同 [`window_title_bar`]，只是标题那一格让调用方自己画。
///
/// 播放器窗口要在这儿放设备切换菜单（Windows 的 caption 也是这么做的），而设备列表
/// 只有它自己知道。除了这一格，拖动、品牌图标、窗口按钮都照旧。
///
/// 目前只有 Linux 的播放器用它：Windows 播放器整条 caption 都是自己画的
/// （`windows_presenter/chrome.rs`），不经过 [`window_title_bar`]。
#[cfg_attr(windows, allow(dead_code, reason = "Windows 播放器自己画 caption"))]
pub(crate) fn window_title_bar_with_identity(
    ui: &mut egui::Ui,
    window: &Window,
    alias: &str,
    move_state: Option<&mut WindowMoveState>,
    tooltips: bool,
    identity: &mut dyn FnMut(&mut egui::Ui),
) -> bool {
    window_title_bar_impl(
        ui,
        window,
        alias,
        move_state,
        tooltips,
        None,
        Some(identity),
    )
}

pub(crate) fn window_title_bar_alert(
    ui: &mut egui::Ui,
    window: &Window,
    alias: &str,
    move_state: Option<&mut WindowMoveState>,
    tooltips: bool,
    alert: Option<&TitleBarAlert>,
) -> bool {
    window_title_bar_impl(ui, window, alias, move_state, tooltips, alert, None)
}

fn window_title_bar_impl(
    ui: &mut egui::Ui,
    window: &Window,
    alias: &str,
    move_state: Option<&mut WindowMoveState>,
    tooltips: bool,
    alert: Option<&TitleBarAlert>,
    identity: Option<&mut dyn FnMut(&mut egui::Ui)>,
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
    // 调用方自己画标题那一格时（播放器的设备切换按钮），拖动区和 Windows 的
    // `player-title-drag` 一样从那一格右边开始，免得按住设备按钮一拖就拖动了窗口；
    // 那一格要先画出来才知道有多宽，所以拖动区放到后面登记（两者不重叠，先后无所谓）。
    let mut move_state = move_state;
    let has_identity = identity.is_some();
    if !has_identity {
        title_drag(ui, window, caption_rect, move_state.take());
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
    // 让调用方画标题时（设备切换按钮），这一格也要像 Windows 的 caption 那样封顶，
    // 否则长别名会一直画到屏标签那一行上。
    let brand_rect = if identity.is_some() {
        egui::Rect::from_min_max(
            brand_rect.min,
            egui::pos2(
                (brand_rect.left()
                    + crate::ui::theme::VIEWER_IDENTITY_MAX_WIDTH.min(brand_rect.width()))
                .min(brand_rect.right()),
                brand_rect.max.y,
            ),
        )
    } else {
        brand_rect
    };
    let mut caption = ui.new_child(
        egui::UiBuilder::new()
            .max_rect(brand_rect)
            .layout(egui::Layout::left_to_right(egui::Align::Center)),
    );
    caption.set_clip_rect(brand_rect);
    paint_brand_logo(&mut caption);
    match identity {
        Some(draw) => {
            draw(&mut caption);
            let start = caption
                .min_rect()
                .right()
                .clamp(caption_rect.left(), caption_rect.right());
            title_drag(
                ui,
                window,
                egui::Rect::from_min_max(egui::pos2(start, caption_rect.top()), caption_rect.max),
                move_state.take(),
            );
        }
        None => {
            caption.add(
                egui::Label::new(
                    egui::RichText::new(alias)
                        .size(crate::ui::theme::SMALL)
                        .color(crate::ui::theme::TEXT),
                )
                .truncate(),
            );
        }
    }
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

/// 标题栏上能拖动窗口的那一片。
fn title_drag(
    ui: &mut egui::Ui,
    window: &Window,
    area: egui::Rect,
    move_state: Option<&mut WindowMoveState>,
) {
    let drag = ui.interact(
        area,
        ui.id().with("window-title-drag"),
        egui::Sense::click_and_drag(),
    );
    if let Some(state) = move_state {
        update_nonmodal_window_move(ui.ctx(), window, &drag, state);
    } else if handle_title_drag(window, &drag) {
        let _ = window.drag_window();
    }
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
            } else if window.fullscreen().is_some() {
                "退出全屏"
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
        if !tooltips {
            ""
        } else if window.fullscreen().is_some() {
            // Only a player window goes fullscreen.
            "关闭当前串流窗口"
        } else {
            "关闭"
        },
    )
    .clicked()
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
