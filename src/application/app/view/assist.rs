//! Connecting through the server: the access code prompt, notices, and
//! editing saved records (`docs/access-model.md` §2.4).
use super::*;
use crate::account::assist::{SavedKind, normalize_connect_id};
use crate::application::app::assist::DeletePrompt;

impl DeviceCenterApp {
    /// Notices of a connection through the server being checked, and of
    /// record changes.
    pub(super) fn assist_notices(&mut self, ui: &mut egui::Ui) {
        if let Some(deadline) = self.assist.message_until {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                self.assist.message.clear();
                self.assist.message_until = None;
            } else {
                ui.ctx().request_repaint_after(remaining);
            }
        }
        if let Some((_, deadline)) = &self.assist.connect_error {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                self.assist.connect_error = None;
            } else {
                ui.ctx().request_repaint_after(remaining);
            }
        }
        crate::ui::controls::observe_notice(
            ui.ctx(),
            "assist-connect-error",
            t().center.connection,
            crate::ui::controls::DialogIcon::Warning,
            self.assist
                .connect_error
                .as_ref()
                .map(|(message, _)| message.as_str()),
        );
        if self.assist.querying {
            if crate::ui::controls::observe_notice_action(
                ui.ctx(),
                "assist-query",
                t().center.contacting,
                crate::ui::controls::DialogIcon::Waiting,
                Some(&self.assist.message),
                t().center.cancel_connection,
            )
            .is_some()
            {
                self.cancel_assist_check();
            }
        } else {
            crate::ui::controls::clear_notice(ui.ctx(), "assist-query");
            if !self.assist.busy && !self.assist.message.is_empty() {
                crate::ui::controls::notice(
                    ui.ctx(),
                    "assist-result",
                    t().center.connection,
                    crate::ui::controls::DialogIcon::Info,
                    std::mem::take(&mut self.assist.message),
                );
                self.assist.message_until = None;
            }
        }
    }

    pub(super) fn assist_dialogs(&mut self, ctx: &egui::Context) {
        if let Some(message) = &self.assist.error_dialog {
            let mut close = false;
            let response = egui::Modal::new(egui::Id::new("assist-error"))
                .frame(dialog_frame())
                .show(ctx, |ui| {
                    ui.set_width(400.0);
                    close = crate::ui::controls::dialog_header(
                        ui,
                        t().center.not_completed,
                        crate::ui::controls::DialogIcon::Error,
                        true,
                    );
                    ui.add(egui::Label::new(message).wrap());
                    close |= crate::ui::controls::dialog_actions(
                        ui,
                        Some(crate::ui::controls::DialogAction::new(t().common.got_it)),
                        None,
                    )
                    .0;
                });
            if close || response.should_close() {
                self.assist.error_dialog = None;
            }
            return;
        }
        if let Some(mut pending) = self.assist.password_prompt.take() {
            let mut submit = false;
            let mut cancel = false;
            let response = egui::Modal::new(egui::Id::new("assist-password"))
                .frame(dialog_frame())
                .show(ctx, |ui| {
                    ui.set_width(360.0);
                    cancel = crate::ui::controls::dialog_header(
                        ui,
                        t().center.enter_access_code,
                        crate::ui::controls::DialogIcon::Info,
                        true,
                    );
                    ui.label(
                        RichText::new((t().center.needs_access_code)(
                            &crate::session::controller::assist_target_label(&pending.id),
                        ))
                        .color(MUTED),
                    );
                    ui.add_space(18.0);
                    let input = ui.add_sized(
                        [360.0, theme::CONTROL_HEIGHT],
                        singleline_input(&mut self.assist.code)
                            .hint_text(t().center.access_code)
                            .char_limit(256),
                    );
                    if pending.focus {
                        input.request_focus();
                        pending.focus = false;
                    }
                    if input.has_focus()
                        && ui.input(|i| i.key_pressed(egui::Key::Enter))
                        && !self.assist.code.is_empty()
                    {
                        submit = true;
                    }
                    let (accept, dismiss) = crate::ui::controls::dialog_actions(
                        ui,
                        Some(
                            crate::ui::controls::DialogAction::new(t().common.connect)
                                .enabled(!self.assist.code.is_empty()),
                        ),
                        Some(t().common.cancel),
                    );
                    submit |= accept;
                    cancel |= dismiss;
                });
            if submit {
                let code = std::mem::take(&mut self.assist.code);
                self.launch_assist(pending, code);
            } else if cancel || response.should_close() {
                self.assist.code.clear();
            } else {
                self.assist.password_prompt = Some(pending);
            }
        }
        if let Some(mut edit) = self.assist.favorite_editor.take() {
            let mut submit = false;
            let mut cancel = false;
            let response = egui::Modal::new(egui::Id::new("assist-favorite-editor"))
                .frame(dialog_frame())
                .show(ctx, |ui| {
                    ui.set_width(380.0);
                    cancel = crate::ui::controls::dialog_header(
                        ui,
                        if edit.editing {
                            t().center.edit_favorite
                        } else {
                            t().center.add_favorite
                        },
                        crate::ui::controls::DialogIcon::Edit,
                        true,
                    );
                    ui.label(RichText::new(t().center.short_number).color(MUTED));
                    ui.add_enabled_ui(!edit.editing, |ui| {
                        if ui
                            .add_sized(
                                [380.0, theme::CONTROL_HEIGHT],
                                singleline_input(&mut edit.id).char_limit(24),
                            )
                            .changed()
                        {
                            edit.code.clear();
                            edit.code_changed = false;
                        }
                    });
                    ui.add_space(10.0);
                    ui.label(RichText::new(t().center.note).color(MUTED));
                    ui.add_sized(
                        [380.0, theme::CONTROL_HEIGHT],
                        singleline_input(&mut edit.remark)
                            .hint_text(t().common.optional)
                            .char_limit(128),
                    );
                    ui.add_space(10.0);
                    ui.label(RichText::new(t().center.access_code).color(MUTED));
                    edit.code_changed |= ui
                        .add_sized(
                            [380.0, theme::CONTROL_HEIGHT],
                            singleline_input(&mut edit.code)
                                .hint_text(t().common.optional)
                                .char_limit(256),
                        )
                        .changed();
                    let (accept, dismiss) =
                        crate::ui::controls::dialog_actions(
                            ui,
                            Some(crate::ui::controls::DialogAction::new(t().common.save).enabled(
                                normalize_connect_id(&edit.id).is_ok() && !self.assist.busy,
                            )),
                            Some(t().common.cancel),
                        );
                    submit = accept;
                    cancel |= dismiss;
                });
            if submit {
                self.request_assist_operation(AssistOperation::Save {
                    id: edit.id,
                    remark: edit.remark,
                    code: edit.code_changed.then_some(edit.code),
                });
            } else if !cancel && !response.should_close() {
                self.assist.favorite_editor = Some(edit);
            }
        }
        if let Some(prompt) = self.assist.delete_prompt.take() {
            let mut submit = false;
            let mut cancel = false;
            let response = egui::Modal::new(egui::Id::new("assist-delete-record"))
                .frame(dialog_frame())
                .show(ctx, |ui| {
                    ui.set_width(380.0);
                    let title = match &prompt {
                        DeletePrompt::Recent => t().center.clear_recent_title,
                        DeletePrompt::Device(SavedKind::Recent, _) => t().center.remove_record_title,
                        DeletePrompt::Device(SavedKind::Favorites, _) => {
                            t().center.remove_favorite_title
                        }
                    };
                    cancel = crate::ui::controls::dialog_header(
                        ui,
                        title,
                        crate::ui::controls::DialogIcon::Warning,
                        true,
                    );
                    if let DeletePrompt::Device(_, item) = &prompt {
                        ui.label(item.title());
                        ui.label(RichText::new(&item.connect_id).color(MUTED));
                    }
                    let (accept, dismiss) = crate::ui::controls::dialog_actions(
                        ui,
                        Some(
                            crate::ui::controls::DialogAction::new(t().common.ok)
                                .enabled(!self.assist.busy)
                                .danger(true),
                        ),
                        Some(t().common.cancel),
                    );
                    submit = accept;
                    cancel |= dismiss;
                });
            if submit {
                let operation = match prompt {
                    DeletePrompt::Recent => AssistOperation::ClearRecent,
                    DeletePrompt::Device(kind, item) => AssistOperation::Delete {
                        kind,
                        publisher_id: item.publisher_device_id,
                    },
                };
                self.request_assist_operation(operation);
            } else if !cancel && !response.should_close() {
                self.assist.delete_prompt = Some(prompt);
            }
        }
    }
}
