use super::*;
use egui::{Rect, Response, Sense, Ui, pos2};

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum AnnotationIcon {
    Pen,
    Laser,
    Line,
    Arrow,
    Rectangle,
    Ellipse,
    Eraser,
    Undo,
    Redo,
    Clear,
    Board,
    Exit,
}

pub(crate) fn annotation_button(
    ui: &mut Ui,
    icon: AnnotationIcon,
    selected: bool,
    hint: &str,
) -> Response {
    ink_button(ui, icon, selected, hint, theme::ANNOTATION_BUTTON)
}
pub(crate) fn annotation_tool_button(
    ui: &mut Ui,
    icon: AnnotationIcon,
    selected: bool,
    hint: &str,
) -> Response {
    ink_button(ui, icon, selected, hint, theme::ANNOTATION_TOOL_SIZE)
}
fn ink_button(
    ui: &mut Ui,
    icon: AnnotationIcon,
    selected: bool,
    hint: &str,
    size: f32,
) -> Response {
    let (rect, response) = ui.allocate_exact_size(vec2(size, size), Sense::click());
    let painter = ui.painter();
    if selected || response.hovered() {
        painter.rect_filled(
            rect,
            theme::CONTROL_RADIUS,
            if selected {
                theme::SELECTED
            } else {
                theme::HOVER
            },
        );
    }
    let color = if !ui.is_enabled() {
        theme::MUTED.gamma_multiply(0.45)
    } else if selected {
        theme::ACCENT
    } else {
        theme::TEXT
    };
    let icon_rect = Rect::from_center_size(rect.center(), vec2(16., 16.));
    paint_annotation_icon(painter, icon_rect, icon, color);
    response.on_hover_text(hint)
}

pub(crate) fn annotation_frame() -> egui::Frame {
    egui::Frame::new()
        .fill(theme::BG)
        .stroke(Stroke::new(1., theme::LINE))
        .corner_radius(theme::PANEL_RADIUS)
        .inner_margin(theme::ANNOTATION_PANEL_MARGIN)
}
pub(crate) fn annotation_tool_frame() -> egui::Frame {
    egui::Frame::new()
        .fill(theme::SIDEBAR)
        .corner_radius(theme::CONTROL_RADIUS)
        .inner_margin(5)
}
pub(crate) fn annotation_header(
    ui: &mut Ui,
    can_undo: bool,
    can_redo: bool,
) -> (Response, bool, bool, bool, bool) {
    let mut undo = false;
    let mut redo = false;
    let mut close = false;
    let mut end = false;
    let height = theme::ANNOTATION_HEADER_HEIGHT;
    let row = ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 2.;
        let (rect, response) = ui.allocate_exact_size(
            vec2(ui.available_width() - 4. * (height + 2.), height),
            Sense::drag(),
        );
        let p = ui.painter();
        for x in [0., 5.] {
            for y in [-5., 0., 5.] {
                p.circle_filled(
                    pos2(rect.left() + 3. + x, rect.center().y + y),
                    1.,
                    theme::MUTED,
                );
            }
        }
        p.text(
            pos2(rect.left() + 22., rect.center().y),
            egui::Align2::LEFT_CENTER,
            "批注",
            egui::FontId::proportional(theme::COMPACT_TEXT),
            theme::TEXT,
        );
        let response = response.on_hover_cursor(egui::CursorIcon::Grab);
        if response.dragged() {
            ui.ctx().set_cursor_icon(egui::CursorIcon::Grabbing);
        }
        ui.spacing_mut().item_spacing.x = 2.;
        undo = ui
            .add_enabled_ui(can_undo, |ui| {
                ink_button(ui, AnnotationIcon::Undo, false, "撤销 · Ctrl+Z", height)
            })
            .inner
            .clicked();
        redo = ui
            .add_enabled_ui(can_redo, |ui| {
                ink_button(ui, AnnotationIcon::Redo, false, "重做 · Ctrl+Y", height)
            })
            .inner
            .clicked();
        end = ink_button(ui, AnnotationIcon::Exit, false, "结束批注", height).clicked();
        close = close_button(ui, "收起面板", height).clicked();
        response
    });
    (row.inner, close, undo, redo, end)
}

pub(crate) fn annotation_clear_dialog(ctx: &egui::Context, uncertain: bool) -> Option<bool> {
    let mut action = None;
    let modal = egui::Modal::new(egui::Id::new("annotation-clear-confirm"))
        .frame(dialog_frame())
        .show(ctx, |ui| {
            ui.set_width(
                theme::ANNOTATION_CLEAR_WIDTH.min((ctx.content_rect().width() - 64.).max(160.)),
            );
            if dialog_header(ui, "清空批注？", DialogIcon::Warning, true) {
                action = Some(false);
            }
            ui.add(
                egui::Label::new(
                    RichText::new(if uncertain {
                        "将清除全部批注并恢复桌面"
                    } else {
                        "将清除本次连接的全部笔迹"
                    })
                    .size(theme::BODY)
                    .color(theme::MUTED),
                )
                .wrap(),
            );
            let (accept, cancel) = dialog_actions(
                ui,
                Some(DialogAction::new("清空").danger(true)),
                Some("取消"),
            );
            if accept {
                action = Some(true);
            } else if cancel {
                action = Some(false);
            }
        });
    if action.is_none() && modal.should_close() {
        action = Some(false);
    }
    action
}
fn ink_property(ui: &mut Ui, width: f32) -> (Rect, Response) {
    let (r, response) = ui.allocate_exact_size(vec2(width, 30.), Sense::click());
    ui.painter().rect_filled(
        r,
        theme::CONTROL_RADIUS,
        if response.hovered() {
            theme::HOVER
        } else {
            theme::SURFACE
        },
    );
    let c = pos2(r.right() - 12., r.center().y);
    ui.painter().add(egui::Shape::line(
        vec![c + vec2(-3., -1.5), c + vec2(0., 1.5), c + vec2(3., -1.5)],
        Stroke::new(1., theme::MUTED),
    ));
    (r, response)
}
pub(crate) fn annotation_color(ui: &mut Ui, rgb: &mut [u8; 3], opacity: &mut u8) {
    let (r, response) = ink_property(ui, theme::ANNOTATION_COLOR_WIDTH);
    let alpha = (u32::from(*opacity) * 255 / 100) as u8;
    let swatch = Rect::from_center_size(pos2(r.left() + 16., r.center().y), vec2(16., 16.));
    egui::color_picker::show_color_at(
        ui.painter(),
        Color32::from_rgba_unmultiplied(rgb[0], rgb[1], rgb[2], alpha),
        swatch,
    );
    ui.painter().text(
        pos2(r.left() + 32., r.center().y),
        egui::Align2::LEFT_CENTER,
        format!("{}%", opacity),
        egui::FontId::proportional(theme::SMALL),
        theme::TEXT,
    );
    egui::Popup::from_toggle_button_response(&response)
        .close_behavior(egui::PopupCloseBehavior::CloseOnClickOutside)
        .show(|ui| super::annotation_color::show(ui, rgb, opacity));
}
pub(crate) fn annotation_board(
    ui: &mut Ui,
    active: Option<[u8; 3]>,
    color: &mut [u8; 3],
) -> Option<Option<[u8; 3]>> {
    let response = annotation_button(ui, AnnotationIcon::Board, active.is_some(), "白板背景");
    let mut action = None;
    egui::Popup::from_toggle_button_response(&response)
        .close_behavior(egui::PopupCloseBehavior::CloseOnClickOutside)
        .show(|ui| {
            action = annotation_board_content(ui, active, color);
        });
    action
}
fn annotation_board_content(
    ui: &mut Ui,
    active: Option<[u8; 3]>,
    color: &mut [u8; 3],
) -> Option<Option<[u8; 3]>> {
    let mut action = None;
    ui.set_width(theme::ANNOTATION_BOARD_PICKER_WIDTH);
    ui.spacing_mut().item_spacing = vec2(8., 8.);
    ui.horizontal(|ui| {
        ui.label(RichText::new("白板").size(theme::COMPACT_TEXT).strong());
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if ui
                .add_sized(
                    vec2(52., theme::COMPACT_HEIGHT),
                    egui::Button::new(if active.is_some() { "关闭" } else { "开启" })
                        .selected(active.is_some()),
                )
                .clicked()
            {
                action = Some(if active.is_some() { None } else { Some(*color) });
            }
        });
    });
    ui.horizontal(|ui| {
        for (name, rgb) in theme::ANNOTATION_BOARD_COLORS {
            let selected = active.unwrap_or(*color) == rgb;
            let (r, response) =
                ui.allocate_exact_size(theme::ANNOTATION_BOARD_SWATCH, Sense::click());
            ui.painter().rect(
                r,
                theme::CONTROL_RADIUS,
                if selected {
                    theme::SELECTED
                } else if response.hovered() {
                    theme::HOVER
                } else {
                    theme::SURFACE
                },
                Stroke::new(1., if selected { theme::ACCENT } else { theme::LINE }),
                egui::StrokeKind::Inside,
            );
            let swatch =
                Rect::from_min_max(r.min + vec2(6., 6.), pos2(r.right() - 6., r.top() + 29.));
            ui.painter()
                .rect_filled(swatch, 3., Color32::from_rgb(rgb[0], rgb[1], rgb[2]));
            ui.painter().text(
                pos2(r.center().x, r.bottom() - 12.),
                egui::Align2::CENTER_CENTER,
                name,
                egui::FontId::proportional(theme::SMALL),
                if selected { theme::TEXT } else { theme::MUTED },
            );
            if response.clicked() {
                *color = rgb;
                if active.is_some() {
                    action = Some(Some(rgb));
                }
            }
        }
    });
    action
}
fn annotation_laser_options(ui: &mut Ui, tail_ms: &mut u16) {
    ui.add_space(8.);
    ui.label(RichText::new("拖尾时长").strong());
    ui.horizontal(|ui| {
        for (value, label) in [(60, "短"), (100, "中"), (200, "长")] {
            if ui.selectable_label(*tail_ms == value, label).clicked() {
                *tail_ms = value;
            }
        }
    });
    ui.spacing_mut().slider_width = theme::ANNOTATION_PICKER_WIDTH - 80.;
    ui.add(
        egui::Slider::new(
            tail_ms,
            crate::features::stream_control::annotation::LASER_TAIL_MIN
                ..=crate::features::stream_control::annotation::LASER_TAIL_MAX,
        )
        .suffix(" ms"),
    );
}
pub(crate) fn annotation_width(
    ui: &mut Ui,
    width: &mut f32,
    tool: AnnotationIcon,
    tail_ms: Option<&mut u16>,
) {
    let laser = tool == AnnotationIcon::Laser;
    let (r, response) = ink_property(ui, theme::ANNOTATION_SIZE_WIDTH);
    let y = r.center().y;
    if laser {
        ui.painter().circle_filled(
            pos2(r.left() + 18., y),
            (*width * 0.5).clamp(2., 6.),
            theme::TEXT,
        );
    } else {
        ui.painter().line_segment(
            [pos2(r.left() + 9., y), pos2(r.left() + 27., y)],
            Stroke::new((*width).clamp(1., 5.), theme::TEXT),
        );
    }
    ui.painter().text(
        pos2(r.left() + 34., y),
        egui::Align2::LEFT_CENTER,
        format!("{width:.1} px"),
        egui::FontId::proportional(theme::SMALL),
        theme::TEXT,
    );
    egui::Popup::from_toggle_button_response(&response)
        .close_behavior(egui::PopupCloseBehavior::CloseOnClickOutside)
        .show(|ui| {
            ui.set_width(theme::ANNOTATION_PICKER_WIDTH);
            ui.spacing_mut().interact_size.y = theme::COMPACT_HEIGHT;
            ui.spacing_mut().item_spacing.x = 4.0;
            ui.label(
                RichText::new(if laser {
                    "光点大小"
                } else {
                    "线条粗细"
                })
                .strong(),
            );
            ui.horizontal(|ui| {
                for value in if laser {
                    [3., 5., 8., 12.]
                } else {
                    [1., 3., 6., 12.]
                } {
                    if ui
                        .selectable_label((*width - value).abs() < 0.01, format!("{value:.0} px"))
                        .clicked()
                    {
                        *width = value;
                    }
                }
            });
            ui.spacing_mut().slider_width = theme::ANNOTATION_PICKER_WIDTH - 64.;
            ui.add(
                egui::Slider::new(width, 1.0..=64.0)
                    .logarithmic(true)
                    .suffix(" px"),
            );
            if let Some(tail_ms) = tail_ms {
                annotation_laser_options(ui, tail_ms);
            }
        });
}
pub(crate) fn paint_annotation_icon(
    p: &egui::Painter,
    r: Rect,
    icon: AnnotationIcon,
    color: Color32,
) {
    let stroke = Stroke::new(theme::ICON_STROKE, color);
    let at = |x: f32, y: f32| pos2(r.left() + x * r.width(), r.top() + y * r.height());
    let line = |a, b| {
        p.line_segment([a, b], stroke);
    };
    match icon {
        AnnotationIcon::Pen => {
            p.add(egui::Shape::closed_line(
                vec![
                    at(0.1, 0.9),
                    at(0.15, 0.62),
                    at(0.72, 0.05),
                    at(0.95, 0.28),
                    at(0.38, 0.85),
                ],
                stroke,
            ));
            line(at(0.62, 0.15), at(0.85, 0.38));
        }
        AnnotationIcon::Laser => {
            p.circle_filled(r.center(), 2.4, color);
            p.circle_stroke(r.center(), 4.7, Stroke::new(1.0, color));
            for (a, b) in [
                (at(0.5, 0.0), at(0.5, 0.12)),
                (at(0.5, 0.88), at(0.5, 1.0)),
                (at(0.0, 0.5), at(0.12, 0.5)),
                (at(0.88, 0.5), at(1.0, 0.5)),
            ] {
                line(a, b);
            }
        }
        AnnotationIcon::Line => line(at(0.1, 0.9), at(0.9, 0.1)),
        AnnotationIcon::Arrow => {
            line(at(0.1, 0.9), at(0.9, 0.1));
            line(at(0.4, 0.1), at(0.9, 0.1));
            line(at(0.9, 0.1), at(0.9, 0.6));
        }
        AnnotationIcon::Rectangle => {
            p.rect_stroke(r.shrink(1.0), 0, stroke, egui::StrokeKind::Inside);
        }
        AnnotationIcon::Ellipse => {
            p.circle_stroke(r.center(), r.width() * 0.45, stroke);
        }
        AnnotationIcon::Eraser => {
            p.add(egui::Shape::closed_line(
                vec![at(0.0, 0.6), at(0.6, 0.0), at(1.0, 0.4), at(0.4, 1.0)],
                stroke,
            ));
            line(at(0.3, 0.3), at(0.7, 0.7));
        }
        AnnotationIcon::Undo | AnnotationIcon::Redo => {
            let f = |x, y| {
                at(
                    if icon == AnnotationIcon::Undo {
                        x
                    } else {
                        1.0 - x
                    },
                    y,
                )
            };
            p.add(egui::Shape::line(
                vec![
                    f(0.15, 0.32),
                    f(0.65, 0.32),
                    f(0.85, 0.48),
                    f(0.85, 0.75),
                    f(0.7, 0.9),
                    f(0.5, 0.9),
                ],
                stroke,
            ));
            line(f(0.4, 0.05), f(0.1, 0.32));
            line(f(0.1, 0.32), f(0.4, 0.6));
        }
        AnnotationIcon::Exit => {
            p.add(egui::Shape::line(
                vec![
                    at(0.45, 0.05),
                    at(0.05, 0.05),
                    at(0.05, 0.95),
                    at(0.45, 0.95),
                ],
                stroke,
            ));
            line(at(0.4, 0.5), at(1., 0.5));
            line(at(0.75, 0.25), at(1., 0.5));
            line(at(0.75, 0.75), at(1., 0.5));
        }
        AnnotationIcon::Board => {
            p.rect_stroke(
                Rect::from_min_max(at(0.05, 0.05), at(0.95, 0.7)),
                2.,
                stroke,
                egui::StrokeKind::Inside,
            );
            line(at(0.5, 0.7), at(0.5, 1.));
            line(at(0.25, 1.), at(0.75, 1.));
        }
        AnnotationIcon::Clear => {
            line(at(0.1, 0.2), at(0.9, 0.2));
            line(at(0.4, 0.0), at(0.6, 0.0));
            p.add(egui::Shape::line(
                vec![
                    at(0.23, 0.35),
                    at(0.28, 0.95),
                    at(0.72, 0.95),
                    at(0.77, 0.35),
                ],
                stroke,
            ));
        }
    }
}
