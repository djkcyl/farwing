//! The "设备" page (`docs/access-model.md` §3): one connect bar for a
//! device ID, a short number or an address, and one list of devices,
//! filtered — this account's computers, devices friends share, devices on
//! the local network, and devices connected to before. A device is known by
//! the same ID in every list; how it is reached is chosen automatically.
use super::cards::{self, Tone};
use super::*;
use crate::account::assist::{SavedDevice, SavedKind};
use crate::application::app::assist::{DeletePrompt, FavoriteEditor};
use crate::application::app::direct::DeviceTab;
use crate::transport::direct::store;

/// What the connect bar was given.
enum Target {
    ShortNumber(String),
    DeviceId(String),
    Address(String),
}

fn parse_target(text: &str) -> Result<Target, String> {
    let text = text.trim();
    if text.is_empty() {
        return Err("请输入设备 ID、短号或地址".into());
    }
    if let Ok(number) = crate::account::assist::normalize_connect_id(text) {
        return Ok(Target::ShortNumber(number));
    }
    if let Some(id) = crate::account::api::parse_device_id(text) {
        return Ok(Target::DeviceId(id));
    }
    crate::transport::direct::Target::parse(text)
        .map(|target| Target::Address(target.to_string()))
        .map_err(|_| "无法识别的设备 ID、短号或地址".into())
}

fn detail(ui: &mut egui::Ui, text: &str) {
    ui.add(egui::Label::new(RichText::new(text).size(theme::SMALL).color(MUTED)).wrap());
}

/// A card for one device: icon, name with chips, detail lines, actions.
pub(super) fn device_card(
    ui: &mut egui::Ui,
    id: &str,
    name: &str,
    chips: &[(&str, Color32)],
    lines: &[String],
    actions: impl FnOnce(&mut egui::Ui),
) {
    device_card_with(ui, id, name, chips, None, lines, actions);
}

/// [`device_card`], with what the device may do as icons after the chips.
pub(super) fn device_card_with(
    ui: &mut egui::Ui,
    id: &str,
    name: &str,
    chips: &[(&str, Color32)],
    grants: Option<&[String]>,
    lines: &[String],
    actions: impl FnOnce(&mut egui::Ui),
) {
    ui.push_id(id, |ui| {
        cards::card().show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal(|ui| {
                let (icon, _) = ui.allocate_exact_size(vec2(34.0, 34.0), Sense::hover());
                ui.painter().rect_filled(icon, 6.0, BG);
                paint_icon(ui.painter(), icon.shrink(6.0), Icon::Monitor, MUTED);
                ui.add_space(2.0);
                ui.vertical(|ui| {
                    ui.set_max_width((ui.available_width() - 230.0).max(160.0));
                    ui.spacing_mut().item_spacing.y = 3.0;
                    ui.horizontal(|ui| {
                        ui.add(egui::Label::new(RichText::new(name).strong()).truncate());
                        for (text, color) in chips {
                            cards::chip(ui, text, *color);
                        }
                        if let Some(grants) = grants {
                            cards::caps_icons(ui, grants, "无权限");
                        }
                    });
                    for line in lines {
                        ui.add(
                            egui::Label::new(RichText::new(line).size(theme::SMALL).color(MUTED))
                                .truncate(),
                        );
                    }
                });
                ui.with_layout(egui::Layout::right_to_left(Align::Center), actions);
            });
        });
    });
    ui.add_space(6.0);
}

/// An empty list explained in a card.
pub(super) fn empty_card(ui: &mut egui::Ui, title: &str, hint: &str) {
    cards::card().show(ui, |ui| {
        ui.set_width(ui.available_width());
        ui.label(RichText::new(title).strong());
        if !hint.is_empty() {
            detail(ui, hint);
        }
    });
}

/// One device in the "最近与收藏" tab.
struct RecentRow {
    title: String,
    device_id: Option<String>,
    short_number: Option<String>,
    address: Option<String>,
    /// It admitted this device without a code last time.
    trusted: bool,
    last: i64,
    /// The server record (an account's recent assistance).
    saved: Option<SavedDevice>,
}

impl DeviceCenterApp {
    pub(super) fn devices_page(&mut self, ui: &mut egui::Ui) {
        self.direct.poll_messages();
        let access = self.access();
        if !self.direct.tab_chosen {
            self.direct.tab = if access == Access::Account {
                DeviceTab::Mine
            } else {
                DeviceTab::Nearby
            };
        }
        if self.direct.tab == DeviceTab::Nearby {
            self.direct.scan_continuously(ui.ctx());
        }
        let tab = self.direct.tab;
        let mut refresh = false;
        cards::page_header(
            ui,
            "设备",
            "",
            |ui| {
                self.active_view(ui);
                let busy = match tab {
                    DeviceTab::Mine | DeviceTab::Friends => self.refresh_pending,
                    // Searching goes on quietly; only the first round shows.
                    DeviceTab::Nearby => {
                        let lan = self.direct.lan();
                        lan.scanning && lan.found.is_none()
                    }
                    DeviceTab::Recent => self.assist.loading,
                };
                let enabled = match tab {
                    DeviceTab::Mine | DeviceTab::Friends => access == Access::Account,
                    DeviceTab::Recent | DeviceTab::Nearby => true,
                };
                if busy {
                    ui.add_sized(
                        [theme::CONTROL_HEIGHT, theme::CONTROL_HEIGHT],
                        egui::Spinner::new().size(18.0),
                    );
                } else if enabled
                    && assist_components::glyph(ui, Icon::Refresh, "刷新", MUTED).clicked()
                {
                    refresh = true;
                }
            },
        );
        if refresh {
            match tab {
                DeviceTab::Mine | DeviceTab::Friends => self.request_refresh(),
                DeviceTab::Nearby => self.direct.scan(ui.ctx()),
                DeviceTab::Recent => self.request_assist_refresh(),
            }
        }
        self.connect_bar(ui);
        if self.cached_devices {
            cards::banner(ui, "暂时连不上服务器", Tone::Warning);
        }
        if let Some((message, _)) = &self.direct.message {
            cards::banner(ui, message, Tone::Success);
        }
        if let Some(error) = self.direct.error.clone() {
            cards::banner(ui, &error, Tone::Warning);
        }
        self.assist_notices(ui);
        self.pairing_card(ui);
        ui.add_space(12.0);
        let counts = self.tab_counts(access);
        let label = |name: &str, count: Option<usize>| match count {
            Some(count) => format!("{name}  {count}"),
            None => name.to_owned(),
        };
        let labels = [
            label("我的设备", counts[0]),
            label("好友共享", counts[1]),
            label("附近", counts[2]),
            label("最近与收藏", counts[3]),
        ];
        let before = self.direct.tab;
        if cards::tabs(
            ui,
            &mut self.direct.tab,
            &[
                (DeviceTab::Mine, labels[0].as_str()),
                (DeviceTab::Friends, labels[1].as_str()),
                (DeviceTab::Nearby, labels[2].as_str()),
                (DeviceTab::Recent, labels[3].as_str()),
            ],
        ) {
            self.direct.tab_chosen = true;
            match self.direct.tab {
                DeviceTab::Nearby => self.direct.scan(ui.ctx()),
                DeviceTab::Recent if access == Access::Account => self.request_assist_refresh(),
                _ => {}
            }
        }
        if before != self.direct.tab {
            self.direct.error = None;
        }
        ui.add_space(12.0);
        match self.direct.tab {
            DeviceTab::Mine | DeviceTab::Friends if access != Access::Account => {
                let friends = self.direct.tab == DeviceTab::Friends;
                match cards::locked(
                    ui,
                    if friends { "登录后查看好友共享的设备" } else { "登录后查看你的设备" },
                    &[self.sign_in_label(), "附近的设备"],
                ) {
                    Some(0) => self.sign_in(),
                    Some(_) => {
                        self.direct.tab = DeviceTab::Nearby;
                        self.direct.tab_chosen = true;
                        self.direct.scan(ui.ctx());
                    }
                    None => {}
                }
            }
            DeviceTab::Mine => self.own_devices_tab(ui),
            DeviceTab::Friends => {
                crate::ui::controls::page_scroll("friend-devices").show(ui, |ui| {
                    self.friend_devices_tab(ui);
                });
            }
            DeviceTab::Nearby => {
                crate::ui::controls::page_scroll("nearby-devices").show(ui, |ui| {
                    self.nearby_tab(ui);
                });
            }
            DeviceTab::Recent => {
                crate::ui::controls::page_scroll("recent-devices").show(ui, |ui| {
                    self.recent_tab(ui, access);
                });
            }
        }
    }

    /// How many devices each tab lists (`None` when unknown or locked).
    fn tab_counts(&self, access: Access) -> [Option<usize>; 4] {
        let account = access == Access::Account;
        let own = self.devices.as_ref().filter(|_| account).map(|list| {
            list.devices
                .iter()
                .filter(|d| self.show_in_watching_list(d))
                .count()
        });
        let shared = self
            .devices
            .as_ref()
            .filter(|_| account)
            .map(|list| list.shared.len());
        let scanned = self.direct.lan().found.is_some();
        let nearby = scanned.then(|| self.direct.nearby().len());
        [own, shared, nearby, None]
    }

    /// One field for every way of addressing a device.
    fn connect_bar(&mut self, ui: &mut egui::Ui) {
        let mut connect = false;
        let parsed = parse_target(&self.direct.target);
        cards::card().show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal(|ui| {
                let width = (ui.available_width() - 92.0).max(200.0);
                let response = ui.add_sized(
                    [width, crate::ui::controls::HEIGHT],
                    singleline_input(&mut self.direct.target)
                        .hint_text("设备 ID、短号或地址"),
                );
                if response.changed() {
                    self.direct.error = None;
                }
                if response.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                    connect = true;
                }
                if ui
                    .add_enabled(!self.direct.target.trim().is_empty(), primary("连接"))
                    .clicked()
                {
                    connect = true;
                }
            });
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                ui.label(RichText::new("访问码").size(theme::SMALL).color(MUTED));
                let response = ui
                    .add_enabled_ui(!self.direct.ask, |ui| {
                        ui.add_sized(
                            [220.0, crate::ui::controls::HEIGHT],
                            singleline_input(&mut self.direct.code)
                                .password(true)
                                .hint_text("可选"),
                        )
                    })
                    .inner;
                if std::mem::take(&mut self.direct.focus_code) {
                    response.request_focus();
                }
                if response.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                    connect = true;
                }
                ui.add_space(8.0);
                ui.checkbox(&mut self.direct.ask, "不用访问码，请对方确认");
            });
            if let Err(error) = &parsed
                && !self.direct.target.trim().is_empty()
            {
                detail(ui, error);
            }
        });
        if connect {
            let (target, code, ask) = (
                self.direct.target.clone(),
                self.direct.code.clone(),
                self.direct.ask,
            );
            self.direct.code.clear();
            self.connect_target(&target, &code, ask);
        }
    }

    /// Connect to whatever the connect bar was given.
    fn connect_target(&mut self, text: &str, code: &str, ask: bool) {
        let code = code.trim();
        match parse_target(text) {
            Err(error) => self.direct.error = Some(error),
            Ok(Target::Address(address)) => {
                // A nearby device that asked for a code keeps its name.
                let name = self
                    .direct
                    .target_name
                    .clone()
                    .filter(|(target, _)| *target == address)
                    .map(|(_, name)| name);
                self.connect_address(&address, code, ask, name.as_deref());
            }
            Ok(Target::ShortNumber(number)) => {
                if self.access() == Access::NoServer {
                    self.direct.error = Some("没有服务器，不能用短号连接".into());
                } else {
                    self.direct.error = None;
                    self.request_assist_connect(number, code.to_owned());
                }
            }
            Ok(Target::DeviceId(id)) => self.connect_device_id(&id, code, ask),
        }
    }

    /// Reach a device by its ID: through the account for this account's
    /// and friends' devices, directly when it is nearby, through the server
    /// when there is one, else at the address it had last time.
    fn connect_device_id(&mut self, id: &str, code: &str, ask: bool) {
        self.direct.error = None;
        let account_device = self
            .devices
            .as_ref()
            .is_some_and(|l| l.devices.iter().chain(&l.shared).any(|d| d.device_id == id));
        if account_device && code.is_empty() && !ask {
            self.selected_device_id = Some(id.to_owned());
            self.start_viewer();
            return;
        }
        if let Some(host) = self
            .direct
            .nearby()
            .into_iter()
            .find(|h| h.announce.device_id == id)
        {
            self.connect_address(&host.target().to_string(), code, ask, Some(&host.announce.name));
            return;
        }
        if self.access() != Access::NoServer {
            self.request_assist_connect(id.to_owned(), code.to_owned());
            return;
        }
        match store::known_host(id).filter(|h| !h.address.is_empty()) {
            Some(host) => self.connect_address(&host.address, code, ask, Some(&host.name)),
            None => {
                self.direct.error = Some(format!(
                    "找不到设备 {}",
                    crate::account::api::device_id_text(id)
                ))
            }
        }
    }

    /// Connect directly: by this device's key, with an access code, or
    /// asking the other side's user.
    fn connect_address(&mut self, address: &str, code: &str, ask: bool, name: Option<&str>) {
        let code = (!code.is_empty()).then(|| code.to_owned());
        let ask = ask && code.is_none();
        match crate::session::controller::DirectRequest::parse(address, code, ask) {
            Ok(request) => {
                self.direct.error = None;
                let request = match name {
                    Some(name) => request.named(name),
                    None => request,
                };
                self.spawn_direct_viewer(request);
            }
            Err(error) => self.direct.error = Some(format!("{error:#}")),
        }
    }

    /// Fill the connect bar for a device that needs an access code.
    fn ask_for_code(&mut self, target: String, name: &str) {
        self.direct.target_name = Some((target.clone(), name.to_owned()));
        self.direct.target = target;
        self.direct.code.clear();
        self.direct.ask = false;
        self.direct.focus_code = true;
        self.direct.note(format!("{name} 需要访问码"));
    }

    fn friend_devices_tab(&mut self, ui: &mut egui::Ui) {
        let shared = self
            .devices
            .as_ref()
            .map(|list| list.shared.clone())
            .unwrap_or_default();
        if shared.is_empty() {
            empty_card(ui, "还没有好友共享的设备", "");
            return;
        }
        // Grouped by owner; owners and their devices by name, then ID.
        let mut shared = shared;
        shared.sort_by(DeviceInfo::list_order);
        let mut owners: Vec<(String, Vec<DeviceInfo>)> = Vec::new();
        for device in shared {
            let owner = device
                .owner
                .as_ref()
                .map_or_else(|| "好友".to_owned(), |o| o.display_name().to_owned());
            match owners.iter_mut().find(|(name, _)| *name == owner) {
                Some((_, list)) => list.push(device),
                None => owners.push((owner, vec![device])),
            }
        }
        owners.sort_by(|a, b| a.0.to_lowercase().cmp(&b.0.to_lowercase()).then_with(|| a.0.cmp(&b.0)));
        let nearby = self.direct.nearby();
        let mut picked = None;
        for (owner, devices) in owners {
            cards::section_heading(ui, &format!("{owner} 共享"), Some(devices.len()), |_| {});
            for device in &devices {
                if nearby.iter().any(|h| h.announce.device_id == device.device_id) {
                    cards::chip(ui, "在局域网中", GREEN);
                }
                if let Some(action) = self.friend_device_row(ui, device) {
                    picked = Some((device.device_id.clone(), action));
                }
            }
        }
        match picked {
            Some((id, super::friends::DeviceAction::Connect)) => {
                self.selected_device_id = Some(id);
                self.start_viewer();
            }
            Some((id, super::friends::DeviceAction::Details)) => self.open_details(id),
            None => {}
        }
    }

    fn nearby_tab(&mut self, ui: &mut egui::Ui) {
        let (first_scan, error) = {
            let lan = self.direct.lan();
            (lan.scanning && lan.found.is_none(), lan.error.clone())
        };
        let found = self.direct.nearby();
        if first_scan {
            ui.horizontal(|ui| {
                ui.spinner();
                detail(ui, "正在查找局域网中的设备…");
            });
            ui.add_space(6.0);
        }
        if let Some(error) = error {
            cards::banner(ui, &error, Tone::Warning);
        }
        if found.is_empty() {
            if !first_scan {
                empty_card(ui, "附近没有发现设备", "");
            }
            return;
        }
        let own = store::own_devices_account()
            .map(|account| store::own_devices(&account))
            .unwrap_or_default();
        let shared = self
            .devices
            .as_ref()
            .map(|list| list.shared.clone())
            .unwrap_or_default();
        // Devices this one trusts (the host side, in this process or the
        // background service).
        let trusting = self
            .direct_host_state()
            .map(|state| state.trusted)
            .unwrap_or_default();
        let pairing = self.direct.pairing_active();
        enum Pick {
            Key(String, String),
            Server(String),
            Code(String, String),
            Pair(String, String, String),
        }
        let mut picked = None;
        for host in found {
            let announce = &host.announce;
            let target = host.target().to_string();
            let methods = |m: &str| announce.methods.iter().any(|x| x == m);
            let friend = shared.iter().find(|d| d.device_id == announce.device_id);
            let owner_chip;
            let mut chips: Vec<(&str, Color32)> = Vec::new();
            // It lets this device in by its key: no code, no confirmation.
            let admits = store::known_host(&announce.device_id).is_some_and(|h| h.trusted);
            let trusted = trusting.iter().find(|d| d.device_id == announce.device_id);
            let paired = trusted.is_some_and(|d| d.paired) || (admits && trusted.is_some());
            let own_device = own.iter().any(|d| d.device_id == announce.device_id);
            if own_device {
                chips.push(("本账号", GREEN));
            } else if let Some(device) = friend {
                owner_chip = format!(
                    "{} 共享",
                    device
                        .owner
                        .as_ref()
                        .map_or("好友", |o| o.display_name())
                );
                chips.push((owner_chip.as_str(), AMBER));
            } else if paired {
                chips.push(("已配对", GREEN));
            } else if admits {
                chips.push(("对方信任本机", BLUE));
            } else {
                chips.push((
                    if methods(farwing_protocol::direct::method::ASSIST) {
                        "需要访问码或确认"
                    } else {
                        "未开启访问码"
                    },
                    MUTED,
                ));
            }
            let accepts_code = methods(farwing_protocol::direct::method::ASSIST);
            device_card(
                ui,
                &announce.device_id,
                &announce.name,
                &chips,
                &[
                    format!("{target} · Farwing {}", announce.version),
                    format!("ID {}", crate::account::api::device_id_text(&announce.device_id)),
                ],
                |ui| {
                    if own_device || admits {
                        if ui.add(primary("连接")).clicked() {
                            picked = Some(Pick::Key(target.clone(), announce.name.clone()));
                        }
                    } else if let Some(device) = friend {
                        // It takes friends' keys directly (also while the
                        // server cannot be reached); otherwise through it.
                        let direct = methods(farwing_protocol::direct::method::FRIEND);
                        if ui.add(primary("连接")).clicked() {
                            picked = Some(if direct {
                                Pick::Key(target.clone(), announce.name.clone())
                            } else {
                                Pick::Server(device.device_id.clone())
                            });
                        }
                    } else if ui.add_enabled(accepts_code, primary("连接")).clicked() {
                        picked = Some(Pick::Code(target.clone(), announce.name.clone()));
                    }
                    if !own_device
                        && friend.is_none()
                        && !paired
                        && ui
                            .add_enabled(!pairing, crate::ui::controls::secondary("配对"))
                            .clicked()
                    {
                        // Two devices may share a name: show which one.
                        picked = Some(Pick::Pair(
                            target.clone(),
                            announce.device_id.clone(),
                            format!(
                                "{}（{}）",
                                announce.name,
                                crate::account::api::device_id_text(&announce.device_id)
                            ),
                        ));
                    }
                },
            );
        }
        match picked {
            Some(Pick::Key(target, name)) => self.connect_address(&target, "", false, Some(&name)),
            Some(Pick::Server(id)) => {
                self.selected_device_id = Some(id);
                self.start_viewer();
            }
            Some(Pick::Code(target, name)) => self.ask_for_code(target, &name),
            Some(Pick::Pair(target, device_id, name)) => match crate::transport::direct::Target::parse(&target) {
                Ok(target) => {
                    self.direct.error = None;
                    self.direct.pair(ui.ctx(), target, device_id, name);
                }
                Err(error) => self.direct.error = Some(format!("{error:#}")),
            },
            None => {}
        }
    }

    /// Finish a pairing whatever page is shown: once both users agreed the
    /// host trusts this device, so trust it back.
    pub(super) fn finish_pairing(&mut self, ctx: &egui::Context) {
        let outcome = {
            let mut slot = self.direct.pairing.lock().unwrap_or_else(|e| e.into_inner());
            if slot.as_ref().is_none_or(|p| p.outcome.is_none()) {
                return;
            }
            slot.take().map(|p| (p.name, p.outcome, p.cancel))
        };
        let Some((name, Some(outcome), cancel)) = outcome else {
            return;
        };
        match outcome {
            Ok(host) => {
                let done = format!("已与 {} 配对", host.name);
                self.direct.perform(
                    ctx,
                    self.host.as_ref(),
                    crate::transport::direct::managed::Action::Trust {
                        device_id: host.device_id,
                        public_key: host.public_key,
                        name: host.name,
                        grants: farwing_protocol::caps::all(),
                        paired: true,
                    },
                    done,
                );
            }
            Err(_) if cancel.is_cancelled() => self.direct.note("已取消配对"),
            Err(error) => self.direct.error = Some(format!("与 {name} 配对失败：{error}")),
        }
    }

    fn pairing_card(&mut self, ui: &mut egui::Ui) {
        let state = {
            let slot = self.direct.pairing.lock().unwrap_or_else(|e| e.into_inner());
            slot.as_ref()
                .filter(|p| p.outcome.is_none())
                .map(|p| (p.name.clone(), p.code.clone(), p.agree.is_none(), p.cancel.clone()))
        };
        let Some((name, code, agreed, cancel)) = state else {
            return;
        };
        let mut stop = false;
        let mut agree = false;
        cards::card().show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal(|ui| {
                ui.vertical(|ui| {
                    ui.label(RichText::new(format!("正在与 {name} 配对")).strong());
                    match &code {
                        Some(code) => {
                            let split = code.len().min(3);
                            ui.label(
                                RichText::new(format!("{} {}", &code[..split], &code[split..]))
                                    .size(theme::DIALOG_TITLE)
                                    .strong()
                                    .color(BLUE),
                            );
                            detail(
                                ui,
                                if agreed {
                                    "等待对方确认…"
                                } else {
                                    "确认对方显示相同的配对码"
                                },
                            );
                        }
                        None => detail(ui, "正在连接对方…"),
                    }
                });
                ui.with_layout(egui::Layout::right_to_left(Align::Center), |ui| {
                    stop = ui.add(crate::ui::controls::secondary("取消")).clicked();
                    if code.is_some() && !agreed {
                        agree = ui.add(primary("配对")).clicked();
                    }
                });
            });
        });
        ui.add_space(8.0);
        if agree {
            self.direct.agree_pairing();
        }
        if stop {
            cancel.cancel();
        }
    }

    /// Devices connected to before (this device's own record, and the
    /// account's assistance history on the server) and favourites.
    fn recent_tab(&mut self, ui: &mut egui::Ui, access: Access) {
        let account = access == Access::Account;
        // Devices this one trusts: with one that trusts this device too, paired.
        let trusting: Vec<String> = self
            .direct_host_state()
            .map(|state| state.trusted.into_iter().map(|d| d.device_id).collect())
            .unwrap_or_default();
        let available = !self.assist.busy && !self.logout_pending && !self.mutation_pending;
        let lists = self.assist.lists.clone().unwrap_or_default();
        if account {
            let mut add = false;
            cards::section_heading(ui, "收藏", Some(lists.favorites.len()), |ui| {
                add = ui
                    .add_enabled(available, crate::ui::controls::secondary("添加收藏"))
                    .clicked();
            });
            if add {
                self.assist.favorite_editor = Some(FavoriteEditor {
                    id: String::new(),
                    remark: String::new(),
                    editing: false,
                    code: String::new(),
                    code_changed: false,
                });
            }
            if lists.favorites.is_empty() {
                empty_card(
                    ui,
                    if self.assist.loading && self.assist.lists.is_none() {
                        "正在读取收藏…"
                    } else {
                        "还没有收藏"
                    },
                    "",
                );
            }
            let mut favorites = lists.favorites.clone();
            favorites.sort_by(|a, b| {
                crate::account::api::list_order(
                    a.title(),
                    &a.publisher_device_id,
                    b.title(),
                    &b.publisher_device_id,
                )
                .then_with(|| a.connect_id.cmp(&b.connect_id))
            });
            let mut connect = None;
            for item in &favorites {
                let id = crate::account::api::key_id_text(&item.public_key);
                let mut edit = false;
                let mut remove = false;
                device_card(
                    ui,
                    &item.publisher_device_id,
                    item.title(),
                    &[("收藏", AMBER)],
                    &[format!(
                        "短号 {} · ID {id}",
                        assist_components::grouped_id(&item.connect_id)
                    )],
                    |ui| {
                        if ui.add_enabled(available, primary("连接")).clicked() {
                            connect = Some(item.connect_id.clone());
                        }
                        remove = ui
                            .add_enabled(available, crate::ui::controls::secondary("取消收藏"))
                            .clicked();
                        edit = ui
                            .add_enabled(available, crate::ui::controls::secondary("编辑"))
                            .clicked();
                    },
                );
                if edit {
                    self.assist.favorite_editor = Some(FavoriteEditor {
                        id: item.connect_id.clone(),
                        remark: item.remark.clone(),
                        editing: true,
                        code: item.saved_code.clone(),
                        code_changed: false,
                    });
                }
                if remove {
                    self.assist.delete_prompt =
                        Some(DeletePrompt::Device(SavedKind::Favorites, item.clone()));
                }
            }
            if let Some(number) = connect {
                self.request_assist_connect(number, String::new());
            }
        }
        // This device's record and the account's history, once per device.
        let mut rows: Vec<RecentRow> = Vec::new();
        for host in store::known_hosts() {
            rows.push(RecentRow {
                title: host.name.clone(),
                device_id: Some(host.device_id.clone()),
                short_number: (!host.connect_id.is_empty()).then(|| host.connect_id.clone()),
                address: (!host.address.is_empty()).then(|| host.address.clone()),
                trusted: host.trusted,
                last: host.last_seen,
                saved: None,
            });
        }
        if account {
            for item in &lists.recent {
                let id = store::device_id_of(&item.public_key);
                if let Some(row) = rows
                    .iter_mut()
                    .find(|row| id.is_some() && row.device_id == id)
                {
                    row.short_number = Some(item.connect_id.clone());
                    row.saved = Some(item.clone());
                    row.last = row.last.max(item.last_connected_at.unwrap_or(0));
                    continue;
                }
                rows.push(RecentRow {
                    title: item.title().to_owned(),
                    device_id: id,
                    short_number: Some(item.connect_id.clone()),
                    address: None,
                    trusted: false,
                    last: item.last_connected_at.unwrap_or(0),
                    saved: Some(item.clone()),
                });
            }
        }
        rows.sort_by(|a, b| {
            crate::account::api::list_order(
                &a.title,
                a.device_id.as_deref().unwrap_or_default(),
                &b.title,
                b.device_id.as_deref().unwrap_or_default(),
            )
            .then_with(|| a.address.cmp(&b.address))
        });
        let mut clear = false;
        cards::section_heading(ui, "最近连接", Some(rows.len()), |ui| {
            if account && !lists.recent.is_empty() {
                clear = ui
                    .add_enabled(available, crate::ui::controls::secondary("清空服务器记录"))
                    .clicked();
            }
        });
        if clear {
            self.assist.delete_prompt = Some(DeletePrompt::Recent);
        }
        if rows.is_empty() {
            empty_card(ui, "还没有连接记录", "");
            return;
        }
        enum Pick {
            Connect(RecentRow),
            Favorite(SavedDevice),
            Forget(String, Option<SavedDevice>),
        }
        let mut picked = None;
        for row in rows {
            let mut chips: Vec<(&str, Color32)> = Vec::new();
            if row.trusted {
                let paired = row.device_id.as_ref().is_some_and(|id| trusting.contains(id));
                chips.push(if paired { ("已配对", GREEN) } else { ("对方信任本机", BLUE) });
            }
            if row.saved.as_ref().is_some_and(|s| s.is_favorite) {
                chips.push(("收藏", AMBER));
            }
            let mut first = Vec::new();
            if let Some(address) = &row.address {
                first.push(format!("地址 {address}"));
            }
            if let Some(number) = &row.short_number {
                first.push(format!("短号 {}", assist_components::grouped_id(number)));
            }
            first.push(format!("最近 {}", cards::when(row.last)));
            let mut lines = vec![first.join(" · ")];
            if let Some(id) = &row.device_id {
                lines.push(format!("ID {}", crate::account::api::device_id_text(id)));
            }
            let key = row
                .device_id
                .clone()
                .or_else(|| row.short_number.clone())
                .unwrap_or_default();
            let title = if row.title.is_empty() {
                row.short_number.clone().unwrap_or_else(|| "未命名设备".into())
            } else {
                row.title.clone()
            };
            let mut action = None;
            device_card(ui, &key, &title, &chips, &lines, |ui| {
                if ui.add_enabled(available, primary("连接")).clicked() {
                    action = Some(0);
                }
                if ui
                    .add_enabled(available, crate::ui::controls::secondary("删除"))
                    .clicked()
                {
                    action = Some(1);
                }
                if let Some(saved) = &row.saved
                    && account
                    && !saved.is_favorite
                    && ui
                        .add_enabled(available, crate::ui::controls::secondary("收藏"))
                        .clicked()
                {
                    action = Some(2);
                }
            });
            match action {
                Some(0) => picked = Some(Pick::Connect(row)),
                Some(1) => picked = Some(Pick::Forget(key, row.saved.clone())),
                Some(2) => picked = row.saved.clone().map(Pick::Favorite),
                _ => {}
            }
        }
        match picked {
            Some(Pick::Connect(row)) => {
                match (&row.device_id, &row.address, row.trusted) {
                    // Reached directly before and it did not trust this
                    // device: it needs its access code again.
                    (_, Some(address), false) if row.short_number.is_none() => {
                        let name = if row.title.is_empty() { address.clone() } else { row.title.clone() };
                        self.ask_for_code(address.clone(), &name);
                    }
                    (Some(id), _, _) if row.address.is_some() || row.short_number.is_none() => {
                        self.connect_device_id(id, "", false);
                    }
                    _ => {
                        if let Some(number) = row.short_number {
                            self.request_assist_connect(number, String::new());
                        }
                    }
                }
            }
            Some(Pick::Favorite(item)) => {
                self.assist.favorite_editor = Some(FavoriteEditor {
                    id: item.connect_id.clone(),
                    remark: item.remark.clone(),
                    editing: false,
                    code: item.saved_code.clone(),
                    code_changed: false,
                });
            }
            Some(Pick::Forget(key, saved)) => {
                if let Err(error) = store::forget_host(&key) {
                    self.direct.error = Some(format!("删除失败：{error:#}"));
                }
                if let Some(item) = saved {
                    self.assist.delete_prompt = Some(DeletePrompt::Device(SavedKind::Recent, item));
                } else {
                    self.direct.note("已删除这条记录");
                }
            }
            None => {}
        }
    }
}
