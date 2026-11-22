use super::*;
use crate::account::assist::{SavedKind, normalize_connect_id};
use crate::application::app::assist::{DeletePrompt, FavoriteEditor};
use crate::application::app::assist_link;

impl DeviceCenterApp {
    /// 处理系统按 `uuremote://` 协议注册拉起的协助链接：切到「开始协助」页、
    /// 填好设备 ID 和验证码，然后走与手动输入完全一样的连接流程。界面还在登录、
    /// 登出或别的写操作时先排队；未登录就只填内容，不代替用户按下连接。
    pub(in crate::application::app) fn tick_assist_link(&mut self) {
        let settled = self.assist.password_prompt.is_none()
            && !self.assist.busy
            && !self.logout_pending
            && !self.server_pending
            && !self.login_running
            && !self.login_restoring
            && !self.mutation_pending
            && !self.exit_requested;
        let Some(activation) = assist_link::take(settled) else {
            return;
        };
        let link = match activation {
            assist_link::Activation::Failed(message) => {
                // 登录页不画协助弹窗，未登录时把原因留在登录页上。
                if self.needs_login() {
                    self.login_error = Some(message);
                } else {
                    self.assist.fail(message);
                }
                return;
            }
            assist_link::Activation::Link(link) => link,
        };
        self.center_ui.page = Page::Assist;
        if self.needs_login() {
            self.assist.connect_id.clone_from(&link.connect_id);
            self.assist.direct_code.clone_from(&link.code);
            self.assist.code.clear();
            self.assist.message = "协助链接已填入，登录后点「连接」开始".into();
            self.assist.message_until = None;
            return;
        }
        self.request_assist_connect(link.connect_id, link.code);
    }

    pub(super) fn assist_page(&mut self, ui: &mut egui::Ui, favorites: bool) {
        if let Some(deadline) = self.assist.message_until {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                self.assist.message.clear();
                self.assist.message_until = None;
            } else {
                ui.ctx().request_repaint_after(remaining);
            }
        }
        ui.horizontal(|ui| {
            ui.set_min_height(theme::CONTROL_HEIGHT);
            ui.label(
                RichText::new(if favorites {
                    "收藏设备"
                } else {
                    "远程协助"
                })
                .size(theme::TITLE)
                .strong(),
            );
            ui.with_layout(egui::Layout::right_to_left(Align::Center), |ui| {
                if self.assist.loading {
                    ui.add_sized(
                        [theme::CONTROL_HEIGHT, theme::CONTROL_HEIGHT],
                        egui::Spinner::new().size(18.0),
                    );
                } else if assist_components::glyph(ui, Icon::Refresh, "刷新记录", MUTED).clicked()
                {
                    self.request_assist_refresh();
                }
                if favorites
                    && ui
                        .add_enabled(!self.assist.busy, login_button("添加收藏"))
                        .clicked()
                {
                    self.assist.favorite_editor = Some(FavoriteEditor {
                        id: String::new(),
                        remark: String::new(),
                        editing: false,
                        code: String::new(),
                        code_changed: false,
                    });
                }
                self.active_view(ui);
            });
        });
        self.assist_page_notices(ui, favorites);
        ui.add_space(18.0);
        let available = !self.assist.busy && !self.logout_pending && !self.mutation_pending;
        let mut connect = None;
        if favorites {
            connect = self.assist_favorite_records(ui, available);
        } else {
            crate::ui::controls::page_scroll("assist-page").show(ui, |ui| {
                self.host_assist_controls(ui);
                ui.add_space(16.0);
                crate::ui::controls::section_frame().show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    assist_components::header(
                        ui,
                        "远控伙伴设备",
                        Some("通过伙伴的设备 ID 发起远程连接"),
                        |_| {},
                    );
                    ui.separator();
                    egui::Frame::new()
                        .inner_margin(theme::ASSIST_CARD_MARGIN)
                        .show(ui, |ui| {
                            ui.spacing_mut().item_spacing.y = 8.0;
                            let (changed, requested) = assist_components::connect_inputs(
                                ui,
                                &mut self.assist.connect_id,
                                &mut self.assist.direct_code,
                                available,
                            );
                            if changed {
                                self.assist.connect_error = None;
                            }
                            if requested {
                                connect = Some((
                                    self.assist.connect_id.clone(),
                                    self.assist.direct_code.clone(),
                                ));
                            }
                            ui.add_space(20.0);
                            ui.separator();
                            ui.add_space(8.0);
                            if let Some(recent) = self.assist_recent_records(ui, available) {
                                connect = Some(recent);
                            }
                        });
                });
            });
        }
        if let Some((id, code)) = connect {
            self.request_assist_connect(id, code);
        }
    }

    fn assist_recent_records(
        &mut self,
        ui: &mut egui::Ui,
        available: bool,
    ) -> Option<(String, String)> {
        let mut items = self
            .assist
            .lists
            .as_ref()
            .map(|l| l.recent.clone())
            .unwrap_or_default();
        items.sort_by(|a, b| {
            b.last_connected_at
                .cmp(&a.last_connected_at)
                .then_with(|| a.connect_id.cmp(&b.connect_id))
        });
        ui.horizontal(|ui| {
            ui.set_min_height(theme::CONTROL_HEIGHT);
            ui.label(RichText::new("最近连接").strong());
            ui.with_layout(egui::Layout::right_to_left(Align::Center), |ui| {
                if ui
                    .add_enabled(
                        available && !items.is_empty(),
                        login_button("清空记录").frame(false),
                    )
                    .clicked()
                {
                    self.assist.delete_prompt = Some(DeletePrompt::Recent);
                }
            });
        });
        if items.is_empty() {
            self.assist_records_empty(ui, false);
        }
        let mut connect = None;
        if let Some((index, action)) = assist_components::recent_grid(ui, &items, available) {
            let item = &items[index];
            match action {
                assist_components::RecentAction::Connect => {
                    connect = Some((item.connect_id.clone(), String::new()))
                }
                assist_components::RecentAction::Remove => {
                    self.assist.delete_prompt =
                        Some(DeletePrompt::Device(SavedKind::Recent, item.clone()))
                }
                assist_components::RecentAction::Favorite if item.is_favorite => {
                    self.assist.delete_prompt =
                        Some(DeletePrompt::Device(SavedKind::Favorites, item.clone()))
                }
                assist_components::RecentAction::Favorite => {
                    self.assist.favorite_editor = Some(FavoriteEditor {
                        id: item.connect_id.clone(),
                        remark: item.remark.clone(),
                        editing: false,
                        code: item.saved_code.clone(),
                        code_changed: false,
                    })
                }
            }
        }
        connect
    }

    fn assist_records_empty(&self, ui: &mut egui::Ui, favorites: bool) {
        ui.label(
            RichText::new(if self.assist.loading && self.assist.lists.is_none() {
                "正在读取记录…"
            } else if self.assist.lists.is_none() && self.assist.last_list_error.is_some() {
                "记录读取失败，请刷新重试"
            } else if favorites {
                "暂无收藏"
            } else {
                "暂无最近连接"
            })
            .color(MUTED),
        );
    }

    fn assist_favorite_records(
        &mut self,
        ui: &mut egui::Ui,
        available: bool,
    ) -> Option<(String, String)> {
        let mut items = self
            .assist
            .lists
            .as_ref()
            .map(|l| l.favorites.clone())
            .unwrap_or_default();
        items.sort_by(|a, b| {
            b.favorited_at
                .cmp(&a.favorited_at)
                .then_with(|| a.connect_id.cmp(&b.connect_id))
        });
        if items.is_empty() {
            self.assist_records_empty(ui, true);
        }
        let mut connect = None;
        crate::ui::controls::page_scroll("assist-favorites").show(ui, |ui| {
            for item in &items {
                ui.push_id(&item.publisher_device_id, |ui| {
                    egui::Frame::new()
                        .inner_margin(egui::Margin::symmetric(12, 10))
                        .show(ui, |ui| {
                            ui.horizontal(|ui| {
                                let (icon, _) =
                                    ui.allocate_exact_size(vec2(28.0, 42.0), Sense::hover());
                                paint_icon(ui.painter(), icon, Icon::Monitor, MUTED);
                                ui.allocate_ui_with_layout(
                                    vec2((ui.available_width() - 188.0).max(140.0), 44.0),
                                    egui::Layout::top_down(Align::Min),
                                    |ui| {
                                        ui.add(
                                            egui::Label::new(
                                                RichText::new(item.title()).size(theme::BODY),
                                            )
                                            .truncate(),
                                        );
                                        ui.label(
                                            RichText::new(assist_components::grouped_id(
                                                &item.connect_id,
                                            ))
                                            .size(theme::SMALL)
                                            .color(MUTED),
                                        );
                                    },
                                );
                                ui.with_layout(egui::Layout::right_to_left(Align::Center), |ui| {
                                    let valid = item.validate().is_ok();
                                    if ui
                                        .add_enabled(
                                            available && valid,
                                            login_button("连接")
                                                .min_size(vec2(78.0, theme::CONTROL_HEIGHT)),
                                        )
                                        .clicked()
                                    {
                                        connect = Some((item.connect_id.clone(), String::new()));
                                    }
                                    ui.add_enabled_ui(available && valid, |ui| {
                                        if assist_components::glyph(
                                            ui,
                                            Icon::Close,
                                            "取消收藏",
                                            MUTED,
                                        )
                                        .clicked()
                                        {
                                            self.assist.delete_prompt = Some(DeletePrompt::Device(
                                                SavedKind::Favorites,
                                                item.clone(),
                                            ));
                                        }
                                        if assist_components::glyph(
                                            ui,
                                            Icon::Edit,
                                            "编辑收藏",
                                            MUTED,
                                        )
                                        .clicked()
                                        {
                                            self.assist.favorite_editor = Some(FavoriteEditor {
                                                id: item.connect_id.clone(),
                                                remark: item.remark.clone(),
                                                editing: true,
                                                code: item.saved_code.clone(),
                                                code_changed: false,
                                            });
                                        }
                                    });
                                });
                            });
                        });
                    ui.separator();
                });
            }
        });
        connect
    }
    fn assist_page_notices(&mut self, ui: &mut egui::Ui, favorites: bool) {
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
            "远程协助",
            crate::ui::controls::DialogIcon::Warning,
            self.assist
                .connect_error
                .as_ref()
                .map(|(message, _)| message.as_str())
                .filter(|_| !favorites),
        );
        if self.assist.querying {
            if crate::ui::controls::observe_notice_action(
                ui.ctx(),
                "assist-query",
                "检查对端设备",
                crate::ui::controls::DialogIcon::Waiting,
                Some(&self.assist.message),
                "取消连接",
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
                    "远程协助",
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
                        "操作未完成",
                        crate::ui::controls::DialogIcon::Error,
                        true,
                    );
                    ui.add(egui::Label::new(message).wrap());
                    close |= crate::ui::controls::dialog_actions(
                        ui,
                        Some(crate::ui::controls::DialogAction::new("知道了")),
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
                        "设备验证码",
                        crate::ui::controls::DialogIcon::Info,
                        true,
                    );
                    ui.label(RichText::new(&pending.id).color(MUTED));
                    ui.add_space(18.0);
                    let input = ui.add_sized(
                        [360.0, theme::CONTROL_HEIGHT],
                        singleline_input(&mut self.assist.code)
                            .hint_text("输入对端设备验证码")
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
                            crate::ui::controls::DialogAction::new("连接")
                                .enabled(!self.assist.code.is_empty()),
                        ),
                        Some("取消"),
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
                            "编辑收藏"
                        } else {
                            "添加收藏"
                        },
                        crate::ui::controls::DialogIcon::Edit,
                        true,
                    );
                    ui.label(RichText::new("设备 ID").color(MUTED));
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
                    ui.label(RichText::new("备注").color(MUTED));
                    ui.add_sized(
                        [380.0, theme::CONTROL_HEIGHT],
                        singleline_input(&mut edit.remark)
                            .hint_text("可选")
                            .char_limit(128),
                    );
                    ui.add_space(10.0);
                    ui.label(RichText::new("设备验证码").color(MUTED));
                    edit.code_changed |= ui
                        .add_sized(
                            [380.0, theme::CONTROL_HEIGHT],
                            singleline_input(&mut edit.code)
                                .hint_text("可选，仅保存在本机")
                                .char_limit(256),
                        )
                        .changed();
                    let (accept, dismiss) =
                        crate::ui::controls::dialog_actions(
                            ui,
                            Some(crate::ui::controls::DialogAction::new("保存").enabled(
                                normalize_connect_id(&edit.id).is_ok() && !self.assist.busy,
                            )),
                            Some("取消"),
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
                        DeletePrompt::Recent => "清空最近连接？",
                        DeletePrompt::Device(SavedKind::Recent, _) => "移除这条记录？",
                        DeletePrompt::Device(SavedKind::Favorites, _) => "取消收藏？",
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
                            crate::ui::controls::DialogAction::new("确认")
                                .enabled(!self.assist.busy)
                                .danger(true),
                        ),
                        Some("取消"),
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
