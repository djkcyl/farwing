//! Progress stays inside the owning action, without adding a page-wide status row.
use super::*;
pub(crate) fn task_button_progress(ui: &egui::Ui, button: egui::Rect, fraction: f32) {
    let inner = button.shrink(3.0);
    let rect = egui::Rect::from_min_size(
        egui::pos2(inner.left(), inner.bottom() - theme::TASK_PROGRESS_HEIGHT),
        vec2(
            inner.width() * fraction.clamp(0.0, 1.0),
            theme::TASK_PROGRESS_HEIGHT,
        ),
    );
    ui.painter()
        .rect_filled(rect, theme::CONTROL_RADIUS, ACCENT);
}
