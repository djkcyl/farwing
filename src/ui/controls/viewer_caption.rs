//! Shared icons and interaction states for the player caption.
use crate::i18n::t;
use crate::ui::theme;

pub(crate) fn viewer_fullscreen_handle(ui: &mut egui::Ui) -> (egui::Response, egui::Rect) {
    let (hit_rect, response) = ui.allocate_exact_size(
        theme::VIEWER_FULLSCREEN_HANDLE_HIT_SIZE,
        egui::Sense::click(),
    );
    let line = egui::Rect::from_center_size(
        egui::pos2(
            hit_rect.center().x,
            hit_rect.top() + theme::VIEWER_FULLSCREEN_HANDLE_CENTER_Y,
        ),
        theme::VIEWER_FULLSCREEN_HANDLE_SIZE,
    );
    let hovered = response.hovered()
        && ui.input(|i| i.pointer.hover_pos().is_some_and(|p| hit_rect.contains(p)));
    // A dark outline keeps the small handle legible over bright desktop content.
    ui.painter().rect(
        line,
        theme::CONTROL_RADIUS,
        if hovered { theme::TEXT } else { theme::MUTED },
        egui::Stroke::new(1.0, theme::VIEWER_SCRIM),
        egui::StrokeKind::Outside,
    );
    response.widget_info(|| {
        egui::WidgetInfo::labeled(
            egui::WidgetType::Button,
            ui.is_enabled(),
            t().controls.show_fullscreen_toolbar,
        )
    });
    (response, hit_rect)
}

#[derive(Clone, Copy)]
pub(crate) enum ViewerCaptionIcon {
    #[cfg(windows)]
    Plugins,
    Annotation,
    OneToOne,
    Mouse,
    #[cfg(target_os = "linux")]
    AskControl,
    #[cfg(target_os = "linux")]
    Audio { muted: bool },
    Microphone,
    Quality,
    Participants,
    Minimize,
    Maximize,
    Restore,
    Close,
}

pub(crate) fn viewer_caption_button(
    ui: &mut egui::Ui,
    icon: ViewerCaptionIcon,
    selected: bool,
    tooltip: &str,
) -> egui::Response {
    let (rect, response) = ui.allocate_exact_size(
        egui::vec2(theme::VIEWER_CAPTION_BUTTON, theme::VIEWER_CAPTION_BUTTON),
        egui::Sense::click(),
    );
    paint_caption_button(ui, rect, &response, icon, selected);
    #[cfg(target_os = "linux")]
    if !tooltip.is_empty() {
        response.widget_info(|| match icon {
            ViewerCaptionIcon::Mouse | ViewerCaptionIcon::Audio { .. } => egui::WidgetInfo::selected(
                egui::WidgetType::Button,
                ui.is_enabled(),
                selected,
                match icon {
                    ViewerCaptionIcon::Mouse => t().app.cap_control,
                    _ => t().viewer.mute,
                },
            ),
            _ => egui::WidgetInfo::labeled(egui::WidgetType::Button, ui.is_enabled(), tooltip),
        });
    }
    if tooltip.is_empty() {
        response.widget_info(|| {
            egui::WidgetInfo::labeled(
                egui::WidgetType::Button,
                ui.is_enabled(),
                match icon {
                    ViewerCaptionIcon::Minimize => t().controls.minimize,
                    ViewerCaptionIcon::Close => t().common.close,
                    _ => t().controls.window_actions,
                },
            )
        });
        response
    } else {
        response.on_hover_text(tooltip)
    }
}

/// Session participants; stays selected while its popup list is open and
/// shows the head count once someone else joined.
pub(crate) fn viewer_participants_button(
    ui: &mut egui::Ui,
    count: usize,
    tooltip: &str,
) -> egui::Response {
    let (rect, response) = ui.allocate_exact_size(
        egui::vec2(theme::VIEWER_CAPTION_BUTTON, theme::VIEWER_CAPTION_BUTTON),
        egui::Sense::click(),
    );
    let open = egui::Popup::is_id_open(ui.ctx(), egui::Popup::default_response_id(&response));
    paint_caption_button(ui, rect, &response, ViewerCaptionIcon::Participants, open);
    if count > 1 {
        let text = ui.painter().layout_no_wrap(
            count.min(99).to_string(),
            egui::FontId::proportional(theme::MICRO),
            egui::Color32::WHITE,
        );
        let size = egui::vec2((text.size().x + 6.0).max(13.0), 13.0);
        let badge =
            egui::Rect::from_min_size(egui::pos2(rect.right() - size.x, rect.top() + 1.0), size);
        ui.painter()
            .rect_filled(badge, size.y / 2.0, crate::ui::theme::ACCENT);
        ui.painter().galley(
            badge.center() - text.size() / 2.0,
            text,
            egui::Color32::WHITE,
        );
    }
    response.on_hover_text(tooltip)
}

fn paint_caption_button(
    ui: &egui::Ui,
    rect: egui::Rect,
    response: &egui::Response,
    icon: ViewerCaptionIcon,
    selected: bool,
) {
    let fill = if !ui.is_enabled() {
        egui::Color32::TRANSPARENT
    } else if matches!(icon, ViewerCaptionIcon::Close) && response.hovered() {
        crate::ui::theme::DANGER_FILL
    } else if selected {
        crate::ui::theme::SELECTED
    } else if response.hovered() {
        theme::HOVER
    } else {
        egui::Color32::TRANSPARENT
    };
    ui.painter().rect_filled(rect, theme::CONTROL_RADIUS, fill);
    #[cfg(target_os = "linux")]
    if response.has_focus() {
        ui.painter().rect_stroke(
            rect,
            theme::CONTROL_RADIUS,
            egui::Stroke::new(1.0, theme::BORDER_FOCUS),
            egui::StrokeKind::Inside,
        );
    }
    let color = if !ui.is_enabled() {
        theme::DISABLED
    } else if selected {
        crate::ui::theme::ACCENT
    } else if response.hovered() {
        theme::TEXT
    } else {
        crate::ui::theme::MUTED
    };
    paint_caption_icon(ui.painter(), rect, icon, color);
}

fn paint_caption_icon(
    painter: &egui::Painter,
    rect: egui::Rect,
    icon: ViewerCaptionIcon,
    color: egui::Color32,
) {
    let center = rect.center();
    let stroke = egui::Stroke::new(theme::ICON_STROKE, color);
    match icon {
        ViewerCaptionIcon::Microphone => {
            painter.rect_stroke(
                egui::Rect::from_center_size(center + egui::vec2(0.0, -3.0), egui::vec2(6.0, 10.0)),
                3.0,
                stroke,
                egui::StrokeKind::Inside,
            );
            painter.line(
                vec![
                    center + egui::vec2(-6.0, -2.0),
                    center + egui::vec2(-6.0, 1.0),
                    center + egui::vec2(-3.0, 4.0),
                    center + egui::vec2(3.0, 4.0),
                    center + egui::vec2(6.0, 1.0),
                    center + egui::vec2(6.0, -2.0),
                ],
                stroke,
            );
            painter.line_segment(
                [center + egui::vec2(0.0, 4.0), center + egui::vec2(0.0, 8.0)],
                stroke,
            );
            painter.line_segment(
                [
                    center + egui::vec2(-4.0, 8.0),
                    center + egui::vec2(4.0, 8.0),
                ],
                stroke,
            );
        }
        ViewerCaptionIcon::Annotation => super::annotation::paint_annotation_icon(
            painter,
            egui::Rect::from_center_size(center, egui::vec2(16., 16.)),
            super::annotation::AnnotationIcon::Pen,
            color,
        ),
        #[cfg(windows)]
        ViewerCaptionIcon::Plugins => crate::plugins::paint_plugin_icon(painter, rect, color),
        ViewerCaptionIcon::OneToOne => {
            for (x, y) in [(-1.0, -1.0), (1.0, -1.0), (-1.0, 1.0), (1.0, 1.0)] {
                let corner = center + egui::vec2(x * 7.0, y * 6.0);
                painter.line(
                    vec![
                        corner - egui::vec2(x * 4.0, 0.0),
                        corner,
                        corner - egui::vec2(0.0, y * 4.0),
                    ],
                    stroke,
                );
            }
            painter.rect_stroke(
                egui::Rect::from_center_size(center, egui::vec2(3.0, 3.0)),
                0.0,
                stroke,
                egui::StrokeKind::Inside,
            );
        }
        ViewerCaptionIcon::Mouse => paint_mouse(painter, center, stroke),
        #[cfg(target_os = "linux")]
        ViewerCaptionIcon::AskControl => {
            paint_mouse(painter, center - egui::vec2(3.0, 0.0), stroke);
            let plus = center + egui::vec2(7.0, 0.0);
            for offset in [egui::vec2(3.0, 0.0), egui::vec2(0.0, 3.0)] {
                painter.line_segment([plus - offset, plus + offset], stroke);
            }
        }
        #[cfg(target_os = "linux")]
        ViewerCaptionIcon::Audio { muted } => {
            super::paint_headphones(painter, rect, color);
            if muted {
                let slash = egui::vec2(8.0, 8.0);
                painter.line_segment([center - slash, center + slash], stroke);
            }
        }
        ViewerCaptionIcon::Quality => {
            for (offset, knob) in [(-5.0, -3.0), (0.0, 4.0), (5.0, -1.0)] {
                painter.line_segment(
                    [
                        egui::pos2(center.x - 7.0, center.y + offset),
                        egui::pos2(center.x + 7.0, center.y + offset),
                    ],
                    stroke,
                );
                painter.circle_filled(egui::pos2(center.x + knob, center.y + offset), 2.0, color);
            }
        }
        ViewerCaptionIcon::Participants => {
            let shoulders = |center: egui::Pos2, radius: egui::Vec2, from: f32, to: f32| {
                (0..=12)
                    .map(|i| {
                        let angle = from + (to - from) * i as f32 / 12.0;
                        center + egui::vec2(angle.cos() * radius.x, angle.sin() * radius.y)
                    })
                    .collect::<Vec<_>>()
            };
            let pi = std::f32::consts::PI;
            // The second person stands behind the first, offset right.
            painter.circle_stroke(center + egui::vec2(4.5, -4.5), 2.5, stroke);
            painter.line(
                shoulders(
                    center + egui::vec2(4.5, 6.0),
                    egui::vec2(4.5, 5.0),
                    -0.45 * pi,
                    0.0,
                ),
                stroke,
            );
            painter.circle_stroke(center + egui::vec2(-2.0, -2.5), 3.0, stroke);
            painter.line(
                shoulders(
                    center + egui::vec2(-2.0, 8.0),
                    egui::vec2(5.5, 5.5),
                    pi,
                    2.0 * pi,
                ),
                stroke,
            );
        }
        ViewerCaptionIcon::Minimize => {
            painter.line_segment(
                [
                    egui::pos2(center.x - 6.0, center.y + 4.0),
                    egui::pos2(center.x + 6.0, center.y + 4.0),
                ],
                stroke,
            );
        }
        ViewerCaptionIcon::Maximize => {
            painter.rect_stroke(
                egui::Rect::from_center_size(center, egui::vec2(11.0, 9.0)),
                0.5,
                stroke,
                egui::StrokeKind::Inside,
            );
        }
        ViewerCaptionIcon::Restore => {
            let back = egui::Rect::from_min_size(
                egui::pos2(center.x - 4.0, center.y - 6.0),
                egui::vec2(9.0, 8.0),
            );
            let front = back.translate(egui::vec2(-2.5, 2.5));
            painter.rect_stroke(back, 0.5, stroke, egui::StrokeKind::Inside);
            painter.rect_filled(front, 0.0, crate::ui::theme::SIDEBAR);
            painter.rect_stroke(front, 0.5, stroke, egui::StrokeKind::Inside);
        }
        ViewerCaptionIcon::Close => {
            painter.line_segment(
                [center - egui::vec2(5.0, 5.0), center + egui::vec2(5.0, 5.0)],
                stroke,
            );
            painter.line_segment(
                [
                    center + egui::vec2(-5.0, 5.0),
                    center + egui::vec2(5.0, -5.0),
                ],
                stroke,
            );
        }
    }
}

fn paint_mouse(painter: &egui::Painter, center: egui::Pos2, stroke: egui::Stroke) {
    let body = egui::Rect::from_center_size(center, egui::vec2(11.0, 16.0));
    painter.rect_stroke(body, 5.0, stroke, egui::StrokeKind::Inside);
    painter.line_segment(
        [
            center + egui::vec2(0.0, -6.0),
            center + egui::vec2(0.0, -2.0),
        ],
        stroke,
    );
    painter.line_segment(
        [
            center + egui::vec2(-4.5, -1.0),
            center + egui::vec2(4.5, -1.0),
        ],
        stroke,
    );
}
