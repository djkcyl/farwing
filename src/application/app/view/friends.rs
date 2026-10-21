//! Friends: adding friends, answering requests, and what each friend may do
//! on this account's devices.
use super::cards::{self, Tone};
use super::*;
use crate::application::app::friends::{FriendOperation, GrantEditor};
use farwing_protocol::api::{Grants, UserSummary};

/// How one device is shared with the friend being edited.
#[derive(Clone, Copy, PartialEq, Eq)]
enum DeviceMode {
    Default,
    Custom,
    Hidden,
}

impl DeviceMode {
    const ALL: [Self; 3] = [Self::Default, Self::Custom, Self::Hidden];
    fn label(self) -> &'static str {
        match self {
            Self::Default => "跟随默认",
            Self::Custom => "单独设置",
            Self::Hidden => "不共享",
        }
    }
    fn of(grants: &Grants, device_id: &str) -> Self {
        match grants.devices.get(device_id) {
            None => Self::Default,
            Some(caps) if caps.is_empty() => Self::Hidden,
            Some(_) => Self::Custom,
        }
    }
}

/// A person: avatar, name, the lines under it and actions.
fn person_header(
    ui: &mut egui::Ui,
    user: &UserSummary,
    lines: impl FnOnce(&mut egui::Ui),
    actions: impl FnOnce(&mut egui::Ui),
) {
    ui.horizontal(|ui| {
        cards::avatar(ui, &user.user_id, user.display_name(), 40.0);
        ui.add_space(4.0);
        ui.vertical(|ui| {
            ui.spacing_mut().item_spacing.y = 3.0;
            ui.horizontal(|ui| {
                ui.label(RichText::new(user.display_name()).strong());
                if user.display_name() != user.username {
                    ui.label(
                        RichText::new(format!("@{}", user.username))
                            .size(theme::SMALL)
                            .color(MUTED),
                    );
                }
            });
            lines(ui);
        });
        ui.with_layout(egui::Layout::right_to_left(Align::Center), actions);
    });
}

/// A person on a card of its own.
fn person_card(
    ui: &mut egui::Ui,
    user: &UserSummary,
    lines: impl FnOnce(&mut egui::Ui),
    actions: impl FnOnce(&mut egui::Ui),
) {
    ui.push_id(&user.user_id, |ui| {
        cards::card().show(ui, |ui| {
            ui.set_width(ui.available_width());
            person_header(ui, user, lines, actions);
        });
    });
    ui.add_space(6.0);
}

/// What was asked for one of a friend's devices.
pub(super) enum DeviceAction {
    Connect,
    Details,
}

/// The capabilities a friend starts with on a device set apart.
fn friend_default() -> Vec<String> {
    farwing_protocol::caps::normalize(farwing_protocol::caps::DEFAULT_FRIEND)
}

fn detail(ui: &mut egui::Ui, text: &str) {
    ui.label(RichText::new(text).size(theme::SMALL).color(MUTED));
}

impl DeviceCenterApp {
    pub(super) fn friends_page(&mut self, ui: &mut egui::Ui) {
        if self.friends.data.is_none() && !self.friends.loading && self.friends.error.is_none() {
            self.request_friends(FriendOperation::Refresh);
        }
        if self.friends.loading || self.friends.busy {
            ui.ctx().request_repaint_after(Duration::from_millis(150));
        }
        if self
            .friends
            .message
            .as_ref()
            .is_some_and(|(_, until)| Instant::now() >= *until)
        {
            self.friends.message = None;
        }
        let data = self.friends.data.clone();
        crate::ui::controls::page_scroll("friends-page").show(ui, |ui| {
            let loading = self.friends.loading;
            let busy = self.friends.busy;
            let mut refresh = false;
            cards::page_header(
                ui,
                "好友",
                "",
                |ui| {
                    refresh = ui
                        .add_enabled(!loading, crate::ui::controls::secondary("刷新"))
                        .clicked();
                    if loading || busy {
                        ui.spinner();
                    }
                },
            );
            if refresh {
                self.request_friends(FriendOperation::Refresh);
            }
            self.add_friend_card(ui);
            ui.add_space(10.0);
            if let Some((message, _)) = &self.friends.message {
                cards::banner(ui, message, Tone::Success);
            }
            if let Some(error) = &self.friends.error
                && self.friends.editor.is_none()
            {
                cards::banner(ui, error, Tone::Warning);
            }
            let Some(data) = data else {
                if loading {
                    ui.add_space(12.0);
                    ui.horizontal(|ui| {
                        ui.spinner();
                        detail(ui, "正在读取好友…");
                    });
                }
                return;
            };
            self.friend_requests(ui, &data);
            self.friend_list(ui, &data);
        });
        self.grant_editor(ui.ctx());
        self.friend_confirmations(ui.ctx());
    }

    fn add_friend_card(&mut self, ui: &mut egui::Ui) {
        let mut submit = false;
        cards::card().show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.label(RichText::new("添加好友").strong());
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                let width = (ui.available_width() - 112.0).max(160.0);
                let response = ui.add_sized(
                    [width, crate::ui::controls::HEIGHT],
                    singleline_input(&mut self.friends.add_username).hint_text("对方的用户名"),
                );
                if response.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                    submit = true;
                }
                if ui
                    .add_enabled(
                        !self.friends.busy && !self.friends.add_username.trim().is_empty(),
                        primary("发送申请"),
                    )
                    .clicked()
                {
                    submit = true;
                }
            });
        });
        if submit && !self.friends.busy && !self.friends.add_username.trim().is_empty() {
            let username = std::mem::take(&mut self.friends.add_username);
            self.request_friends(FriendOperation::Add(username.trim().to_owned()));
        }
    }

    fn friend_requests(&mut self, ui: &mut egui::Ui, data: &crate::application::app::friends::FriendsData) {
        let pending = data.incoming.len() + data.outgoing.len();
        if pending == 0 {
            return;
        }
        cards::section_heading(ui, "待处理的申请", Some(pending), |_| {});
        let busy = self.friends.busy;
        for request in &data.incoming {
            let mut answer = None;
            person_card(
                ui,
                &request.user,
                |ui| detail(ui, &format!("向你发出了好友申请 · {}", cards::when(request.created_at))),
                |ui| {
                    if ui.add_enabled(!busy, primary("接受")).clicked() {
                        answer = Some(true);
                    }
                    if ui
                        .add_enabled(!busy, crate::ui::controls::secondary("拒绝"))
                        .clicked()
                    {
                        answer = Some(false);
                    }
                },
            );
            if let Some(accept) = answer {
                self.request_friends(FriendOperation::Answer(request.id.clone(), accept));
            }
        }
        for request in &data.outgoing {
            let mut cancel = false;
            person_card(
                ui,
                &request.user,
                |ui| detail(ui, &format!("等待对方接受 · 发出于 {}", cards::when(request.created_at))),
                |ui| {
                    cancel = ui
                        .add_enabled(!busy, crate::ui::controls::secondary("撤回"))
                        .clicked();
                },
            );
            if cancel {
                self.request_friends(FriendOperation::Cancel(request.id.clone()));
            }
        }
    }

    fn friend_list(&mut self, ui: &mut egui::Ui, data: &crate::application::app::friends::FriendsData) {
        let searchable = data.friends.len() > 5;
        let search = &mut self.friends.search;
        cards::section_heading(ui, "我的好友", Some(data.friends.len()), |ui| {
            if searchable {
                ui.add_sized(
                    [200.0, crate::ui::controls::HEIGHT],
                    singleline_input(search).hint_text("搜索好友"),
                );
            }
        });
        if data.friends.is_empty() {
            cards::card().show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.add_space(8.0);
                ui.vertical_centered(|ui| {
                    ui.label(RichText::new("还没有好友").strong());
                });
                ui.add_space(8.0);
            });
            return;
        }
        let query = if searchable {
            self.friends.search.trim().to_lowercase()
        } else {
            String::new()
        };
        let shared = self
            .devices
            .as_ref()
            .map(|list| list.shared.clone())
            .unwrap_or_default();
        let busy = self.friends.busy;
        let mut shown = 0;
        let mut picked = None;
        for friend in &data.friends {
            let user = &friend.user;
            if !query.is_empty()
                && !user.username.to_lowercase().contains(&query)
                && !user.nickname.to_lowercase().contains(&query)
            {
                continue;
            }
            shown += 1;
            let theirs: Vec<_> = shared
                .iter()
                .filter(|d| d.owner.as_ref().is_some_and(|o| o.user_id == user.user_id))
                .collect();
            let mut edit = false;
            let mut remove = false;
            ui.push_id(&user.user_id, |ui| {
                cards::card().show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    person_header(
                        ui,
                        user,
                        |ui| detail(ui, &format!("{} 成为好友", cards::when(friend.since))),
                        |ui| {
                            edit = ui.add_enabled(!busy, primary("设置权限")).clicked();
                            remove = ui
                                .add_enabled(!busy, crate::ui::controls::secondary("删除"))
                                .clicked();
                        },
                    );
                    ui.add_space(8.0);
                    if theirs.is_empty() {
                        detail(ui, "TA 还没有共享设备给你");
                    } else {
                        detail(ui, &format!("TA 共享给你的设备 · {}", theirs.len()));
                        ui.add_space(4.0);
                        for device in &theirs {
                            if let Some(action) = self.friend_device_row(ui, device) {
                                picked = Some((device.device_id.clone(), action));
                            }
                        }
                    }
                });
            });
            ui.add_space(6.0);
            if edit {
                self.friends.error = None;
                self.friends.editor = Some(GrantEditor {
                    user: user.clone(),
                    grants: None,
                    original: None,
                    saving: false,
                });
                self.request_friends(FriendOperation::LoadGrants(user.clone()));
            }
            if remove {
                self.friends.confirm_remove = Some(user.clone());
            }
        }
        if shown == 0 {
            detail(ui, "没有符合搜索条件的好友");
        }
        match picked {
            Some((id, DeviceAction::Connect)) => {
                self.selected_device_id = Some(id);
                self.start_viewer();
            }
            Some((id, DeviceAction::Details)) => self.open_details(id),
            None => {}
        }
    }

    /// One device a friend shares: what it is, what may be done, actions.
    pub(super) fn friend_device_row(&self, ui: &mut egui::Ui, device: &DeviceInfo) -> Option<DeviceAction> {
        let status = self.device_status(device);
        let issue = self.viewer_action_issue(device);
        let open = self.viewer_for_device(&device.device_id).is_some();
        let mut action = None;
        ui.push_id(&device.device_id, |ui| {
            egui::Frame::new()
                .fill(BG)
                .corner_radius(theme::CONTROL_RADIUS)
                .inner_margin(egui::Margin::symmetric(10, 8))
                .show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    ui.horizontal(|ui| {
                        let (icon, _) = ui.allocate_exact_size(vec2(26.0, 26.0), Sense::hover());
                        device_visuals::system_icon(ui.painter(), icon, device.platform);
                        ui.add_space(4.0);
                        ui.vertical(|ui| {
                            ui.spacing_mut().item_spacing.y = 3.0;
                            ui.horizontal(|ui| {
                                ui.add(
                                    egui::Label::new(RichText::new(display_alias(device)).strong())
                                        .truncate(),
                                );
                                ui.label(
                                    RichText::new(status.label())
                                        .size(theme::SMALL)
                                        .color(status.color()),
                                );
                                cards::caps_icons(ui, &device.granted, "无权限");
                            });
                            detail(
                                ui,
                                &format!(
                                    "ID {}",
                                    crate::account::api::device_id_text(&device.device_id)
                                ),
                            );
                        });
                        ui.with_layout(egui::Layout::right_to_left(Align::Center), |ui| {
                            let connect = ui.add_enabled(
                                issue.is_none(),
                                primary(if open { "已打开" } else { "连接" }),
                            );
                            let connect = match &issue {
                                Some(issue) => connect.on_disabled_hover_text(issue),
                                None => connect,
                            };
                            if connect.clicked() {
                                action = Some(DeviceAction::Connect);
                            }
                            if ui.add(crate::ui::controls::secondary("详情")).clicked() {
                                action = Some(DeviceAction::Details);
                            }
                        });
                    });
                });
        });
        ui.add_space(4.0);
        action
    }

    /// What one friend may do: a default for every device, and devices
    /// set apart or not shared at all.
    fn grant_editor(&mut self, ctx: &egui::Context) {
        let Some(editor) = &mut self.friends.editor else {
            return;
        };
        let devices: Vec<(String, String, String)> = self
            .catalog
            .as_ref()
            .map(|c| {
                c.groups
                    .desktop_devices
                    .iter()
                    .map(|d| {
                        (
                            d.device_id.clone(),
                            display_alias(d).to_owned(),
                            crate::account::api::device_id_text(&d.device_id),
                        )
                    })
                    .collect()
            })
            .unwrap_or_default();
        // Sharing again restores what was saved, or the new-friend default.
        let shared_default = editor
            .original
            .as_ref()
            .map(|g| g.default.clone())
            .filter(|caps| !caps.is_empty())
            .unwrap_or_else(friend_default);
        let busy = self.friends.busy;
        let error = self.friends.error.clone();
        let mut close = false;
        let mut save: Option<Grants> = None;
        let title = format!("{} 可以做什么", editor.user.display_name());
        let response = egui::Modal::new(egui::Id::new("friend-grants"))
            .frame(dialog_frame())
            .show(ctx, |ui| {
                ui.set_width(640.0);
                close |= crate::ui::controls::dialog_header(
                    ui,
                    &title,
                    crate::ui::controls::DialogIcon::Edit,
                    true,
                );
                let Some(grants) = &mut editor.grants else {
                    match &error {
                        Some(error) if !busy => {
                            cards::banner(ui, error, Tone::Warning);
                        }
                        _ => {
                            ui.horizontal(|ui| {
                                ui.spinner();
                                ui.label("正在读取权限…");
                            });
                        }
                    }
                    let (_, dismiss) = crate::ui::controls::dialog_actions(ui, None, Some("关闭"));
                    close |= dismiss;
                    return;
                };
                ui.label(RichText::new("所有设备").strong());
                ui.add_space(4.0);
                let sharing = !grants.default.is_empty();
                ui.horizontal(|ui| {
                    if cards::choice(ui, sharing, "按以下权限共享").clicked() && !sharing {
                        grants.default = shared_default.clone();
                    }
                    if cards::choice(ui, !sharing, "默认都不共享").clicked() && sharing {
                        grants.default.clear();
                    }
                });
                ui.add_space(4.0);
                if !grants.default.is_empty() {
                    cards::caps_editor(ui, "default", &mut grants.default);
                }
                ui.add_space(14.0);
                ui.separator();
                ui.add_space(6.0);
                ui.label(RichText::new("按设备设置").strong());
                ui.add_space(6.0);
                egui::ScrollArea::vertical()
                    .max_height(300.0)
                    .auto_shrink([false, true])
                    .show(ui, |ui| {
                        if devices.is_empty() {
                            detail(ui, "账号中还没有其他电脑");
                        }
                        for (device_id, name, shown_id) in &devices {
                            let mode = DeviceMode::of(grants, device_id);
                            let mut chosen = mode;
                            ui.push_id(device_id, |ui| {
                                ui.horizontal(|ui| {
                                    ui.vertical(|ui| {
                                        ui.spacing_mut().item_spacing.y = 2.0;
                                        ui.add(egui::Label::new(name.as_str()).truncate());
                                        detail(ui, &format!("ID {shown_id}"));
                                    });
                                    ui.with_layout(
                                        egui::Layout::right_to_left(Align::Center),
                                        |ui| {
                                            egui::ComboBox::from_id_salt("mode")
                                                .width(110.0)
                                                .selected_text(mode.label())
                                                .show_ui(ui, |ui| {
                                                    for option in DeviceMode::ALL {
                                                        ui.selectable_value(
                                                            &mut chosen,
                                                            option,
                                                            option.label(),
                                                        );
                                                    }
                                                });
                                            let effective = grants
                                                .devices
                                                .get(device_id)
                                                .unwrap_or(&grants.default);
                                            cards::caps_icons(ui, effective, "不共享");
                                        },
                                    );
                                });
                                if chosen != mode {
                                    match chosen {
                                        DeviceMode::Default => {
                                            grants.devices.remove(device_id);
                                        }
                                        DeviceMode::Custom => {
                                            let start = if grants.default.is_empty() {
                                                friend_default()
                                            } else {
                                                grants.default.clone()
                                            };
                                            grants.devices.insert(device_id.clone(), start);
                                        }
                                        DeviceMode::Hidden => {
                                            grants.devices.insert(device_id.clone(), Vec::new());
                                        }
                                    }
                                }
                                if let Some(caps) = grants
                                    .devices
                                    .get_mut(device_id)
                                    .filter(|caps| !caps.is_empty())
                                {
                                    ui.indent("caps", |ui| {
                                        cards::caps_editor(ui, "device", caps);
                                    });
                                }
                            });
                            ui.add_space(4.0);
                        }
                    });
                if let Some(error) = &error {
                    cards::banner(ui, error, Tone::Warning);
                }
                let changed = editor.original.as_ref() != Some(grants);
                let (accept, dismiss) = crate::ui::controls::dialog_actions(
                    ui,
                    Some(
                        crate::ui::controls::DialogAction::new(if editor.saving {
                            "正在保存…"
                        } else {
                            "保存"
                        })
                        .enabled(!busy && changed),
                    ),
                    Some("取消"),
                );
                if accept {
                    save = Some(grants.clone());
                }
                close |= dismiss;
            });
        if response.should_close() {
            close = true;
        }
        if let Some(grants) = save {
            let user_id = editor.user.user_id.clone();
            editor.saving = true;
            self.request_friends(FriendOperation::SaveGrants(user_id, grants));
        } else if close {
            self.friends.editor = None;
            self.friends.error = None;
        }
    }

    fn friend_confirmations(&mut self, ctx: &egui::Context) {
        let Some(user) = self.friends.confirm_remove.clone() else {
            return;
        };
        match cards::confirm(
            ctx,
            "remove-friend",
            "删除好友？",
            &format!("{} 与你之间的权限都会撤销", user.display_name()),
            "删除好友",
        ) {
            Some(true) => {
                self.friends.confirm_remove = None;
                self.request_friends(FriendOperation::Remove(user.user_id));
            }
            Some(false) => self.friends.confirm_remove = None,
            None => {}
        }
    }
}
