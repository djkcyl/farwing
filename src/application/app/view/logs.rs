use super::*;
use crate::ui::log_settings::LogSettings;

#[derive(Default)]
pub(super) struct LogUi {
    pub(super) export: super::diagnostic_export::ExportUi,
    settings: LogSettings,
}

impl DeviceCenterApp {
    pub(super) fn log_folder_button(&mut self, ui: &mut egui::Ui) {
        self.center_ui.logs.settings.folder_button(ui);
    }

    pub(super) fn logs_page(&mut self, ui: &mut egui::Ui) {
        self.diagnostic_export_status(ui);
        self.center_ui.logs.settings.show(ui);
    }
}
