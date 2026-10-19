use super::theme;
use egui::{Align, FontId, Response, Ui, Vec2, vec2};

fn margin(height: f32) -> egui::Margin {
    if height >= theme::CONTROL_HEIGHT {
        theme::INPUT_MARGIN
    } else {
        theme::INPUT_COMPACT_MARGIN
    }
}

/// Standard form fields and compact toolbar fields use the same colors and
/// typography; the reserved atom also keeps an empty TextEdit at its full height.
pub(crate) fn singleline(value: &mut String, height: f32) -> egui::TextEdit<'_> {
    let margin = margin(height);
    egui::TextEdit::singleline(value)
        .font(FontId::proportional(if height >= theme::CONTROL_HEIGHT {
            theme::BODY
        } else {
            theme::COMPACT_TEXT
        }))
        .background_color(theme::SIDEBAR)
        .text_color(theme::TEXT)
        .prefix(egui::Atom {
            size: Some(vec2(
                0.,
                (height - margin.top as f32 - margin.bottom as f32).max(0.),
            )),
            ..Default::default()
        })
        .vertical_align(Align::Center)
        .margin(margin)
}

/// Preserve DragValue's parsing, limits and dragging while keeping its button
/// and text-edit phases the same size and appearance as other input fields.
pub(crate) fn number_input(ui: &mut Ui, size: Vec2, value: egui::DragValue<'_>) -> Response {
    ui.allocate_ui_with_layout(
        size,
        egui::Layout::centered_and_justified(egui::Direction::LeftToRight),
        |ui| {
            super::configure(ui.style_mut(), size.y);
            let margin = margin(size.y);
            let font = egui::TextStyle::Body.resolve(ui.style());
            let text_height = ui.fonts_mut(|fonts| fonts.row_height(&font));
            ui.spacing_mut().button_padding =
                vec2(margin.left as f32, ((size.y - text_height) / 2.).max(0.));
            ui.spacing_mut().interact_size = size;
            ui.style_mut().drag_value_text_style = egui::TextStyle::Body;
            let inactive = &mut ui.style_mut().visuals.widgets.inactive;
            inactive.bg_fill = theme::SIDEBAR;
            inactive.weak_bg_fill = theme::SIDEBAR;
            ui.add_sized(size, value)
        },
    )
    .inner
}
