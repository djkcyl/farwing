//! One product installation entry; service and driver backends remain separate.
use super::*;
use crate::platform::windows::components::{self as install, Kind, Operation, Status};
struct ResultState {
    status: Result<Status, String>,
    components: Vec<(Kind, Result<Status, String>)>,
    operation: Option<(Kind, Operation, Result<bool, String>)>,
}
/// The background service as it matters to others connecting.
pub(super) struct ServiceReach {
    pub state: &'static str,
    pub color: Color32,
    /// What to do about it, if anything.
    pub action: Option<&'static str>,
}

#[derive(Default)]
pub(in crate::application::app) struct Manager {
    status: Option<Status>,
    components: Vec<(Kind, Result<Status, String>)>,
    pending: Option<std::sync::mpsc::Receiver<ResultState>>,
    confirm: Option<Operation>,
    audio_confirm: Option<Operation>,
    error: Option<String>,
    reboot: bool,
    allow_sas: bool,
    removal: install::RemovalOptions,
    offered: bool,
    completed: bool,
    handoff: bool,
}
impl Manager {
    fn work(&mut self, operation: Option<Operation>, ctx: egui::Context) {
        self.work_kind(Kind::Suite, operation, ctx);
    }
    fn work_kind(&mut self, kind: Kind, operation: Option<Operation>, ctx: egui::Context) {
        if self.pending.is_some() {
            return;
        }
        let (tx, rx) = std::sync::mpsc::channel();
        self.pending = Some(rx);
        self.error = None;
        let allow_sas = self.allow_sas;
        let removal = self.removal;
        std::thread::spawn(move || {
            let operation = operation.map(|op| {
                (
                    kind,
                    op,
                    install::request(
                        kind,
                        op,
                        kind == Kind::Suite && op == Operation::Install && allow_sas,
                        if kind == Kind::Suite && op == Operation::Uninstall {
                            removal
                        } else {
                            Default::default()
                        },
                    )
                    .map_err(|e| format!("{e:#}")),
                )
            });
            let status = install::status(Kind::Suite).map_err(|e| format!("{e:#}"));
            let components = Kind::ALL
                .into_iter()
                .map(|kind| (kind, install::status(kind).map_err(|e| format!("{e:#}"))))
                .collect();
            let _ = tx.send(ResultState {
                status,
                components,
                operation,
            });
            ctx.request_repaint();
        });
    }
    fn poll(&mut self) {
        let Some(rx) = &self.pending else { return };
        match rx.try_recv() {
            Ok(result) => {
                self.pending = None;
                self.components = result.components;
                match result.status {
                    Ok(status) => self.status = Some(status),
                    Err(error) => self.error = Some(error),
                }
                if let Some((kind, operation, result)) = result.operation {
                    match result {
                        Ok(reboot) => {
                            self.reboot |= reboot;
                            self.completed = true;
                            self.handoff =
                                kind == Kind::Suite && operation == Operation::Install && !reboot;
                        }
                        Err(error) => self.error = Some(error),
                    }
                }
            }
            Err(std::sync::mpsc::TryRecvError::Empty) => (),
            Err(_) => {
                self.pending = None;
                self.error = Some(t().center.install_interrupted.into());
            }
        }
    }
    pub(in crate::application::app) fn busy(&self) -> bool {
        self.pending.is_some()
    }
    pub(super) fn button(&mut self, ui: &mut egui::Ui, active: bool) {
        // Machine-wide components (service, drivers, installed program)
        // belong to the default profile; other profiles run portable.
        if !crate::product::is_default_profile() {
            return;
        }
        self.poll();
        if self.status.is_none() && self.error.is_none() && self.pending.is_none() {
            self.work(None, ui.ctx().clone());
        }
        if !self.reboot && self.status.as_ref().is_none_or(|s| s.installed && s.ready) {
            return;
        }
        let installed = self.status.as_ref().is_some_and(|s| s.installed);
        let text = &t().center;
        let label = if self.pending.is_some() {
            text.working
        } else if self.reboot {
            text.restart_required
        } else if installed {
            text.update_service
        } else {
            text.install_service
        };
        let actionable = self
            .status
            .as_ref()
            .is_some_and(|s| !s.installed || !s.ready);
        let response = ui.add_enabled(
            actionable && !active && !self.reboot && self.pending.is_none(),
            crate::ui::controls::centered_primary(label).min_size(vec2(ui.available_width(), 30.0)),
        );
        if response.clicked() {
            self.confirm = Some(Operation::Install);
        }
        response.on_hover_text(if active {
            text.being_controlled
        } else {
            self.status
                .as_ref()
                .map_or(text.checking_service, |s| s.label.as_str())
        });
    }
    /// What the background service means for reaching this device, and the
    /// action it calls for (default profile only).
    pub(super) fn reach(&self) -> Option<ServiceReach> {
        if !crate::product::is_default_profile() {
            return None;
        }
        let status = self.status.as_ref()?;
        let text = &t().center;
        Some(if status.ready {
            ServiceReach {
                state: text.installed,
                color: GREEN,
                action: None,
            }
        } else if status.installed {
            ServiceReach {
                state: text.update_required,
                color: AMBER,
                action: Some(text.update_service),
            }
        } else {
            ServiceReach {
                state: text.not_installed,
                color: MUTED,
                action: Some(text.install_service),
            }
        })
    }
    pub(super) fn request_install(&mut self) {
        if self.pending.is_none() && !self.reboot {
            self.confirm = Some(Operation::Install);
        }
    }
    pub(super) fn management(&mut self, ui: &mut egui::Ui, active: bool) {
        let text = &t().center;
        if !crate::product::is_default_profile() {
            section(ui, text.service_management);
            ui.label((text.portable_profile)(crate::product::profile()));
            return;
        }
        self.poll();
        section(ui, text.service_management);
        let label = self
            .status
            .as_ref()
            .map_or(text.checking, |s| s.label.as_str())
            .to_owned();
        let removable = self.status.as_ref().is_some_and(|s| s.removable);
        let installable = self.status.as_ref().is_some_and(|s| !s.installed);
        form_row(ui, text.resident_service, &label, |ui| {
            let busy = if active {
                text.being_controlled
            } else {
                text.working
            };
            if installable
                && ui
                    .add_enabled(
                        !active && !self.reboot && self.pending.is_none(),
                        crate::ui::controls::primary(text.install_service),
                    )
                    .on_disabled_hover_text(busy)
                    .clicked()
            {
                self.confirm = Some(Operation::Install);
            }
            if !removable {
                return;
            }
            let idle = !active && !self.reboot && self.pending.is_none();
            if ui
                .add_enabled(idle, egui::Button::new(text.repair))
                .on_disabled_hover_text(busy)
                .clicked()
            {
                self.confirm = Some(Operation::Install);
            }
            if ui
                .add_enabled(idle, egui::Button::new(text.uninstall_service))
                .on_disabled_hover_text(busy)
                .clicked()
            {
                self.removal = Default::default();
                self.confirm = Some(Operation::Uninstall);
            }
        });
        self.audio_management(ui, active);
    }
    fn audio_management(&mut self, ui: &mut egui::Ui, active: bool) {
        let audio_result = self
            .components
            .iter()
            .find(|(kind, _)| *kind == Kind::AudioDriver)
            .map(|(_, status)| status);
        let audio = audio_result.and_then(|status| status.as_ref().ok());
        let installed = audio.is_some_and(|s| s.installed);
        let removable = audio.is_some_and(|s| s.removable);
        let text = &t().center;
        let label = match audio_result {
            Some(Ok(status)) => status.label.clone(),
            Some(Err(error)) => (text.check_failed)(error),
            None => text.checking.into(),
        };
        form_row(ui, text.virtual_audio, &label, |ui| {
            if !audio.is_some_and(|status| status.ready)
                && ui
                    .add_enabled(
                        !active && self.pending.is_none(),
                        egui::Button::new(if installed {
                            text.update_driver
                        } else {
                            text.install_driver
                        }),
                    )
                    .clicked()
            {
                self.audio_confirm = Some(Operation::Install);
            }
            if ui
                .add_enabled(
                    !active && self.pending.is_none() && removable,
                    egui::Button::new(text.uninstall_driver),
                )
                .clicked()
            {
                self.audio_confirm = Some(Operation::Uninstall);
            }
        });
    }
    fn audio_dialog(&mut self, ctx: &egui::Context, active: bool) {
        if let Some(operation) = self.audio_confirm {
            let text = &t().center;
            let title = if operation == Operation::Install {
                if self.components.iter().any(|(kind, status)| {
                    *kind == Kind::AudioDriver && status.as_ref().is_ok_and(|s| s.installed)
                }) {
                    text.update_virtual_audio
                } else {
                    text.install_virtual_audio
                }
            } else {
                text.uninstall_virtual_audio
            };
            let mut accepted = false;
            let mut dismissed = false;
            let result = egui::Modal::new(egui::Id::new("audio-driver-confirm"))
                .frame(crate::ui::controls::dialog_frame()).show(ctx, |ui| {
                    ui.set_width(460.0);
                    dismissed = crate::ui::controls::dialog_header(ui, title, crate::ui::controls::DialogIcon::Warning, true);
                    if operation == Operation::Uninstall {
                        ui.label(text.removes_virtual_audio);
                    }
                    let (yes, close) = crate::ui::controls::dialog_actions(ui,
                        Some(crate::ui::controls::DialogAction::new(title).enabled(!active && self.pending.is_none())), Some(t().common.cancel));
                    accepted = yes;
                    dismissed |= close;
                });
            if accepted {
                self.audio_confirm = None;
                self.work_kind(Kind::AudioDriver, Some(operation), ctx.clone());
            } else if dismissed || result.should_close() {
                self.audio_confirm = None;
            }
        }
    }
    fn dialogs(&mut self, ctx: &egui::Context, active: bool, home: bool) {
        self.audio_dialog(ctx, active);
        if home
            && crate::product::is_default_profile()
            && !self.offered
            && !active
            && self.pending.is_none()
            && self.confirm.is_none()
            && self.audio_confirm.is_none()
            && self.error.is_none()
            && self.status.is_some()
            && ctx.memory(|m| m.top_modal_layer().is_none())
        {
            self.offered = true;
            let remember = (|| -> anyhow::Result<bool> {
                let path = crate::product::local_data_dir()
                    .context(t().center.local_settings_unavailable)?
                    .join("resident-install-prompt.seen");
                let seen = path.exists();
                if !seen {
                    std::fs::create_dir_all(path.parent().unwrap())?;
                    std::fs::write(path, b"1")?;
                }
                Ok(seen)
            })();
            match remember {
                Ok(false) if self.status.as_ref().is_some_and(|s| !s.installed) => {
                    self.confirm = Some(Operation::Install)
                }
                Err(e) => self.error = Some(e.to_string()),
                _ => (),
            }
        }
        self.poll();
        crate::ui::controls::observe_notice(
            ctx,
            "component-error",
            t().center.service_management,
            crate::ui::controls::DialogIcon::Error,
            self.error.as_deref(),
        );
        if let Some(operation) = self.confirm {
            let mut accepted = false;
            let mut dismiss = false;
            let result = egui::Modal::new(egui::Id::new("component-confirm")).frame(crate::ui::controls::dialog_frame()).show(ctx, |ui| {
                ui.set_width(480.0);
                let text = &t().center;
                let verb = if operation == Operation::Uninstall { text.uninstall_service } else if self.status.as_ref().is_some_and(|s| s.installed) { text.update_service } else { text.install_service };
                dismiss = crate::ui::controls::dialog_header(ui,verb,crate::ui::controls::DialogIcon::Warning,true);
                if operation == Operation::Install {
                    let drivers = crate::platform::windows::input::install::bundled() || crate::platform::windows::display::install::bundled();
                    ui.checkbox(&mut self.allow_sas, text.allow_sas);
                    if drivers { ui.collapsing(text.drivers_and_certificates, |ui| {
                        if let Some((subject, thumbprint, _)) = Kind::InputDriver.certificate() { ui.label(subject); ui.label(egui::RichText::new(thumbprint).monospace().small()); }
                        ui.label(text.certificate_stores);
                        ui.hyperlink_to(text.driver_source, "https://github.com/SudoMaker/SudoVDA");
                    }); }
                } else {
                    ui.label(text.stops_startup_access);
                    ui.checkbox(&mut self.removal.remove_display_driver, text.also_remove_display_driver);
                    ui.checkbox(&mut self.removal.remove_audio_driver, text.also_remove_audio_driver);
                }
                for (kind,status) in &self.components { ui.horizontal(|ui| { ui.label(kind.label()); ui.label(match status { Ok(s) => s.label.as_str(), Err(e) => e.as_str() }); }); }
                let (yes, close) = crate::ui::controls::dialog_actions(ui,Some(crate::ui::controls::DialogAction::new(verb).enabled(!active && self.pending.is_none())),Some(t().common.cancel)); accepted = yes; dismiss |= close;
            });
            if accepted {
                self.confirm = None;
                self.work(Some(operation), ctx.clone());
            } else if dismiss || result.should_close() {
                self.confirm = None;
            }
        }
        if self.pending.is_some() {
            ctx.request_repaint_after(Duration::from_millis(250));
        }
    }
}
impl DeviceCenterApp {
    pub(in crate::application::app) fn component_dialogs(&mut self, ctx: &egui::Context) {
        self.center_ui.components.poll();
        if std::mem::take(&mut self.center_ui.components.handoff) {
            crate::application::schedule_installed_handoff();
            self.exit_requested = true;
            self.exit_ready = true;
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        }
        if self.exit_requested {
            return;
        }
        let _ = std::mem::take(&mut self.center_ui.components.completed);
        if self.logout_pending {
            self.center_ui.components.confirm = None;
            self.center_ui.components.audio_confirm = None;
            return;
        }
        let active = self
            .host
            .as_ref()
            .is_some_and(|h| h.status().session_active);
        self.center_ui.components.dialogs(ctx, active, self.center_ui.page == Page::Devices);
    }
}
