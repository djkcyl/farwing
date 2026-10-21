//! Shared control appearance for the center, viewer menus and node editor.
use super::theme::{self, HOVER, LINE, MUTED, SURFACE, TEXT};
use egui::{Color32, RichText, Stroke, vec2};
mod audio_quality;
mod diagnostics;
pub(crate) use audio_quality::{
    audio_quality, audio_quality_segments, remote_audio_quality, remote_audio_quality_inline,
};
mod dialogs;
pub(crate) use diagnostics::{
    diagnostics_action, diagnostics_empty, diagnostics_label, diagnostics_row, diagnostics_table,
};
mod host_notice;
mod inputs;
pub(crate) use host_notice::host_notice;
mod input_rate;
pub(crate) use input_rate::input_rate_warning;
mod notices;
pub(crate) use inputs::{number_input, singleline};
pub(crate) use notices::{
    clear_notice, notice, observe_form_notice, observe_notice, observe_notice_action,
    progress_notice, show_notices,
};
pub(crate) mod file_icons;
pub(crate) mod files;
pub(crate) use dialogs::{DialogAction, DialogIcon, dialog_actions, dialog_header};
mod mapping;
mod performance;
mod updates;
mod viewer_caption;
pub(crate) use mapping::{
    MappingRow, MappingRowAction, mapping_empty, mapping_row, mapping_table_header,
};
pub(crate) use performance::{
    PerformanceTrace, metric_pair, metric_pair_colored, performance_frame, performance_header,
    performance_trace,
};
pub(crate) use updates::update_actions;
pub(crate) use viewer_caption::{
    ViewerCaptionIcon, viewer_caption_button, viewer_fullscreen_handle, viewer_participants_button,
};

pub const HEIGHT: f32 = theme::CONTROL_HEIGHT;
pub const COMPACT_HEIGHT: f32 = theme::COMPACT_HEIGHT;
pub const ACCENT: Color32 = theme::ACCENT;

/// Pages use the whole remaining viewport for both scrolling and content.
/// Preserve page IDs so each page retains independent scrolling state.
pub(crate) fn page_scroll(id: impl egui::AsIdSalt) -> egui::ScrollArea {
    egui::ScrollArea::vertical()
        .id_salt(id)
        .auto_shrink([false, false])
}

pub(crate) fn paint_file_icon(p: &egui::Painter, r: egui::Rect, color: Color32, folder: bool) {
    let c = r.center();
    let s = Stroke::new(theme::ICON_STROKE, color);
    let points = if folder {
        vec![
            c + vec2(-8., -5.),
            c + vec2(-2., -5.),
            c + vec2(0., -2.),
            c + vec2(8., -2.),
            c + vec2(8., 7.),
            c + vec2(-8., 7.),
            c + vec2(-8., -5.),
        ]
    } else {
        vec![
            c + vec2(-6., -8.),
            c + vec2(2., -8.),
            c + vec2(6., -4.),
            c + vec2(6., 8.),
            c + vec2(-6., 8.),
            c + vec2(-6., -8.),
        ]
    };
    p.add(egui::Shape::line(points, s));
}

pub(crate) fn device_status_badge(
    ui: &mut egui::Ui,
    rect: egui::Rect,
    label: &str,
    color: Color32,
) {
    ui.painter()
        .rect_filled(rect, theme::CONTROL_RADIUS, color.gamma_multiply(0.09));
    ui.painter()
        .circle_filled(rect.left_center() + vec2(10.0, 0.0), 2.5, color);
    let mut badge = ui.new_child(
        egui::UiBuilder::new()
            .max_rect(egui::Rect::from_min_max(
                rect.min + vec2(18.0, 2.0),
                rect.max - vec2(4.0, 2.0),
            ))
            .layout(egui::Layout::left_to_right(egui::Align::Center)),
    );
    badge.add(egui::Label::new(RichText::new(label).size(theme::SMALL).color(color)).truncate());
}

pub(crate) fn paint_port_mapping_icon(p: &egui::Painter, rect: egui::Rect, color: Color32) {
    let center = rect.center();
    let scale = rect.width().min(rect.height()) / 20.0;
    let point = |x, y| center + vec2(x, y) * scale;
    let stroke = Stroke::new(theme::ICON_STROKE, color);
    for direction in [1.0, -1.0] {
        let y = -4.0 * direction;
        p.line_segment(
            [point(-8.0 * direction, y), point(8.0 * direction, y)],
            stroke,
        );
        p.add(egui::Shape::line(
            vec![
                point(4.0 * direction, y - 4.0),
                point(8.0 * direction, y),
                point(4.0 * direction, y + 4.0),
            ],
            stroke,
        ));
    }
}

pub(crate) fn switch(ui: &mut egui::Ui, value: &mut bool) -> egui::Response {
    sized_switch(ui, value, vec2(34.0, 20.0))
}
pub(crate) fn service_switch(ui: &mut egui::Ui, value: &mut bool) -> egui::Response {
    sized_switch(ui, value, theme::SERVICE_SWITCH_SIZE)
}
fn sized_switch(ui: &mut egui::Ui, value: &mut bool, size: egui::Vec2) -> egui::Response {
    let (rect, mut response) = ui.allocate_exact_size(size, egui::Sense::click());
    if response.clicked() {
        *value = !*value;
        response.mark_changed();
    }
    ui.painter().rect_filled(
        rect,
        size.y / 2.0,
        if *value { theme::ACCENT } else { theme::LINE },
    );
    ui.painter().circle_filled(
        egui::pos2(
            if *value {
                rect.right() - size.y / 2.0
            } else {
                rect.left() + size.y / 2.0
            },
            rect.center().y,
        ),
        size.y / 2.0 - 3.0,
        theme::TEXT,
    );
    response
}

pub(crate) fn connection_wallpaper(ui: &egui::Ui, texture: &egui::TextureHandle, details: bool) {
    let rect = ui.available_rect_before_wrap();
    let size = texture.size_vec2();
    if rect.width() <= 0.0 || rect.height() <= 0.0 || size.x <= 0.0 || size.y <= 0.0 {
        return;
    }
    let scale = (rect.width() / size.x).max(rect.height() / size.y);
    let uv = egui::Rect::from_center_size(egui::pos2(0.5, 0.5), rect.size() / (size * scale));
    ui.painter().image(texture.id(), rect, uv, Color32::WHITE);
    let dim = if details {
        theme::CONNECTION_WALLPAPER_DETAIL_DIM
    } else {
        theme::CONNECTION_WALLPAPER_DIM
    };
    ui.painter().rect_filled(
        rect,
        0.0,
        Color32::from_rgba_unmultiplied(theme::BG.r(), theme::BG.g(), theme::BG.b(), dim),
    );
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum ShortcutRowAction {
    None,
    Edit,
    Clear,
    Cancel,
}

pub(crate) fn shortcut_row(
    ui: &mut egui::Ui,
    id: usize,
    label: &str,
    binding: Option<&str>,
    recording: bool,
    pending: Option<&str>,
    last: bool,
) -> ShortcutRowAction {
    use egui::{Align2, FontId, Rect, Sense, pos2};
    let (rect, _) = ui.allocate_exact_size(
        vec2(ui.available_width(), theme::SHORTCUT_ROW_HEIGHT),
        Sense::hover(),
    );
    let painter = ui.painter_at(rect);
    painter.text(
        rect.left_center() + vec2(16.0, 0.0),
        Align2::LEFT_CENTER,
        label,
        FontId::proportional(theme::BODY),
        TEXT,
    );
    if !last {
        painter.line_segment(
            [
                rect.left_bottom() + vec2(16.0, 0.0),
                rect.right_bottom() - vec2(16.0, 0.0),
            ],
            Stroke::new(0.5, LINE),
        );
    }
    let keys = Rect::from_min_max(
        pos2(
            rect.left() + theme::SHORTCUT_LABEL_WIDTH,
            rect.center().y - 19.0,
        ),
        pos2(rect.right() - 56.0, rect.center().y + 19.0),
    );
    let clear = Rect::from_center_size(
        pos2(rect.right() - 28.0, rect.center().y),
        vec2(COMPACT_HEIGHT, COMPACT_HEIGHT),
    );
    let response = ui
        .interact(keys, ui.id().with((id, "binding")), Sense::click())
        .on_hover_cursor(egui::CursorIcon::PointingHand);
    response.widget_info(|| {
        egui::WidgetInfo::labeled(egui::WidgetType::Button, true, format!("修改{label}快捷键"))
    });
    if recording || response.hovered() {
        painter.rect_filled(
            keys,
            theme::CONTROL_RADIUS,
            if recording { theme::SELECTED } else { SURFACE },
        );
    }
    if recording {
        painter.rect_stroke(
            keys,
            theme::CONTROL_RADIUS,
            Stroke::new(1.0, ACCENT),
            egui::StrokeKind::Inside,
        );
        painter.text(
            keys.center() + vec2(0.0, -7.0),
            Align2::CENTER_CENTER,
            pending.unwrap_or("请按快捷键"),
            FontId::proportional(theme::SMALL),
            TEXT,
        );
        painter.text(
            keys.center() + vec2(0.0, 9.0),
            Align2::CENTER_CENTER,
            "Esc 取消",
            FontId::proportional(theme::TINY),
            MUTED,
        );
    } else if let Some(binding) = binding {
        let key_painter = ui.painter_at(keys);
        let caps: Vec<_> = binding
            .split(" + ")
            .map(|key| {
                let galley = key_painter.layout_no_wrap(
                    key.into(),
                    FontId::proportional(theme::SMALL),
                    TEXT,
                );
                let width = (galley.size().x + 16.0).max(28.0);
                (galley, width)
            })
            .collect();
        let total = caps.iter().map(|(_, width)| width).sum::<f32>()
            + 6.0 * caps.len().saturating_sub(1) as f32;
        let mut x = keys.center().x - total * 0.5;
        for (galley, width) in caps {
            let cap = Rect::from_min_size(pos2(x, keys.center().y - 13.0), vec2(width, 26.0));
            key_painter.rect_filled(cap, theme::CONTROL_RADIUS, SURFACE);
            key_painter.rect_stroke(
                cap,
                theme::CONTROL_RADIUS,
                Stroke::new(1.0, LINE),
                egui::StrokeKind::Inside,
            );
            key_painter.galley(cap.center() - galley.size() * 0.5, galley, TEXT);
            x += width + 6.0;
        }
    } else {
        painter.text(
            keys.center(),
            Align2::CENTER_CENTER,
            "未绑定",
            FontId::proportional(theme::SMALL),
            MUTED,
        );
    }
    let clear_response = ui
        .scope_builder(egui::UiBuilder::new().max_rect(clear), |ui| {
            ui.add_enabled_ui(binding.is_some(), |ui| {
                close_button(ui, "解绑快捷键", COMPACT_HEIGHT)
            })
            .inner
        })
        .inner;
    let action = if clear_response.clicked() {
        ShortcutRowAction::Clear
    } else if response.clicked() {
        if recording {
            ShortcutRowAction::Cancel
        } else {
            ShortcutRowAction::Edit
        }
    } else {
        ShortcutRowAction::None
    };
    ui.advance_cursor_after_rect(rect);
    action
}

pub fn paint_keyboard(painter: &egui::Painter, rect: egui::Rect, color: Color32) {
    let center = rect.center();
    painter.rect_stroke(
        egui::Rect::from_center_size(center, vec2(21.0, 15.0)),
        2.0,
        Stroke::new(1.5, color),
        egui::StrokeKind::Inside,
    );
    for y in [-3.5, 0.0] {
        for x in [-6.0, -2.0, 2.0, 6.0] {
            painter.rect_filled(
                egui::Rect::from_center_size(center + vec2(x, y), vec2(1.8, 1.8)),
                0.0,
                color,
            );
        }
    }
    painter.line_segment(
        [center + vec2(-4.0, 4.0), center + vec2(4.0, 4.0)],
        Stroke::new(1.5, color),
    );
}

pub fn connection_indicator(ui: &mut egui::Ui, failed: bool, ready: bool) {
    let (rect, _) = ui.allocate_exact_size(vec2(40.0, 40.0), egui::Sense::hover());
    if failed {
        ui.painter()
            .circle_stroke(rect.center(), 17.0, Stroke::new(1.5, theme::RED));
        paint_close(ui.painter(), rect.shrink(8.0), theme::RED);
    } else if ready {
        let center = rect.center();
        ui.painter()
            .circle_stroke(center, 17.0, Stroke::new(1.5, theme::GREEN));
        ui.painter().add(egui::Shape::line(
            vec![
                center + vec2(-7.0, 0.0),
                center + vec2(-2.0, 5.0),
                center + vec2(8.0, -6.0),
            ],
            Stroke::new(2.0, theme::GREEN),
        ));
    } else {
        ui.put(rect, egui::Spinner::new().size(32.0).color(ACCENT));
    }
}

pub fn quiet_button(label: &str) -> egui::Button<'_> {
    egui::Button::new(RichText::new(label).size(theme::SMALL).color(MUTED))
        .frame(false)
        .min_size(vec2(84.0, HEIGHT))
}

pub fn connection_details_header(ui: &mut egui::Ui, elapsed: &str) -> bool {
    let (rect, _) = ui.allocate_exact_size(vec2(ui.available_width(), 36.0), egui::Sense::hover());
    ui.painter().text(
        rect.left_center() + vec2(12.0, 0.0),
        egui::Align2::LEFT_CENTER,
        "连接详情",
        egui::FontId::proportional(theme::COMPACT_TEXT),
        TEXT,
    );
    ui.painter().text(
        rect.right_center() - vec2(106.0, 0.0),
        egui::Align2::RIGHT_CENTER,
        elapsed,
        egui::FontId::proportional(theme::SMALL),
        MUTED,
    );
    let close_rect =
        egui::Rect::from_min_max(egui::pos2(rect.right() - 94.0, rect.top()), rect.max);
    ui.put(close_rect, quiet_button("收起详情")).clicked()
}

pub fn connection_stage_row(
    ui: &mut egui::Ui,
    label: &str,
    active: bool,
    complete: bool,
    failed: bool,
) {
    let (rect, _) = ui.allocate_exact_size(vec2(ui.available_width(), 40.0), egui::Sense::hover());
    if active {
        ui.painter()
            .rect_filled(rect, theme::CONTROL_RADIUS, SURFACE);
    }
    let color = if failed {
        theme::RED
    } else if active {
        ACCENT
    } else {
        MUTED
    };
    let center = rect.left_center() + vec2(16.0, 0.0);
    if complete {
        ui.painter().add(egui::Shape::line(
            vec![
                center + vec2(-4.0, 0.0),
                center + vec2(-1.0, 3.0),
                center + vec2(5.0, -4.0),
            ],
            Stroke::new(1.3, color),
        ));
    } else {
        ui.painter()
            .circle_stroke(center, 4.0, Stroke::new(1.3, color));
    }
    ui.painter().text(
        rect.left_center() + vec2(34.0, 0.0),
        egui::Align2::LEFT_CENTER,
        label,
        egui::FontId::proportional(theme::COMPACT_TEXT),
        if active { TEXT } else { MUTED },
    );
}

pub fn connection_detail_row(
    ui: &mut egui::Ui,
    elapsed: &str,
    title: &str,
    detail: &str,
    current: bool,
    failed: bool,
) -> egui::Rect {
    let width = ui.available_width();
    let title_left = theme::CONNECTION_DETAIL_TIME_WIDTH + 24.0;
    let detail_left = title_left + theme::CONNECTION_DETAIL_TITLE_WIDTH + 16.0;
    let title = ui.painter().layout(
        title.into(),
        egui::FontId::proportional(theme::SMALL),
        if failed { theme::RED } else { TEXT },
        theme::CONNECTION_DETAIL_TITLE_WIDTH,
    );
    let detail = ui.painter().layout(
        detail.into(),
        egui::FontId::proportional(theme::SMALL),
        MUTED,
        (width - detail_left - 16.0).max(1.0),
    );
    let height = title.size().y.max(detail.size().y).max(20.0) + 16.0;
    let (rect, _) = ui.allocate_exact_size(vec2(width, height), egui::Sense::hover());
    if current {
        ui.painter()
            .rect_filled(rect, theme::CONTROL_RADIUS, SURFACE);
    }
    ui.painter().text(
        rect.min + vec2(theme::CONNECTION_DETAIL_TIME_WIDTH, 10.0),
        egui::Align2::RIGHT_TOP,
        elapsed,
        egui::FontId::monospace(theme::SMALL),
        MUTED,
    );
    if current || failed {
        ui.painter().circle_filled(
            rect.min + vec2(10.0, 16.0),
            2.5,
            if failed { theme::RED } else { ACCENT },
        );
    }
    ui.painter()
        .galley(rect.min + vec2(title_left, 8.0), title, TEXT);
    ui.painter()
        .galley(rect.min + vec2(detail_left, 8.0), detail, MUTED);
    rect
}

pub fn viewer_device_button_width(ui: &egui::Ui, label: &str) -> f32 {
    let label_width = ui
        .painter()
        .layout_no_wrap(
            label.to_owned(),
            egui::FontId::proportional(theme::COMPACT_TEXT),
            TEXT,
        )
        .size()
        .x;
    label_width + 30.0
}

/// A compact caption action; its background follows the label, not the entire title column.
pub fn viewer_device_button(ui: &mut egui::Ui, label: &str) -> egui::Response {
    let (rect, response) = ui.allocate_exact_size(
        vec2(
            viewer_device_button_width(ui, label)
                .min(ui.available_width())
                .max(24.0),
            theme::COMPACT_HEIGHT,
        ),
        egui::Sense::click(),
    );
    let open = egui::Popup::is_id_open(ui.ctx(), egui::Popup::default_response_id(&response));
    if response.hovered() || response.is_pointer_button_down_on() || open {
        ui.painter()
            .rect_filled(rect, theme::CONTROL_RADIUS, SURFACE);
    }
    let text_rect = rect.shrink2(vec2(6.0, 0.0));
    let mut text_ui = ui.new_child(
        egui::UiBuilder::new()
            .max_rect(egui::Rect::from_min_max(
                text_rect.min,
                egui::pos2(text_rect.right() - 16.0, text_rect.bottom()),
            ))
            .layout(egui::Layout::left_to_right(egui::Align::Center)),
    );
    text_ui.add(
        egui::Label::new(RichText::new(label).size(theme::COMPACT_TEXT).color(TEXT)).truncate(),
    );
    let center = egui::pos2(rect.right() - 12.0, rect.center().y);
    ui.painter().add(egui::Shape::line(
        vec![
            center + vec2(-3.0, -1.5),
            center + vec2(0.0, 1.5),
            center + vec2(3.0, -1.5),
        ],
        Stroke::new(1.0, MUTED),
    ));
    response.on_hover_text("切换在线设备")
}

pub fn viewer_caption_separator(ui: &egui::Ui, center: egui::Pos2) {
    ui.painter().line_segment(
        [center - vec2(0.0, 8.0), center + vec2(0.0, 8.0)],
        Stroke::new(1.0, LINE),
    );
}

pub fn device_menu_header(ui: &mut egui::Ui, busy: bool) -> bool {
    let (rect, _) = ui.allocate_exact_size(
        vec2(ui.available_width(), theme::MENU_HEIGHT),
        egui::Sense::hover(),
    );
    ui.painter().text(
        rect.left_center() + vec2(10.0, 0.0),
        egui::Align2::LEFT_CENTER,
        "切换设备",
        egui::FontId::proportional(theme::SMALL),
        MUTED,
    );
    let button =
        egui::Rect::from_center_size(rect.right_center() - vec2(14.0, 0.0), vec2(26.0, 26.0));
    if busy {
        ui.put(button, egui::Spinner::new().size(14.0));
        return false;
    }
    let response = ui.interact(
        button,
        ui.id().with("refresh-devices"),
        egui::Sense::click(),
    );
    if response.hovered() {
        ui.painter()
            .rect_filled(button, theme::CONTROL_RADIUS, HOVER);
    }
    let center = button.center();
    let points = (0..=20)
        .map(|i| {
            let angle = 0.3 + i as f32 / 20.0 * std::f32::consts::TAU * 0.8;
            center + vec2(angle.cos(), angle.sin()) * 5.5
        })
        .collect::<Vec<_>>();
    let end = *points.last().unwrap();
    let stroke = Stroke::new(1.3, if response.hovered() { TEXT } else { MUTED });
    ui.painter().add(egui::Shape::line(points, stroke));
    ui.painter().add(egui::Shape::line(
        vec![end + vec2(-3.0, -1.0), end, end + vec2(-1.0, 3.0)],
        stroke,
    ));
    response.on_hover_text("刷新设备列表").clicked()
}

pub fn device_menu_row(
    ui: &mut egui::Ui,
    label: &str,
    device_id: &str,
    status: &str,
    status_color: Color32,
    selected: bool,
    enabled: bool,
) -> egui::Response {
    let enabled = enabled && ui.is_enabled();
    let (rect, response) = ui.allocate_exact_size(
        vec2(ui.available_width(), theme::DEVICE_MENU_ROW_HEIGHT),
        if enabled {
            egui::Sense::click()
        } else {
            egui::Sense::hover()
        },
    );
    if selected || (enabled && response.hovered()) {
        ui.painter().rect_filled(
            rect,
            theme::CONTROL_RADIUS,
            if selected { SURFACE } else { HOVER },
        );
    }
    let color = if selected || enabled { TEXT } else { MUTED };
    let center = rect.left_center() + vec2(19.0, -1.0);
    let stroke = Stroke::new(1.2, if selected { ACCENT } else { MUTED });
    ui.painter().rect_stroke(
        egui::Rect::from_center_size(center, vec2(16.0, 11.0)),
        2.0,
        stroke,
        egui::StrokeKind::Inside,
    );
    ui.painter()
        .line_segment([center + vec2(0.0, 5.5), center + vec2(0.0, 8.5)], stroke);
    ui.painter()
        .line_segment([center + vec2(-4.0, 8.5), center + vec2(4.0, 8.5)], stroke);
    let status_width = if selected {
        16.0
    } else {
        ui.painter()
            .layout_no_wrap(
                status.into(),
                egui::FontId::proportional(theme::TINY),
                status_color,
            )
            .size()
            .x
    };
    let mut job = egui::text::LayoutJob::simple_singleline(
        label.into(),
        egui::FontId::proportional(theme::COMPACT_TEXT),
        color,
    );
    job.wrap.max_width = (rect.width() - 52.0 - status_width).max(0.0);
    job.wrap.max_rows = 1;
    job.wrap.break_anywhere = true;
    let text = ui.painter().layout_job(job);
    ui.painter().galley(
        rect.left_center() + vec2(38.0, -text.size().y - 1.0),
        text,
        color,
    );
    ui.painter().text(
        rect.left_center() + vec2(38.0, 2.0),
        egui::Align2::LEFT_TOP,
        format!("ID {device_id}"),
        egui::FontId::proportional(theme::TINY),
        MUTED,
    );
    if selected {
        let center = rect.right_center() - vec2(17.0, 0.0);
        ui.painter().add(egui::Shape::line(
            vec![
                center + vec2(-3.5, 0.0),
                center + vec2(-0.5, 3.0),
                center + vec2(4.5, -3.0),
            ],
            Stroke::new(1.5, ACCENT),
        ));
    } else {
        ui.painter().text(
            rect.right_center() - vec2(10.0, 0.0),
            egui::Align2::RIGHT_CENTER,
            status,
            egui::FontId::proportional(theme::TINY),
            status_color,
        );
    }
    response.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, enabled, label));
    response
}

pub fn configure(style: &mut egui::Style, height: f32) {
    theme::typography(style, height);
    style.drag_value_text_style = egui::TextStyle::Body;
    style.spacing.interact_size.y = height;
    style.spacing.button_padding = if height >= HEIGHT {
        vec2(12.0, 6.0)
    } else {
        vec2(8.0, 4.0)
    };
    style.visuals.extreme_bg_color = theme::SIDEBAR;
    style.visuals.override_text_color = Some(TEXT);
    style.visuals.weak_text_color = Some(MUTED);
    style.visuals.selection.bg_fill = theme::SELECTION;
    style.visuals.selection.stroke = Stroke::new(1.0, ACCENT);
    for widgets in [
        &mut style.visuals.widgets.inactive,
        &mut style.visuals.widgets.open,
    ] {
        widgets.bg_fill = SURFACE;
        widgets.weak_bg_fill = SURFACE;
        widgets.bg_stroke = Stroke::new(1.0, LINE);
        widgets.fg_stroke.color = TEXT;
        widgets.corner_radius = theme::CONTROL_RADIUS.into();
    }
    let hovered = &mut style.visuals.widgets.hovered;
    hovered.bg_fill = theme::HOVER;
    hovered.weak_bg_fill = hovered.bg_fill;
    hovered.bg_stroke = Stroke::new(1.0, theme::BORDER_FOCUS);
    hovered.fg_stroke.color = TEXT;
    hovered.corner_radius = theme::CONTROL_RADIUS.into();
    let active = &mut style.visuals.widgets.active;
    active.bg_fill = ACCENT;
    active.weak_bg_fill = theme::ACTIVE;
    active.bg_stroke = Stroke::new(1.0, ACCENT);
    active.fg_stroke.color = TEXT;
    active.corner_radius = theme::CONTROL_RADIUS.into();
}

pub fn dialog_frame() -> egui::Frame {
    egui::Frame::new()
        .fill(theme::BG)
        .stroke(Stroke::new(1.0, LINE))
        .corner_radius(theme::PANEL_RADIUS)
        .inner_margin(egui::Margin::same(theme::DIALOG_MARGIN))
}

/// Release text is displayed locally; it never loads remote images or HTML.
pub(crate) fn release_notes(ui: &mut egui::Ui, notes: &str) {
    if notes.trim().is_empty() {
        ui.label(RichText::new("暂无更新说明").color(MUTED));
        return;
    }
    for line in notes.lines() {
        let line = line.trim_end();
        if line.is_empty() {
            ui.add_space(6.0);
            continue;
        }
        let heading = line.trim_start_matches('#');
        if heading.len() != line.len() && heading.starts_with(' ') {
            ui.add(
                egui::Label::new(RichText::new(heading.trim()).strong().size(theme::BODY))
                    .wrap()
                    .selectable(true),
            );
        } else if let Some(item) = line.strip_prefix("- ").or_else(|| line.strip_prefix("* ")) {
            ui.horizontal_top(|ui| {
                ui.label(RichText::new("•").color(MUTED));
                ui.add(egui::Label::new(item).wrap().selectable(true));
            });
        } else {
            ui.add(egui::Label::new(line).wrap().selectable(true));
        }
    }
}

pub fn primary(label: &str) -> egui::Button<'_> {
    egui::Button::new(RichText::new(label).color(Color32::WHITE))
        .fill(ACCENT)
        .stroke(Stroke::NONE)
        .min_size(vec2(64.0, HEIGHT))
}

pub(crate) fn centered_button(label: &str) -> egui::Button<'_> {
    egui::Button::new((egui::Atom::grow(), label, egui::Atom::grow()))
        .gap(0.0)
        .min_size(vec2(64.0, theme::CONTROL_HEIGHT))
}

pub(crate) fn centered_primary(label: &str) -> egui::Button<'_> {
    egui::Button::new((
        egui::Atom::grow(),
        RichText::new(label).color(Color32::WHITE),
        egui::Atom::grow(),
    ))
    .gap(0.0)
    .fill(ACCENT)
    .stroke(Stroke::NONE)
    .min_size(vec2(64.0, HEIGHT))
}

pub(crate) fn section_frame() -> egui::Frame {
    egui::Frame::new()
        .fill(SURFACE)
        .stroke(Stroke::new(1.0, LINE))
        .corner_radius(theme::PANEL_RADIUS)
}

pub(crate) fn visibility_button(ui: &mut egui::Ui, visible: &mut bool) -> egui::Response {
    let (_, response) = icon_button_area(ui, HEIGHT);
    response.widget_info(|| {
        egui::WidgetInfo::labeled(
            egui::WidgetType::Button,
            ui.is_enabled(),
            if *visible {
                "隐藏访问码"
            } else {
                "显示访问码"
            },
        )
    });
    if response.clicked() {
        *visible = !*visible;
    }
    let center = response.rect.center();
    let color = if !ui.is_enabled() {
        theme::DISABLED
    } else if response.hovered() || response.has_focus() {
        TEXT
    } else {
        MUTED
    };
    let stroke = Stroke::new(theme::ICON_STROKE, color);
    let points = [
        (-9., 0.),
        (-5., -5.),
        (0., -6.),
        (5., -5.),
        (9., 0.),
        (5., 5.),
        (0., 6.),
        (-5., 5.),
    ]
    .map(|(x, y)| center + vec2(x, y))
    .to_vec();
    ui.painter().add(egui::Shape::closed_line(points, stroke));
    ui.painter().circle_stroke(center, 2.5, stroke);
    if !*visible {
        ui.painter()
            .line_segment([center + vec2(-8., 8.), center + vec2(8., -8.)], stroke);
    }
    response
}

/// A full button target with a vector chevron, independent of font glyphs.
pub fn back_button(ui: &mut egui::Ui, label: &str) -> egui::Response {
    let text =
        ui.painter()
            .layout_no_wrap(label.into(), egui::FontId::proportional(theme::BODY), TEXT);
    let response = ui.add_sized(
        vec2(text.size().x + 48.0, HEIGHT),
        egui::Button::new("").frame(false),
    );
    response.widget_info(|| {
        egui::WidgetInfo::labeled(egui::WidgetType::Button, ui.is_enabled(), label)
    });
    let center = response.rect.left_center() + vec2(18.0, 0.0);
    let color = ui.style().interact(&response).fg_stroke.color;
    ui.painter().add(egui::Shape::line(
        vec![
            center + vec2(3.0, -5.0),
            center + vec2(-2.0, 0.0),
            center + vec2(3.0, 5.0),
        ],
        Stroke::new(1.6, color),
    ));
    ui.painter().galley(
        response.rect.left_center() + vec2(34.0, -text.size().y / 2.0),
        text,
        color,
    );
    response.on_hover_text("返回上一页（Esc）")
}

pub fn paint_close(painter: &egui::Painter, rect: egui::Rect, color: Color32) {
    let center = rect.center();
    let stroke = Stroke::new(1.4, color);
    for sign in [-1.0, 1.0] {
        painter.line_segment(
            [
                center + vec2(-4.5, -4.5 * sign),
                center + vec2(4.5, 4.5 * sign),
            ],
            stroke,
        );
    }
}

pub(crate) fn edit_button(ui: &mut egui::Ui, hint: &str, size: f32) -> egui::Response {
    let (rect, response) = icon_button_area(ui, size);
    response
        .widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, ui.is_enabled(), hint));
    let center = rect.center();
    let color = if response.hovered() { TEXT } else { MUTED };
    let stroke = Stroke::new(theme::ICON_STROKE, color);
    ui.painter().add(egui::Shape::closed_line(
        vec![
            center + vec2(-6.0, 6.0),
            center + vec2(-5.0, 1.0),
            center + vec2(2.0, -6.0),
            center + vec2(6.0, -2.0),
            center + vec2(-1.0, 5.0),
        ],
        stroke,
    ));
    ui.painter()
        .line_segment([center + vec2(0.0, -4.0), center + vec2(4.0, 0.0)], stroke);
    response.on_hover_text(hint)
}

pub(crate) fn icon_button_area(ui: &mut egui::Ui, size: f32) -> (egui::Rect, egui::Response) {
    let (rect, response) = ui.allocate_exact_size(vec2(size, size), egui::Sense::click());
    let visuals = ui.style().interact(&response);
    if ui.is_enabled()
        && (response.hovered() || response.is_pointer_button_down_on() || response.has_focus())
    {
        ui.painter().rect(
            rect,
            theme::CONTROL_RADIUS,
            visuals.weak_bg_fill,
            if response.has_focus() {
                visuals.bg_stroke
            } else {
                Stroke::NONE
            },
            egui::StrokeKind::Inside,
        );
    }
    (
        rect,
        response.on_hover_cursor(egui::CursorIcon::PointingHand),
    )
}

/// An unobtrusive button with the same hover/pressed states as icon controls.
pub(crate) fn value_button(
    ui: &mut egui::Ui,
    size: egui::Vec2,
    label: impl Into<egui::WidgetText>,
) -> egui::Response {
    ui.scope(|ui| {
        ui.spacing_mut().button_padding = vec2(0.0, 0.0);
        let disabled = &mut ui.visuals_mut().widgets.noninteractive;
        disabled.weak_bg_fill = Color32::TRANSPARENT;
        disabled.bg_fill = Color32::TRANSPARENT;
        disabled.bg_stroke = Stroke::NONE;
        let inactive = &mut ui.visuals_mut().widgets.inactive;
        inactive.weak_bg_fill = Color32::TRANSPARENT;
        inactive.bg_fill = Color32::TRANSPARENT;
        inactive.bg_stroke = Stroke::NONE;
        ui.add_sized(
            size,
            egui::Button::new((label.into(), egui::Atom::grow())).gap(0.0),
        )
        .on_hover_cursor(egui::CursorIcon::PointingHand)
    })
    .inner
}

pub fn close_button(ui: &mut egui::Ui, hint: &str, size: f32) -> egui::Response {
    let (rect, response) = icon_button_area(ui, size);
    paint_close(
        ui.painter(),
        rect,
        if !ui.is_enabled() {
            theme::DISABLED
        } else if response.hovered() {
            TEXT
        } else {
            MUTED
        },
    );
    response.on_hover_text(hint)
}

pub(crate) fn audio_output_button(ui: &mut egui::Ui, hint: &str, size: f32) -> egui::Response {
    let (rect, response) = icon_button_area(ui, size);
    response
        .widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, ui.is_enabled(), hint));
    let color = if !ui.is_enabled() {
        theme::DISABLED
    } else if response.hovered() {
        TEXT
    } else {
        MUTED
    };
    paint_headphones(ui.painter(), rect, color);
    response.on_hover_text(hint)
}

pub(crate) fn paint_headphones(painter: &egui::Painter, rect: egui::Rect, color: Color32) {
    let center = rect.center();
    let stroke = Stroke::new(theme::ICON_STROKE, color);
    let band = (0..=20)
        .map(|step| {
            let angle = std::f32::consts::PI * (1.0 + step as f32 / 20.0);
            center + vec2(6.0 * angle.cos(), 6.0 * angle.sin() - 1.0)
        })
        .collect();
    painter.add(egui::Shape::line(band, stroke));
    // Join the band at each earcup's top center, away from its rounded corners.
    for x in [-6.0, 6.0] {
        painter.rect_stroke(
            egui::Rect::from_center_size(center + vec2(x, 2.5), vec2(4.0, 7.0)),
            1.0,
            stroke,
            egui::StrokeKind::Middle,
        );
    }
}

/// Compact display tabs for the viewer caption.
pub fn screen_tab(
    ui: &mut egui::Ui,
    label: &str,
    selected: bool,
    pending: bool,
    draggable: bool,
    closable: bool,
) -> (egui::Response, bool) {
    let font = egui::FontId::proportional(theme::SMALL);
    let color = if selected { TEXT } else { MUTED };
    let text = ui
        .painter()
        .layout_no_wrap(label.to_owned(), font.clone(), color);
    let width =
        (text.size().x + 48.0).clamp(theme::SCREEN_TAB_MIN_WIDTH, theme::SCREEN_TAB_MAX_WIDTH);
    let (rect, layout) =
        ui.allocate_exact_size(vec2(width, theme::SCREEN_TAB_HEIGHT), egui::Sense::hover());
    let close_rect =
        egui::Rect::from_center_size(rect.left_center() + vec2(16.0, 0.0), vec2(24.0, 24.0));
    let body = if closable {
        egui::Rect::from_min_max(rect.min + vec2(28.0, 0.0), rect.max)
    } else {
        rect
    };
    let response = ui.interact(
        body,
        layout.id.with("body"),
        if draggable {
            egui::Sense::click_and_drag()
        } else {
            egui::Sense::click()
        },
    );
    let close =
        closable.then(|| ui.interact(close_rect, layout.id.with("close"), egui::Sense::click()));
    let hovered = layout.contains_pointer()
        || response.hovered()
        || close.as_ref().is_some_and(|r| r.hovered());
    if selected || hovered {
        ui.painter().rect_filled(
            rect,
            theme::CONTROL_RADIUS,
            if selected { theme::SELECTED } else { HOVER },
        );
    }
    if let Some(close) = close
        .as_ref()
        .filter(|r| hovered || r.has_focus() || r.is_pointer_button_down_on())
    {
        if close.hovered() || close.has_focus() {
            ui.painter()
                .rect_filled(close_rect, theme::CONTROL_RADIUS, HOVER);
        }
        paint_close(
            ui.painter(),
            close_rect,
            if close.hovered() { TEXT } else { MUTED },
        );
    } else {
        let center = rect.left_center() + vec2(16.0, -1.0);
        let icon = egui::Rect::from_center_size(center, vec2(13.0, 9.0));
        let stroke = Stroke::new(1.1, if selected { ACCENT } else { MUTED });
        ui.painter()
            .rect_stroke(icon, 2.0, stroke, egui::StrokeKind::Inside);
        ui.painter()
            .line_segment([center + vec2(0.0, 4.0), center + vec2(0.0, 7.0)], stroke);
        ui.painter()
            .line_segment([center + vec2(-3.5, 7.0), center + vec2(3.5, 7.0)], stroke);
    }
    let mut job = egui::text::LayoutJob::simple(label.to_owned(), font, color, width - 48.0);
    job.wrap.max_rows = 1;
    job.wrap.break_anywhere = true;
    let text = ui.fonts_mut(|fonts| fonts.layout_job(job));
    ui.painter().galley(
        rect.left_center() + vec2(30.0, -text.size().y / 2.0),
        text,
        color,
    );
    if selected {
        let line = egui::Rect::from_min_max(
            rect.left_bottom() + vec2(10.0, -2.0),
            rect.right_bottom() - vec2(10.0, 0.0),
        );
        ui.painter().rect_filled(line, 1.0, ACCENT);
    }
    if pending {
        let center = rect.right_center() - vec2(10.0, 0.0);
        let angle = ui.input(|i| i.time) as f32 * 5.0;
        ui.painter()
            .circle_stroke(center, 3.5, Stroke::new(1.0, LINE));
        ui.painter()
            .circle_filled(center + vec2(angle.cos(), angle.sin()) * 3.5, 1.3, ACCENT);
        ui.ctx()
            .request_repaint_after(std::time::Duration::from_millis(33));
    }
    response.widget_info(|| {
        egui::WidgetInfo::selected(egui::WidgetType::Button, ui.is_enabled(), selected, label)
    });
    let mut response = response.union(layout);
    let mut closed = false;
    if let Some(close) = close {
        closed = close.clicked();
        close.widget_info(|| {
            egui::WidgetInfo::labeled(egui::WidgetType::Button, ui.is_enabled(), "关闭虚拟屏")
        });
        response = response.union(close.on_hover_text("关闭虚拟屏"));
    }
    (response, closed)
}

pub fn screen_add(ui: &mut egui::Ui, enabled: bool) -> egui::Response {
    ui.add_enabled_ui(enabled, |ui| {
        let (rect, response) = ui.allocate_exact_size(
            vec2(28.0, theme::SCREEN_TAB_HEIGHT),
            if enabled {
                egui::Sense::click()
            } else {
                egui::Sense::hover()
            },
        );
        if response.hovered() && enabled {
            ui.painter().rect_filled(rect, theme::CONTROL_RADIUS, HOVER);
        }
        let stroke = Stroke::new(1.3, if enabled { MUTED } else { theme::DISABLED });
        let center = rect.center();
        ui.painter()
            .line_segment([center - vec2(4.5, 0.0), center + vec2(4.5, 0.0)], stroke);
        ui.painter()
            .line_segment([center - vec2(0.0, 4.5), center + vec2(0.0, 4.5)], stroke);
        response.widget_info(|| {
            egui::WidgetInfo::labeled(egui::WidgetType::Button, enabled, "添加虚拟屏")
        });
        response
    })
    .inner
}

/// Standard viewer menu row, shared by setting pages and their choice lists.
pub fn menu_row(
    ui: &mut egui::Ui,
    label: &str,
    detail: &str,
    selected: Option<bool>,
    enabled: bool,
    more: bool,
) -> egui::Response {
    let enabled = enabled && ui.is_enabled();
    let (rect, response) = ui.allocate_exact_size(
        vec2(ui.available_width(), theme::MENU_HEIGHT),
        if enabled {
            egui::Sense::click()
        } else {
            egui::Sense::hover()
        },
    );
    if selected == Some(true) || (response.hovered() && enabled) {
        ui.painter().rect_filled(
            rect,
            theme::CONTROL_RADIUS,
            if selected == Some(true) {
                theme::SELECTED
            } else {
                HOVER
            },
        );
    }
    let color = if enabled || selected == Some(true) {
        TEXT
    } else {
        theme::DISABLED
    };
    if selected == Some(true) {
        let origin = rect.left_center() + vec2(13.0, 0.0);
        let stroke = Stroke::new(1.5, ACCENT);
        ui.painter()
            .line_segment([origin + vec2(-3.0, 0.0), origin + vec2(-0.5, 2.5)], stroke);
        ui.painter()
            .line_segment([origin + vec2(-0.5, 2.5), origin + vec2(4.5, -3.0)], stroke);
    }
    let label_left = rect.left() + if selected.is_some() { 28.0 } else { 10.0 };
    let detail_color = if enabled || selected == Some(true) {
        MUTED
    } else {
        theme::DISABLED
    };
    let mut detail_job = egui::text::LayoutJob::simple_singleline(
        detail.to_owned(),
        egui::FontId::proportional(theme::SMALL),
        detail_color,
    );
    detail_job.wrap.max_width = rect.width() * 0.5;
    detail_job.wrap.max_rows = 1;
    detail_job.wrap.break_anywhere = true;
    let detail_galley = ui.painter().layout_job(detail_job);
    let detail_width = detail_galley.size().x;
    let label_width =
        (rect.right() - label_left - detail_width - if more { 36.0 } else { 20.0 }).max(0.0);
    let mut job = egui::text::LayoutJob::simple_singleline(
        label.to_owned(),
        egui::FontId::proportional(theme::COMPACT_TEXT),
        color,
    );
    job.wrap.max_width = label_width;
    job.wrap.max_rows = 1;
    job.wrap.break_anywhere = true;
    let galley = ui.painter().layout_job(job);
    ui.painter().galley(
        egui::pos2(label_left, rect.center().y - galley.size().y / 2.0),
        galley,
        color,
    );
    ui.painter().galley(
        rect.right_center()
            - vec2(
                (if more { 26.0 } else { 10.0 }) + detail_width,
                detail_galley.size().y / 2.0,
            ),
        detail_galley,
        if enabled || selected == Some(true) {
            MUTED
        } else {
            theme::DISABLED
        },
    );
    if more {
        let center = rect.right_center() - vec2(12.0, 0.0);
        let stroke = Stroke::new(1.1, MUTED);
        ui.painter()
            .line_segment([center + vec2(-2.0, -3.5), center + vec2(1.5, 0.0)], stroke);
        ui.painter()
            .line_segment([center + vec2(1.5, 0.0), center + vec2(-2.0, 3.5)], stroke);
    }
    response
}

pub fn secondary(label: &str) -> egui::Button<'_> {
    egui::Button::new(label).min_size(vec2(64.0, HEIGHT))
}

/// Transient status and optional action share a fixed row without moving the form.
pub(crate) fn status_row(
    ui: &mut egui::Ui,
    message: &str,
    color: Color32,
    action: Option<&str>,
) -> bool {
    let (rect, _) =
        ui.allocate_exact_size(vec2(ui.available_width(), HEIGHT), egui::Sense::hover());
    let mut row = ui.new_child(
        egui::UiBuilder::new()
            .max_rect(rect)
            .layout(egui::Layout::right_to_left(egui::Align::Center)),
    );
    let clicked = action.is_some_and(|label| row.add(secondary(label)).clicked());
    row.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
        ui.add(egui::Label::new(RichText::new(message).color(color)).truncate())
            .on_hover_text(message);
    });
    clicked
}

/// Keep setting edits inside the same menu row, without adding a button footer.
pub fn setting_row(
    ui: &mut egui::Ui,
    label: &str,
    detail: &str,
    edited: bool,
    pending: bool,
) -> (egui::Response, bool, bool) {
    let (rect, _) = ui.allocate_exact_size(
        vec2(ui.available_width(), theme::MENU_HEIGHT),
        egui::Sense::hover(),
    );
    let actions_width = if edited || pending {
        2.0 * COMPACT_HEIGHT + 4.0
    } else {
        0.0
    };
    let mut row = ui.new_child(egui::UiBuilder::new().id_salt(("setting", label)).max_rect(
        egui::Rect::from_min_max(rect.min, rect.right_bottom() - vec2(actions_width, 0.0)),
    ));
    let response = menu_row(&mut row, label, detail, None, true, !edited && !pending);
    if !edited && !pending {
        return (response, false, false);
    }
    let mut actions = ui.new_child(
        egui::UiBuilder::new()
            .id_salt(("setting-actions", label))
            .max_rect(egui::Rect::from_min_max(
                rect.right_top() - vec2(actions_width, 0.0),
                rect.max,
            ))
            .layout(egui::Layout::left_to_right(egui::Align::Center)),
    );
    actions.spacing_mut().item_spacing.x = 4.0;
    let cancel = close_button(
        &mut actions,
        &if edited {
            format!("撤销{label}修改")
        } else {
            "停止等待".into()
        },
        COMPACT_HEIGHT,
    )
    .clicked();
    let apply = if edited {
        let response = actions.add_sized(
            vec2(COMPACT_HEIGHT, COMPACT_HEIGHT),
            egui::Button::new("")
                .fill(theme::SELECTED)
                .stroke(Stroke::NONE),
        );
        response.widget_info(|| {
            egui::WidgetInfo::labeled(
                egui::WidgetType::Button,
                actions.is_enabled(),
                format!("应用{label}"),
            )
        });
        let center = response.rect.center();
        actions.painter().line(
            vec![
                center + vec2(-4.0, 0.0),
                center + vec2(-1.0, 3.0),
                center + vec2(5.0, -3.0),
            ],
            Stroke::new(1.6, ACCENT),
        );
        response.on_hover_text(format!("应用{label}")).clicked()
    } else {
        actions.add_sized(
            vec2(COMPACT_HEIGHT, COMPACT_HEIGHT),
            egui::Spinner::new().size(16.0).color(MUTED),
        );
        false
    };
    (response, apply, cancel)
}
mod annotation;
mod annotation_color;
pub(crate) use annotation::{
    AnnotationIcon, annotation_board, annotation_button, annotation_clear_dialog, annotation_color,
    annotation_frame, annotation_header, annotation_tool_button, annotation_tool_frame,
    annotation_width, paint_annotation_icon,
};

mod task_status;
pub(crate) use task_status::task_button_progress;
