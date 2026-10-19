//! File browser and transfer table primitives share the application's visual language.
use super::theme;
use egui::{Align2, Color32, FontId, Rect, Response, Sense, Stroke, Ui, pos2, vec2};

#[derive(Clone, Copy)]
pub(crate) enum Icon {
    Back,
    Forward,
    Up,
    Search,
    Refresh,
    Pause,
    Resume,
    Close,
    Collapse,
    Expand,
    Computer,
}

// Fixed slots deliberately do not advance the parent's cursor. A breadcrumb's
// child layout must never move the adjacent search/refresh buttons.
pub(crate) fn icon_at(ui: &mut Ui, rect: Rect, icon: Icon, label: &str, enabled: bool) -> Response {
    let response = ui.interact(
        rect,
        ui.id().with(label),
        if enabled {
            Sense::click()
        } else {
            Sense::hover()
        },
    );
    if enabled && response.hovered() {
        ui.painter()
            .rect_filled(rect, theme::CONTROL_RADIUS, theme::HOVER);
    }
    paint_icon(
        ui.painter(),
        rect,
        icon,
        if !enabled {
            theme::DISABLED
        } else if response.hovered() {
            theme::TEXT
        } else {
            theme::MUTED
        },
    );
    response.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, enabled, label));
    response.on_hover_text(label)
}

#[derive(Clone, Copy)]
pub(crate) enum ButtonStyle {
    Quiet,
    Secondary,
    Primary,
}

pub(crate) fn button_at(
    ui: &mut Ui,
    rect: Rect,
    label: &str,
    enabled: bool,
    style: ButtonStyle,
) -> Response {
    let response = ui.interact(
        rect,
        ui.id()
            .with((label, rect.min.x.to_bits(), rect.min.y.to_bits())),
        if enabled {
            Sense::click()
        } else {
            Sense::hover()
        },
    );
    let fill = match style {
        ButtonStyle::Primary if enabled => theme::ACCENT,
        ButtonStyle::Primary | ButtonStyle::Secondary => theme::SURFACE,
        ButtonStyle::Quiet => Color32::TRANSPARENT,
    };
    let fill = if response.hovered() && enabled && !matches!(style, ButtonStyle::Primary) {
        theme::HOVER
    } else {
        fill
    };
    let stroke = if matches!(style, ButtonStyle::Secondary) {
        Stroke::new(1., theme::LINE)
    } else {
        Stroke::NONE
    };
    ui.painter().rect(
        rect,
        theme::CONTROL_RADIUS,
        fill,
        stroke,
        egui::StrokeKind::Inside,
    );
    let color = if !enabled {
        theme::DISABLED
    } else if matches!(style, ButtonStyle::Primary) {
        Color32::WHITE
    } else {
        theme::TEXT
    };
    let mut job = egui::text::LayoutJob::simple_singleline(
        label.into(),
        FontId::proportional(theme::COMPACT_TEXT),
        color,
    );
    job.wrap.max_width = (rect.width() - 12.).max(0.);
    job.wrap.max_rows = 1;
    job.wrap.break_anywhere = true;
    let galley = ui.fonts_mut(|f| f.layout_job(job));
    ui.painter()
        .with_clip_rect(rect.intersect(ui.clip_rect()))
        .galley(rect.center() - galley.size() / 2., galley, color);
    response.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, enabled, label));
    response
}

pub(crate) fn icon(ui: &mut Ui, icon: Icon, label: &str, enabled: bool) -> Response {
    let response = ui.add_enabled(
        enabled,
        egui::Button::new("")
            .frame(false)
            .min_size(vec2(theme::COMPACT_HEIGHT, theme::COMPACT_HEIGHT)),
    );
    paint_icon(
        ui.painter(),
        response.rect,
        icon,
        if !enabled {
            theme::DISABLED
        } else if response.hovered() {
            theme::TEXT
        } else {
            theme::MUTED
        },
    );
    response.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, enabled, label));
    response.on_hover_text(label)
}

pub(crate) fn paint_icon(p: &egui::Painter, rect: Rect, icon: Icon, color: Color32) {
    let c = rect.center();
    let s = Stroke::new(theme::ICON_STROKE, color);
    let line = |a: egui::Vec2, b: egui::Vec2| {
        p.line_segment([c + a, c + b], s);
    };
    match icon {
        Icon::Back | Icon::Forward => {
            let d = if matches!(icon, Icon::Back) { -1. } else { 1. };
            line(vec2(-6., 0.), vec2(6., 0.));
            line(vec2(6. * d, 0.), vec2(1. * d, -5.));
            line(vec2(6. * d, 0.), vec2(1. * d, 5.));
        }
        Icon::Up => {
            line(vec2(0., 6.), vec2(0., -6.));
            line(vec2(0., -6.), vec2(-5., -1.));
            line(vec2(0., -6.), vec2(5., -1.));
        }
        Icon::Search => {
            p.circle_stroke(c - vec2(2., 2.), 5., s);
            line(vec2(2., 2.), vec2(7., 7.));
        }
        Icon::Refresh => {
            let points = (0..25)
                .map(|i| {
                    let a = 0.4 + i as f32 * std::f32::consts::TAU * 0.85 / 24.;
                    c + vec2(a.cos(), a.sin()) * 7.
                })
                .collect();
            p.add(egui::Shape::line(points, s));
            line(vec2(6.5, 2.8), vec2(7., -3.));
            line(vec2(6.5, 2.8), vec2(1., 2.));
        }
        Icon::Pause => {
            line(vec2(-3., -6.), vec2(-3., 6.));
            line(vec2(3., -6.), vec2(3., 6.));
        }
        Icon::Resume => {
            p.add(egui::Shape::closed_line(
                vec![c + vec2(-4., -6.), c + vec2(6., 0.), c + vec2(-4., 6.)],
                s,
            ));
        }
        Icon::Close => super::paint_close(p, rect.shrink(6.), color),
        Icon::Collapse | Icon::Expand => {
            let d = if matches!(icon, Icon::Collapse) {
                1.
            } else {
                -1.
            };
            line(vec2(-5., -2. * d), vec2(0., 3. * d));
            line(vec2(0., 3. * d), vec2(5., -2. * d));
        }
        Icon::Computer => {
            p.rect_stroke(
                Rect::from_center_size(c - vec2(0., 2.), vec2(18., 12.)),
                2,
                s,
                egui::StrokeKind::Inside,
            );
            line(vec2(0., 4.), vec2(0., 7.));
            line(vec2(-5., 8.), vec2(5., 8.));
        }
    }
}

pub(crate) fn text(ui: &Ui, rect: Rect, value: &str, color: Color32, right: bool) {
    let mut job = egui::text::LayoutJob::simple_singleline(
        value.into(),
        FontId::proportional(theme::SMALL),
        color,
    );
    job.wrap.max_width = rect.width().max(0.);
    job.wrap.max_rows = 1;
    job.wrap.break_anywhere = true;
    let galley = ui.fonts_mut(|f| f.layout_job(job));
    let x = if right {
        rect.right() - galley.size().x
    } else {
        rect.left()
    };
    ui.painter()
        .with_clip_rect(rect.intersect(ui.clip_rect()))
        .galley(
            pos2(x, rect.center().y - galley.size().y / 2.),
            galley,
            color,
        );
}

pub(crate) fn panel(ui: &Ui, rect: Rect, active: bool) {
    ui.painter().rect(
        rect,
        theme::CONTROL_RADIUS,
        theme::SIDEBAR,
        Stroke::new(
            1.,
            if active {
                theme::BORDER_FOCUS
            } else {
                theme::LINE
            },
        ),
        egui::StrokeKind::Inside,
    );
}

pub(crate) fn divider(ui: &mut Ui, rect: Rect, id: egui::Id, vertical: bool) -> Response {
    let response = ui
        .interact(rect, id, Sense::drag())
        .on_hover_cursor(if vertical {
            egui::CursorIcon::ResizeHorizontal
        } else {
            egui::CursorIcon::ResizeVertical
        });
    if response.hovered() || response.dragged() {
        let (a, b) = if vertical {
            (rect.center_top(), rect.center_bottom())
        } else {
            (rect.left_center(), rect.right_center())
        };
        ui.painter()
            .line_segment([a, b], Stroke::new(2., theme::ACCENT));
    }
    response
}

pub(crate) fn file_columns(rect: Rect) -> [Rect; 3] {
    let size_x = rect.right() - theme::FILES_SIZE_WIDTH;
    let date_x = size_x - theme::FILES_DATE_WIDTH;
    [
        Rect::from_min_max(rect.min, pos2(date_x, rect.bottom())),
        Rect::from_min_max(pos2(date_x, rect.top()), pos2(size_x, rect.bottom())),
        Rect::from_min_max(pos2(size_x, rect.top()), rect.max),
    ]
}

pub(crate) fn file_row(
    ui: &mut Ui,
    name: &str,
    modified: &str,
    size: &str,
    entry_type: i32,
    icon_type: &str,
    selected: bool,
) -> Response {
    let (r, response) = ui.allocate_exact_size(
        vec2(ui.available_width(), theme::FILES_ROW_HEIGHT),
        Sense::click(),
    );
    if selected || response.hovered() {
        ui.painter().rect_filled(
            r,
            theme::CONTROL_RADIUS,
            if selected {
                theme::SELECTED
            } else {
                theme::HOVER
            },
        );
    }
    let cols = file_columns(r);
    let ir = Rect::from_center_size(r.left_center() + vec2(15., 0.), vec2(18., 18.));
    super::file_icons::paint(ui.painter(), ir, name, entry_type, icon_type);
    let mut name_rect = cols[0];
    name_rect.min.x += 32.;
    name_rect.max.x -= 8.;
    text(ui, name_rect, name, theme::TEXT, false);
    text(
        ui,
        cols[1].shrink2(vec2(6., 0.)),
        modified,
        theme::MUTED,
        false,
    );
    text(ui, cols[2].shrink2(vec2(8., 0.)), size, theme::MUTED, true);
    response
}

pub(crate) fn table_heading(ui: &mut Ui, rect: Rect, label: &str, right: bool) -> Response {
    text(ui, rect.shrink2(vec2(8., 0.)), label, theme::MUTED, right);
    ui.interact(rect, ui.id().with(label), Sense::click())
}

pub(crate) fn empty(ui: &Ui, rect: Rect, message: &str) {
    ui.painter().text(
        rect.center(),
        Align2::CENTER_CENTER,
        message,
        FontId::proportional(theme::BODY),
        theme::MUTED,
    );
}
