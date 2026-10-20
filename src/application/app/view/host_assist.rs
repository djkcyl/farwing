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

/// An invitation to connect to this device, with what the other side needs.
fn share_text(device_id: &str, short: &str, settings: &Settings) -> String {
    let mut text = String::from("我正在使用 Farwing，邀请你连接我的电脑。\n\n");
    text.push_str(&format!(
        "设备 ID：{}\n",
        crate::account::api::device_id_text(device_id)
    ));
    if !short.is_empty() {
        text.push_str(&format!("短号：{}\n", assist_components::grouped_id(short)));
    }
    if !settings.mode.needs_password() {
        text.push_str("无需访问码，连接时由我在本机确认。\n");
    } else {
        if settings.passwords == Passwords::Custom {
            text.push_str("访问码请单独向我获取。\n");
        } else {
            text.push_str(&format!("访问码：{}\n", settings.code));
        }
        if settings.mode.needs_confirmation() {
            text.push_str("连接时还需要我在本机确认。\n");
        }
    }
    text.push_str("\n打开 Farwing，在“设备”页输入设备 ID 或短号（同一局域网时也可以在“附近”中选择）连接。");
    text
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
    super::cards::card().show(ui, |ui| {
        ui.set_width(ui.available_width());
        ui.horizontal(|ui| {
            ui.label(RichText::new("用访问码连接本机").strong());
            ui.with_layout(egui::Layout::right_to_left(Align::Center), |ui| {
                ui.add_enabled_ui(!saving, |ui| {
                    crate::ui::controls::service_switch(ui, &mut action.enabled);
                });
                action.settings = glyph(ui, Icon::Settings, "访问码设置", MUTED).clicked();
            });
        });
        let enabled = allowed && action.enabled;
        if !enabled {
            *show_code = false;
        }
        if !action.enabled {
            ui.add(
                egui::Label::new(
                    RichText::new(
                        "开启后，其他设备输入你的访问码（或经你确认）即可作为访客连接本机，权限为访客权限；访客只能连接这一次。",
                    )
                    .size(theme::SMALL)
                    .color(MUTED),
                )
                .wrap(),
            );
        } else {
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                let password = if settings.passwords == Passwords::Custom {
                    &settings.custom_code
                } else {
                    &settings.code
                };
                let label = if !settings.mode.needs_password() {
                    "访问码"
                } else {
                    settings.passwords.label()
                };
                ui.label(RichText::new(label).color(MUTED));
                let code = if !settings.mode.needs_password() {
                    "不需要访问码"
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
                    "已复制访问码",
                );
                if settings.mode.needs_password() {
                    ui.add_enabled_ui(enabled, |ui| {
                        crate::ui::controls::visibility_button(ui, show_code);
                    });
                    if settings.passwords.random() {
                        action.refresh = ui
                            .add_enabled_ui(enabled && !saving, |ui| {
                                glyph(ui, Icon::Refresh, "换一个临时访问码", MUTED)
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
                            login_button(if copied.is_some() { "已复制" } else { "复制邀请" })
                                .min_size(vec2(theme::ASSIST_SHARE_WIDTH, theme::CONTROL_HEIGHT)),
                        )
                        .on_hover_text("复制设备 ID、短号与访问码，发给对方")
                        .clicked()
                    {
                        ui.ctx()
                            .copy_text(share_text(device_id, &snapshot.connect_id, settings));
                        ui.ctx().data_mut(|data| data.insert_temp(copied_id, Instant::now()));
                        ui.ctx().request_repaint_after(Duration::from_secs(2));
                    }
                });
            });
            let how = match settings.mode {
                Mode::Password => "输入访问码即可连接",
                Mode::PasswordConfirmation => "输入访问码后，还需要你在本机确认",
                Mode::Confirmation => "不用访问码，每次由你在本机确认",
            };
            ui.add(
                egui::Label::new(
                    RichText::new(format!(
                        "{how} · 访客可以：{}",
                        super::cards::caps_summary(&settings.grants())
                    ))
                    .size(theme::SMALL)
                    .color(MUTED),
                )
                .wrap(),
            );
        }
        if action.enabled && !allowed {
            ui.colored_label(AMBER, "请先开启上方的“允许连接本机”");
        }
        if let Some(error) = save_error {
            ui.colored_label(AMBER, format!("设置未保存：{error}"));
        }
        if let Some(error) = &snapshot.error {
            ui.horizontal(|ui| {
                ui.colored_label(AMBER, error);
                action.retry = ui.add(crate::ui::controls::secondary("重试")).clicked();
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
            self.status = StatusMessage::error("访问码后台已停止");
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
        let response = egui::Modal::new(egui::Id::new("host-assist-password-editor"))
            .frame(dialog_frame())
            .show(ctx, |ui| {
                ui.set_width(theme::MESSAGE_DIALOG_WIDTH);
                cancel = crate::ui::controls::dialog_header(
                    ui,
                    "固定访问码",
                    crate::ui::controls::DialogIcon::Edit,
                    true,
                );
                ui.label("长期有效，8至16位字母和数字，必须同时包含两者。长期访问更推荐“信任”对方的设备。");
                ui.add_space(12.0);
                ui.label("新的固定访问码");
                ui.add_sized(
                    [ui.available_width(), theme::CONTROL_HEIGHT],
                    singleline_input(&mut editor.code)
                        .password(!editor.visible)
                        .char_limit(128),
                );
                ui.label("再次输入");
                ui.add_sized(
                    [ui.available_width(), theme::CONTROL_HEIGHT],
                    singleline_input(&mut editor.confirmation)
                        .password(!editor.visible)
                        .char_limit(128),
                );
                ui.checkbox(&mut editor.visible, "显示访问码");
                let valid = Settings::valid_custom_code(&editor.code);
                let matching = editor.code == editor.confirmation;
                if !editor.code.is_empty() && !valid {
                    ui.colored_label(AMBER, "访问码不符合格式要求");
                } else if !editor.confirmation.is_empty() && !matching {
                    ui.colored_label(AMBER, "两次输入的访问码不一致");
                }
                let (accept, dismiss) = crate::ui::controls::dialog_actions(
                    ui,
                    Some(
                        crate::ui::controls::DialogAction::new("保存并应用")
                            .enabled(valid && matching && !host.status().saving),
                    ),
                    Some("取消"),
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
        let modal = egui::Modal::new(egui::Id::new("host-assist-settings"))
            .frame(dialog_frame())
            .show(ctx, |ui| {
                ui.set_width(theme::MESSAGE_DIALOG_WIDTH);
                close = crate::ui::controls::dialog_header(
                    ui,
                    "访问码设置",
                    crate::ui::controls::DialogIcon::Info,
                    true,
                );
                ui.add_enabled_ui(!host.status().saving, |ui| {
                    ui.label(RichText::new("访客怎样连接").color(MUTED));
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
                        ui.label(RichText::new("访问码类型").color(MUTED));
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
                            ui.label("固定访问码");
                            ui.label(
                                RichText::new(if settings.custom_code.is_empty() {
                                    "未设置"
                                } else {
                                    "已设置"
                                })
                                .color(MUTED),
                            );
                            ui.with_layout(egui::Layout::right_to_left(Align::Center), |ui| {
                                if ui
                                    .add(crate::ui::controls::secondary(
                                        if settings.custom_code.is_empty() {
                                            "设置"
                                        } else {
                                            "修改"
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
                    ui.label(RichText::new("访客可以").color(MUTED));
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
                    Some(crate::ui::controls::DialogAction::new("完成")),
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
