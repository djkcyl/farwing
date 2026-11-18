//! Shared icons and interaction states for the player caption.
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
        egui::WidgetInfo::labeled(egui::WidgetType::Button, ui.is_enabled(), "展开全屏操作栏")
    });
    (response, hit_rect)
}

#[derive(Clone, Copy)]
pub(crate) enum ViewerCaptionIcon {
    Plugins,
    Annotation,
    OneToOne,
    Mouse,
    Microphone,
    Clipboard,
    Quality,
    Fullscreen,
    ExitFullscreen,
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
    let size = if matches!(
        icon,
        ViewerCaptionIcon::Fullscreen | ViewerCaptionIcon::ExitFullscreen
    ) {
        egui::vec2(
            theme::VIEWER_FULLSCREEN_BUTTON_WIDTH,
            theme::WINDOW_TITLE_CONTENT_HEIGHT,
        )
    } else {
        egui::Vec2::splat(theme::VIEWER_CAPTION_BUTTON)
    };
    let (rect, response) = ui.allocate_exact_size(size, egui::Sense::click());
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
    if tooltip.is_empty() {
        response.widget_info(|| {
            egui::WidgetInfo::labeled(
                egui::WidgetType::Button,
                ui.is_enabled(),
                match icon {
                    ViewerCaptionIcon::Minimize => "最小化",
                    ViewerCaptionIcon::Fullscreen => "全屏",
                    ViewerCaptionIcon::ExitFullscreen => "退出全屏",
                    ViewerCaptionIcon::Close => "关闭",
                    _ => "窗口操作",
                },
            )
        });
        response
    } else {
        response.on_hover_text(tooltip)
    }
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
            super::AnnotationIcon::Pen,
            color,
        ),
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
        ViewerCaptionIcon::Mouse => {
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
        ViewerCaptionIcon::Clipboard => {
            // 一块带夹子的板子，缩到 12px 也还认得出。
            let board =
                egui::Rect::from_center_size(center + egui::vec2(0.0, 1.0), egui::vec2(11.0, 13.0));
            painter.rect_stroke(board, 2.0, stroke, egui::StrokeKind::Inside);
            painter.rect_filled(
                egui::Rect::from_center_size(
                    egui::pos2(center.x, board.top() + 1.0),
                    egui::vec2(5.5, 3.5),
                ),
                1.0,
                color,
            );
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
        ViewerCaptionIcon::Fullscreen | ViewerCaptionIcon::ExitFullscreen => {
            let exiting = matches!(icon, ViewerCaptionIcon::ExitFullscreen);
            let (distance, direction) = if exiting {
                (egui::vec2(3.0, 2.0), 1.0)
            } else {
                (egui::vec2(7.0, 6.0), -1.0)
            };
            for (x, y) in [(-1.0, -1.0), (1.0, -1.0), (-1.0, 1.0), (1.0, 1.0)] {
                let corner = center + egui::vec2(x * distance.x, y * distance.y);
                painter.line(
                    vec![
                        corner + egui::vec2(x * 4.0 * direction, 0.0),
                        corner,
                        corner + egui::vec2(0.0, y * 4.0 * direction),
                    ],
                    stroke,
                );
            }
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
