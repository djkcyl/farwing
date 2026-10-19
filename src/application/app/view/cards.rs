//! Building blocks of the friends and direct-connection pages: cards, avatars,
//! chips, banners, tabs, confirmations and the capability editor shared by
//! friend grants and pairings.
use super::*;
use crate::application::app::friends::cap_label;
use farwing_protocol::caps;

/// Capabilities by what they let the other side do, in display order.
const GROUPS: [(&str, &[&str]); 3] = [
    ("观看", &[caps::VIEW, caps::AUDIO, caps::ANNOTATION]),
    (
        "操作",
        &[caps::CONTROL, caps::CLIPBOARD, caps::MICROPHONE, caps::FILES],
    ),
    (
        "管理",
        &[caps::DISPLAY, caps::PORTS, caps::POWER, caps::DIAGNOSTICS],
    ),
];

/// Common capability sets, offered as one-click choices.
const PRESETS: [(&str, &[&str]); 3] = [
    ("仅观看", &[caps::VIEW, caps::AUDIO, caps::ANNOTATION]),
    (
        "协作",
        &[
            caps::VIEW,
            caps::CONTROL,
            caps::CLIPBOARD,
            caps::FILES,
            caps::AUDIO,
            caps::MICROPHONE,
            caps::ANNOTATION,
        ],
    ),
    ("完全控制", caps::ALL),
];

const AVATAR_COLORS: [Color32; 6] = [
    Color32::from_rgb(75, 136, 235),
    Color32::from_rgb(102, 207, 156),
    Color32::from_rgb(238, 190, 111),
    Color32::from_rgb(190, 132, 240),
    Color32::from_rgb(241, 125, 132),
    Color32::from_rgb(92, 196, 214),
];

/// The shared card: the controls' section frame with room inside.
pub(super) fn card() -> egui::Frame {
    crate::ui::controls::section_frame().inner_margin(egui::Margin::symmetric(14, 12))
}

/// Page title with one explanatory line under it, actions on the right.
pub(super) fn page_header(
    ui: &mut egui::Ui,
    title: &str,
    subtitle: &str,
    actions: impl FnOnce(&mut egui::Ui),
) {
    ui.horizontal(|ui| {
        let width = (ui.available_width() - 240.0).max(240.0);
        ui.vertical(|ui| {
            ui.set_max_width(width);
            ui.label(RichText::new(title).size(theme::TITLE).strong());
            if !subtitle.is_empty() {
                ui.add(egui::Label::new(RichText::new(subtitle).color(MUTED)).wrap());
            }
        });
        ui.with_layout(egui::Layout::right_to_left(Align::Center), actions);
    });
    ui.add_space(16.0);
}

/// A section title with an optional count, actions on the right.
pub(super) fn section_heading(
    ui: &mut egui::Ui,
    title: &str,
    count: Option<usize>,
    actions: impl FnOnce(&mut egui::Ui),
) {
    ui.add_space(22.0);
    ui.horizontal(|ui| {
        ui.label(RichText::new(title).size(theme::SECTION).strong());
        if let Some(count) = count {
            ui.label(RichText::new(count.to_string()).color(MUTED));
        }
        ui.with_layout(egui::Layout::right_to_left(Align::Center), actions);
    });
    ui.add_space(8.0);
}

/// A small rounded label.
pub(super) fn chip(ui: &mut egui::Ui, text: &str, color: Color32) -> egui::Response {
    let galley = ui.painter().layout_no_wrap(
        text.into(),
        FontId::proportional(theme::TINY),
        color,
    );
    let (rect, response) =
        ui.allocate_exact_size(galley.size() + vec2(14.0, 6.0), Sense::hover());
    ui.painter()
        .rect_filled(rect, rect.height() / 2.0, color.gamma_multiply(0.16));
    ui.painter()
        .galley(rect.center() - galley.size() / 2.0, galley, color);
    response
}

/// A round avatar with the first letter of `name`; `seed` picks the color.
pub(super) fn avatar(ui: &mut egui::Ui, seed: &str, name: &str, size: f32) {
    let (rect, _) = ui.allocate_exact_size(vec2(size, size), Sense::hover());
    let hash = seed
        .bytes()
        .fold(0u32, |hash, byte| hash.wrapping_mul(31).wrapping_add(u32::from(byte)));
    let color = AVATAR_COLORS[hash as usize % AVATAR_COLORS.len()];
    let painter = ui.painter();
    painter.circle_filled(rect.center(), size / 2.0, color.gamma_multiply(0.22));
    let initial = name
        .trim()
        .chars()
        .next()
        .map_or_else(|| "?".to_owned(), |c| c.to_uppercase().collect());
    painter.text(
        rect.center(),
        egui::Align2::CENTER_CENTER,
        initial,
        FontId::proportional(size * 0.44),
        color,
    );
}

#[derive(Clone, Copy)]
pub(super) enum Tone {
    Success,
    Warning,
    Info,
}

/// A one-line message across the page.
pub(super) fn banner(ui: &mut egui::Ui, text: &str, tone: Tone) {
    let color = match tone {
        Tone::Success => GREEN,
        Tone::Warning => AMBER,
        Tone::Info => BLUE,
    };
    egui::Frame::new()
        .fill(color.gamma_multiply(0.1))
        .stroke(Stroke::new(1.0, color.gamma_multiply(0.45)))
        .corner_radius(theme::CONTROL_RADIUS)
        .inner_margin(egui::Margin::symmetric(12, 8))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.add(egui::Label::new(RichText::new(text).color(color)).wrap());
        });
    ui.add_space(8.0);
}

/// A framed toggle among a few choices.
pub(super) fn choice(ui: &mut egui::Ui, selected: bool, label: &str) -> egui::Response {
    ui.add(
        egui::Button::new(RichText::new(label).color(if selected { TEXT } else { MUTED }))
            .selected(selected)
            .min_size(vec2(0.0, 28.0)),
    )
}

/// Segmented tabs; returns whether the selection changed.
pub(super) fn tabs<T: Copy + PartialEq>(
    ui: &mut egui::Ui,
    value: &mut T,
    options: &[(T, &str)],
) -> bool {
    let mut changed = false;
    egui::Frame::new()
        .fill(SIDEBAR)
        .corner_radius(theme::PANEL_RADIUS)
        .inner_margin(3)
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 3.0;
                for (option, label) in options {
                    let selected = *value == *option;
                    let button = egui::Button::new(
                        RichText::new(*label).color(if selected { TEXT } else { MUTED }),
                    )
                    .fill(if selected {
                        theme::SELECTED
                    } else {
                        Color32::TRANSPARENT
                    })
                    .stroke(Stroke::NONE)
                    .min_size(vec2(150.0, 30.0));
                    let response = ui.add(button);
                    response.widget_info(|| {
                        egui::WidgetInfo::selected(egui::WidgetType::SelectableLabel, true, selected, *label)
                    });
                    if response.clicked() && !selected {
                        *value = *option;
                        changed = true;
                    }
                }
            });
        });
    changed
}

/// A confirmation dialog: `Some(true)` confirmed, `Some(false)` dismissed,
/// `None` while open.
pub(super) fn confirm(
    ctx: &egui::Context,
    id: &str,
    title: &str,
    body: &str,
    action: &str,
) -> Option<bool> {
    let mut answer = None;
    let response = egui::Modal::new(egui::Id::new(id))
        .frame(dialog_frame())
        .show(ctx, |ui| {
            ui.set_width(430.0);
            if crate::ui::controls::dialog_header(
                ui,
                title,
                crate::ui::controls::DialogIcon::Warning,
                true,
            ) {
                answer = Some(false);
            }
            ui.add(egui::Label::new(body).wrap());
            let (accept, dismiss) = crate::ui::controls::dialog_actions(
                ui,
                Some(crate::ui::controls::DialogAction::new(action).danger(true)),
                Some("取消"),
            );
            if accept {
                answer = Some(true);
            } else if dismiss {
                answer = Some(false);
            }
        });
    if answer.is_none() && response.should_close() {
        answer = Some(false);
    }
    answer
}

fn same_set(list: &[String], preset: &[&str]) -> bool {
    caps::normalize(list) == caps::normalize(preset)
}

/// What a capability set allows, in a few words.
pub(super) fn caps_summary(list: &[String]) -> String {
    if list.is_empty() {
        return "无权限".into();
    }
    if let Some((label, _)) = PRESETS.iter().find(|(_, preset)| same_set(list, preset)) {
        return (*label).into();
    }
    let labels: Vec<_> = caps::normalize(list).iter().map(|c| cap_label(c)).collect();
    if labels.len() <= 3 {
        labels.join("、")
    } else {
        format!("{} 等 {} 项", labels[..3].join("、"), labels.len())
    }
}

/// What a friend sees of one device: nothing at all without capabilities.
pub(super) fn share_summary(list: &[String]) -> String {
    if list.is_empty() {
        "不共享 · 不可见".into()
    } else {
        caps_summary(list)
    }
}

/// Edit a capability set: presets, then the capabilities by group. Returns
/// whether anything changed.
pub(super) fn caps_editor(ui: &mut egui::Ui, id: impl egui::AsIdSalt, list: &mut Vec<String>) -> bool {
    let mut changed = false;
    ui.push_id(id, |ui| {
        ui.horizontal_wrapped(|ui| {
            group_label(ui, "快速选择");
            for (label, preset) in PRESETS {
                let selected = same_set(list, preset);
                if choice(ui, selected, label).clicked() && !selected {
                    *list = caps::normalize(preset);
                    changed = true;
                }
            }
        });
        ui.add_space(4.0);
        for (group, members) in GROUPS {
            ui.horizontal_wrapped(|ui| {
                group_label(ui, group);
                for cap in members {
                    let mut on = list.iter().any(|c| c == cap);
                    if ui.checkbox(&mut on, cap_label(cap)).changed() {
                        changed = true;
                        if on {
                            list.push((*cap).to_owned());
                        } else {
                            list.retain(|c| c != cap);
                        }
                    }
                }
            });
        }
        if !list.is_empty() && !list.iter().any(|c| c == caps::VIEW) {
            ui.label(
                RichText::new("未勾选“观看画面”时，对方无法连接")
                    .size(theme::SMALL)
                    .color(AMBER),
            );
        }
    });
    if changed {
        *list = caps::normalize(list);
    }
    changed
}

fn group_label(ui: &mut egui::Ui, text: &str) {
    let (rect, _) = ui.allocate_exact_size(vec2(56.0, 20.0), Sense::hover());
    ui.painter().text(
        rect.left_center(),
        egui::Align2::LEFT_CENTER,
        text,
        FontId::proportional(theme::SMALL),
        MUTED,
    );
}

/// A small padlock centred on `center`, `size` high.
pub(super) fn paint_lock(painter: &egui::Painter, center: egui::Pos2, size: f32, color: Color32) {
    let body = egui::Rect::from_center_size(
        center + vec2(0.0, size * 0.18),
        vec2(size * 0.7, size * 0.48),
    );
    painter.rect_filled(body, size * 0.08, color);
    let radius = size * 0.22;
    let arch: Vec<_> = (0..=12)
        .map(|i| {
            let angle = std::f32::consts::PI * (1.0 + i as f32 / 12.0);
            egui::pos2(center.x + angle.cos() * radius, body.top() + angle.sin() * radius)
        })
        .collect();
    painter.add(egui::Shape::line(arch, Stroke::new(size * 0.1, color)));
}

/// What a feature needs before it can be used and the ways to get there.
/// Returns the action chosen; the first one is the primary.
pub(super) fn locked(ui: &mut egui::Ui, title: &str, hint: &str, actions: &[&str]) -> Option<usize> {
    let mut chosen = None;
    card().show(ui, |ui| {
        ui.set_width(ui.available_width());
        ui.add_space(12.0);
        ui.vertical_centered(|ui| {
            let (rect, _) = ui.allocate_exact_size(vec2(40.0, 40.0), Sense::hover());
            ui.painter()
                .circle_filled(rect.center(), 20.0, BLUE.gamma_multiply(0.14));
            paint_lock(ui.painter(), rect.center(), 18.0, BLUE);
            ui.add_space(8.0);
            ui.label(RichText::new(title).size(theme::SECTION).strong());
            ui.set_max_width(520.0);
            ui.add(egui::Label::new(RichText::new(hint).color(MUTED)).wrap());
            if actions.is_empty() {
                return;
            }
            ui.add_space(12.0);
            let widths: Vec<f32> = actions
                .iter()
                .map(|label| {
                    ui.painter()
                        .layout_no_wrap((*label).into(), FontId::proportional(theme::BODY), TEXT)
                        .size()
                        .x
                        + 40.0
                })
                .collect();
            let total = widths.iter().sum::<f32>() + 8.0 * (widths.len() - 1) as f32;
            ui.horizontal(|ui| {
                ui.add_space(((ui.available_width() - total) / 2.0).max(0.0));
                for (index, (label, width)) in actions.iter().zip(widths).enumerate() {
                    let button = if index == 0 {
                        primary(label)
                    } else {
                        crate::ui::controls::secondary(label)
                    };
                    if ui.add(button.min_size(vec2(width, theme::CONTROL_HEIGHT))).clicked() {
                        chosen = Some(index);
                    }
                }
            });
        });
        ui.add_space(12.0);
    });
    chosen
}

/// A local date and time.
pub(super) fn when(seconds: i64) -> String {
    if seconds <= 0 {
        return "未知".into();
    }
    chrono::DateTime::from_timestamp(seconds, 0)
        .map(|t| {
            t.with_timezone(&chrono::Local)
                .format("%Y-%m-%d %H:%M")
                .to_string()
        })
        .unwrap_or_default()
}
