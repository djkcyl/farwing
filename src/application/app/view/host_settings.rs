use super::*;
use crate::i18n::t;

impl DeviceCenterApp {
    pub(super) fn notification_settings(&mut self, ui: &mut egui::Ui) {
        let text = &t().settings;
        section(ui, text.section_notifications);
        let mut mode = self.notifications.mode;
        form_row(
            ui,
            text.notification_style,
            "",
            |ui| {
                egui::ComboBox::from_id_salt("remote-notification-mode")
                    .width(238.)
                    .selected_text(mode.label())
                    .show_ui(ui, |ui| {
                        for item in super::super::notifications::Mode::ALL {
                            ui.selectable_value(&mut mode, item, item.label());
                        }
                    });
            },
        );
        if mode != self.notifications.mode {
            self.notifications.set_mode(mode);
        }
        if let Some(error) = &self.notifications.error {
            ui.label(egui::RichText::new(error).color(theme::AMBER));
            if ui.button(text.retry_notifications).clicked() {
                self.notifications.set_mode(mode);
            }
        }
        if ui
            .add_enabled(
                self.notifications.available(),
                crate::ui::controls::secondary(text.show_notifications),
            )
            .clicked()
        {
            self.notifications.reopen();
        }
    }
    pub(super) fn save_host_settings(&mut self) {
        if self
            .worker
            .commands
            .send(GuiCommand::SaveHostSettings {
                generation: self.login_generation,
            })
            .is_err()
        {
            self.status = StatusMessage::error(t().settings.host_save_failed);
        }
    }
    pub(super) fn host_settings(&mut self, ui: &mut egui::Ui) {
        ui.add_enabled_ui(!self.center_ui.components.busy(), |ui| {
            self.host_settings_controls(ui)
        });
        self.wol_setup_dialog(ui.ctx());
    }
    fn host_access_settings(&mut self, ui: &mut egui::Ui, host: &crate::features::host::Handle) {
        // Remote power and Wake-on-LAN go through the account's server.
        let account = !host.is_guest();
        let mut files = host.file_transfer_allowed();
        let mut ports = host.port_mapping_allowed();
        let mut power = host.power_allowed();
        let mut wol = host.wol_allowed();
        let mut clipboard = host.clipboard_settings();
        let mut max_participants = host.max_participants();
        let before = (files, ports, power, wol, clipboard, max_participants);
        let enabled = usize::from(clipboard.enabled)
            + usize::from(clipboard.enabled && clipboard.files)
            + usize::from(files)
            + usize::from(ports)
            + if account {
                usize::from(power) + usize::from(wol)
            } else {
                0
            };
        let total = if account { 6 } else { 4 };
        let id = egui::Id::new(("host-access-settings", self.login_generation));
        let mut open = ui.data(|data| data.get_temp::<bool>(id).unwrap_or(false));
        let text = &t().settings;
        form_row(
            ui,
            text.access_permissions,
            &(text.permissions_enabled)(enabled, total),
            |ui| {
                if ui
                    .add(crate::ui::controls::secondary(if open {
                        text.collapse
                    } else {
                        text.expand
                    }))
                    .clicked()
                {
                    open = !open;
                }
            },
        );
        ui.data_mut(|data| data.insert_temp(id, open));
        if open {
            crate::ui::controls::section_frame()
                .inner_margin(theme::DIALOG_MARGIN)
                .show(ui, |ui| {
                    ui.columns(2, |columns| {
                        let ui = &mut columns[0];
                        ui.label(RichText::new(text.files_and_clipboard).strong());
                        ui.checkbox(&mut files, text.file_transfer);
                        ui.checkbox(&mut clipboard.enabled, text.clipboard_sync);
                        ui.add_enabled_ui(clipboard.enabled, |ui| {
                            ui.checkbox(&mut clipboard.files, text.clipboard_files);
                        });
                        let ui = &mut columns[1];
                        ui.label(RichText::new(text.system_and_network).strong());
                        ui.checkbox(&mut ports, text.port_forwarding);
                        if account {
                            ui.checkbox(&mut power, text.power_control);
                            ui.checkbox(&mut wol, text.wol_assistance);
                        }
                    });
                    ui.add_space(theme::MENU_GROUP_GAP);
                    ui.horizontal(|ui| {
                        ui.label(text.max_participants);
                        egui::ComboBox::from_id_salt("host-max-participants")
                            .selected_text((text.people)(max_participants))
                            .show_ui(ui, |ui| {
                                for limit in 1..=farwing_protocol::limits::MAX_PARTICIPANTS {
                                    ui.selectable_value(
                                        &mut max_participants,
                                        limit,
                                        (text.people)(limit),
                                    );
                                }
                            });
                    });
                });
            ui.add_space(theme::MENU_GROUP_GAP);
        }
        let results = [
            (files != before.0).then(|| host.set_file_transfer_allowed(files)),
            (ports != before.1).then(|| host.set_port_mapping_allowed(ports)),
            (power != before.2).then(|| host.set_power_allowed(power)),
            (wol != before.3).then(|| host.set_wol_allowed(wol)),
            (clipboard != before.4).then(|| host.set_clipboard_settings(clipboard)),
            (max_participants != before.5).then(|| host.set_max_participants(max_participants)),
        ];
        let mut changed = false;
        for result in results.into_iter().flatten() {
            match result {
                Ok(()) => changed = true,
                Err(error) => self.status = StatusMessage::error(error.to_string()),
            }
        }
        if changed {
            self.save_host_settings();
        }
        let status = host.status();
        if let Some(error) = status.clipboard.error {
            ui.colored_label(RED, error);
        }
        if let Some(message) = status.power_message {
            ui.label(RichText::new(message).small().color(MUTED));
        }
    }
    /// The background service, as it matters to others connecting.
    pub(super) fn service_row(&mut self, ui: &mut egui::Ui) {
        let Some(reach) = self.center_ui.components.reach() else {
            return;
        };
        let mut install = false;
        let mut manage = false;
        let text = &t().settings;
        form_row(ui, text.background_service, reach.state, |ui| match reach.action {
            Some(label) => install = ui.add(crate::ui::controls::primary(label)).clicked(),
            None => manage = ui.add(crate::ui::controls::secondary(text.manage)).clicked(),
        });
        if install {
            self.center_ui.components.request_install();
        }
        if manage {
            self.center_ui.page = Page::Settings;
            self.center_ui.settings_tab = super::settings::SettingsTab::Service;
        }
    }
    fn host_settings_controls(&mut self, ui: &mut egui::Ui) {
        let host = self.host.clone();
        let mut allowed = host.as_ref().is_some_and(|h| h.allowed());
        let text = &t().settings;
        section(ui, text.section_connections);
        form_row(
            ui,
            text.allow_connections,
            "",
            |ui| {
                if ui
                    .add_enabled_ui(host.is_some(), |ui| {
                        crate::ui::controls::switch(ui, &mut allowed)
                    })
                    .inner
                    .changed()
                    && let Some(host) = host.as_ref()
                {
                    host.set_allowed(allowed);
                    self.save_host_settings();
                }
            },
        );
        self.service_row(ui);
        self.direct_settings(ui);
        if let Some(host) = host.as_ref() {
            section(ui, text.section_permissions);
            if host.request_audio_devices()
                && self
                    .worker
                    .commands
                    .send(GuiCommand::RefreshHostAudioDevices {
                        generation: self.login_generation,
                    })
                    .is_err()
            {
                host.audio_devices_failed();
            }
            self.host_access_settings(ui, host);
            if !host.is_guest() {
                self.wol_setup_entry(ui);
            }
            section(ui, text.section_video_audio);
            let inventory = host.audio_devices();
            let mut quality = host.audio_quality();
            let mut quality_changed = false;
            form_row(
                ui,
                text.audio_quality,
                "",
                |ui| {
                    quality_changed = crate::ui::controls::audio_quality(ui, &mut quality);
                },
            );
            if quality_changed {
                match host.set_audio_quality(quality) {
                    Ok(()) => self.save_host_settings(),
                    Err(error) => self.status = StatusMessage::error(error.to_string()),
                }
            }
            let original_defaults = host.audio_defaults();
            let mut selected_defaults = original_defaults;
            form_row(
                ui,
                text.default_audio_devices,
                "",
                |ui| {
                    egui::ComboBox::from_id_salt("host-audio-defaults")
                        .width(238.0)
                        .selected_text(selected_defaults.label())
                        .show_ui(ui, |ui| {
                            for choice in crate::features::host::audio::DefaultDevices::ALL {
                                ui.selectable_value(&mut selected_defaults, choice, choice.label());
                            }
                        });
                },
            );
            if selected_defaults != original_defaults {
                match host.set_audio_defaults(selected_defaults) {
                    Ok(()) => self.save_host_settings(),
                    Err(error) => self.status = StatusMessage::error(error.to_string()),
                }
            }
            if let Some(error) = host.status().microphone.routing_error {
                ui.colored_label(RED, error);
            }
            let original_device = host.audio_device();
            let mut selected_device = original_device.clone();
            let selected_name = match original_device.as_ref() {
                None => text.system_default_device.to_owned(),
                Some(device) => inventory
                    .devices
                    .iter()
                    .find(|d| d.id == device.id)
                    .map(|d| d.name.clone())
                    .unwrap_or_else(|| {
                        if inventory.updated.is_some() && inventory.error.is_none() {
                            (text.device_disconnected)(&device.name)
                        } else {
                            device.name.clone()
                        }
                    }),
            };
            form_row(ui, text.audio_capture_device, "", |ui| {
                egui::ComboBox::from_id_salt("host-audio-device")
                    .width(238.0)
                    .truncate()
                    .selected_text(&selected_name)
                    .show_ui(ui, |ui| {
                        ui.selectable_value(&mut selected_device, None, text.system_default_device);
                        for device in &inventory.devices {
                            ui.push_id(&device.id, |ui| {
                                let chosen =
                                    selected_device.as_ref().is_some_and(|d| d.id == device.id);
                                if ui.selectable_label(chosen, &device.name).clicked() {
                                    selected_device = Some(device.clone());
                                }
                            });
                        }
                        if let Some(device) = original_device.as_ref()
                            && !inventory.devices.iter().any(|d| d.id == device.id)
                        {
                            ui.add_enabled(false, egui::Label::new(&selected_name));
                        }
                        if let Some(error) = &inventory.error {
                            ui.colored_label(RED, error);
                        } else if inventory.pending && inventory.updated.is_none() {
                            ui.label(text.reading_devices);
                        } else if inventory.devices.is_empty() {
                            ui.label(text.no_playback_devices);
                        }
                    });
            });
            if selected_device != original_device {
                match host.set_audio_device(selected_device) {
                    Ok(()) => self.save_host_settings(),
                    Err(error) => self.status = StatusMessage::error(error.to_string()),
                }
            }
            use crate::features::host::{EncoderCodec, EncoderMode, EncodingSettings};
            let original = host.encoding_settings();
            let mut selected = original;
            let capabilities = host.capabilities();
            let available = |settings: EncodingSettings| {
                settings.validate().is_ok()
                    && capabilities
                        .as_ref()
                        .is_none_or(|caps| caps.codecs.iter().any(|cap| settings.accepts(cap)))
            };
            form_row(
                ui,
                text.codec,
                "",
                |ui| {
                    if selected.codec != EncoderCodec::Automatic {
                        ui.label(selected.codec.label());
                    } else {
                        egui::ComboBox::from_id_salt("host-codec-preferred")
                            .width(238.)
                            .selected_text(selected.preferred_codec.label())
                            .show_ui(ui, |ui| {
                                for codec in crate::media::CodecPreference::PREFERRED {
                                    ui.selectable_value(
                                        &mut selected.preferred_codec,
                                        codec,
                                        codec.label(),
                                    );
                                }
                            });
                    }
                },
            );
            form_row(
                ui,
                text.encoder,
                "",
                |ui| {
                    if selected.mode == EncoderMode::Software {
                        ui.label(text.software_encoding_only);
                    } else {
                        self.media_inventory
                            .draw(ui, "host-encoder-gpu", &mut selected.gpu, true);
                    }
                },
            );
            form_row(
                ui,
                text.gpu_scheduling,
                "",
                |ui| {
                    egui::ComboBox::from_id_salt("host-gpu-scheduling")
                        .width(238.)
                        .selected_text(if selected.gpu_compatibility {
                            text.prefer_compatibility
                        } else {
                            text.prefer_smoothness
                        })
                        .show_ui(ui, |ui| {
                            ui.selectable_value(
                                &mut selected.gpu_compatibility,
                                false,
                                text.prefer_smoothness,
                            );
                            ui.selectable_value(
                                &mut selected.gpu_compatibility,
                                true,
                                text.prefer_compatibility,
                            );
                        });
                },
            );
            let restriction = super::super::media_preferences::limits_title(
                selected.mode,
                (selected.codec != EncoderCodec::Automatic).then(|| selected.codec.label()),
            );
            egui::CollapsingHeader::new(restriction)
                .id_salt("host-codec-limits")
                .show(ui, |ui| {
                    form_row(ui, text.encoding_mode, "", |ui| {
                        egui::ComboBox::from_id_salt("host-encoder-mode")
                            .width(238.)
                            .selected_text(selected.mode.label())
                            .show_ui(ui, |ui| {
                                for mode in EncoderMode::ALL {
                                    ui.add_enabled_ui(
                                        available(EncodingSettings { mode, ..selected }),
                                        |ui| {
                                            ui.selectable_value(
                                                &mut selected.mode,
                                                mode,
                                                mode.label(),
                                            );
                                        },
                                    );
                                }
                            });
                    });
                    form_row(ui, text.codec_restriction, "", |ui| {
                        egui::ComboBox::from_id_salt("host-encoder-codec")
                            .width(238.)
                            .selected_text(if selected.codec == EncoderCodec::Automatic {
                                text.unrestricted
                            } else {
                                selected.codec.label()
                            })
                            .show_ui(ui, |ui| {
                                for codec in EncoderCodec::ALL {
                                    ui.add_enabled_ui(
                                        available(EncodingSettings { codec, ..selected }),
                                        |ui| {
                                            ui.selectable_value(
                                                &mut selected.codec,
                                                codec,
                                                if codec == EncoderCodec::Automatic {
                                                    text.unrestricted
                                                } else {
                                                    codec.label()
                                                },
                                            );
                                        },
                                    );
                                }
                            });
                    });
                });
            if selected != original {
                match host.set_encoding_settings(selected) {
                    Ok(()) => self.save_host_settings(),
                    Err(error) => self.status = StatusMessage::error(error.to_string()),
                }
            }
        }
        let status = host.as_ref().map(|host| host.status()).unwrap_or_default();

        let message = if status.message.is_empty() {
            text.host_disabled
        } else {
            &status.message
        };
        let message = if status.saving {
            (text.saving_settings)(message)
        } else {
            message.to_owned()
        };
        // Listed participants each have their own disconnect button below.
        let listed = !status.connections.is_empty();
        let action = if status.session_active {
            (!listed).then_some(t().common.disconnect)
        } else if allowed && !status.ready && status.error.is_some() {
            Some(t().common.retry)
        } else {
            None
        };
        if crate::ui::controls::status_row(
            ui,
            &message,
            if status.connected { GREEN } else { MUTED },
            action,
        ) && let Some(host) = host.as_ref()
        {
            if status.session_active {
                host.disconnect();
            } else {
                host.retry();
            }
        }
        if listed {
            let participants = super::super::controlled_caption::participants(
                &status,
                self.devices.as_ref(),
                self.catalog.as_ref(),
            );
            if let Some(action) =
                super::super::controlled_caption::participants_list(ui, &participants)
            {
                self.participant_action(ui.ctx(), action);
            }
        }
        for (source, error) in [
            ("host-settings-error", status.settings_error),
            ("host-session-error", status.error),
        ] {
            // Service maintenance intentionally stops the resident endpoint;
            // its temporary polling failure is not a settings failure.
            if self.center_ui.components.busy() {
                crate::ui::controls::clear_notice(ui.ctx(), (source, self.login_generation));
                continue;
            }
            crate::ui::controls::observe_notice(
                ui.ctx(),
                (source, self.login_generation),
                text.tab_host,
                crate::ui::controls::DialogIcon::Error,
                error.as_deref(),
            );
        }
        ui.ctx().request_repaint_after(Duration::from_millis(250));
    }
}
