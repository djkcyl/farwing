use super::*;
use crate::i18n::t;

impl DeviceCenterApp {
    pub(super) fn fetch_diagnostics(&mut self, device: crate::account::api::DeviceInfo) {
        if self.logout_pending || self.mutation_pending {
            return;
        }
        if self
            .worker
            .commands
            .send(GuiCommand::Diagnostics {
                generation: self.login_generation,
                device,
                options: self.media,
            })
            .is_err()
        {
            self.status = StatusMessage::error(t().settings.get_diagnostics_failed);
        }
    }
}
pub(super) fn button_label(snapshot: Option<&crate::diagnostics::remote::Snapshot>) -> String {
    use crate::diagnostics::remote::Phase;
    let text = &t().settings;
    let Some(state) = snapshot else {
        return text.get_diagnostics.into();
    };
    if state.busy {
        let (busy, percent) = match state.phase {
            Phase::Connecting => return text.connecting.into(),
            Phase::Preparing => (text.packing, text.packing_percent),
            Phase::Downloading => (text.downloading, text.downloading_percent),
            Phase::Saving => return text.saving.into(),
        };
        return if state.progress.total > 0 {
            percent(state.progress.fraction() * 100.0)
        } else {
            busy.into()
        };
    }
    if state.path.is_some() {
        text.diagnostics_saved.into()
    } else if state.error.is_some() {
        text.get_failed.into()
    } else {
        text.get_diagnostics.into()
    }
}

pub(super) fn popup(
    response: &egui::Response,
    controller: &str,
    target: &str,
    snapshot: Option<&crate::diagnostics::remote::Snapshot>,
    can_start: bool,
) -> bool {
    let mut start = response.clicked() && can_start && snapshot.is_none_or(|s| s.cancelled);
    let text = &t().settings;
    egui::Popup::from_toggle_button_response(response)
        .close_behavior(egui::PopupCloseBehavior::CloseOnClickOutside)
        .show(|ui| {
            ui.set_width(theme::DIAGNOSTIC_POPUP_WIDTH);
            ui.spacing_mut().item_spacing = egui::vec2(8.0, 10.0);
            ui.horizontal(|ui| {
                ui.label(
                    egui::RichText::new(text.remote_diagnostics)
                        .size(theme::SECTION)
                        .strong(),
                );
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if crate::ui::controls::close_button(
                        ui,
                        text.collapse_panel,
                        crate::ui::controls::COMPACT_HEIGHT,
                    )
                    .clicked()
                    {
                        ui.close();
                    }
                });
            });
            ui.separator();
            match snapshot {
                Some(state) if state.busy => {
                    let p = &state.progress;
                    if p.total > 0 {
                        ui.label(&p.stage);
                        ui.add(egui::ProgressBar::new(p.fraction()).show_percentage());
                    } else {
                        ui.horizontal(|ui| {
                            ui.add(egui::Spinner::new().size(16.0));
                            ui.label(&p.stage);
                        });
                    }
                    if ui.button(text.cancel_get).clicked() {
                        crate::diagnostics::remote::service::cancel(controller, target);
                    }
                    ui.ctx().request_repaint_after(Duration::from_millis(100));
                }
                Some(state) if state.path.is_some() => {
                    ui.label(
                        egui::RichText::new(if state.warnings > 0 {
                            text.saved_partial
                        } else {
                            text.diagnostics_saved
                        })
                        .color(if state.warnings > 0 {
                            theme::AMBER
                        } else {
                            theme::GREEN
                        }),
                    );
                    let path = state.path.as_ref().unwrap();
                    ui.add(
                        egui::Label::new(
                            egui::RichText::new(
                                path.file_name().unwrap_or_default().to_string_lossy(),
                            )
                            .size(theme::SMALL)
                            .color(theme::MUTED),
                        )
                        .truncate(),
                    )
                    .on_hover_text(path.display().to_string());
                    ui.horizontal(|ui| {
                        if ui.button(t().app.open_folder).clicked() {
                            if let Err(error) = crate::diagnostics::bundle::open_folder(path) {
                                tracing::warn!(%error,"open diagnostic folder failed");
                                crate::ui::controls::notice(
                                    ui.ctx(),
                                    ("diagnostic-folder", target),
                                    text.cant_open_folder,
                                    crate::ui::controls::DialogIcon::Error,
                                    path.display().to_string(),
                                );
                            }
                        }
                        start |= ui
                            .add_enabled(can_start, egui::Button::new(text.get_again))
                            .clicked();
                    });
                }
                Some(state) if state.error.is_some() => {
                    ui.label(
                        egui::RichText::new(state.error.as_deref().unwrap()).color(theme::AMBER),
                    );
                    start |= ui
                        .add_enabled(can_start, egui::Button::new(t().common.retry))
                        .clicked();
                }
                Some(_) => {
                    ui.label(egui::RichText::new(text.get_cancelled).color(theme::MUTED));
                    start |= ui
                        .add_enabled(can_start, egui::Button::new(text.get_again))
                        .clicked();
                }
                None => {
                    ui.horizontal(|ui| {
                        ui.add(egui::Spinner::new().size(16.0));
                        ui.label(text.preparing_connection);
                    });
                }
            }
        });
    start
}
