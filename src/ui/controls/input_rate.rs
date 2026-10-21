//! Passive warning painted without registering an input region.
use super::theme;
use crate::i18n::t;

pub(crate) fn input_rate_warning(ctx: &egui::Context, hz: u32, bounds: egui::Rect) {
    let margin = theme::INPUT_WARNING_MARGIN;
    let width = theme::INPUT_WARNING_WIDTH.min(bounds.width() - margin * 2.0);
    if width < 120.0 {
        return;
    }
    let painter = ctx.layer_painter(egui::LayerId::new(
        egui::Order::Foreground,
        egui::Id::new("mouse-polling-warning"),
    ));
    let title = painter.layout(
        (t().controls.high_mouse_rate)(hz),
        egui::FontId::proportional(theme::BODY),
        theme::AMBER,
        width - margin * 2.0,
    );
    let height = margin * 2.0 + title.size().y;
    let rect = egui::Rect::from_min_size(
        egui::pos2(bounds.right() - width - margin, bounds.top() + margin),
        egui::vec2(width, height),
    );
    // Painting alone registers no egui Area; the native input adapter must keep
    // accepting points underneath this warning, including during a drag.
    painter.rect(
        rect,
        theme::PANEL_RADIUS,
        theme::SURFACE,
        egui::Stroke::new(1.0, theme::LINE),
        egui::StrokeKind::Inside,
    );
    painter.galley(rect.min + egui::vec2(margin, margin), title, theme::AMBER);
}
