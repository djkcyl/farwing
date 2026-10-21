use crate::i18n::t;
use super::*;
use egui::{FontId, Rect, Sense};

#[derive(Clone, Copy)]
pub(crate) enum DialogIcon {
    Required,
    Waiting,
    Error,
    Ready,
    Warning,
    Info,
    Edit,
    Files,
}

fn paint_dialog_icon(ui: &mut egui::Ui, rect: Rect, icon: DialogIcon) {
    let color = match icon {
        DialogIcon::Required | DialogIcon::Warning => theme::AMBER,
        DialogIcon::Error => theme::RED,
        _ => theme::ACCENT,
    };
    let p = ui.painter_at(rect);
    p.rect_filled(rect, theme::CONTROL_RADIUS, theme::SURFACE);
    let c = rect.center();
    let s = Stroke::new(theme::ICON_STROKE, color);
    let line = |a: egui::Vec2, b: egui::Vec2| {
        p.line_segment([c + a, c + b], s);
    };
    match icon {
        DialogIcon::Waiting => {
            ui.put(
                rect.shrink(7.),
                egui::Spinner::new().size(rect.width() - 14.).color(color),
            );
        }
        DialogIcon::Error | DialogIcon::Info => {
            p.circle_stroke(c, 8., s);
            let d = if matches!(icon, DialogIcon::Info) {
                -1.
            } else {
                1.
            };
            line(vec2(0., -4. * d), vec2(0., 1. * d));
            p.circle_filled(c + vec2(0., 4.5 * d), 1., color);
        }
        DialogIcon::Warning => {
            p.add(egui::Shape::closed_line(
                vec![c + vec2(0., -8.), c + vec2(9., 7.), c + vec2(-9., 7.)],
                s,
            ));
            line(vec2(0., -3.), vec2(0., 1.));
            p.circle_filled(c + vec2(0., 4.), 1., color);
        }
        DialogIcon::Ready => {
            line(vec2(-7., 0.), vec2(-2., 5.));
            line(vec2(-2., 5.), vec2(7., -5.));
        }
        DialogIcon::Required => {
            line(vec2(0., 6.), vec2(0., -7.));
            line(vec2(-5., -2.), vec2(0., -7.));
            line(vec2(0., -7.), vec2(5., -2.));
            line(vec2(-8., 8.), vec2(8., 8.));
        }
        DialogIcon::Files => paint_file_icon(&p, rect, color, true),
        DialogIcon::Edit => {
            p.add(egui::Shape::closed_line(
                vec![
                    c + vec2(-6., 6.),
                    c + vec2(-5., 1.),
                    c + vec2(3., -7.),
                    c + vec2(7., -3.),
                    c + vec2(-1., 5.),
                ],
                s,
            ));
        }
    }
}


pub(crate) fn dialog_header(
    ui: &mut egui::Ui,
    title: &str,
    icon: DialogIcon,
    closable: bool,
) -> bool {
    super::notices::register_dialog(ui.ctx(), ui.layer_id());
    configure(ui.style_mut(), theme::CONTROL_HEIGHT);
    ui.spacing_mut().item_spacing = vec2(8., 8.);
    let h = theme::DIALOG_HEADER_HEIGHT;
    let (row, _) = ui.allocate_exact_size(vec2(ui.available_width(), h), Sense::hover());
    let icon_rect = Rect::from_min_size(row.min, vec2(h, h));
    paint_dialog_icon(ui, icon_rect, icon);
    let close_rect = Rect::from_min_size(row.right_top() - vec2(h, 0.), vec2(h, h));
    let title_rect = Rect::from_min_max(
        row.min + vec2(h + 12., 0.),
        egui::pos2(
            if closable {
                close_rect.left() - 8.
            } else {
                row.right()
            },
            row.bottom(),
        ),
    );
    let mut job = egui::text::LayoutJob::simple_singleline(
        title.into(),
        FontId::proportional(theme::DIALOG_HEADING),
        TEXT,
    );
    job.wrap.max_width = title_rect.width().max(0.);
    job.wrap.max_rows = 1;
    job.wrap.break_anywhere = true;
    let galley = ui.fonts_mut(|f| f.layout_job(job));
    ui.painter().galley(
        title_rect.left_center() - vec2(0., galley.size().y / 2.),
        galley,
        TEXT,
    );
    let close = if closable {
        let response = ui.interact(close_rect, ui.id().with("dialog-close"), Sense::click());
        if response.hovered() {
            ui.painter()
                .rect_filled(close_rect, theme::CONTROL_RADIUS, HOVER);
        }
        paint_close(
            ui.painter(),
            close_rect,
            if response.hovered() { TEXT } else { MUTED },
        );
        response.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, true, t().common.close));
        response.on_hover_text(t().common.close).clicked()
    } else {
        false
    };
    // allocate_exact_size already advances by item_spacing.y.
    ui.add_space((theme::DIALOG_HEADER_GAP - ui.spacing().item_spacing.y).max(0.));
    close
}

pub(crate) struct DialogAction<'a> {
    label: &'a str,
    enabled: bool,
    danger: bool,
}
impl<'a> DialogAction<'a> {
    pub fn new(label: &'a str) -> Self {
        Self {
            label,
            enabled: true,
            danger: false,
        }
    }
    pub fn enabled(mut self, enabled: bool) -> Self {
        self.enabled = enabled;
        self
    }
    pub fn danger(mut self, danger: bool) -> Self {
        self.danger = danger;
        self
    }
}

pub(crate) fn dialog_actions(
    ui: &mut egui::Ui,
    primary_action: Option<DialogAction<'_>>,
    secondary_label: Option<&str>,
) -> (bool, bool) {
    dialog_actions_with_hint(ui, primary_action, secondary_label.map(|label| (label, "")))
}

pub(super) fn dialog_actions_with_hint(
    ui: &mut egui::Ui,
    primary_action: Option<DialogAction<'_>>,
    secondary_label: Option<(&str, &str)>,
) -> (bool, bool) {
    ui.add_space((theme::DIALOG_ACTION_GAP - ui.spacing().item_spacing.y).max(0.));
    let labels = primary_action
        .as_ref()
        .map(|a| a.label)
        .into_iter()
        .chain(secondary_label.map(|(label, _)| label));
    let width = labels
        .map(|label| {
            ui.painter()
                .layout_no_wrap(label.into(), FontId::proportional(theme::BODY), TEXT)
                .size()
                .x
                + 24.
        })
        .fold(theme::DIALOG_ACTION_WIDTH, f32::max);
    let count = usize::from(primary_action.is_some()) + usize::from(secondary_label.is_some());
    let width = width
        .min((ui.available_width() - 8. * count.saturating_sub(1) as f32) / count.max(1) as f32);
    let (row, _) = ui.allocate_exact_size(
        vec2(ui.available_width(), theme::CONTROL_HEIGHT),
        Sense::hover(),
    );
    let mut right = row.right();
    let mut clicked = (false, false);
    if let Some(a) = primary_action {
        let rect = Rect::from_min_size(
            egui::pos2(right - width, row.top()),
            vec2(width, row.height()),
        );
        clicked.0 = action_button(ui, rect, a.label, a.enabled, true, a.danger, "");
        right -= width + 8.;
    }
    if let Some((label, hint)) = secondary_label {
        let rect = Rect::from_min_size(
            egui::pos2(right - width, row.top()),
            vec2(width, row.height()),
        );
        clicked.1 = action_button(ui, rect, label, true, false, false, hint);
    }
    clicked
}

fn action_button(
    ui: &mut egui::Ui,
    rect: Rect,
    label: &str,
    enabled: bool,
    primary: bool,
    danger: bool,
    hint: &str,
) -> bool {
    let button = if primary {
        super::primary(label)
    } else {
        super::secondary(label)
    };
    let button = if danger {
        button.fill(theme::DANGER_FILL)
    } else {
        button
    };
    ui.scope_builder(
        egui::UiBuilder::new()
            .id_salt(("dialog-action", label))
            .max_rect(rect),
        |ui| {
            let response = ui
                .add_enabled_ui(enabled, |ui| ui.put(rect, button.truncate()))
                .inner;
            if hint.is_empty() {
                response.clicked()
            } else {
                response.on_hover_text(hint).clicked()
            }
        },
    )
    .inner
}
