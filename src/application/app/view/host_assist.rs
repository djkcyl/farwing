use super::*;
use crate::features::host::assist::{Action, Mode, Passwords, Settings};

pub(super) struct PasswordEditor {
    generation: u64,
    passwords: Passwords,
    code: String,
    confirmation: String,
    visible: bool,
}

struct CardAction {
    enabled: bool,
    settings: bool,
    refresh: bool,
    retry: bool,
}

/// An invitation to connect to this device, with what the other side needs;
/// `server`: the origin of the server this device is reached through.
fn share_text(device_id: &str, short: &str, settings: &Settings, server: Option<&str>) -> String {
    let words = &t().center;
    let mut lines = vec![
        words.share_intro.to_owned(),
        String::new(),
        (words.share_device_id)(&crate::account::api::device_id_text(device_id)),
    ];
    if !short.is_empty() {
        lines.push((words.share_short_number)(&assist_components::grouped_id(short)));
    }
    if !settings.mode.needs_password() {
        lines.push(words.share_no_code.to_owned());
    } else {
        lines.push(if settings.passwords == Passwords::Custom {
            words.share_code_separately.to_owned()
        } else {
            (words.share_code)(&settings.code)
        });
        if settings.mode.needs_confirmation() {
            lines.push(words.share_also_approve.to_owned());
        }
    }
    lines.push(String::new());
    lines.push(words.share_how_to.to_owned());
    if let Some(server) = server {
        lines.push((words.share_browser)(&visit_link(server, device_id, settings)));
    }
    lines.join("\n")
}

/// The web console's visitor page with this device and its access code
/// filled in. The code goes in the fragment, which browsers do not send to
/// the server.
fn visit_link(server: &str, device_id: &str, settings: &Settings) -> String {
    let mut link = format!("{server}/visit?id={device_id}");
    if !settings.mode.needs_password() {
        link.push_str("&confirm=1");
    } else if settings.passwords != Passwords::Custom {
        link.push_str("#code=");
        link.extend(url::form_urlencoded::byte_serialize(settings.code.as_bytes()));
    }
    link
}

/// The access code: on or off, the code to tell others, how it is checked.
fn access_code_card(
    ui: &mut egui::Ui,
    snapshot: &crate::features::host::assist::Snapshot,
    allowed: bool,
    saving: bool,
    show_code: &mut bool,
    save_error: Option<&str>,
    device_id: &str,
) -> CardAction {
    use super::assist_components::glyph;
    let mut action = CardAction {
        enabled: snapshot.settings.enabled,
        settings: false,
        refresh: false,
        retry: false,
    };
    let settings = &snapshot.settings;
    let text = &t().center;
    super::cards::card().show(ui, |ui| {
        ui.set_width(ui.available_width());
        ui.horizontal(|ui| {
            ui.label(RichText::new(text.connect_with_code).strong());
            ui.with_layout(egui::Layout::right_to_left(Align::Center), |ui| {
                ui.add_enabled_ui(!saving, |ui| {
                    crate::ui::controls::service_switch(ui, &mut action.enabled);
                });
                action.settings =
                    glyph(ui, Icon::Settings, text.access_code_settings, MUTED).clicked();
            });
        });
        let enabled = allowed && action.enabled;
        if !enabled {
            *show_code = false;
        }
        if action.enabled {
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                let password = if settings.passwords == Passwords::Custom {
                    &settings.custom_code
                } else {
                    &settings.code
                };
                let label = if !settings.mode.needs_password() {
                    text.access_code
                } else {
                    settings.passwords.label()
                };
                ui.label(RichText::new(label).color(MUTED));
                let code = if !settings.mode.needs_password() {
                    text.no_access_code_needed
                } else if *show_code && enabled {
                    password.as_str()
                } else {
                    "••••••••"
                };
                assist_components::copy_value(
                    ui,
                    "access-code",
                    vec2(200.0, theme::CONTROL_HEIGHT),
                    RichText::new(code)
                        .monospace()
                        .size(theme::DIALOG_TITLE)
                        .color(if enabled { TEXT } else { MUTED }),
                    (enabled && settings.mode.needs_password() && !password.is_empty())
                        .then_some(password.as_str()),
                    text.copy_access_code,
                    text.access_code_copied,
                );
                if settings.mode.needs_password() {
                    ui.add_enabled_ui(enabled, |ui| {
                        crate::ui::controls::visibility_button(ui, show_code);
                    });
                    if settings.passwords.random() {
                        action.refresh = ui
                            .add_enabled_ui(enabled && !saving, |ui| {
                                glyph(ui, Icon::Refresh, text.new_temporary_code, MUTED)
                            })
                            .inner
                            .clicked();
                    }
                }
                ui.with_layout(egui::Layout::right_to_left(Align::Center), |ui| {
                    let copied_id = ui.id().with("access-share-copied");
                    let copied = ui
                        .ctx()
                        .data(|data| data.get_temp::<Instant>(copied_id))
                        .filter(|at| at.elapsed() < Duration::from_secs(2));
                    if ui
                        .add_enabled(
                            enabled,
                            login_button(if copied.is_some() {
                                t().common.copied
                            } else {
                                text.copy_invitation
                            })
                                .min_size(vec2(theme::ASSIST_SHARE_WIDTH, theme::CONTROL_HEIGHT)),
                        )
                        .clicked()
                    {
                        let server = crate::account::server::ServerConfig::load()
                            .ok()
                            .flatten()
                            .filter(|_| !crate::transport::nat::settings::current().serverless);
                        ui.ctx().copy_text(share_text(
                            device_id,
                            &snapshot.connect_id,
                            settings,
                            server.as_ref().map(|s| s.origin()),
                        ));
                        ui.ctx().data_mut(|data| data.insert_temp(copied_id, Instant::now()));
                        ui.ctx().request_repaint_after(Duration::from_secs(2));
                    }
                });
            });
            ui.horizontal_wrapped(|ui| {
                ui.label(
                    RichText::new((text.guests_can_in_mode)(settings.mode.label()))
                        .size(theme::SMALL)
                        .color(MUTED),
                );
                super::cards::caps_icons(ui, &settings.grants(), text.no_permissions);
            });
        }
        if action.enabled && !allowed {
            ui.colored_label(AMBER, text.allow_connections_off);
        }
        if let Some(error) = save_error {
            ui.colored_label(AMBER, (text.settings_not_saved)(error));
        }
        if let Some(error) = &snapshot.error {
            ui.horizontal(|ui| {
                ui.colored_label(AMBER, error);
                action.retry = ui.add(crate::ui::controls::secondary(t().common.retry)).clicked();
            });
        }
    });
    action
}

impl DeviceCenterApp {
    fn edit_assist_password(&mut self, passwords: Passwords) {
        self.center_ui.host_assist_settings_open = false;
        self.center_ui.host_assist_password = Some(PasswordEditor {
            generation: self.login_generation,
            passwords,
            code: String::new(),
            confirmation: String::new(),
            visible: false,
        });
    }
    fn host_assist_action(&mut self, action: Action) {
        if self
            .worker
            .commands
            .send(GuiCommand::HostAssist {
                generation: self.login_generation,
                action,
            })
            .is_err()
        {
            self.status = StatusMessage::error(t().center.access_code_service_stopped);
        }
    }

    pub(super) fn host_assist_controls(&mut self, ui: &mut egui::Ui) {
        let Some(host) = self.host.clone() else {
            return;
        };
        let snapshot = host.assistance.snapshot();
        let device_id = crate::transport::direct::local_device_id().unwrap_or_default();
        let action = access_code_card(
            ui,
            &snapshot,
            host.allowed(),
            host.status().saving,
            &mut self.center_ui.host_assist_show_code,
            host.status().settings_error.as_deref(),
            &device_id,
        );
        let mut settings = snapshot.settings.clone();
        settings.enabled = action.enabled;
        if (settings.enabled && settings.code.is_empty()) || action.refresh {
            settings.refresh_code();
        }
        if settings != snapshot.settings {
            // Turning the access code on also allows connections at all.
            if settings.enabled && !snapshot.settings.enabled && !host.allowed() {
                host.set_allowed(true);
            }
            match host.set_assistance(settings) {
                Ok(()) => self.save_host_settings(),
                Err(error) => self.status = StatusMessage::error(error.to_string()),
            }
        }
        if action.settings {
            self.center_ui.host_assist_settings_open = true;
        }
        if action.retry {
            self.host_assist_action(Action::Refresh);
        }
    }
    pub(super) fn host_assist_dialog(&mut self, ctx: &egui::Context) -> bool {
        if self.assist_password_dialog(ctx) {
            return true;
        }
        self.assist_settings_dialog(ctx)
    }

    fn assist_password_dialog(&mut self, ctx: &egui::Context) -> bool {
        let Some(mut editor) = self.center_ui.host_assist_password.take() else {
            return false;
        };
        if editor.generation != self.login_generation || self.logout_pending {
            return false;
        }
        let Some(host) = self.host.clone() else {
            return false;
        };
        let mut save = false;
        let mut cancel = false;
        let text = &t().center;
        let response = egui::Modal::new(egui::Id::new("host-assist-password-editor"))
            .frame(dialog_frame())
            .show(ctx, |ui| {
                ui.set_width(theme::MESSAGE_DIALOG_WIDTH);
                cancel = crate::ui::controls::dialog_header(
                    ui,
                    text.fixed_access_code,
                    crate::ui::controls::DialogIcon::Edit,
                    true,
                );
                ui.label(text.new_fixed_access_code);
                ui.add_sized(
                    [ui.available_width(), theme::CONTROL_HEIGHT],
                    singleline_input(&mut editor.code)
                        .password(!editor.visible)
                        .char_limit(128),
                );
                ui.label(text.enter_again);
                ui.add_sized(
                    [ui.available_width(), theme::CONTROL_HEIGHT],
                    singleline_input(&mut editor.confirmation)
                        .password(!editor.visible)
                        .char_limit(128),
                );
                ui.checkbox(&mut editor.visible, text.show_access_code);
                let valid = Settings::valid_custom_code(&editor.code);
                let matching = editor.code == editor.confirmation;
                if !editor.code.is_empty() && !valid {
                    ui.colored_label(AMBER, text.code_rule);
                } else if !editor.confirmation.is_empty() && !matching {
                    ui.colored_label(AMBER, text.codes_dont_match);
                }
                let (accept, dismiss) = crate::ui::controls::dialog_actions(
                    ui,
                    Some(
                        crate::ui::controls::DialogAction::new(text.save_and_apply)
                            .enabled(valid && matching && !host.status().saving),
                    ),
                    Some(t().common.cancel),
                );
                save = accept;
                cancel |= dismiss;
            });
        if save {
            self.center_ui.host_assist_settings_open = true;
            self.center_ui.host_assist_show_code = false;
            let mut settings = host.assistance.settings();
            settings.custom_code = std::mem::take(&mut editor.code);
            settings.passwords = editor.passwords;
            if settings.mode == Mode::Confirmation {
                settings.mode = Mode::Password;
            }
            match host.set_assistance(settings) {
                Ok(()) => self.save_host_settings(),
                Err(error) => self.status = StatusMessage::error(error.to_string()),
            }
        } else if !cancel && !response.should_close() {
            self.center_ui.host_assist_password = Some(editor);
        } else {
            self.center_ui.host_assist_settings_open = true;
        }
        true
    }

    fn assist_settings_dialog(&mut self, ctx: &egui::Context) -> bool {
        if !self.center_ui.host_assist_settings_open {
            return false;
        }
        let Some(host) = self.host.clone() else {
            return false;
        };
        let original = host.assistance.settings();
        let mut settings = original.clone();
        let mut close = false;
        let text = &t().center;
        let modal = egui::Modal::new(egui::Id::new("host-assist-settings"))
            .frame(dialog_frame())
            .show(ctx, |ui| {
                ui.set_width(theme::MESSAGE_DIALOG_WIDTH);
                close = crate::ui::controls::dialog_header(
                    ui,
                    text.access_code_settings,
                    crate::ui::controls::DialogIcon::Info,
                    true,
                );
                ui.add_enabled_ui(!host.status().saving, |ui| {
                    ui.label(RichText::new(text.guest_connection_method).color(MUTED));
                    egui::ComboBox::from_id_salt("host-assist-mode")
                        .width(ui.available_width())
                        .selected_text(settings.mode.label())
                        .show_ui(ui, |ui| {
                            for mode in Mode::ALL {
                                ui.selectable_value(&mut settings.mode, mode, mode.label());
                            }
                        });
                    if settings.mode.needs_password() {
                        ui.add_space(12.0);
                        ui.label(RichText::new(text.access_code_type).color(MUTED));
                        let mut selected = settings.passwords;
                        egui::ComboBox::from_id_salt("host-assist-passwords")
                            .width(ui.available_width())
                            .selected_text(selected.label())
                            .show_ui(ui, |ui| {
                                for choice in Passwords::ALL {
                                    ui.selectable_value(&mut selected, choice, choice.label());
                                }
                            });
                        if selected != settings.passwords {
                            if selected.custom() && settings.custom_code.is_empty() {
                                self.edit_assist_password(selected);
                            } else {
                                settings.passwords = selected;
                            }
                        }
                        ui.add_space(12.0);
                        ui.horizontal(|ui| {
                            ui.label(text.fixed_access_code);
                            ui.label(
                                RichText::new(if settings.custom_code.is_empty() {
                                    text.not_set
                                } else {
                                    text.set
                                })
                                .color(MUTED),
                            );
                            ui.with_layout(egui::Layout::right_to_left(Align::Center), |ui| {
                                if ui
                                    .add(crate::ui::controls::secondary(
                                        if settings.custom_code.is_empty() {
                                            text.set_action
                                        } else {
                                            text.change
                                        },
                                    ))
                                    .clicked()
                                {
                                    self.edit_assist_password(
                                        if settings.passwords == Passwords::Random {
                                            Passwords::Custom
                                        } else {
                                            settings.passwords
                                        },
                                    );
                                }
                            });
                        });
                    }
                    ui.add_space(12.0);
                    ui.label(RichText::new(text.guests_can).color(MUTED));
                    let current = settings.grants();
                    let mut chosen = current.clone();
                    ui.horizontal_wrapped(|ui| {
                        ui.add_enabled(
                            false,
                            egui::Checkbox::new(
                                &mut true,
                                crate::application::app::friends::cap_label(
                                    farwing_protocol::caps::VIEW,
                                ),
                            ),
                        );
                        for cap in Settings::OFFERED {
                            let mut on = chosen.iter().any(|c| c == cap);
                            if ui
                                .checkbox(&mut on, crate::application::app::friends::cap_label(cap))
                                .changed()
                            {
                                if on {
                                    chosen.push((*cap).to_owned());
                                } else {
                                    chosen.retain(|c| c != cap);
                                }
                            }
                        }
                    });
                    if chosen != current {
                        settings.grants = Some(farwing_protocol::caps::normalize(&chosen));
                    }
                });
                close |= crate::ui::controls::dialog_actions(
                    ui,
                    Some(crate::ui::controls::DialogAction::new(t().common.done)),
                    None,
                )
                .0;
            });
        if settings != original {
            self.center_ui.host_assist_show_code = false;
            match host.set_assistance(settings) {
                Ok(()) => self.save_host_settings(),
                Err(error) => self.status = StatusMessage::error(error.to_string()),
            }
        }
        if close || modal.should_close() {
            self.center_ui.host_assist_settings_open = false;
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_visit_link_carries_what_the_visitor_needs() {
        let server = "https://farwing.example.com";
        let id = "abcdefghijklmnop";
        let mut settings = Settings {
            code: "AB2CD3EF".into(),
            ..Settings::default()
        };
        assert_eq!(
            visit_link(server, id, &settings),
            "https://farwing.example.com/visit?id=abcdefghijklmnop#code=AB2CD3EF"
        );
        settings.passwords = Passwords::Custom;
        assert_eq!(visit_link(server, id, &settings), format!("{server}/visit?id={id}"));
        settings.mode = Mode::Confirmation;
        assert_eq!(visit_link(server, id, &settings), format!("{server}/visit?id={id}&confirm=1"));
    }
}
