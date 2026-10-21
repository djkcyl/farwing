//! Settings: one page, categories on its left.
use super::*;
use crate::i18n::t;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub(super) enum SettingsTab {
    #[default]
    Account,
    Language,
    Host,
    Controller,
    Network,
    Shortcuts,
    Plugins,
    Service,
    Diagnostics,
    Logs,
    About,
}

impl SettingsTab {
    /// The navigation groups, titled in the current language.
    fn groups() -> [(&'static str, &'static [Self]); 4] {
        let text = &t().settings;
        [
            (text.group_general, &[Self::Account, Self::Language]),
            (
                text.group_remote,
                &[Self::Host, Self::Controller, Self::Network],
            ),
            (text.group_extensions, &[Self::Shortcuts, Self::Plugins]),
            (
                text.group_system,
                &[Self::Service, Self::Diagnostics, Self::Logs, Self::About],
            ),
        ]
    }

    fn label(self) -> &'static str {
        let text = &t().settings;
        match self {
            Self::Account => text.tab_account,
            Self::Language => text.tab_language,
            Self::Host => text.tab_host,
            Self::Controller => text.tab_controller,
            Self::Network => text.tab_network,
            Self::Shortcuts => text.tab_shortcuts,
            Self::Plugins => text.tab_plugins,
            Self::Service => text.tab_service,
            Self::Diagnostics => text.tab_diagnostics,
            Self::Logs => text.tab_logs,
            Self::About => text.tab_about,
        }
    }

    fn icon(self) -> Icon {
        match self {
            Self::Account => Icon::Friends,
            Self::Language => Icon::Settings,
            Self::Host => Icon::Monitor,
            Self::Controller => Icon::Assist,
            Self::Network => Icon::Refresh,
            Self::Shortcuts => Icon::Keyboard,
            Self::Plugins => Icon::Plugins,
            Self::Service => Icon::Settings,
            Self::Diagnostics => Icon::Diagnostics,
            Self::Logs => Icon::Logs,
            Self::About => Icon::Info,
        }
    }
}

impl DeviceCenterApp {
    pub(super) fn settings_page(&mut self, ui: &mut egui::Ui) {
        let full = ui.available_rect_before_wrap();
        let nav = egui::Rect::from_min_size(full.min, vec2(200.0, full.height()));
        let content = egui::Rect::from_min_max(egui::pos2(nav.right() + 29.0, full.top()), full.max);
        ui.painter()
            .vline(nav.right() + 14.0, full.y_range(), Stroke::new(1.0, LINE));
        let current = self.center_ui.settings_tab;
        let mut chosen = None;
        ui.scope_builder(
            egui::UiBuilder::new()
                .id_salt("settings-nav")
                .max_rect(nav)
                .layout(egui::Layout::top_down(Align::Min)),
            |ui| {
                ui.label(RichText::new(t().common.settings).size(theme::TITLE).strong());
                ui.add_space(14.0);
                ui.spacing_mut().item_spacing.y = 4.0;
                for (group, tabs) in SettingsTab::groups() {
                    ui.label(RichText::new(group).small().color(MUTED));
                    for tab in tabs {
                        if nav_item(ui, tab.icon(), tab.label(), None, false, current == *tab) {
                            chosen = Some(*tab);
                        }
                    }
                    ui.add_space(10.0);
                }
            },
        );
        if let Some(tab) = chosen
            && tab != current
        {
            self.center_ui.settings_tab = tab;
            if tab == SettingsTab::About {
                self.center_ui.legal_document = None;
            }
        }
        let tab = self.center_ui.settings_tab;
        ui.scope_builder(
            egui::UiBuilder::new()
                .id_salt(("settings-content", tab))
                .max_rect(content)
                .layout(egui::Layout::top_down(Align::Min)),
            |ui| {
                ui.horizontal(|ui| {
                    ui.set_min_height(theme::CONTROL_HEIGHT);
                    ui.label(RichText::new(tab.label()).size(theme::TITLE).strong());
                    ui.with_layout(egui::Layout::right_to_left(Align::Center), |ui| match tab {
                        SettingsTab::Plugins => self.center_ui.plugins.header_actions(ui),
                        SettingsTab::Diagnostics => self.diagnostic_export_button(ui),
                        SettingsTab::Logs => {
                            self.log_folder_button(ui);
                            self.diagnostic_export_button(ui);
                        }
                        _ => {}
                    });
                });
                ui.add_space(14.0);
                self.alert(ui);
                match tab {
                    SettingsTab::Account => {
                        crate::ui::controls::page_scroll("settings-account")
                            .show(ui, |ui| self.account_settings(ui));
                    }
                    SettingsTab::Language => self.language_settings(ui),
                    SettingsTab::Host => {
                        crate::ui::controls::page_scroll("settings-host").show(ui, |ui| {
                            self.host_settings(ui);
                            self.notification_settings(ui);
                        });
                    }
                    SettingsTab::Controller => self.controller_settings(ui),
                    SettingsTab::Network => {
                        crate::ui::controls::page_scroll("settings-network")
                            .show(ui, |ui| self.network_settings(ui));
                    }
                    SettingsTab::Shortcuts => {
                        crate::ui::controls::page_scroll("settings-shortcuts")
                            .show(ui, |ui| self.center_ui.shortcuts.draw(ui));
                    }
                    SettingsTab::Plugins => self.center_ui.plugins.show(ui),
                    SettingsTab::Service => {
                        let active = self
                            .host
                            .as_ref()
                            .is_some_and(|h| h.status().session_active);
                        crate::ui::controls::page_scroll("settings-service")
                            .show(ui, |ui| self.center_ui.components.management(ui, active));
                    }
                    SettingsTab::Diagnostics => self.diagnostics_page(ui),
                    SettingsTab::Logs => self.logs_page(ui),
                    SettingsTab::About => self.about_page(ui),
                }
            },
        );
        ui.allocate_rect(full, Sense::hover());
    }

    /// The account, its server and this device.
    fn account_settings(&mut self, ui: &mut egui::Ui) {
        if self.access() != Access::Account {
            self.signed_out_settings(ui);
            return;
        }
        let text = &t().settings;
        section(ui, text.section_account);
        let account = if self.account_name.trim().is_empty() {
            text.signed_in.to_owned()
        } else {
            self.account_name.clone()
        };
        let mut logout = false;
        form_row(ui, &account, "", |ui| {
            logout = ui
                .add_enabled(!self.logout_pending, crate::ui::controls::secondary(t().common.sign_out))
                .clicked();
        });
        if logout {
            self.logout();
        }

        section(ui, text.section_server);
        let server = crate::account::server::ServerConfig::load().ok().flatten();
        let state = self.server_state().label();
        let mut change = false;
        form_row(
            ui,
            server.as_ref().map_or(text.not_set, |server| server.origin()),
            state,
            |ui| {
                change = ui
                    .add_enabled(!self.logout_pending, crate::ui::controls::secondary(text.change_server))
                    .clicked();
            },
        );
        if change && self.can_logout() {
            self.center_ui.change_server = true;
        }
        let certificate = match &server {
            Some(server) => match server.pinned_fingerprint() {
                Some(pin) => (text.certificate_pinned)(&pin),
                None if server.has_custom_ca() => text.certificate_custom_ca.to_owned(),
                None => text.certificate_windows.to_owned(),
            },
            None => "—".to_owned(),
        };
        form_row(ui, text.certificate, &certificate, |_| {});

        section(ui, text.section_this_device);
        let current = self.devices.as_ref().map(|list| list.current_device.clone());
        match current {
            Some(device) => {
                let mut details = false;
                form_row(
                    ui,
                    display_alias(&device),
                    &format!(
                        "ID {}",
                        crate::account::api::device_id_text(&device.device_id)
                    ),
                    |ui| {
                        details = ui.add(crate::ui::controls::secondary(text.device_details)).clicked();
                    },
                );
                if details {
                    self.open_details(device.device_id.clone());
                }
            }
            None => form_row(ui, text.reading_this_device, "", |_| {}),
        }
    }

    /// The account category without an account: how to sign in, the
    /// server, and this device.
    fn signed_out_settings(&mut self, ui: &mut egui::Ui) {
        use super::super::login_form::LoginStep;
        let text = &t().settings;
        section(ui, text.section_account);
        let label = self.sign_in_label();
        let mut sign_in = false;
        form_row(ui, t().common.signed_out, "", |ui| {
            sign_in = ui.add(primary(label)).clicked();
        });
        if sign_in {
            self.sign_in();
        }

        section(ui, text.section_server);
        let server = self.saved_server();
        let mut change = false;
        form_row(
            ui,
            server.as_deref().unwrap_or(text.not_set),
            "",
            |ui| {
                change = ui
                    .add(crate::ui::controls::secondary(if server.is_some() {
                        text.change_server
                    } else {
                        text.set_server
                    }))
                    .clicked();
            },
        );
        if change {
            self.open_sign_in(LoginStep::Server);
        }

        section(ui, text.section_this_device);
        match self.local_identity() {
            Some((name, id)) => form_row(ui, &name, &format!("ID {id}"), |_| {}),
            None => form_row(ui, text.identity_unavailable, "", |_| {}),
        }
    }

    /// How this device connects to others.
    fn controller_settings(&mut self, ui: &mut egui::Ui) {
        let original_media = self.media;
        let text = &t().settings;
        crate::ui::controls::page_scroll("settings-controller").show(ui, |ui| {
            form_row(
                ui,
                text.cursor_color,
                "",
                |ui| {
                    let swatch = |ui: &mut egui::Ui, color: u32, selected: bool| {
                        let (rect, response) =
                            ui.allocate_exact_size(egui::vec2(20.0, 20.0), egui::Sense::click());
                        let fill = egui::Color32::from_rgb(
                            (color >> 16) as u8,
                            (color >> 8) as u8,
                            color as u8,
                        );
                        ui.painter().circle_filled(rect.center(), 8.0, fill);
                        if selected {
                            ui.painter().circle_stroke(
                                rect.center(),
                                10.0,
                                egui::Stroke::new(2.0, crate::ui::theme::TEXT),
                            );
                        }
                        response.clicked()
                    };
                    ui.horizontal(|ui| {
                        if ui
                            .selectable_label(self.media.pointer_color == 0, text.automatic)
                            .clicked()
                        {
                            self.media.pointer_color = 0;
                        }
                        for color in crate::protocol::participants::PALETTE {
                            if swatch(ui, color, self.media.pointer_color == color) {
                                self.media.pointer_color = color;
                            }
                        }
                    });
                },
            );
            form_row(ui, text.frame_rate, "", |ui| {
                egui::ComboBox::from_id_salt("center-fps")
                    .width(238.0)
                    .selected_text(self.media.frame_rate.label(self.local_display))
                    .show_ui(ui, |ui| {
                        for choice in FrameRateChoice::available(self.local_display) {
                            ui.selectable_value(
                                &mut self.media.frame_rate,
                                choice,
                                choice.label(self.local_display),
                            );
                        }
                    });
            });
            form_row(
                ui,
                text.codec,
                "",
                |ui| {
                    if self.media.codec.restricted() {
                        ui.label(self.media.codec.label());
                        return;
                    }
                    egui::ComboBox::from_id_salt("center-codec")
                        .width(238.0)
                        .selected_text(self.media.codec.label())
                        .show_ui(ui, |ui| {
                            for choice in CodecPreference::PREFERRED {
                                ui.selectable_value(&mut self.media.codec, choice, choice.label());
                            }
                        });
                },
            );
            form_row(
                ui,
                text.decoder,
                "",
                |ui| {
                    if self.media.decoder.mode.hardware() {
                        self.media_inventory.draw(
                            ui,
                            "center-decoder-gpu",
                            &mut self.media.decoder.gpu,
                            false,
                        );
                    } else {
                        ui.label(text.software_decoding_only);
                    }
                },
            );
            form_row(
                ui,
                text.route,
                "",
                |ui| {
                    egui::ComboBox::from_id_salt("center-route")
                        .width(238.0)
                        .selected_text(self.media.transport.label())
                        .show_ui(ui, |ui| {
                            for choice in [
                                TransportChoice::Auto,
                                TransportChoice::P2p,
                                TransportChoice::Relay,
                            ] {
                                ui.selectable_value(
                                    &mut self.media.transport,
                                    choice,
                                    choice.label(),
                                );
                            }
                        });
                },
            );
            let restriction = super::media_preferences::limits_title(
                self.media.decoder.mode,
                self.media
                    .codec
                    .restricted()
                    .then(|| self.media.codec.label()),
            );
            egui::CollapsingHeader::new(restriction)
                .id_salt("controller-codec-limits")
                .show(ui, |ui| {
                    form_row(ui, text.decoding_mode, "", |ui| {
                        egui::ComboBox::from_id_salt("center-decoder")
                            .width(238.)
                            .selected_text(self.media.decoder.mode.label())
                            .show_ui(ui, |ui| {
                                for mode in crate::media::selection::ProcessingMode::ALL {
                                    ui.selectable_value(
                                        &mut self.media.decoder.mode,
                                        mode,
                                        mode.label(),
                                    );
                                }
                            });
                    });
                    form_row(ui, text.codec_restriction, "", |ui| {
                        egui::ComboBox::from_id_salt("center-codec-only")
                            .width(238.)
                            .selected_text(if self.media.codec.restricted() {
                                self.media.codec.label()
                            } else {
                                text.unrestricted
                            })
                            .show_ui(ui, |ui| {
                                if ui
                                    .selectable_label(!self.media.codec.restricted(), text.unrestricted)
                                    .clicked()
                                {
                                    self.media.codec = CodecPreference::Auto;
                                }
                                for codec in CodecPreference::ONLY {
                                    ui.selectable_value(
                                        &mut self.media.codec,
                                        codec,
                                        codec.label(),
                                    );
                                }
                            });
                    });
                });
        });
        if self.media != original_media {
            if let Err(error) = crate::media::preferences::save(self.media) {
                self.media = original_media;
                self.status =
                    StatusMessage::error((text.media_save_failed)(&format!("{error:#}")));
            }
        }
    }

    /// NAT traversal, for both roles.
    fn network_settings(&mut self, ui: &mut egui::Ui) {
        section(ui, t().settings.section_traversal);
        self.edit_network(ui, Self::traversal_rows);
    }

    /// How others reach this device directly (Settings → Host settings).
    pub(super) fn direct_settings(&mut self, ui: &mut egui::Ui) {
        section(ui, t().settings.section_direct);
        self.edit_network(ui, Self::direct_rows);
    }

    /// Show some of the network switches and apply what changed. Sessions
    /// read them when they start. In service mode the background service
    /// hosts, so the switches shown are its own and changes go to it as well.
    fn edit_network(
        &mut self,
        ui: &mut egui::Ui,
        rows: fn(&mut Self, &mut egui::Ui, &mut crate::transport::nat::settings::Settings),
    ) {
        let service = crate::transport::direct::managed::managed();
        match self.direct.take_outcome() {
            Some(Ok(message)) => self.status = StatusMessage::info(message),
            Some(Err(error)) => self.status = StatusMessage::error(error),
            None => {}
        }
        let network = if service {
            crate::transport::direct::managed::remote()
                .map(|state| state.settings)
                .unwrap_or_else(crate::transport::nat::settings::current)
        } else {
            crate::transport::nat::settings::current()
        };
        let mut edited = network;
        rows(self, ui, &mut edited);
        if edited != network && !self.direct.sending {
            // This process's controller sessions read the local copy.
            match crate::transport::nat::settings::save(edited) {
                Ok(()) if service => {
                    let host = self.host.clone();
                    self.direct.send(
                        ui.ctx(),
                        host.as_ref(),
                        crate::transport::direct::managed::Action::Network(edited),
                        t().common.saved,
                    );
                }
                Ok(()) => {
                    if (edited.serverless, edited.direct, edited.direct_port, edited.lan_discovery)
                        != (network.serverless, network.direct, network.direct_port, network.lan_discovery)
                    {
                        self.status = StatusMessage::info(t().settings.applied);
                    }
                }
                Err(error) => self.status = StatusMessage::error(format!("{error:#}")),
            }
        }
    }

    fn traversal_rows(&mut self, ui: &mut egui::Ui, edited: &mut crate::transport::nat::settings::Settings) {
        let text = &t().settings;
        form_row(
            ui,
            text.aggressive_punching,
            "",
            |ui| {
                crate::ui::controls::switch(ui, &mut edited.aggressive_punching);
            },
        );
        form_row(
            ui,
            text.allow_port_mapping,
            "",
            |ui| {
                crate::ui::controls::switch(ui, &mut edited.port_mapping);
            },
        );
    }

    fn direct_rows(&mut self, ui: &mut egui::Ui, edited: &mut crate::transport::nat::settings::Settings) {
        let text = &t().settings;
        form_row(
            ui,
            text.allow_direct,
            "",
            |ui| {
                crate::ui::controls::switch(ui, &mut edited.direct);
            },
        );
        form_row(
            ui,
            text.direct_port,
            "TCP / UDP",
            |ui| {
                ui.add_enabled(
                    edited.direct,
                    egui::DragValue::new(&mut edited.direct_port).range(1024..=65_000),
                );
            },
        );
        form_row(
            ui,
            text.lan_discovery,
            "",
            |ui| {
                ui.add_enabled_ui(edited.direct, |ui| {
                    crate::ui::controls::switch(ui, &mut edited.lan_discovery);
                });
            },
        );
        // Without a server there is nothing to stop using.
        if self.access() == Access::NoServer && !edited.serverless {
            return;
        }
        form_row(
            ui,
            text.direct_only,
            "",
            |ui| {
                crate::ui::controls::switch(ui, &mut edited.serverless);
            },
        );
    }

    /// The interface language: Windows' or a chosen one. Applies at once.
    fn language_settings(&mut self, ui: &mut egui::Ui) {
        use crate::i18n::{Choice, Language};
        let label = |choice: Choice| match choice {
            Choice::System => t().settings.language_system,
            Choice::ZhCn => Language::ZhCn.name(),
            Choice::En => Language::En.name(),
        };
        // The saved choice, read from disk once rather than every frame.
        let id = egui::Id::new("settings-language-choice");
        let current = ui
            .data(|data| data.get_temp::<Choice>(id))
            .unwrap_or_else(crate::i18n::choice);
        ui.data_mut(|data| data.insert_temp(id, current));
        let mut chosen = current;
        form_row(ui, t().settings.interface_language, "", |ui| {
            egui::ComboBox::from_id_salt("settings-language")
                .width(238.0)
                .selected_text(label(chosen))
                .show_ui(ui, |ui| {
                    for choice in [Choice::System, Choice::ZhCn, Choice::En] {
                        ui.selectable_value(&mut chosen, choice, label(choice));
                    }
                });
        });
        if chosen == current {
            return;
        }
        match crate::i18n::choose(chosen) {
            Ok(()) => {
                ui.data_mut(|data| data.insert_temp(id, chosen));
                ui.ctx().send_viewport_cmd(egui::ViewportCommand::Title(
                    super::super::window_title(),
                ));
                ui.ctx().request_repaint();
            }
            Err(error) => {
                self.status =
                    StatusMessage::error((t().settings.language_failed)(&format!("{error:#}")));
            }
        }
    }
}
