use super::*;

pub(super) fn glyph(ui: &mut egui::Ui, icon: Icon, label: &str, color: Color32) -> egui::Response {
    let (_, response) = crate::ui::controls::icon_button_area(ui, theme::CONTROL_HEIGHT);
    response.widget_info(|| {
        egui::WidgetInfo::labeled(egui::WidgetType::Button, ui.is_enabled(), label)
    });
    paint_icon(
        ui.painter(),
        response.rect,
        icon,
        if !ui.is_enabled() {
            theme::DISABLED
        } else if color == MUTED && (response.hovered() || response.has_focus()) {
            TEXT
        } else {
            color
        },
    );
    response
}

pub(super) fn grouped_id(id: &str) -> String {
    if id.len() == 9 && id.bytes().all(|b| b.is_ascii_digit()) {
        format!("{} {} {}", &id[..3], &id[3..6], &id[6..])
    } else {
        id.to_owned()
    }
}

pub(super) fn copy_value(
    ui: &mut egui::Ui,
    salt: &str,
    size: egui::Vec2,
    display: RichText,
    value: Option<&str>,
    feedback: &str,
) {
    let copied_id = ui.id().with(("assist-copy", salt));
    // Keep only a short-lived value fingerprint, never the credential text.
    let fingerprint = egui::Id::new(value);
    let copied = ui
        .ctx()
        .data(|data| data.get_temp::<(Instant, egui::Id)>(copied_id));
    let copied =
        copied.filter(|(at, which)| *which == fingerprint && at.elapsed() < Duration::from_secs(2));
    if copied.is_none() {
        ui.ctx()
            .data_mut(|data| data.remove::<(Instant, egui::Id)>(copied_id));
    }
    let label = if copied.is_some() {
        RichText::new(feedback).size(theme::BODY).color(GREEN)
    } else {
        display
    };
    let response = ui
        .add_enabled_ui(value.is_some(), |ui| {
            crate::ui::controls::value_button(ui, size, label)
        })
        .inner;
    response.widget_info(|| {
        egui::WidgetInfo::labeled(
            egui::WidgetType::Button,
            ui.is_enabled() && value.is_some(),
            feedback.trim_start_matches("已"),
        )
    });
    if response.clicked()
        && let Some(value) = value
    {
        ui.ctx().copy_text(value.to_owned());
        ui.ctx()
            .data_mut(|data| data.insert_temp(copied_id, (Instant::now(), fingerprint)));
        ui.ctx().request_repaint();
    }
    if let Some((at, _)) = copied {
        ui.ctx()
            .request_repaint_after(Duration::from_secs(2).saturating_sub(at.elapsed()));
    }
}
