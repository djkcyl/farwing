use super::access::{Blocker, server_button};
use super::*;
use crate::application::app::updates::State;
use crate::i18n::t;

#[derive(Clone, Copy)]
pub(super) enum LegalDocument {
    License,
    ThirdParty,
}

impl DeviceCenterApp {
    pub(super) fn about_page(&mut self, ui: &mut egui::Ui) {
        if self.center_ui.legal_document.is_some() {
            self.legal_document(ui);
            return;
        }
        ui.add_space(8.0);
        let text = &t().settings;
        crate::ui::controls::page_scroll("about-page").show(ui, |ui| {
            ui.horizontal(|ui| {
                let (icon, _) = ui.allocate_exact_size(vec2(88.0, 88.0), Sense::hover());
                crate::ui::branding::paint(
                    ui.painter(),
                    icon,
                    crate::ui::branding::Art::App,
                    Color32::WHITE,
                );
                ui.add_space(16.0);
                ui.vertical(|ui| {
                    ui.add_space(5.0);
                    ui.label(
                        RichText::new(crate::APP_NAME)
                            .size(crate::ui::theme::TITLE)
                            .strong(),
                    );
                    ui.label(
                        RichText::new(format!(
                            "v{}  ·  {} {}",
                            env!("CARGO_PKG_VERSION"),
                            "Windows",
                            std::env::consts::ARCH
                        ))
                        .size(crate::ui::theme::SMALL)
                        .color(MUTED),
                    );
                });
            });

            ui.add_space(28.0);
            ui.separator();
            ui.add_space(20.0);
            self.about_updates(ui);
            ui.add_space(20.0);
            ui.separator();
            ui.add_space(20.0);

            ui.label(
                RichText::new(text.copyright_section)
                    .size(crate::ui::theme::SECTION)
                    .strong(),
            );
            ui.add_space(6.0);
            ui.label(
                RichText::new(text.ffmpeg_notice)
                    .size(crate::ui::theme::COMPACT_TEXT)
                    .color(MUTED),
            );
            ui.horizontal_wrapped(|ui| {
                if ui.link(text.license_notice).clicked() {
                    self.center_ui.legal_document = Some(LegalDocument::License);
                }
                ui.add_space(16.0);
                if ui.link(text.third_party_notices).clicked() {
                    self.center_ui.legal_document = Some(LegalDocument::ThirdParty);
                }
            });
        });
    }

    fn legal_document(&mut self, ui: &mut egui::Ui) {
        let Some(document) = self.center_ui.legal_document else {
            return;
        };
        let (title, text) = match document {
            LegalDocument::License => (
                "Copyright and License Notice",
                concat!(
                    "Farwing is licensed under the GNU Affero General Public License v3.0 only.\n\n",
                    include_str!("../../../../LICENSE"),
                ),
            ),
            LegalDocument::ThirdParty => (
                "Third-Party Notices and Licenses",
                concat!(
                    include_str!("../../../../THIRD_PARTY_NOTICES"),
                    "\n\nAppendix: SpeexDSP BSD license (Rust resampler adaptation)\n\n",
                    include_str!("../../../media/audio/COPYING.SpeexDSP"),
                    "\n\nAppendix: GNU LGPL 2.1 (covered components are licensed LGPL-2.1-or-later)\n\n",
                    include_str!("../../../media/decoder/COPYING.FFmpeg"),
                    "\n\nAppendix: Farwing Display attribution and Microsoft Public License\n\n",
                    include_str!("../../../../drivers/display/UPSTREAM.md"),
                    "\n\n",
                    include_str!("../../../../drivers/display/LICENSE"),
                    "\n\nAppendix: Farwing Audio attribution\n\n",
                    include_str!("../../../../drivers/audio/UPSTREAM.md"),
                    "\n\nAppendix: OxideAV MIT license (retained AVC/HEVC syntax primitives)\n\n",
                    include_str!("../../../../crates/codec/kernels/h264/COPYING.OxideAV"),
                    "\n\nAppendix: AV1 syntax (ChromiumOS adaptation, BSD-3-Clause)\n\n",
                    include_str!(
                        "../../../../crates/codec/kernels/av1/licenses/COPYING.ChromiumOS"
                    ),
                    "\n\nAppendix: AV1 syntax validation (dav1d adaptation)\n\n",
                    include_str!("../../../../crates/codec/kernels/av1/licenses/COPYING.dav1d"),
                    "\n\n",
                    include_str!("../../../../crates/codec/kernels/av1/licenses/COPYING.rav1d"),
                    "\n\nAppendix: AV1 Rust decoder API (MIT)\n\n",
                    include_str!("../../../../crates/codec/kernels/av1/licenses/COPYING.rust-api"),
                    "\n\nAppendix: AV1 decoder SIMD contributions\n\n",
                    include_str!("../../../../crates/codec/kernels/av1/licenses/COPYING.rav1e"),
                    "\n\n",
                    include_str!("../../../../crates/codec/kernels/av1/licenses/PATENTS.rav1e"),
                    "\n\n--- Bundled fonts ---\n\n",
                    include_str!("../../../../assets/fonts/LICENSE")
                ),
            ),
        };
        let back = crate::ui::controls::back_button(ui, t().settings.back_to_about).clicked();
        ui.add_space(12.0);
        ui.label(RichText::new(title).size(theme::DIALOG_TITLE).strong());
        if back || ui.input_mut(|input| input.consume_key(egui::Modifiers::NONE, egui::Key::Escape))
        {
            self.center_ui.legal_document = None;
            ui.ctx().request_repaint();
            return;
        }
        ui.add_space(18.0);
        ui.separator();
        ui.add_space(18.0);
        crate::ui::controls::page_scroll(("legal-page", title)).show(ui, |ui| {
            ui.add(
                egui::Label::new(RichText::new(text).monospace().size(crate::ui::theme::BODY))
                    .wrap()
                    .selectable(true),
            );
        });
    }

    fn about_updates(&mut self, ui: &mut egui::Ui) {
        let text = &t().settings;
        let away = self.blocker() == Some(Blocker::Unreachable);
        crate::ui::controls::observe_notice(
            ui.ctx(),
            "client-update-error",
            text.update_check_failed,
            crate::ui::controls::DialogIcon::Error,
            match &self.updates.state {
                State::Failed(error) if !away => Some(error.as_str()),
                _ => None,
            },
        );
        let beta = self.updates.allow_prerelease();
        let (message, color, destination) = match &self.updates.state {
            State::Checking => ((text.update_checking)(beta), MUTED, None),
            State::Current => ((text.update_current)(beta), GREEN, None),
            State::Ahead => ((text.update_ahead)(beta), MUTED, None),
            State::NoRelease => ((text.update_none)(beta), MUTED, None),
            State::Unreachable => (Blocker::Unreachable.reason().into(), MUTED, None),
            State::Failed(_) => (text.update_check_failed.into(), MUTED, None),
            State::Available {
                version,
                url,
                prerelease,
                ..
            } => (
                if *prerelease {
                    (text.update_found_beta)(version)
                } else {
                    (text.update_found_release)(version)
                },
                BLUE,
                Some(url.clone()),
            ),
        };
        let checking = matches!(self.updates.state, State::Checking);
        let wait = self.updates.retry_wait();
        let enabled = !checking && (destination.is_some() || wait == 0);
        let button = if destination.is_some() {
            text.view_new_version
        } else if checking {
            text.checking
        } else {
            text.check_for_updates
        };
        let mut clicked = false;
        let (row, _) = ui.allocate_exact_size(vec2(ui.available_width(), 56.0), Sense::hover());
        let mut row_ui = ui.new_child(egui::UiBuilder::new().max_rect(row));
        row_ui.with_layout(egui::Layout::right_to_left(Align::Center), |ui| {
            let blocker = (away && destination.is_none()).then_some(Blocker::Unreachable);
            let response = server_button(ui, blocker, enabled, primary(button));
            clicked = response.clicked();
            if !enabled && !checking && blocker.is_none() {
                response.on_disabled_hover_text((text.check_again_in)(wait));
            }
            ui.with_layout(egui::Layout::top_down(Align::Min), |ui| {
                ui.label(RichText::new(text.software_update).size(theme::SECTION).strong());
                ui.add(
                    egui::Label::new(
                        RichText::new(&message)
                            .size(theme::COMPACT_TEXT)
                            .color(color),
                    )
                    .truncate(),
                )
                .on_hover_text(&message);
            });
        });
        if clicked {
            if destination.is_some() {
                self.updates.dialog_open = true;
            } else {
                self.updates.request(ui.ctx());
            }
        }
        let mut allow = self.updates.allow_prerelease();
        form_row_content(ui, text.allow_beta, "", |ui| {
            if crate::ui::controls::switch(ui, &mut allow).changed() {
                let _ = self.updates.set_allow_prerelease(allow, ui.ctx());
            }
        });
        if let Some(error) = &self.updates.settings_error {
            ui.colored_label(AMBER, error);
        }
    }
}
