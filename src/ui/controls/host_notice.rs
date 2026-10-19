//! One measured card layout for access, transfer and result notifications.
use super::theme;
use crate::application::app::notifications::{ButtonKind, Card, Verb};
use egui::{Align, FontId, Layout, Rect, RichText, Sense, Ui, UiBuilder, Vec2, pos2, vec2};
use std::sync::Arc;

fn text(
    ui: &Ui,
    value: &str,
    size: f32,
    color: egui::Color32,
    width: f32,
    rows: usize,
) -> Arc<egui::Galley> {
    let mut job = egui::text::LayoutJob::simple(
        value.to_owned(),
        FontId::proportional(size),
        color,
        width.max(1.),
    );
    job.wrap.max_rows = rows;
    job.wrap.break_anywhere = true;
    ui.fonts_mut(|fonts| fonts.layout_job(job))
}
fn label(ui: &mut Ui, rect: Rect, galley: Arc<egui::Galley>) {
    let mut child = ui.new_child(
        UiBuilder::new()
            .max_rect(rect)
            .layout(Layout::top_down(Align::Min)),
    );
    child.add(egui::Label::new(galley).selectable(false));
}
fn line(ui: &mut Ui, value: &str, size: f32, color: egui::Color32, rows: usize) {
    if value.is_empty() {
        return;
    }
    let galley = text(ui, value, size, color, ui.available_width(), rows);
    let (rect, _) =
        ui.allocate_exact_size(vec2(ui.available_width(), galley.size().y), Sense::hover());
    label(ui, rect, galley);
}
fn button(ui: &mut Ui, rect: Rect, title: &str, kind: ButtonKind, enabled: bool) -> bool {
    let mut child = ui.new_child(
        UiBuilder::new()
            .max_rect(rect)
            .layout(Layout::left_to_right(Align::Center)),
    );
    child.spacing_mut().interact_size.y = theme::NOTIFICATION_ACTION_HEIGHT;
    child.spacing_mut().button_padding = vec2(8., 3.);
    let mut button = egui::Button::new(RichText::new(title).size(theme::SMALL).color(
        if matches!(kind, ButtonKind::Danger) {
            theme::RED
        } else {
            theme::TEXT
        },
    ))
    .min_size(rect.size());
    if matches!(kind, ButtonKind::Primary) {
        button = button.fill(theme::ACCENT).stroke(egui::Stroke::NONE);
    }
    child.add_enabled(enabled, button).clicked()
}
fn header(ui: &mut Ui, card: &Card) -> bool {
    let height = theme::NOTIFICATION_HEADER_HEIGHT;
    let (rect, _) = ui.allocate_exact_size(vec2(ui.available_width(), height), Sense::hover());
    let close = Rect::from_min_size(pos2(rect.right() - height, rect.top()), Vec2::splat(height));
    let mut close_ui = ui.new_child(UiBuilder::new().max_rect(close));
    let dismiss = super::close_button(&mut close_ui, "收起通知", height).clicked();
    let mut end = close.left() - theme::NOTIFICATION_GAP;
    if card.confirmation {
        let seconds = format!(
            "{}s",
            (card.expires_at - chrono::Utc::now().timestamp()).max(0)
        );
        let galley = text(ui, &seconds, theme::SMALL, theme::MUTED, 48., 1);
        let r = Rect::from_min_size(
            pos2(
                end - galley.size().x,
                rect.center().y - galley.size().y / 2.,
            ),
            galley.size(),
        );
        label(ui, r, galley);
        end = r.left() - theme::NOTIFICATION_ACTION_GAP;
    }
    let mut logo = ui.new_child(UiBuilder::new().max_rect(Rect::from_min_size(
        pos2(rect.left(), rect.center().y - theme::WINDOW_LOGO_SIZE / 2.),
        Vec2::splat(theme::WINDOW_LOGO_SIZE),
    )));
    crate::ui::chrome::paint_brand_logo(&mut logo);
    let left = rect.left() + theme::WINDOW_LOGO_SIZE + theme::NOTIFICATION_ACTION_GAP;
    let galley = text(ui, &card.title, theme::SMALL, theme::MUTED, end - left, 1);
    label(
        ui,
        Rect::from_min_size(
            pos2(left, rect.center().y - galley.size().y / 2.),
            galley.size(),
        ),
        galley,
    );
    dismiss
}
fn actions(ui: &mut Ui, card: &Card) -> Option<Verb> {
    let actions = card.actions();
    if actions.is_empty() {
        return None;
    }
    let height = theme::NOTIFICATION_ACTION_HEIGHT;
    let gap = theme::NOTIFICATION_ACTION_GAP;
    let (row, _) = ui.allocate_exact_size(vec2(ui.available_width(), height), Sense::hover());
    let width = (row.width() - gap * (actions.len() - 1) as f32) / actions.len() as f32;
    let mut clicked = None;
    for (i, action) in actions.into_iter().enumerate() {
        let rect = Rect::from_min_size(
            pos2(row.left() + i as f32 * (width + gap), row.top()),
            vec2(width, height),
        );
        if button(ui, rect, action.label, action.kind, action.enabled) {
            clicked = Some(action.verb);
        }
    }
    clicked
}
pub(crate) fn host_notice(ui: &mut Ui, card: &Card) -> Option<Verb> {
    ui.push_id(&card.ticket, |ui| {
        let mut action = None;
        egui::Frame::new()
            .fill(theme::BG)
            .inner_margin(theme::NOTIFICATION_PADDING)
            .show(ui, |ui| {
                ui.spacing_mut().item_spacing =
                    vec2(theme::NOTIFICATION_ACTION_GAP, theme::NOTIFICATION_GAP);
                if header(ui, card) {
                    action = Some(Verb::Dismiss);
                }
                line(ui, &card.body, theme::BODY, theme::TEXT, 2);
                let mut detail = card.detail.clone();
                if let Some(progress) = card.transfer.as_ref().and_then(|t| t.progress) {
                    let fraction = f32::from(progress.min(1000)) / 1000.;
                    let (rect, _) = ui.allocate_exact_size(
                        vec2(ui.available_width(), theme::NOTIFICATION_PROGRESS_HEIGHT),
                        Sense::hover(),
                    );
                    ui.painter().rect_filled(rect, 2., theme::LINE);
                    if fraction > 0. {
                        ui.painter().rect_filled(
                            Rect::from_min_size(
                                rect.min,
                                vec2(rect.width() * fraction, rect.height()),
                            ),
                            2.,
                            theme::ACCENT,
                        );
                    }
                    detail = format!("{} · {:.0}%", detail, fraction * 100.);
                }
                line(ui, &detail, theme::SMALL, theme::MUTED, 2);
                if let Some(clicked) = actions(ui, card) {
                    action = Some(clicked);
                }
            });
        action
    })
    .inner
}
