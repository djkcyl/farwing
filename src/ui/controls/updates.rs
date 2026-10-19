use super::*;


pub(crate) fn update_actions(
    ui: &mut egui::Ui,
    primary_label: Option<&str>,
    secondary_label: Option<(&str, &str)>,
) -> (bool, bool) {
    super::dialogs::dialog_actions_with_hint(
        ui,
        primary_label.map(DialogAction::new),
        secondary_label,
    )
}

