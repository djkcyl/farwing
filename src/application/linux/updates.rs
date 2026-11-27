use super::{State, UpdateCheck};
use crate::i18n::t;
use crate::ui::{controls, theme};
use egui::RichText;
use std::time::Duration;

impl UpdateCheck {
    pub(in crate::application) fn show_about(&mut self, ui: &mut egui::Ui) {
        // This also catches server settings saved by guest setup or by a server move.
        self.recheck_source(ui.ctx());
        let text = &t().settings;
        ui.label(
            RichText::new(text.software_update)
                .size(theme::SECTION)
                .strong(),
        );

        let checking = matches!(self.state, State::Checking);
        let wait = self.retry_wait();
        let label = if checking {
            text.checking
        } else {
            text.check_for_updates
        };
        let check = ui.add_enabled(!checking && wait == 0, controls::secondary(label));
        if check.clicked() {
            self.request(ui.ctx());
        } else if !checking && wait != 0 {
            check.on_disabled_hover_text((text.check_again_in)(wait));
            ui.ctx().request_repaint_after(Duration::from_secs(1));
        }

        let mut beta = self.allow_prerelease();
        ui.horizontal(|ui| {
            ui.label(text.allow_beta);
            if controls::switch(ui, &mut beta).changed() {
                let _ = self.set_allow_prerelease(beta, ui.ctx());
            }
        });
        if let Some(error) = &self.settings_error {
            ui.colored_label(theme::AMBER, error);
        }

        let beta = self.allow_prerelease();
        let (status, color) = match &self.state {
            State::Checking => ((text.update_checking)(beta), theme::MUTED),
            State::Current => ((text.update_current)(beta), theme::GREEN),
            State::Ahead => ((text.update_ahead)(beta), theme::MUTED),
            State::NoRelease => ((text.update_none)(beta), theme::MUTED),
            State::Unreachable => (t().center.status_unreachable.into(), theme::MUTED),
            State::Failed(error) => ((t().center.update_check_failed)(error), theme::AMBER),
            State::Available {
                version,
                prerelease,
                ..
            } => (
                if *prerelease {
                    (text.update_found_beta)(version)
                } else {
                    (text.update_found_release)(version)
                },
                theme::ACCENT,
            ),
        };
        ui.colored_label(color, status);

        if let State::Available {
            url,
            notes,
            published,
            ..
        } = &self.state
        {
            if let Some(date) = published {
                ui.colored_label(theme::MUTED, (text.released_on)(date));
            }
            if !url.is_empty() {
                ui.hyperlink_to(text.download, url.as_str());
            }
            ui.label(RichText::new(text.release_notes).strong());
            controls::release_notes(ui, notes);
        }
    }
}
