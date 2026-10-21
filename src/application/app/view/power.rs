use super::*;
use crate::account::power::PowerAction;
use crate::i18n::t;

pub(super) struct PowerConfirmation {
    device: DeviceInfo,
    action: PowerAction,
    generation: u64,
}

impl DeviceCenterApp {
    pub(super) fn begin_power(&mut self, device: DeviceInfo, action: PowerAction) {
        if let Err(error) = self.power_available(&device, action) {
            self.status = StatusMessage::warning(error.to_string());
            return;
        }
        self.center_ui.power = Some(PowerConfirmation {
            device,
            action,
            generation: self.login_generation,
        });
    }

    pub(super) fn power_confirmation(&mut self, ctx: &egui::Context) {
        let Some(confirm) = self.center_ui.power.take() else {
            return;
        };
        if confirm.generation != self.login_generation || self.logout_pending {
            return;
        }
        let mut commit = false;
        let mut cancel = false;
        let issue = self.power_available(&confirm.device, confirm.action).err();
        let text = &t().settings;
        let response = egui::Modal::new(egui::Id::new("device-power-confirmation"))
            .frame(dialog_frame()).show(ctx, |ui| {
                ui.set_width(450.0_f32.min(ctx.content_rect().width() - 60.0));
                cancel = crate::ui::controls::dialog_header(ui, (text.power_title)(confirm.action), crate::ui::controls::DialogIcon::Warning, true);
                ui.add(egui::Label::new(RichText::new(display_alias(&confirm.device)).strong()).wrap());
                ui.label(RichText::new(&confirm.device.device_id).monospace().color(MUTED));
                ui.label(format!("{} · {}", confirm.device.platform_label(), confirm.device.status_label()));
                ui.add_space(12.0);
                if confirm.action != PowerAction::Wake {
                    ui.colored_label(AMBER, text.power_warning);
                    if confirm.device.participant_count() > 0 {
                        ui.label((text.power_people_connected)(confirm.device.participant_count()));
                    }
                }
                if let Some(issue) = &issue { ui.colored_label(AMBER, issue.to_string()); }
                let (accept, dismiss) = crate::ui::controls::dialog_actions(ui, Some(crate::ui::controls::DialogAction::new((text.power_confirm)(confirm.action)).enabled(issue.is_none()).danger(confirm.action != PowerAction::Wake)), Some(t().common.cancel));
                commit = accept;
                cancel |= dismiss;
            });
        if commit {
            self.queue_mutation(DeviceMutation::Power {
                device: confirm.device,
                action: confirm.action,
            });
        } else if !cancel && !response.should_close() {
            self.center_ui.power = Some(confirm);
        }
    }

    pub(super) fn power_results(&mut self, ui: &mut egui::Ui) {
        let mut dismiss = Vec::new();
        let text = &t().settings;
        for (id, progress) in &self.power_progress {
            let message = if progress.waiting {
                format!(
                    "{} · {}\n{}",
                    display_alias(&progress.device),
                    progress.message,
                    (text.power_waited)(progress.elapsed())
                )
            } else {
                format!("{} · {}", display_alias(&progress.device), progress.message)
            };
            if crate::ui::controls::observe_notice_action(
                ui.ctx(),
                ("device-power-result", id),
                text.power_notice_title,
                if progress.waiting {
                    crate::ui::controls::DialogIcon::Waiting
                } else {
                    crate::ui::controls::DialogIcon::Info
                },
                Some(&message),
                if progress.waiting {
                    text.stop_waiting
                } else {
                    t().common.got_it
                },
            )
            .is_some()
            {
                dismiss.push(id.clone());
            }
        }
        for id in dismiss {
            self.power_progress.remove(&id);
            crate::ui::controls::clear_notice(ui.ctx(), ("device-power-result", &id));
        }
    }
}
