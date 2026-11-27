use super::theme::{self, BG, LINE, LOGO_LOGIN, TEXT};
use egui::{Align, Color32, Stroke, vec2};

/// The sign-in card: the brand on top, the current step below.
pub(crate) fn card(root: &mut egui::Ui, height: f32, content: impl FnOnce(&mut egui::Ui)) {
    egui::CentralPanel::default()
        .frame(egui::Frame::new().fill(BG).inner_margin(24))
        .show(root, |ui| {
            let bounds = ui.available_rect_before_wrap();
            // The card ends where its content does, measured on the last
            // frame; `height` is the first guess.
            let measured = egui::Id::new("sign-in-content-height");
            let content_height = ui.ctx().data(|d| d.get_temp::<f32>(measured));
            let height = content_height.map_or(height, |h| 98.0 + h + 40.0);
            let card = egui::Rect::from_center_size(
                bounds.center(),
                vec2(
                    460.0_f32.min(bounds.width()),
                    height.min(bounds.height()),
                ),
            );
            let painter = ui.painter();
            painter.rect_filled(
                card.translate(vec2(0.0, 8.0)),
                theme::PANEL_RADIUS,
                Color32::from_black_alpha(28),
            );
            painter.rect_filled(card, theme::PANEL_RADIUS, crate::ui::theme::SURFACE);
            painter.rect_stroke(
                card,
                theme::PANEL_RADIUS,
                Stroke::new(1.0, LINE),
                egui::StrokeKind::Inside,
            );
            let logo = crate::ui::branding::logo_size(LOGO_LOGIN);
            crate::ui::branding::paint_logo(
                painter,
                card.center().x - logo.x * 0.5,
                card.top() + 50.0,
                LOGO_LOGIN,
                TEXT,
            );
            let column = egui::Rect::from_min_max(
                egui::pos2(card.left() + 40.0, card.top() + 98.0),
                egui::pos2(card.right() - 40.0, card.bottom() - 24.0),
            );
            let used = ui
                .scope_builder(
                    egui::UiBuilder::new()
                        .id_salt("sign-in")
                        .max_rect(column)
                        .layout(egui::Layout::top_down(Align::Min)),
                    |ui| {
                        ui.spacing_mut().item_spacing.y = 6.0;
                        content(ui);
                    },
                )
                .response
                .rect
                .height();
            if content_height.is_none_or(|h| (h - used).abs() > 0.5) {
                ui.ctx().data_mut(|d| d.insert_temp(measured, used));
                ui.ctx().request_repaint();
            }
        });
}
