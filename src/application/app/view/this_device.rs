//! The "本机" page (`docs/access-model.md` §3): who may connect to this
//! device and how — the master switch with this device's ID, short number
//! and addresses; the access code; the devices it trusts; the account's
//! devices and friends; and who is connected now.
use super::cards::{self, Tone};
use super::connect::{device_card, empty_card};
use super::*;
use crate::application::app::controlled_caption::ParticipantAction;
use crate::transport::direct::managed::{self, Action, HostDirect};

fn detail(ui: &mut egui::Ui, text: &str) {
    ui.add(egui::Label::new(RichText::new(text).size(theme::SMALL).color(MUTED)).wrap());
}

/// A label column and a value, for the identity card.
fn field(ui: &mut egui::Ui, label: &str, value: impl FnOnce(&mut egui::Ui)) {
    ui.horizontal(|ui| {
        ui.allocate_ui_with_layout(
            vec2(72.0, theme::CONTROL_HEIGHT),
            egui::Layout::left_to_right(Align::Center),
            |ui| {
                // A fixed label column, so the values line up.
                ui.set_min_size(vec2(72.0, theme::CONTROL_HEIGHT));
                ui.label(RichText::new(label).color(MUTED));
            },
        );
        value(ui);
    });
}

impl DeviceCenterApp {
    pub(super) fn this_device_page(&mut self, ui: &mut egui::Ui) {
        self.direct.poll_messages();
        let access = self.access();
        cards::page_header(
            ui,
            "本机",
            "谁可以连接这台电脑，以及怎样连接。",
            |ui| self.active_view(ui),
        );
        if let Some((message, _)) = &self.direct.message {
            cards::banner(ui, message, Tone::Success);
        }
        if let Some(error) = self.direct.error.clone() {
            cards::banner(ui, &error, Tone::Warning);
        }
        if access == Access::NoServer {
            cards::banner(
                ui,
                "没有设置服务器：其他设备只能在同一局域网中直连本机（输入本机地址，或在“附近”中选择）。",
                Tone::Info,
            );
        }
        crate::ui::controls::page_scroll("this-device").show(ui, |ui| {
            let Some(host) = self.host.clone() else {
                cards::card().show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    ui.horizontal(|ui| {
                        ui.spinner();
                        detail(ui, "正在启动本机的被控端…");
                    });
                });
                ui.ctx().request_repaint_after(Duration::from_millis(500));
                return;
            };
            self.identity_card(ui, &host, access);
            cards::section_heading(ui, "访问码", None, |_| {});
            self.host_assist_controls(ui);
            self.trusted_section(ui);
            self.account_access_section(ui, access);
            self.connected_section(ui, &host);
            self.limits_card(ui);
        });
        self.trusted_dialogs(ui.ctx());
    }

    /// The host side of this device: from this process, or from the
    /// background service that hosts in service mode (`None` until its
    /// first snapshot).
    pub(super) fn direct_host_state(&self) -> Option<HostDirect> {
        if managed::managed() {
            managed::remote()
        } else {
            Some(HostDirect::current())
        }
    }

    /// The master switch, and how others reach this device.
    fn identity_card(&mut self, ui: &mut egui::Ui, host: &crate::features::host::Handle, access: Access) {
        let mut allowed = host.allowed();
        let identity = self.local_identity();
        let raw_id = crate::transport::direct::local_device_id().unwrap_or_default();
        let short = host.assistance.snapshot().connect_id;
        let state = self.direct_host_state();
        let addresses = if state.as_ref().is_some_and(|s| s.listening_port.is_some()) {
            self.direct.local_addresses()
        } else {
            Vec::new()
        };
        let mut changed = false;
        let mut open_network = false;
        let mut install_service = false;
        cards::card().show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal(|ui| {
                let (dot, _) = ui.allocate_exact_size(vec2(10.0, 20.0), Sense::hover());
                ui.painter()
                    .circle_filled(dot.center(), 4.5, if allowed { GREEN } else { MUTED });
                ui.label(
                    RichText::new(identity.as_ref().map_or("本机", |(name, _)| name.as_str()))
                        .size(theme::SECTION)
                        .strong(),
                );
                if managed::managed() {
                    cards::chip(ui, "后台服务", BLUE);
                }
                ui.with_layout(egui::Layout::right_to_left(Align::Center), |ui| {
                    changed = crate::ui::controls::service_switch(ui, &mut allowed).changed();
                    ui.label(if allowed { "允许连接本机" } else { "不允许连接本机" });
                });
            });
            ui.add_space(8.0);
            field(ui, "设备 ID", |ui| {
                assist_components::copy_value(
                    ui,
                    "this-device-id",
                    vec2(240.0, theme::CONTROL_HEIGHT),
                    RichText::new(identity.as_ref().map_or("未知", |(_, id)| id.as_str()))
                        .monospace()
                        .strong(),
                    (!raw_id.is_empty()).then_some(raw_id.as_str()),
                    "已复制设备 ID",
                );
                detail(ui, "由本机密钥生成，在任何服务器和局域网中都相同");
            });
            field(ui, "短号", |ui| {
                if !short.is_empty() {
                    assist_components::copy_value(
                        ui,
                        "this-device-short",
                        vec2(240.0, theme::CONTROL_HEIGHT),
                        RichText::new(assist_components::grouped_id(&short))
                            .monospace()
                            .strong(),
                        Some(short.as_str()),
                        "已复制短号",
                    );
                    detail(ui, "服务器分配，方便口头告诉别人");
                } else {
                    detail(
                        ui,
                        match access {
                            Access::NoServer => "没有服务器时没有短号",
                            _ if !allowed => "开启“允许连接本机”后由服务器分配",
                            _ => "正在从服务器获取…",
                        },
                    );
                }
            });
            field(ui, "局域网", |ui| match state.as_ref().map(|s| (s.listening_port, s.settings)) {
                Some((Some(port), settings)) => {
                    let default = port == farwing_protocol::direct::DEFAULT_PORT;
                    if addresses.is_empty() {
                        detail(ui, "没有找到本机的局域网地址；对方可以输入本机的 IP 或主机名");
                    }
                    for address in &addresses {
                        let text = if default {
                            address.to_string()
                        } else {
                            format!("{address}:{port}")
                        };
                        if ui
                            .add(egui::Button::new(RichText::new(&text).monospace()))
                            .on_hover_text("点击复制")
                            .clicked()
                        {
                            ui.ctx().copy_text(text);
                        }
                    }
                    detail(
                        ui,
                        &format!(
                            "直连端口 {port}{}",
                            if settings.lan_discovery { " · 局域网可发现" } else { " · 局域网发现已关闭" }
                        ),
                    );
                }
                Some((None, settings)) if !settings.direct => {
                    detail(ui, "直连已关闭");
                    open_network = ui.add(crate::ui::controls::secondary("直连设置")).clicked();
                }
                Some((None, _)) if !allowed => detail(ui, "开启“允许连接本机”后接受直连"),
                Some((None, _)) => detail(ui, "正在开启直连…"),
                None => detail(ui, "正在读取后台服务的状态…"),
            });
            if let Some((hint, action)) = self.center_ui.components.reach() {
                field(ui, "后台服务", |ui| {
                    detail(ui, hint);
                    if let Some(label) = action {
                        install_service = ui.add(crate::ui::controls::secondary(label)).clicked();
                    }
                });
            }
            if !allowed {
                ui.add_space(4.0);
                detail(
                    ui,
                    "关闭时，任何设备都不能连接本机（包括本账号的设备、好友、受信任设备和访客）。",
                );
            }
        });
        if install_service {
            self.center_ui.components.request_install();
        }
        if changed {
            host.set_allowed(allowed);
            self.save_host_settings();
        }
        if open_network {
            self.center_ui.page = Page::Settings;
            self.center_ui.settings_tab = super::settings::SettingsTab::Host;
        }
    }

    fn trusted_section(&mut self, ui: &mut egui::Ui) {
        let trusted = self
            .direct_host_state()
            .map(|state| state.trusted)
            .unwrap_or_default();
        cards::section_heading(ui, "受信任设备", Some(trusted.len()), |_| {});
        detail(
            ui,
            "这些设备无需访问码和确认即可连接本机，经服务器或直连都可以，按各自的权限。",
        );
        ui.add_space(6.0);
        if trusted.is_empty() {
            empty_card(
                ui,
                "还没有受信任的设备",
                "在“设备 → 附近”中与对方配对；或在访客连接后，在下方“正在连接”中点“信任”，或在连接请求中选“允许并信任”。",
            );
            return;
        }
        for device in trusted {
            let summary = cards::caps_summary(&device.grants);
            // It trusts this device too: the two are paired.
            let paired = crate::transport::direct::store::known_host(&device.device_id)
                .is_some_and(|h| h.trusted);
            let mut edit = false;
            let mut remove = false;
            device_card(
                ui,
                &device.device_id,
                &device.name,
                &if paired {
                    vec![("已配对", GREEN), (summary.as_str(), BLUE)]
                } else {
                    vec![(summary.as_str(), BLUE)]
                },
                &[
                    format!("ID {}", crate::account::api::device_id_text(&device.device_id)),
                    format!(
                        "信任于 {} · 最近连接 {}",
                        cards::when(device.trusted_at),
                        if device.last_seen > 0 {
                            cards::when(device.last_seen)
                        } else {
                            "从未".into()
                        }
                    ),
                ],
                |ui| {
                    remove = ui
                        .add_enabled(!self.direct.sending, crate::ui::controls::secondary("移除"))
                        .clicked();
                    edit = ui
                        .add_enabled(!self.direct.sending, primary("权限"))
                        .clicked();
                },
            );
            if edit {
                self.direct.editing =
                    Some((device.device_id.clone(), device.name.clone(), device.grants.clone()));
            }
            if remove {
                self.direct.confirm_remove = Some((device.device_id.clone(), device.name.clone()));
            }
        }
    }

    fn account_access_section(&mut self, ui: &mut egui::Ui, access: Access) {
        cards::section_heading(ui, "本账号的设备与好友", None, |_| {});
        let mut friends = false;
        let mut sign_in = false;
        cards::card().show(ui, |ui| {
            ui.set_width(ui.available_width());
            if access == Access::Account {
                ui.horizontal(|ui| {
                    ui.label(RichText::new("本账号的其他设备").strong());
                    cards::chip(ui, "完全控制", GREEN);
                });
                detail(ui, "登录同一账号的电脑可以完全控制本机；在局域网中可以直连。");
                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    ui.label(RichText::new("好友").strong());
                    cards::chip(ui, "按你给的权限", AMBER);
                    ui.with_layout(egui::Layout::right_to_left(Align::Center), |ui| {
                        friends = ui.add(crate::ui::controls::secondary("管理好友")).clicked();
                    });
                });
                detail(
                    ui,
                    "好友按你在“好友”页给的权限连接，没有共享的设备对好友不可见；在局域网中也可以直连。",
                );
            } else {
                ui.horizontal(|ui| {
                    detail(ui, "登录后，本账号的其他设备与好友也可以连接本机。");
                    ui.with_layout(egui::Layout::right_to_left(Align::Center), |ui| {
                        sign_in = ui
                            .add(crate::ui::controls::secondary(self.sign_in_label()))
                            .clicked();
                    });
                });
            }
        });
        if friends {
            self.center_ui.page = Page::Friends;
            self.request_friends(super::super::friends::FriendOperation::Refresh);
        }
        if sign_in {
            self.sign_in();
        }
    }

    fn connected_section(&mut self, ui: &mut egui::Ui, host: &crate::features::host::Handle) {
        let status = host.status();
        let participants = super::super::controlled_caption::participants(
            &status,
            self.devices.as_ref(),
            self.catalog.as_ref(),
        );
        cards::section_heading(ui, "正在连接", Some(participants.len()), |_| {});
        let mut action = None;
        cards::card().show(ui, |ui| {
            ui.set_width(ui.available_width());
            if participants.is_empty() {
                detail(ui, "目前没有人连接本机。");
            } else {
                action = super::super::controlled_caption::participants_list(ui, &participants);
            }
        });
        if let Some(error) = status.error {
            cards::banner(ui, &error, Tone::Warning);
        }
        if let Some(action) = action {
            self.participant_action(ui.ctx(), action);
        }
    }

    fn limits_card(&mut self, ui: &mut egui::Ui) {
        cards::section_heading(ui, "上限", None, |_| {});
        let mut open = false;
        cards::card().show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal(|ui| {
                detail(
                    ui,
                    "文件、剪贴板、端口转发、同时连接人数，以及直连与局域网、画面与声音，对所有连接本机的人生效。",
                );
                ui.with_layout(egui::Layout::right_to_left(Align::Center), |ui| {
                    open = ui.add(crate::ui::controls::secondary("被控设置")).clicked();
                });
            });
        });
        if open {
            self.center_ui.page = Page::Settings;
            self.center_ui.settings_tab = super::settings::SettingsTab::Host;
        }
    }

    /// What the host owner chose for a participant.
    pub(super) fn participant_action(&mut self, ctx: &egui::Context, action: ParticipantAction) {
        match action {
            ParticipantAction::Trust {
                device_id,
                public_key,
                name,
            } => {
                let host = self.host.clone();
                let grants = host
                    .as_ref()
                    .map(|h| h.assistance.settings().grants())
                    .unwrap_or_default();
                self.direct.perform(
                    ctx,
                    host.as_ref(),
                    Action::Trust {
                        device_id,
                        public_key,
                        name: name.clone(),
                        grants,
                    },
                    format!("已信任 {name}：以后无需访问码和确认"),
                );
            }
            other => {
                if let Some(host) = &self.host {
                    other.apply(host);
                }
            }
        }
    }

    /// Editing what a trusted device may do, and confirming its removal.
    fn trusted_dialogs(&mut self, ctx: &egui::Context) {
        if let Some((device_id, name, mut grants)) = self.direct.editing.take() {
            let mut save = false;
            let mut close = false;
            let response = egui::Modal::new(egui::Id::new("trusted-grants"))
                .frame(dialog_frame())
                .show(ctx, |ui| {
                    ui.set_width(560.0);
                    close |= crate::ui::controls::dialog_header(
                        ui,
                        &format!("{name} 可以做什么"),
                        crate::ui::controls::DialogIcon::Edit,
                        true,
                    );
                    cards::caps_editor(ui, "trusted-grants", &mut grants);
                    ui.add_space(6.0);
                    detail(ui, "与本机的上限取交集；修改在对方下次连接时生效。");
                    let (accept, dismiss) = crate::ui::controls::dialog_actions(
                        ui,
                        Some(crate::ui::controls::DialogAction::new("保存")),
                        Some("取消"),
                    );
                    save = accept;
                    close |= dismiss;
                });
            if save {
                let host = self.host.clone();
                self.direct.perform(
                    ctx,
                    host.as_ref(),
                    Action::SetGrants { device_id, grants },
                    "权限已保存",
                );
            } else if !close && !response.should_close() {
                self.direct.editing = Some((device_id, name, grants));
            }
        }
        let Some((id, name)) = self.direct.confirm_remove.clone() else {
            return;
        };
        match cards::confirm(
            ctx,
            "trusted-remove",
            "不再信任这台设备？",
            &format!("{name} 以后连接本机需要访问码或你的确认，它正在进行的连接会结束。"),
            "不再信任",
        ) {
            Some(true) => {
                self.direct.confirm_remove = None;
                let host = self.host.clone();
                self.direct.perform(
                    ctx,
                    host.as_ref(),
                    Action::Remove { device_id: id },
                    format!("已不再信任 {name}"),
                );
            }
            Some(false) => self.direct.confirm_remove = None,
            None => {}
        }
    }
}
