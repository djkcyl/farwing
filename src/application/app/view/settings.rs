//! Settings: one page, categories on its left.
use super::*;

#[derive(Clone, Copy, Default, PartialEq, Eq)]
pub(super) enum SettingsTab {
    #[default]
    Account,
    Host,
    Controller,
    Network,
    Shortcuts,
    Plugins,
    Service,
    Diagnostics,
    Logs,
    About,
}

impl SettingsTab {
    const GROUPS: [(&'static str, &'static [Self]); 4] = [
        ("常规", &[Self::Account]),
        ("远程控制", &[Self::Host, Self::Controller, Self::Network]),
        ("扩展", &[Self::Shortcuts, Self::Plugins]),
        ("系统", &[Self::Service, Self::Diagnostics, Self::Logs, Self::About]),
    ];

    fn label(self) -> &'static str {
        match self {
            Self::Account => "账号与服务器",
            Self::Host => "被控设置",
            Self::Controller => "连接其他设备",
            Self::Network => "网络",
            Self::Shortcuts => "快捷键",
            Self::Plugins => "插件",
            Self::Service => "后台服务与驱动",
            Self::Diagnostics => "本机诊断",
            Self::Logs => "日志",
            Self::About => "关于",
        }
    }

    fn hint(self) -> &'static str {
        match self {
            Self::Account => "当前账号、所连接的服务器和本机身份",
            Self::Host => "别人连接本机时：总开关、直连与局域网、权限上限、画面与声音、通知；谁可以连接在“本机”页",
            Self::Controller => "本机连接其他设备时的画质、编解码与线路",
            Self::Network => "打洞与端口映射，主控与被控都适用",
            Self::Shortcuts => "观看窗口中的快捷键",
            Self::Plugins => "视频与输入插件",
            Self::Service => "后台服务，以及输入、虚拟显示与虚拟声卡驱动",
            Self::Diagnostics => "本机的编解码、上报与会话状态，可以导出诊断包",
            Self::Logs => "日志级别与保存位置",
            Self::About => "版本、许可与开源组件",
        }
    }

    fn icon(self) -> Icon {
        match self {
            Self::Account => Icon::Friends,
            Self::Host => Icon::Monitor,
            Self::Controller => Icon::Assist,
            Self::Network => Icon::Refresh,
            Self::Shortcuts => Icon::Keyboard,
            Self::Plugins => Icon::Plugins,
            Self::Service => Icon::Settings,
            Self::Diagnostics => Icon::Diagnostics,
            Self::Logs => Icon::Logs,
            Self::About => Icon::Info,
        }
    }
}

impl DeviceCenterApp {
    pub(super) fn settings_page(&mut self, ui: &mut egui::Ui) {
        let full = ui.available_rect_before_wrap();
        let nav = egui::Rect::from_min_size(full.min, vec2(200.0, full.height()));
        let content = egui::Rect::from_min_max(egui::pos2(nav.right() + 29.0, full.top()), full.max);
        ui.painter()
            .vline(nav.right() + 14.0, full.y_range(), Stroke::new(1.0, LINE));
        let current = self.center_ui.settings_tab;
        let mut chosen = None;
        ui.scope_builder(
            egui::UiBuilder::new()
                .id_salt("settings-nav")
                .max_rect(nav)
                .layout(egui::Layout::top_down(Align::Min)),
            |ui| {
                ui.label(RichText::new("设置").size(theme::TITLE).strong());
                ui.add_space(14.0);
                ui.spacing_mut().item_spacing.y = 4.0;
                for (group, tabs) in SettingsTab::GROUPS {
                    ui.label(RichText::new(group).small().color(MUTED));
                    for tab in tabs {
                        if nav_item(ui, tab.icon(), tab.label(), None, false, current == *tab) {
                            chosen = Some(*tab);
                        }
                    }
                    ui.add_space(10.0);
                }
            },
        );
        if let Some(tab) = chosen
            && tab != current
        {
            self.center_ui.settings_tab = tab;
            if tab == SettingsTab::About {
                self.center_ui.legal_document = None;
            }
        }
        let tab = self.center_ui.settings_tab;
        ui.scope_builder(
            egui::UiBuilder::new()
                .id_salt(("settings-content", tab.label()))
                .max_rect(content)
                .layout(egui::Layout::top_down(Align::Min)),
            |ui| {
                ui.label(RichText::new(tab.label()).size(theme::TITLE).strong());
                ui.label(RichText::new(tab.hint()).color(MUTED));
                ui.add_space(14.0);
                self.alert(ui);
                match tab {
                    SettingsTab::Account => {
                        crate::ui::controls::page_scroll("settings-account")
                            .show(ui, |ui| self.account_settings(ui));
                    }
                    SettingsTab::Host => {
                        crate::ui::controls::page_scroll("settings-host").show(ui, |ui| {
                            self.host_settings(ui);
                            self.notification_settings(ui);
                        });
                    }
                    SettingsTab::Controller => self.controller_settings(ui),
                    SettingsTab::Network => {
                        crate::ui::controls::page_scroll("settings-network")
                            .show(ui, |ui| self.network_settings(ui));
                    }
                    SettingsTab::Shortcuts => {
                        crate::ui::controls::page_scroll("settings-shortcuts")
                            .show(ui, |ui| self.center_ui.shortcuts.draw(ui));
                    }
                    SettingsTab::Plugins => self.center_ui.plugins.show(ui),
                    SettingsTab::Service => {
                        let active = self
                            .host
                            .as_ref()
                            .is_some_and(|h| h.status().session_active);
                        crate::ui::controls::page_scroll("settings-service")
                            .show(ui, |ui| self.center_ui.components.management(ui, active));
                    }
                    SettingsTab::Diagnostics => self.diagnostics_page(ui),
                    SettingsTab::Logs => self.logs_page(ui),
                    SettingsTab::About => self.about_page(ui),
                }
            },
        );
        ui.allocate_rect(full, Sense::hover());
    }

    /// The account, its server and this device.
    fn account_settings(&mut self, ui: &mut egui::Ui) {
        if self.access() != Access::Account {
            self.signed_out_settings(ui);
            return;
        }
        section(ui, "账号");
        let account = if self.account_name.trim().is_empty() {
            "已登录".to_owned()
        } else {
            self.account_name.clone()
        };
        let mut logout = false;
        form_row(ui, &account, "退出后，本机设备会从这个账号中移除", |ui| {
            logout = ui
                .add_enabled(!self.logout_pending, crate::ui::controls::secondary("退出登录"))
                .clicked();
        });
        if logout {
            self.logout();
        }

        section(ui, "服务器");
        let server = crate::account::server::ServerConfig::load().ok().flatten();
        let mut change = false;
        form_row(
            ui,
            server.as_ref().map_or("未设置", |server| server.origin()),
            "更换服务器需要先退出当前账号，然后在登录页填写新的地址",
            |ui| {
                change = ui
                    .add_enabled(!self.logout_pending, crate::ui::controls::secondary("更换服务器…"))
                    .clicked();
            },
        );
        if change && self.can_logout() {
            self.center_ui.change_server = true;
        }
        let certificate = match &server {
            Some(server) => match server.pinned_fingerprint() {
                Some(pin) => format!("已固定证书指纹 {pin}"),
                None if server.has_custom_ca() => "使用自定义 CA 验证".to_owned(),
                None => "由 Windows 信任的证书验证".to_owned(),
            },
            None => "—".to_owned(),
        };
        form_row(ui, "证书", &certificate, |_| {});

        section(ui, "本机");
        let current = self.devices.as_ref().map(|list| list.current_device.clone());
        match current {
            Some(device) => {
                let mut details = false;
                form_row(
                    ui,
                    display_alias(&device),
                    &format!(
                        "ID {}",
                        crate::account::api::device_id_text(&device.device_id)
                    ),
                    |ui| {
                        details = ui.add(crate::ui::controls::secondary("设备详情")).clicked();
                    },
                );
                if details {
                    self.open_details(device.device_id.clone());
                }
            }
            None => form_row(ui, "正在读取本机信息…", "", |_| {}),
        }
    }

    /// The account category without an account: how to sign in, the
    /// server, and this device.
    fn signed_out_settings(&mut self, ui: &mut egui::Ui) {
        use super::super::login_form::LoginStep;
        let access = self.access();
        section(ui, "账号");
        let label = self.sign_in_label();
        let mut sign_in = false;
        form_row(
            ui,
            "未登录",
            if access == Access::Guest {
                "登录后可以使用我的设备、好友共享的设备与收藏"
            } else {
                "登录需要先设置服务器"
            },
            |ui| sign_in = ui.add(primary(label)).clicked(),
        );
        if sign_in {
            self.sign_in();
        }

        section(ui, "服务器");
        let server = self.saved_server();
        let mut change = false;
        form_row(
            ui,
            server.as_deref().unwrap_or("未设置"),
            if server.is_some() {
                "未登录时可以直接更换"
            } else {
                "设置后，不登录也可以用设备 ID、短号连接或被连接"
            },
            |ui| {
                change = ui
                    .add(crate::ui::controls::secondary(if server.is_some() {
                        "更换服务器…"
                    } else {
                        "设置服务器…"
                    }))
                    .clicked();
            },
        );
        if change {
            self.open_sign_in(LoginStep::Server);
        }

        section(ui, "本机");
        match self.local_identity() {
            Some((name, id)) => form_row(ui, &name, &format!("ID {id}"), |_| {}),
            None => form_row(ui, "本机身份暂不可用", "", |_| {}),
        }
    }

    /// How this device connects to others.
    fn controller_settings(&mut self, ui: &mut egui::Ui) {
        let original_media = self.media;
        crate::ui::controls::page_scroll("settings-controller").show(ui, |ui| {
            form_row(
                ui,
                "我的光标颜色",
                "多人连接时其他参与者看到的颜色，连接中修改也会立即生效",
                |ui| {
                    let swatch = |ui: &mut egui::Ui, color: u32, selected: bool| {
                        let (rect, response) =
                            ui.allocate_exact_size(egui::vec2(20.0, 20.0), egui::Sense::click());
                        let fill = egui::Color32::from_rgb(
                            (color >> 16) as u8,
                            (color >> 8) as u8,
                            color as u8,
                        );
                        ui.painter().circle_filled(rect.center(), 8.0, fill);
                        if selected {
                            ui.painter().circle_stroke(
                                rect.center(),
                                10.0,
                                egui::Stroke::new(2.0, crate::ui::theme::TEXT),
                            );
                        }
                        response.clicked()
                    };
                    ui.horizontal(|ui| {
                        if ui
                            .selectable_label(self.media.pointer_color == 0, "自动")
                            .clicked()
                        {
                            self.media.pointer_color = 0;
                        }
                        for color in crate::protocol::participants::PALETTE {
                            if swatch(ui, color, self.media.pointer_color == color) {
                                self.media.pointer_color = color;
                            }
                        }
                    });
                },
            );
            form_row(ui, "串流帧率", "以远端实际刷新率为准", |ui| {
                egui::ComboBox::from_id_salt("center-fps")
                    .width(238.0)
                    .selected_text(self.media.frame_rate.label(self.local_display))
                    .show_ui(ui, |ui| {
                        for choice in FrameRateChoice::available(self.local_display) {
                            ui.selectable_value(
                                &mut self.media.frame_rate,
                                choice,
                                choice.label(self.local_display),
                            );
                        }
                    });
            });
            form_row(
                ui,
                "编码格式",
                if self.media.codec.restricted() {
                    "已固定，可在高级限制中调整"
                } else {
                    "不可用时自动回退；重连后生效"
                },
                |ui| {
                    if self.media.codec.restricted() {
                        ui.label(self.media.codec.label());
                        return;
                    }
                    egui::ComboBox::from_id_salt("center-codec")
                        .width(238.0)
                        .selected_text(self.media.codec.label())
                        .show_ui(ui, |ui| {
                            for choice in CodecPreference::PREFERRED {
                                ui.selectable_value(&mut self.media.codec, choice, choice.label());
                            }
                        });
                },
            );
            form_row(
                ui,
                "解码器",
                "仅影响本机播放；重连后生效",
                |ui| {
                    if self.media.decoder.mode.hardware() {
                        self.media_inventory.draw(
                            ui,
                            "center-decoder-gpu",
                            &mut self.media.decoder.gpu,
                            false,
                        );
                    } else {
                        ui.label("仅软件解码");
                    }
                },
            );
            let restriction = super::media_preferences::limits_title(
                self.media.decoder.mode,
                self.media
                    .codec
                    .restricted()
                    .then(|| self.media.codec.label()),
            );
            egui::CollapsingHeader::new(restriction)
                .id_salt("controller-codec-limits")
                .show(ui, |ui| {
                    form_row(ui, "解码方式", "", |ui| {
                        egui::ComboBox::from_id_salt("center-decoder")
                            .width(238.)
                            .selected_text(self.media.decoder.mode.label())
                            .show_ui(ui, |ui| {
                                for mode in crate::media::selection::ProcessingMode::ALL {
                                    ui.selectable_value(
                                        &mut self.media.decoder.mode,
                                        mode,
                                        mode.label(),
                                    );
                                }
                            });
                    });
                    form_row(ui, "格式限制", "", |ui| {
                        egui::ComboBox::from_id_salt("center-codec-only")
                            .width(238.)
                            .selected_text(if self.media.codec.restricted() {
                                self.media.codec.label()
                            } else {
                                "不限制"
                            })
                            .show_ui(ui, |ui| {
                                if ui
                                    .selectable_label(!self.media.codec.restricted(), "不限制")
                                    .clicked()
                                {
                                    self.media.codec = CodecPreference::Auto;
                                }
                                for codec in CodecPreference::ONLY {
                                    ui.selectable_value(
                                        &mut self.media.codec,
                                        codec,
                                        codec.label(),
                                    );
                                }
                            });
                    });
                });
            form_row(
                ui,
                "连接线路",
                "自动模式支持直连与中转切换",
                |ui| {
                    egui::ComboBox::from_id_salt("center-route")
                        .width(238.0)
                        .selected_text(self.media.transport.label())
                        .show_ui(ui, |ui| {
                            for choice in [
                                TransportChoice::Auto,
                                TransportChoice::P2p,
                                TransportChoice::Relay,
                            ] {
                                ui.selectable_value(
                                    &mut self.media.transport,
                                    choice,
                                    choice.label(),
                                );
                            }
                        });
                },
            );
        });
        if self.media != original_media {
            if let Err(error) = crate::media::preferences::save(self.media) {
                self.media = original_media;
                self.status = StatusMessage::error(format!("保存编解码设置失败：{error:#}"));
            }
        }
    }

    /// NAT traversal, for both roles.
    fn network_settings(&mut self, ui: &mut egui::Ui) {
        section(ui, "打洞与端口映射");
        self.edit_network(ui, Self::traversal_rows);
    }

    /// How others reach this device directly (Settings → 被控设置).
    pub(super) fn direct_settings(&mut self, ui: &mut egui::Ui) {
        section(ui, "直连与局域网");
        self.edit_network(ui, Self::direct_rows);
    }

    /// Show some of the network switches and apply what changed. Sessions
    /// read them when they start. In service mode the background service
    /// hosts, so the switches shown are its own and changes go to it as well.
    fn edit_network(
        &mut self,
        ui: &mut egui::Ui,
        rows: fn(&mut Self, &mut egui::Ui, &mut crate::transport::nat::settings::Settings),
    ) {
        let service = crate::transport::direct::managed::managed();
        match self.direct.take_outcome() {
            Some(Ok(message)) => self.status = StatusMessage::info(message),
            Some(Err(error)) => self.status = StatusMessage::error(error),
            None => {}
        }
        let network = if service {
            crate::transport::direct::managed::remote()
                .map(|state| state.settings)
                .unwrap_or_else(crate::transport::nat::settings::current)
        } else {
            crate::transport::nat::settings::current()
        };
        let mut edited = network;
        rows(self, ui, &mut edited);
        if edited != network && !self.direct.sending {
            // This process's controller sessions read the local copy.
            match crate::transport::nat::settings::save(edited) {
                Ok(()) if service => {
                    let host = self.host.clone();
                    self.direct.send(
                        ui.ctx(),
                        host.as_ref(),
                        crate::transport::direct::managed::Action::Network(edited),
                        if edited.serverless != network.serverless {
                            "已应用到后台服务，被控端正在按新设置重新连接"
                        } else {
                            "已应用到后台服务"
                        },
                    );
                }
                Ok(()) => {
                    if edited.serverless != network.serverless {
                        self.status = StatusMessage::info("已生效，被控端正在按新设置重新连接");
                    } else if (edited.direct, edited.direct_port, edited.lan_discovery)
                        != (network.direct, network.direct_port, network.lan_discovery)
                    {
                        self.status = StatusMessage::info("直连设置已生效");
                    }
                }
                Err(error) => self.status = StatusMessage::error(format!("{error:#}")),
            }
        }
    }

    fn traversal_rows(&mut self, ui: &mut egui::Ui, edited: &mut crate::transport::nat::settings::Settings) {
        form_row(
            ui,
            "激进打洞（对称型 NAT）",
            "对端为对称型 NAT 时预测其端口，主控与被控均适用",
            |ui| {
                crate::ui::controls::switch(ui, &mut edited.aggressive_punching);
            },
        );
        form_row(
            ui,
            "允许 UPnP / NAT-PMP / PCP 端口映射",
            "会话期间在路由器上为直连端口申请映射，结束后删除",
            |ui| {
                crate::ui::controls::switch(ui, &mut edited.port_mapping);
            },
        );
    }

    fn direct_rows(&mut self, ui: &mut egui::Ui, edited: &mut crate::transport::nat::settings::Settings) {
        form_row(
            ui,
            "允许直连",
            "接受局域网和地址直连（本账号设备、好友、受信任设备、访问码）；还需要开启“允许连接本机”",
            |ui| {
                crate::ui::controls::switch(ui, &mut edited.direct);
            },
        );
        form_row(
            ui,
            "直连端口",
            "TCP 与 UDP 都用此端口（所有直连会话共用这一个 UDP 端口）。从公网直连时在路由器上转发 TCP 与 UDP 的这个端口",
            |ui| {
                ui.add_enabled(
                    edited.direct,
                    egui::DragValue::new(&mut edited.direct_port).range(1024..=65_000),
                );
            },
        );
        form_row(
            ui,
            "局域网发现",
            "让同一局域网中的设备在“附近”里看到本机（需要允许直连）",
            |ui| {
                ui.add_enabled_ui(edited.direct, |ui| {
                    crate::ui::controls::switch(ui, &mut edited.lan_discovery);
                });
            },
        );
        // Without a server there is nothing to stop using.
        if self.access() == Access::NoServer && !edited.serverless {
            return;
        }
        form_row(
            ui,
            "仅直连（不连接服务器）",
            "被控端不再连接服务器，只能通过局域网或地址直连；好友和服务器中转不可用",
            |ui| {
                crate::ui::controls::switch(ui, &mut edited.serverless);
            },
        );
    }
}
