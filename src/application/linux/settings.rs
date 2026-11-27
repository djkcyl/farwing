use super::DeviceCenter;
use crate::account::server::ServerConfig;
use crate::features::stream_control::StreamQuality;
use crate::i18n::{Choice, Language, t};
use crate::media::{FrameRateChoice, LocalDisplayInfo, TransportChoice, preferences};
use crate::transport::nat::settings as network;
use crate::ui::{branding, controls, log_settings, theme};
use egui::{Align, Layout, RichText, Sense, vec2};

#[derive(Clone, Copy, Default, Eq, PartialEq)]
enum Tab {
    #[default]
    Account,
    Language,
    Controller,
    Network,
    Shortcuts,
    Logs,
    About,
}

#[derive(Default)]
pub(super) struct SettingsUi {
    tab: Tab,
    logs: log_settings::LogSettings,
    shortcuts: super::viewer_shortcuts::Editor,
    change_server: bool,
    pub(super) error: Option<String>,
}

impl SettingsUi {
    pub(super) fn leave(&mut self) {
        self.shortcuts.cancel_recording();
        self.change_server = false;
    }
}

impl DeviceCenter {
    pub(super) fn settings_page(&mut self, ui: &mut egui::Ui) {
        let text = &t().settings;
        ui.horizontal_wrapped(|ui| {
            for (tab, label) in [
                (Tab::Account, text.tab_account),
                (Tab::Language, text.tab_language),
                (Tab::Controller, text.tab_controller),
                (Tab::Network, text.tab_network),
                (Tab::Shortcuts, text.tab_shortcuts),
                (Tab::Logs, text.tab_logs),
                (Tab::About, text.tab_about),
            ] {
                if ui.selectable_label(self.settings.tab == tab, label).clicked()
                    && self.settings.tab != tab
                {
                    self.settings.leave();
                    self.settings.tab = tab;
                    self.settings.error = None;
                }
            }
        });
        ui.separator();
        if let Some(error) = &self.settings.error {
            ui.colored_label(theme::RED, error);
        }
        // The logs editor owns a scrolling module list.
        if self.settings.tab == Tab::Logs {
            self.settings.logs.folder_button(ui);
            self.settings.logs.show(ui);
            return;
        }
        controls::page_scroll("linux-settings").show(ui, |ui| match self.settings.tab {
            Tab::Account => self.account_and_server(ui),
            Tab::Language => self.language_settings(ui),
            Tab::Controller => self.controller_settings(ui),
            Tab::Network => self.network_settings(ui),
            Tab::Shortcuts => self.settings.shortcuts.draw(ui),
            Tab::Logs => unreachable!(),
            Tab::About => self.about_settings(ui),
        });
    }

    fn account_and_server(&mut self, ui: &mut egui::Ui) {
        let words = t();
        let text = &words.settings;
        ui.label(RichText::new(text.section_account).strong());
        if let Some((client, _)) = &self.account {
            ui.label(client.account_name());
        } else if ui.add_enabled(!self.busy, controls::primary(words.common.sign_in)).clicked() {
            self.open_login();
        }
        ui.add_space(8.0);
        ui.label(RichText::new(text.section_server).strong());
        let config = match ServerConfig::load() {
            Ok(config) => config,
            Err(error) => {
                ui.colored_label(theme::RED, format!("{error:#}"));
                None
            }
        };
        ui.horizontal_wrapped(|ui| {
            ui.label(config.as_ref().map_or(text.not_set, ServerConfig::origin));
            if ui
                .add_enabled(
                    !self.busy && !ServerConfig::overridden(),
                    controls::secondary(if config.is_some() { text.change_server } else { text.set_server }),
                )
                .clicked()
            {
                if self.account.is_some() {
                    self.settings.change_server = true;
                } else {
                    self.sign_in.edit_server();
                    self.login_open = true;
                }
            }
        });
        if let Some(config) = config {
            let trust = match config.pinned_fingerprint() {
                Some(pin) => (text.certificate_pinned)(&pin),
                None if config.has_custom_ca() => text.certificate_custom_ca.into(),
                None => words.center.certificate_trusted_by_system.into(),
            };
            ui.weak(trust);
        }
        if self.settings.change_server {
            let dialog = egui::Modal::new(egui::Id::new("linux-change-server"))
                .frame(controls::dialog_frame())
                .show(ui.ctx(), |ui| {
                    controls::dialog_header(
                        ui,
                        words.center.change_server_title,
                        controls::DialogIcon::Warning,
                        false,
                    );
                    ui.label(words.center.change_server_body);
                    controls::dialog_actions(
                        ui,
                        Some(controls::DialogAction::new(words.center.change_server_action)),
                        Some(words.common.cancel),
                    )
                });
            let (change, cancel) = dialog.inner;
            if change && let Some((client, _)) = &self.account {
                self.sign_in.edit_server();
                self.login_open = true;
                self.signed_out(client.clone());
            }
            if change || cancel || dialog.should_close() {
                self.settings.change_server = false;
            }
        }
    }

    fn language_settings(&mut self, ui: &mut egui::Ui) {
        let text = &t().settings;
        let label = |choice: Choice| match choice {
            Choice::System => text.language_system,
            Choice::ZhCn => Language::ZhCn.name(),
            Choice::En => Language::En.name(),
        };
        let current = crate::i18n::choice();
        let mut choice = current;
        ui.horizontal(|ui| {
            ui.label(text.interface_language);
            egui::ComboBox::from_id_salt("language")
                .selected_text(label(choice))
                .show_ui(ui, |ui| {
                    for value in [Choice::System, Choice::ZhCn, Choice::En] {
                        ui.selectable_value(&mut choice, value, label(value));
                    }
                });
        });
        if choice != current {
            self.settings.error = crate::i18n::choose(choice)
                .err()
                .map(|error| (text.language_failed)(&format!("{error:#}")));
            ui.ctx().request_repaint();
        }
    }

    fn controller_settings(&mut self, ui: &mut egui::Ui) {
        let text = t();
        let mut next = self.media;
        let display = LocalDisplayInfo::FALLBACK;
        egui::Grid::new("linux-controller-preferences").num_columns(2).show(ui, |ui| {
            ui.label(text.viewer.quality_and_stream_settings);
            egui::ComboBox::from_id_salt("quality")
                .selected_text(next.quality.label())
                .show_ui(ui, |ui| {
                    for quality in [StreamQuality::Auto, StreamQuality::Original, StreamQuality::High, StreamQuality::Clear, StreamQuality::Fast] {
                        ui.selectable_value(&mut next.quality, quality, quality.label());
                    }
                });
            ui.end_row();
            ui.label(text.settings.frame_rate);
            egui::ComboBox::from_id_salt("fps")
                .selected_text(next.frame_rate.label(display))
                .show_ui(ui, |ui| {
                    for fps in FrameRateChoice::available(display) {
                        ui.selectable_value(&mut next.frame_rate, fps, fps.label(display));
                    }
                });
            ui.end_row();
            ui.label(text.settings.codec);
            egui::ComboBox::from_id_salt("codec")
                .selected_text(next.codec.label())
                .show_ui(ui, |ui| {
                    for codec in crate::media::decoder::codec_preferences() {
                        ui.selectable_value(&mut next.codec, codec, codec.label());
                    }
                });
            ui.end_row();
            ui.label(text.settings.decoding_mode);
            ui.label(text.settings.software_decoding_only);
            ui.end_row();
            ui.label(text.settings.route);
            egui::ComboBox::from_id_salt("route")
                .selected_text(next.transport.label())
                .show_ui(ui, |ui| {
                    for route in [TransportChoice::Auto, TransportChoice::P2p, TransportChoice::Relay] {
                        ui.selectable_value(&mut next.transport, route, route.label());
                    }
                });
            ui.end_row();
        });
        if self.media != next {
            self.settings.error = match preferences::save(next) {
                Ok(()) => {
                    self.media = next;
                    None
                }
                Err(error) => Some((text.settings.media_save_failed)(&format!("{error:#}"))),
            };
        }
    }

    fn network_settings(&mut self, ui: &mut egui::Ui) {
        let text = &t().settings;
        let current = network::current();
        let mut next = current;
        ui.label(RichText::new(text.section_traversal).strong());
        toggle(ui, text.aggressive_punching, &mut next.aggressive_punching);
        toggle(ui, text.allow_port_mapping, &mut next.port_mapping);
        ui.separator();
        ui.label(RichText::new(text.section_encryption).strong());
        toggle(ui, text.e2ee_only, &mut next.e2ee_only);
        if next != current {
            self.settings.error = network::save(next).err().map(|error| format!("{error:#}"));
        }
    }

    fn about_settings(&mut self, ui: &mut egui::Ui) {
        let text = &t().settings;
        ui.horizontal(|ui| {
            let (rect, _) = ui.allocate_exact_size(vec2(64.0, 64.0), Sense::hover());
            branding::paint(ui.painter(), rect, branding::Art::App, egui::Color32::WHITE);
            ui.vertical(|ui| {
                ui.heading(crate::APP_NAME);
                ui.weak(format!("v{} · Linux {}", env!("CARGO_PKG_VERSION"), std::env::consts::ARCH));
            });
        });
        ui.separator();
        self.updates.show_about(ui);
        ui.separator();
        ui.label(RichText::new(text.copyright_section).strong());
        ui.weak(text.ffmpeg_notice);
        for (label, contents) in [
            (text.license_notice, include_str!("../../../LICENSE")),
            (text.third_party_notices, include_str!("../../../THIRD_PARTY_NOTICES")),
        ] {
            ui.collapsing(label, |ui| {
                ui.add(egui::Label::new(RichText::new(contents).monospace()).wrap().selectable(true));
            });
        }
    }
}

fn toggle(ui: &mut egui::Ui, label: &str, value: &mut bool) {
    ui.horizontal(|ui| {
        ui.label(label);
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            controls::switch(ui, value);
        });
    });
}
