//! Compact, aligned diagnostics; asynchronous values never determine row height.
use super::{LINE, MUTED, TEXT, theme};
use egui::{Align, RichText, Stroke, vec2};

pub(crate) fn diagnostics_row(ui: &mut egui::Ui, label: &str, value: &str) -> egui::Response {
    let (rect, _) = ui.allocate_exact_size(
        vec2(ui.available_width(), theme::DIAGNOSTICS_ROW_HEIGHT),
        egui::Sense::hover(),
    );
    let mut row = ui.new_child(
        egui::UiBuilder::new()
            .max_rect(rect)
            .layout(egui::Layout::left_to_right(Align::Center)),
    );
    diagnostics_label(&mut row, label);
    row.add(egui::Label::new(RichText::new(value).size(theme::COMPACT_TEXT).color(TEXT)).truncate())
        .on_hover_text(value)
}

pub(crate) fn diagnostics_label(ui: &mut egui::Ui, label: &str) -> egui::Response {
    let (rect, _) = ui.allocate_exact_size(
        vec2(
            theme::DIAGNOSTICS_LABEL_WIDTH,
            theme::DIAGNOSTICS_ROW_HEIGHT,
        ),
        egui::Sense::hover(),
    );
    let mut column = ui.new_child(
        egui::UiBuilder::new()
            .max_rect(rect)
            .layout(egui::Layout::left_to_right(Align::Center)),
    );
    column.add(
        egui::Label::new(RichText::new(label).size(theme::COMPACT_TEXT).color(MUTED)).truncate(),
    )
}

pub(crate) fn diagnostics_action(ui: &mut egui::Ui, enabled: bool, label: &str) -> egui::Response {
    ui.add_enabled_ui(enabled, |ui| {
        ui.add_sized(
            vec2(theme::DIAGNOSTICS_ACTION_WIDTH, theme::CONTROL_HEIGHT),
            super::secondary(label),
        )
    })
    .inner
}

pub(crate) fn diagnostics_empty(ui: &mut egui::Ui, message: &str) {
    ui.allocate_ui_with_layout(
        vec2(ui.available_width(), theme::DIAGNOSTICS_BODY_MIN_HEIGHT),
        egui::Layout::centered_and_justified(egui::Direction::TopDown),
        |ui| {
            ui.label(RichText::new(message).size(theme::BODY).color(MUTED));
        },
    );
}

pub(crate) fn diagnostics_table(
    ui: &mut egui::Ui,
    headers: &[&str],
    rows: &[Vec<String>],
    color: impl Fn(usize, usize) -> egui::Color32,
) {
    let width = ui.available_width();
    for (index, cells) in
        std::iter::once(headers.iter().map(|s| (*s).to_owned()).collect::<Vec<_>>())
            .chain(rows.iter().cloned())
            .enumerate()
    {
        let (rect, _) = ui.allocate_exact_size(
            vec2(width, theme::DIAGNOSTICS_ROW_HEIGHT),
            egui::Sense::hover(),
        );
        if index == 0 {
            ui.painter().line_segment(
                [rect.left_bottom(), rect.right_bottom()],
                Stroke::new(1.0, LINE),
            );
        }
        for (column, text) in cells.iter().enumerate() {
            let column_width = 0.76 / (headers.len() - 1) as f32;
            let start = if column == 0 {
                0.0
            } else {
                0.24 + (column - 1) as f32 * column_width
            };
            let end = if column == 0 {
                0.24
            } else {
                0.24 + column as f32 * column_width
            };
            let left = rect.left() + width * start;
            let right = rect.left() + width * end;
            let mut cell = ui.new_child(
                egui::UiBuilder::new()
                    .max_rect(egui::Rect::from_min_max(
                        egui::pos2(left, rect.top()),
                        egui::pos2(right, rect.bottom()),
                    ))
                    .layout(egui::Layout::left_to_right(Align::Center)),
            );
            cell.add(
                egui::Label::new(
                    RichText::new(text.lines().next().unwrap_or_default())
                        .size(theme::COMPACT_TEXT)
                        .color(if index == 0 {
                            MUTED
                        } else {
                            color(index - 1, column)
                        }),
                )
                .truncate(),
            )
            .on_hover_text(text);
        }
    }
}
