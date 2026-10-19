//! Passive warning painted without registering an input region.
use super::theme;

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
        format!("鼠标输入频率较高 · 约 {} Hz", hz),
        egui::FontId::proportional(theme::BODY),
        theme::AMBER,
        width - margin * 2.0,
    );
    let body = painter.layout(
        "高回报率可能增加 CPU 负担和远程输入延迟。可在“高级设置 → 鼠标模式”中开启发送节流。".into(),
        egui::FontId::proportional(theme::COMPACT_TEXT),
        theme::TEXT,
        width - margin * 2.0,
    );
    let height = margin * 2.0 + title.size().y + 6.0 + body.size().y;
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
    let origin = rect.min + egui::vec2(margin, margin);
    let title_height = title.size().y;
    painter.galley(origin, title, theme::AMBER);
    painter.galley(
        origin + egui::vec2(0.0, title_height + 6.0),
        body,
        theme::TEXT,
    );
}
