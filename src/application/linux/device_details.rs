use super::*;
use crate::account::{api::DetailRow, power::PowerAction};
use crate::media::ConnectionMediaOptions;
use farwing_protocol::caps;

pub(super) struct Details {
    pub id: String,
    pub data: Option<DeviceDetail>,
}

impl DeviceCenter {
    pub(super) fn details_page(
        &mut self,
        ui: &mut egui::Ui,
        client: &Arc<AuthenticatedClient>,
        devices: &DeviceList,
    ) {
        let Some(detail) = &self.center.detail else {
            return;
        };
        let Some(device) = devices.all().find(|device| device.device_id == detail.id) else {
            device_list::empty(ui, &(t().center.device_not_found)(&detail.id));
            return;
        };
        let current = device.device_id == devices.current_device.device_id;
        let owned = !device.is_shared();
        let text = t();
        let mut connect = None;
        let mut dialog = None;
        let mut open_ports = false;
        controls::card().show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.heading(alias(device));
            ui.horizontal_wrapped(|ui| {
                ui.label(format!(
                    "{} · {}",
                    platform_label(device.platform),
                    device.status_label()
                ));
                if ui
                    .add(controls::secondary(text.center.copy_device_id))
                    .clicked()
                {
                    ui.ctx().copy_text(device.device_id.clone());
                }
            });
            if let Some(owner) = &device.owner {
                ui.label((text.center.shared_by)(owner.display_name()));
            }
            let granted = |cap| devices.with_capability(&device.device_id, cap).is_ok();
            let online = device.is_connected() && device.controlled_support && device.controllable;
            ui.horizontal_wrapped(|ui| {
                if !current && can_view(device) {
                    if ui
                        .add_enabled(
                            !self.busy && online,
                            controls::primary(text.center.connect_device),
                        )
                        .clicked()
                    {
                        connect = Some(None);
                    }
                    if granted(caps::AUDIO)
                        && ui
                            .add_enabled(
                                !self.busy && online,
                                controls::secondary(text.center.audio_only_connect),
                            )
                            .clicked()
                    {
                        connect = Some(Some(ConnectionMediaOptions {
                            audio_only: true,
                            ..self.media
                        }));
                    }
                }
                if !current
                    && device.platform == farwing_protocol::Platform::Windows
                    && device.policy.camera
                    && granted(caps::CAMERA)
                    && ui
                        .add_enabled(
                            !self.busy && online,
                            controls::secondary(text.camera.menu_title),
                        )
                        .clicked()
                {
                    connect = Some(Some(ConnectionMediaOptions {
                        camera_only: true,
                        ..self.media
                    }));
                }
                if !current
                    && device.platform.is_desktop()
                    && device.policy.ports
                    && granted(caps::PORTS)
                    && client
                        .feature_catalog()
                        .policy_for(device)
                        .supports(crate::account::feature_ability::Feature::Ports)
                {
                    open_ports = ui
                        .add_enabled(
                            !self.busy && online,
                            controls::secondary(text.center.port_forwarding),
                        )
                        .clicked();
                }
            });
        });
        if let Some(owner) = &device.owner {
            let title = (text.center.permissions_of)(owner.display_name());
            device_list::heading(ui, &title, None, true, |_| {});
            ui.horizontal_wrapped(|ui| {
                if device.granted.is_empty() {
                    ui.label(text.center.no_permissions);
                }
                for cap in &device.granted {
                    controls::chip(ui, &crate::i18n::cap_label(cap), theme::ACCENT);
                }
            });
        }
        device_list::heading(ui, text.center.device_info, None, true, |_| {});
        controls::card().show(ui, |ui| {
            ui.set_width(ui.available_width());
            if let Some(data) = &detail.data {
                egui::Grid::new("linux-device-specs")
                    .num_columns(2)
                    .striped(true)
                    .show(ui, |ui| {
                        for (key, value) in &data.details {
                            ui.label(RichText::new(key.label()).color(theme::MUTED));
                            if *key == DetailRow::DeviceId {
                                ui.monospace(device_id_text(value));
                            } else {
                                ui.add(egui::Label::new(value).wrap().selectable(true));
                            }
                            ui.end_row();
                        }
                    });
            } else {
                ui.label(device_id_text(&device.device_id));
                if self.busy {
                    ui.label(text.center.refreshing);
                }
            }
        });
        if owned {
            device_list::heading(ui, text.center.device_management, None, true, |_| {});
            ui.horizontal_wrapped(|ui| {
                if ui
                    .add_enabled(!self.busy, controls::secondary(text.center.rename_device))
                    .clicked()
                {
                    dialog = Some(Dialog::Rename(device.clone(), device.alias.clone()));
                }
                if current {
                    if ui
                        .add_enabled(!self.busy, controls::secondary(text.common.sign_out))
                        .clicked()
                    {
                        dialog = Some(Dialog::SignOut);
                    }
                } else {
                    if ui
                        .add_enabled(
                            !self.busy,
                            controls::secondary(text.center.remove_from_account),
                        )
                        .clicked()
                    {
                        dialog = Some(Dialog::RemoveDevice(device.clone()));
                    }
                    let features = client.feature_catalog();
                    for action in [
                        PowerAction::Wake,
                        PowerAction::Shutdown,
                        PowerAction::Reboot,
                    ] {
                        if (action == PowerAction::Wake || device.policy.power)
                            && action.check(device, &features).is_ok()
                            && ui
                                .add_enabled(!self.busy, controls::secondary(action.label()))
                                .clicked()
                        {
                            dialog = Some(Dialog::Power(device.clone(), action));
                        }
                    }
                }
            });
        }
        if let Some(dialog) = dialog {
            self.center.dialog = Some(dialog);
        }
        if !current
            && device.platform == farwing_protocol::Platform::Windows
            && device.controlled_support
            && devices
                .with_capability(&device.device_id, caps::DIAGNOSTICS)
                .is_ok()
        {
            self.remote_diagnostics(ui, client, device);
        }
        if open_ports {
            if let Some(window) = self.center.ports.get_mut(&device.device_id) {
                window.focus(ui.ctx());
            } else {
                let result = {
                    let _enter = self.runtime.enter();
                    crate::features::port_mapping::ui::PortWindow::open(
                        client.clone(),
                        device.clone(),
                        self.media,
                    )
                };
                match result {
                    Ok(window) => {
                        self.center.ports.insert(device.device_id.clone(), window);
                    }
                    Err(error) => self.show_error(error),
                }
            }
        }
        if let Some(connect) = connect {
            match connect {
                Some(options) => {
                    self.connect_media(client.clone(), device.clone(), options);
                }
                None => self.connect(client.clone(), device.clone()),
            }
        }
    }

    pub(super) fn details_dialog(
        &mut self,
        ctx: &egui::Context,
        client: &Arc<AuthenticatedClient>,
        devices: &DeviceList,
        mut dialog: Dialog,
    ) {
        let text = t();
        let owns = |expected: &DeviceInfo| {
            std::iter::once(&devices.current_device)
                .chain(&devices.devices)
                .any(|d| !d.is_shared() && d.device_id == expected.device_id)
        };
        let answer = match &mut dialog {
            Dialog::Rename(device, name) => modal(
                ctx,
                "linux-rename-device",
                text.center.rename_device,
                DialogIcon::Edit,
                450.0,
                |ui| {
                    device_name(ui, device);
                    ui.add_sized(
                        [ui.available_width(), controls::HEIGHT],
                        controls::singleline(name, controls::HEIGHT)
                            .hint_text(text.center.device_name),
                    );
                    if device.device_id == devices.current_device.device_id {
                        let suggested = client.suggested_device_name();
                        if !suggested.is_empty()
                            && ui
                                .add(controls::secondary(&(text.center.use_short_name)(
                                    &suggested,
                                )))
                                .clicked()
                        {
                            *name = suggested;
                        }
                    }
                    let control = name.chars().any(char::is_control);
                    if control {
                        ui.colored_label(theme::AMBER, text.center.name_control_characters);
                    }
                    actions(
                        ui,
                        text.center.save_name,
                        !self.busy
                            && owns(device)
                            && !name.trim().is_empty()
                            && name.trim() != device.alias
                            && !control,
                        false,
                    )
                },
            ),
            Dialog::RemoveDevice(device) => modal(
                ctx,
                "linux-remove-device",
                text.center.remove_device_title,
                DialogIcon::Warning,
                450.0,
                |ui| {
                    device_name(ui, device);
                    actions(
                        ui,
                        text.common.remove,
                        !self.busy
                            && owns(device)
                            && device.device_id != devices.current_device.device_id,
                        true,
                    )
                },
            ),
            Dialog::Power(device, action) => modal(
                ctx,
                "linux-device-power",
                (text.settings.power_title)(*action),
                DialogIcon::Warning,
                450.0,
                |ui| {
                    device_name(ui, device);
                    ui.label(format!(
                        "{} · {}",
                        device.platform_label(),
                        device.status_label()
                    ));
                    if *action != PowerAction::Wake {
                        ui.colored_label(theme::AMBER, text.settings.power_warning);
                        if device.participant_count() > 0 {
                            ui.label((text.settings.power_people_connected)(
                                device.participant_count(),
                            ));
                        }
                    }
                    let latest = devices
                        .devices
                        .iter()
                        .find(|d| d.device_id == device.device_id);
                    let issue = match latest {
                        Some(latest)
                            if latest.alias != device.alias
                                || latest.platform != device.platform =>
                        {
                            Some(text.session.device_name_or_platform_changed.into())
                        }
                        Some(latest) if latest.participant_count() > device.participant_count() => {
                            Some(text.session.device_new_connection.into())
                        }
                        Some(latest) if !latest.is_shared() => {
                            if *action != PowerAction::Wake && !latest.policy.power {
                                Some(text.session.power_action_unsupported.into())
                            } else {
                                action
                                    .check(latest, &client.feature_catalog())
                                    .err()
                                    .map(|error| error.to_string())
                            }
                        }
                        _ => Some(text.session.device_or_owner_changed.into()),
                    };
                    if let Some(issue) = &issue {
                        ui.colored_label(theme::AMBER, issue);
                    }
                    actions(
                        ui,
                        (text.settings.power_confirm)(*action),
                        !self.busy && issue.is_none(),
                        *action != PowerAction::Wake,
                    )
                },
            ),
            _ => return,
        };
        match (answer, dialog) {
            (None, dialog) => self.center.dialog = Some(dialog),
            (Some(true), Dialog::Rename(device, name)) => {
                self.request_center(move |client| async move {
                    let (alias, message) = client
                        .rename_owned_device(&device.device_id, name.trim())
                        .await?;
                    Ok(Update::Renamed(device.device_id, alias, message))
                })
            }
            (Some(true), Dialog::RemoveDevice(device)) => {
                self.request_center(move |client| async move {
                    let message = client.remove_account_device(&device.device_id).await?;
                    Ok(Update::Removed(device.device_id, message))
                })
            }
            (Some(true), Dialog::Power(device, action)) => {
                self.request_center(move |client| async move {
                    let mut receipt = client.power_owned_device(&device, action, || {}).await?;
                    // The server can route Wake-on-LAN through helpers. This build
                    // does not send a local broadcast, even if the target shares its LAN.
                    receipt.current_device_in_same_network = None;
                    Ok(Update::Message(receipt.summary(action)))
                })
            }
            _ => {}
        }
    }

    fn remote_diagnostics(
        &mut self,
        ui: &mut egui::Ui,
        client: &Arc<AuthenticatedClient>,
        device: &DeviceInfo,
    ) {
        use crate::diagnostics::remote::service;
        let text = t();
        let controller = client.device_id();
        let target = &device.device_id;
        let snapshot = service::snapshot(&controller, target);
        device_list::heading(ui, text.settings.remote_diagnostics, None, true, |_| {});
        controls::card().show(ui, |ui| {
            ui.set_width(ui.available_width());
            if let Some(state) = &snapshot {
                if state.busy {
                    ui.label(&state.progress.stage);
                    if state.progress.total > 0 {
                        ui.add(egui::ProgressBar::new(state.progress.fraction()).show_percentage());
                    } else {
                        ui.spinner();
                    }
                    if ui
                        .add(controls::secondary(text.settings.cancel_get))
                        .clicked()
                    {
                        service::cancel(&controller, target);
                    }
                    ui.ctx()
                        .request_repaint_after(std::time::Duration::from_millis(100));
                    return;
                }
                if let Some(path) = &state.path {
                    ui.colored_label(
                        if state.warnings > 0 {
                            theme::AMBER
                        } else {
                            theme::GREEN
                        },
                        if state.warnings > 0 {
                            text.settings.saved_partial
                        } else {
                            text.settings.diagnostics_saved
                        },
                    );
                    ui.add(egui::Label::new(path.display().to_string()).selectable(true));
                    if ui.add(controls::secondary(text.app.open_folder)).clicked()
                        && let Err(error) = crate::diagnostics::bundle::open_folder(path)
                    {
                        self.show_error(error);
                    }
                } else if let Some(error) = &state.error {
                    ui.colored_label(theme::RED, error);
                } else if state.cancelled {
                    ui.label(text.settings.get_cancelled);
                }
            }
            let available = !self.busy
                && device.is_connected()
                && device.controlled_support
                && device.controllable;
            if ui
                .add_enabled(
                    available,
                    controls::secondary(if snapshot.is_some() {
                        text.settings.get_again
                    } else {
                        text.settings.get_diagnostics
                    }),
                )
                .on_disabled_hover_text(text.center.offline_or_not_allowed)
                .clicked()
            {
                let _enter = self.runtime.enter();
                service::start(
                    client.clone(),
                    device.clone(),
                    ConnectionMediaOptions::default(),
                );
                ui.ctx()
                    .request_repaint_after(std::time::Duration::from_millis(100));
            }
        });
    }
}

fn device_name(ui: &mut egui::Ui, device: &DeviceInfo) {
    ui.label(RichText::new(alias(device)).strong());
    ui.label(
        RichText::new(device_id_text(&device.device_id))
            .monospace()
            .color(theme::MUTED),
    );
}
