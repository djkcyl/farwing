//! Building blocks of the friends and direct-connection pages: cards, avatars,
//! chips, banners, tabs, confirmations and the capability editor shared by
//! friend grants and pairings.
use super::*;
use crate::application::app::friends::cap_label;
use farwing_protocol::caps;

/// Capabilities by what they let the other side do, in display order.
const GROUPS: [&[&str]; 3] = [
    &[caps::VIEW, caps::AUDIO, caps::ANNOTATION],
    &[caps::CONTROL, caps::CLIPBOARD, caps::MICROPHONE, caps::FILES],
    &[caps::DISPLAY, caps::PORTS, caps::POWER, caps::DIAGNOSTICS],
];

/// The title of [`GROUPS`]`[index]`.
fn group_title(index: usize) -> &'static str {
    let text = &t().center;
    match index {
        0 => text.caps_view,
        1 => text.caps_control,
        _ => text.caps_manage,
    }
}

/// Common capability sets, offered as one-click choices.
const PRESETS: [&[&str]; 3] = [
    &[caps::VIEW, caps::AUDIO, caps::ANNOTATION],
    &[
        caps::VIEW,
        caps::CONTROL,
        caps::CLIPBOARD,
        caps::FILES,
        caps::AUDIO,
        caps::MICROPHONE,
        caps::ANNOTATION,
    ],
    caps::ALL,
];

/// The name of [`PRESETS`]`[index]`.
fn preset_title(index: usize) -> &'static str {
    let text = &t().center;
    match index {
        0 => text.preset_view_only,
        1 => text.preset_collaborate,
        _ => text.full_control,
    }
}

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
    ui.add_space(14.0);
    ui.horizontal(|ui| {
        ui.label(RichText::new(title).size(theme::SECTION).strong());
        if let Some(count) = count {
            ui.label(RichText::new(count.to_string()).color(MUTED));
        }
        ui.with_layout(egui::Layout::right_to_left(Align::Center), actions);
    });
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
}

/// A one-line message across the page.
pub(super) fn banner(ui: &mut egui::Ui, text: &str, tone: Tone) {
    let color = match tone {
        Tone::Success => GREEN,
        Tone::Warning => AMBER,
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
                Some(t().common.cancel),
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

/// Edit a capability set: presets, then the capabilities by group. Returns
/// whether anything changed.
pub(super) fn caps_editor(ui: &mut egui::Ui, id: impl egui::AsIdSalt, list: &mut Vec<String>) -> bool {
    let mut changed = false;
    ui.push_id(id, |ui| {
        let width = group_label_width(ui);
        ui.horizontal_wrapped(|ui| {
            group_label(ui, t().center.presets, width);
            for (index, preset) in PRESETS.into_iter().enumerate() {
                let selected = same_set(list, preset);
                if choice(ui, selected, preset_title(index)).clicked() && !selected {
                    *list = caps::normalize(preset);
                    changed = true;
                }
            }
        });
        ui.add_space(4.0);
        for (index, members) in GROUPS.into_iter().enumerate() {
            ui.horizontal_wrapped(|ui| {
                group_label(ui, group_title(index), width);
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
                RichText::new((t().center.view_required)(&cap_label(caps::VIEW)))
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

/// One width for the editor's row titles, so the rows line up.
fn group_label_width(ui: &egui::Ui) -> f32 {
    std::iter::once(t().center.presets)
        .chain((0..GROUPS.len()).map(group_title))
        .map(|text| {
            ui.painter()
                .layout_no_wrap(text.into(), FontId::proportional(theme::SMALL), MUTED)
                .size()
                .x
                + 8.0
        })
        .fold(56.0, f32::max)
}

fn group_label(ui: &mut egui::Ui, text: &str, width: f32) {
    let (rect, _) = ui.allocate_exact_size(vec2(width, 20.0), Sense::hover());
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
pub(super) fn locked(
    ui: &mut egui::Ui,
    title: &str,
    detail: Option<&str>,
    offline: bool,
    actions: &[&str],
) -> Option<usize> {
    let mut chosen = None;
    card().show(ui, |ui| {
        ui.set_width(ui.available_width());
        ui.add_space(12.0);
        ui.vertical_centered(|ui| {
            let (rect, _) = ui.allocate_exact_size(vec2(40.0, 40.0), Sense::hover());
            let color = if offline { AMBER } else { BLUE };
            ui.painter()
                .circle_filled(rect.center(), 20.0, color.gamma_multiply(0.14));
            if offline {
                paint_icon(
                    ui.painter(),
                    egui::Rect::from_center_size(rect.center(), vec2(22.0, 22.0)),
                    Icon::Refresh,
                    color,
                );
            } else {
                paint_lock(ui.painter(), rect.center(), 18.0, color);
            }
            ui.add_space(8.0);
            ui.label(RichText::new(title).size(theme::SECTION).strong());
            if let Some(detail) = detail {
                ui.label(RichText::new(detail).size(theme::SMALL).color(MUTED));
            }
            ui.set_max_width(520.0);
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
        return t().common.unknown.into();
    }
    chrono::DateTime::from_timestamp(seconds, 0)
        .map(|t| {
            t.with_timezone(&chrono::Local)
                .format("%Y-%m-%d %H:%M")
                .to_string()
        })
        .unwrap_or_default()
}

/// Capabilities as a row of icons in a chip, in display order; each names
/// itself on hover. `empty` stands for no capabilities.
pub(super) fn caps_icons(ui: &mut egui::Ui, list: &[String], empty: &str) {
    let granted: Vec<&str> = GROUPS
        .iter()
        .flat_map(|group| group.iter().copied())
        .filter(|cap| list.iter().any(|c| c == cap))
        .collect();
    if granted.is_empty() {
        chip(ui, empty, MUTED);
        return;
    }
    let icon = 14.0;
    let gap = 5.0;
    let pad = vec2(8.0, 3.0);
    let width = granted.len() as f32 * icon + (granted.len() - 1) as f32 * gap;
    let (rect, _) = ui.allocate_exact_size(vec2(width, icon) + pad * 2.0, Sense::hover());
    ui.painter()
        .rect_filled(rect, rect.height() / 2.0, BLUE.gamma_multiply(0.16));
    for (index, cap) in granted.into_iter().enumerate() {
        let cell = egui::Rect::from_min_size(
            rect.min + pad + vec2(index as f32 * (icon + gap), 0.0),
            vec2(icon, icon),
        );
        let response = ui
            .interact(cell, ui.id().with(("cap-icon", rect.min.x as i32, rect.min.y as i32, index)), Sense::hover())
            .on_hover_text(cap_label(cap));
        paint_cap(ui.painter(), cell, cap, if response.hovered() { TEXT } else { BLUE });
    }
}

/// The icon of a capability, drawn in a square.
fn paint_cap(p: &egui::Painter, rect: egui::Rect, cap: &str, color: Color32) {
    let c = rect.center();
    let s = rect.width() / 14.0;
    let q = |x: f32, y: f32| c + vec2(x, y) * s;
    let stroke = Stroke::new(1.3, color);
    let line = |points: &[(f32, f32)]| {
        p.add(egui::Shape::line(points.iter().map(|&(x, y)| q(x, y)).collect(), stroke));
    };
    let boxed = |x: f32, y: f32, w: f32, h: f32, r: f32| {
        p.rect_stroke(
            egui::Rect::from_center_size(q(x, y), vec2(w, h) * s),
            r * s,
            stroke,
            egui::StrokeKind::Middle,
        );
    };
    match cap {
        caps::VIEW => {
            let curve = |sign: f32| {
                (0..=12)
                    .map(|i| {
                        let x = -7.0 + 14.0 * i as f32 / 12.0;
                        (x, sign * 4.2 * (1.0 - (x / 7.0).powi(2)))
                    })
                    .collect::<Vec<_>>()
            };
            line(&curve(-1.0));
            line(&curve(1.0));
            p.circle_stroke(c, 2.2 * s, stroke);
        }
        caps::AUDIO => {
            line(&[(-6.5, -2.0), (-3.5, -2.0), (0.0, -5.5), (0.0, 5.5), (-3.5, 2.0), (-6.5, 2.0), (-6.5, -2.0)]);
            let arc = |r: f32| {
                (0..=8)
                    .map(|i| {
                        let a = -0.8 + 1.6 * i as f32 / 8.0;
                        (1.0 + r * a.cos(), r * a.sin())
                    })
                    .collect::<Vec<_>>()
            };
            line(&arc(3.0));
            line(&arc(5.8));
        }
        caps::ANNOTATION => crate::ui::controls::paint_annotation_icon(
            p,
            rect,
            crate::ui::controls::AnnotationIcon::Pen,
            color,
        ),
        caps::CONTROL => {
            boxed(0.0, 0.0, 9.0, 13.0, 4.5);
            line(&[(0.0, -6.5), (0.0, -2.5)]);
        }
        caps::CLIPBOARD => {
            boxed(0.0, 1.0, 10.0, 12.0, 1.5);
            boxed(0.0, -5.0, 5.0, 2.5, 1.0);
            line(&[(-2.5, 1.0), (2.5, 1.0)]);
            line(&[(-2.5, 3.5), (2.5, 3.5)]);
        }
        caps::MICROPHONE => {
            boxed(0.0, -2.5, 5.0, 8.5, 2.5);
            line(&[(-5.0, -1.5), (-5.0, 0.5), (-2.5, 3.5), (2.5, 3.5), (5.0, 0.5), (5.0, -1.5)]);
            line(&[(0.0, 3.5), (0.0, 6.5)]);
            line(&[(-3.0, 6.5), (3.0, 6.5)]);
        }
        caps::FILES => {
            line(&[(-7.0, -5.0), (-2.5, -5.0), (-0.5, -3.0), (7.0, -3.0), (7.0, 5.5), (-7.0, 5.5), (-7.0, -5.0)]);
            line(&[(-7.0, -1.0), (7.0, -1.0)]);
        }
        caps::DISPLAY => {
            boxed(0.0, -1.5, 14.0, 9.0, 1.5);
            line(&[(0.0, 3.0), (0.0, 6.0)]);
            line(&[(-3.5, 6.0), (3.5, 6.0)]);
        }
        caps::PORTS => crate::ui::controls::paint_port_mapping_icon(p, rect.expand(2.0 * s), color),
        caps::POWER => {
            let arc = (0..=16)
                .map(|i| {
                    // Open at the top, around the stroke.
                    let a = -std::f32::consts::FRAC_PI_2 + 0.6 + (std::f32::consts::TAU - 1.2) * i as f32 / 16.0;
                    (6.0 * a.cos(), 0.5 + 6.0 * a.sin())
                })
                .collect::<Vec<_>>();
            line(&arc);
            line(&[(0.0, -7.0), (0.0, -1.0)]);
        }
        caps::DIAGNOSTICS => {
            line(&[(-7.0, 0.5), (-3.5, 0.5), (-1.5, -5.0), (1.5, 5.5), (3.5, 0.5), (7.0, 0.5)]);
        }
        _ => {
            p.circle_stroke(c, 5.0 * s, stroke);
        }
    }
}
