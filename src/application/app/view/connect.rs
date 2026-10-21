//! The "设备" page (`docs/access-model.md` §3): one connect bar for a
//! device ID, a short number or an address, and one list of devices,
//! filtered — this account's computers, devices friends share, devices on
//! the local network, and devices connected to before. A device is known by
//! the same ID in every list; how it is reached is chosen automatically.
use super::access::{Blocker, server_button};
use super::cards::{self, Tone};
use super::device_list::{self, Order, StateFilter, Tile};
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
        return Err(t().center.enter_target.into());
    }
    if let Ok(number) = crate::account::assist::normalize_connect_id(text) {
        return Ok(Target::ShortNumber(number));
    }
    if let Some(id) = crate::account::api::parse_device_id(text) {
        return Ok(Target::DeviceId(id));
    }
    crate::transport::direct::Target::parse(text)
        .map(|target| Target::Address(target.to_string()))
        .map_err(|_| t().center.unrecognized_target.into())
}

fn detail(ui: &mut egui::Ui, text: &str) {
    ui.add(egui::Label::new(RichText::new(text).size(theme::SMALL).color(MUTED)).wrap());
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
        let blocker = self.blocker();
        let mut refresh = false;
        cards::page_header(
            ui,
            Page::Devices.title(),
            "",
            |ui| {
                self.active_view(ui);
                let busy = match tab {
                    DeviceTab::Mine | DeviceTab::Friends => blocker.is_none() && self.refresh_pending,
                    // Searching goes on quietly; only the first round shows.
                    DeviceTab::Nearby => {
                        let lan = self.direct.lan();
                        lan.scanning && lan.found.is_none()
                    }
                    DeviceTab::Recent => blocker.is_none() && self.assist.loading,
                };
                // What comes from the server refreshes while it answers.
                let enabled = match tab {
                    DeviceTab::Mine | DeviceTab::Friends | DeviceTab::Recent => blocker.is_none(),
                    DeviceTab::Nearby => true,
                };
                if busy {
                    ui.add_sized(
                        [theme::CONTROL_HEIGHT, theme::CONTROL_HEIGHT],
                        egui::Spinner::new().size(18.0),
                    );
                } else if enabled {
                    refresh = assist_components::glyph(ui, Icon::Refresh, t().common.refresh, MUTED)
                        .clicked();
                } else {
                    ui.allocate_exact_size(
                        vec2(theme::CONTROL_HEIGHT, theme::CONTROL_HEIGHT),
                        Sense::hover(),
                    );
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
        let text = &t().center;
        let labels = [
            label(text.tab_mine, counts[0]),
            label(text.tab_friends, counts[1]),
            label(text.tab_nearby, counts[2]),
            label(text.tab_recent, counts[3]),
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
                self.blocked_state(ui, blocker.unwrap_or(Blocker::SignedOut));
            }
            DeviceTab::Mine => self.own_devices_tab(ui),
            DeviceTab::Friends => self.friend_devices_tab(ui),
            DeviceTab::Nearby => self.nearby_tab(ui),
            DeviceTab::Recent => self.recent_tab(ui, access),
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
        let text = &t().center;
        cards::card().show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal(|ui| {
                let width = (ui.available_width() - 92.0).max(200.0);
                let response = ui.add_sized(
                    [width, crate::ui::controls::HEIGHT],
                    singleline_input(&mut self.direct.target)
                        .hint_text(text.target_hint),
                );
                if response.changed() {
                    self.direct.error = None;
                }
                if response.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                    connect = true;
                }
                if ui
                    .add_enabled(!self.direct.target.trim().is_empty(), primary(t().common.connect))
                    .clicked()
                {
                    connect = true;
                }
            });
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                ui.label(RichText::new(text.access_code).size(theme::SMALL).color(MUTED));
                let response = ui
                    .add_enabled_ui(!self.direct.ask, |ui| {
                        ui.add_sized(
                            [220.0, crate::ui::controls::HEIGHT],
                            singleline_input(&mut self.direct.code)
                                .password(true)
                                .hint_text(t().common.optional),
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
                ui.checkbox(&mut self.direct.ask, text.ask_for_approval);
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
                    self.direct.error = Some(t().center.short_number_needs_server.into());
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
                self.direct.error = Some((t().center.device_not_found)(
                    &crate::account::api::device_id_text(id),
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
        self.direct.note((t().center.needs_access_code)(name));
    }

    fn friend_devices_tab(&mut self, ui: &mut egui::Ui) {
        let Some(shared) = self.devices.as_ref().map(|list| list.shared.clone()) else {
            match self.blocker() {
                Some(blocker) => self.blocked_state(ui, blocker),
                None => device_list::empty(ui, t().center.loading_devices),
            }
            return;
        };
        {
            let all: Vec<&DeviceInfo> = shared.iter().collect();
            let state = &mut self.center_ui.device_list.shared;
            device_list::toolbar(ui, &mut state.search, |ui| {
                Order::combo(ui, "shared-order", &mut state.order);
                StateFilter::combo(ui, "shared-filter", &mut state.filter, &all);
            });
        }
        let text = &t().center;
        let owner_of = |device: &DeviceInfo| {
            device
                .owner
                .as_ref()
                .map_or_else(|| text.friend.to_owned(), |o| o.display_name().to_owned())
        };
        let state = &self.center_ui.device_list.shared;
        let query = state.search.trim().to_lowercase();
        let (filter, order) = (state.filter, state.order);
        let total = shared.len();
        let mut shared: Vec<DeviceInfo> = shared
            .into_iter()
            .filter(|d| {
                filter.keeps(d)
                    && device_list::matches(
                        &query,
                        &[display_alias(d), &d.device_id, &d.platform_label(), &owner_of(d)],
                    )
            })
            .collect();
        shared.sort_by(|a, b| order.compare(a, b));
        // Grouped by owner, owners by name; devices keep the chosen order.
        let mut owners: Vec<(String, Vec<DeviceInfo>)> = Vec::new();
        for device in shared {
            let owner = owner_of(&device);
            match owners.iter_mut().find(|(name, _)| *name == owner) {
                Some((_, list)) => list.push(device),
                None => owners.push((owner, vec![device])),
            }
        }
        owners.sort_by(|a, b| a.0.to_lowercase().cmp(&b.0.to_lowercase()).then_with(|| a.0.cmp(&b.0)));
        let nearby: Vec<String> = self
            .direct
            .nearby()
            .into_iter()
            .map(|h| h.announce.device_id)
            .collect();
        let mut picked = None;
        crate::ui::controls::page_scroll("friend-devices").show(ui, |ui| {
            if owners.is_empty() {
                device_list::empty(
                    ui,
                    if total == 0 {
                        text.no_shared_devices
                    } else {
                        text.no_matching_devices
                    },
                );
            }
            for (index, (owner, devices)) in owners.iter().enumerate() {
                device_list::heading(
                    ui,
                    &(text.shared_by)(owner),
                    Some(devices.len()),
                    index == 0,
                    |_| {},
                );
                for device in devices {
                    let on_lan = nearby.contains(&device.device_id);
                    if let Some(action) = self.shared_row(ui, device, on_lan, false) {
                        picked = Some((device.device_id.clone(), action));
                    }
                }
            }
        });
        match picked {
            Some((id, super::friends::DeviceAction::Connect)) => {
                self.selected_device_id = Some(id);
                self.start_viewer();
            }
            Some((id, super::friends::DeviceAction::Details)) => self.open_details(id),
            None => {}
        }
    }

    /// One device a friend shares, as in every other device list (`inset`:
    /// inside a card).
    pub(super) fn shared_row(
        &mut self,
        ui: &mut egui::Ui,
        device: &DeviceInfo,
        on_lan: bool,
        inset: bool,
    ) -> Option<super::friends::DeviceAction> {
        use super::friends::DeviceAction;
        let status = self.device_status(device);
        let issue = self.viewer_action_issue(device);
        let open = self.viewer_for_device(&device.device_id).is_some();
        let selected = self.selected_device_id.as_deref() == Some(&device.device_id);
        let texture =
            self.center_ui
                .wallpapers
                .texture(ui.ctx(), &device.device_id, &device.wallpaper_url);
        let chips: Vec<(&str, Color32)> = if on_lan {
            vec![(t().center.on_lan, GREEN)]
        } else {
            Vec::new()
        };
        let detail = super::devices::detail_line(device);
        let row = device_list::Row {
            title: display_alias(device),
            tile: Tile::Wallpaper {
                texture: texture.as_ref(),
                platform: device.platform,
            },
            status: Some((status.label(), status.color())),
            chips: &chips,
            grants: Some(&device.granted),
            detail: &detail,
            selected,
        };
        let mut action = None;
        let actions = |ui: &mut egui::Ui| {
            let connect = ui.add_enabled(
                issue.is_none(),
                primary(if open {
                    t().center.opened
                } else {
                    t().common.connect
                }),
            );
            let connect = match &issue {
                Some(issue) => connect.on_disabled_hover_text(issue),
                None => connect,
            };
            if connect.clicked() {
                action = Some(DeviceAction::Connect);
            }
            if ui
                .add(crate::ui::controls::secondary(t().common.details))
                .clicked()
            {
                action = Some(DeviceAction::Details);
            }
        };
        let clicked = ui
            .push_id(&device.device_id, |ui| {
                if inset {
                    device_list::inset_row(ui, &row, actions)
                } else {
                    device_list::row(ui, &row, actions)
                }
            })
            .inner;
        action.or_else(|| clicked.then_some(DeviceAction::Details))
    }

    fn nearby_tab(&mut self, ui: &mut egui::Ui) {
        device_list::toolbar(ui, &mut self.center_ui.device_list.nearby, |_| {});
        let (first_scan, error) = {
            let lan = self.direct.lan();
            (lan.scanning && lan.found.is_none(), lan.error.clone())
        };
        let query = self.center_ui.device_list.nearby.trim().to_lowercase();
        let all = self.direct.nearby();
        let total = all.len();
        let found: Vec<_> = all
            .into_iter()
            .filter(|h| {
                device_list::matches(
                    &query,
                    &[&h.announce.name, &h.announce.device_id, &h.target().to_string()],
                )
            })
            .collect();
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
        let text = &t().center;
        enum Pick {
            Key(String, String),
            Server(String),
            Code(String, String),
            Pair(String, String, String),
        }
        let mut picked = None;
        crate::ui::controls::page_scroll("nearby-devices").show(ui, |ui| {
            if let Some(error) = &error {
                cards::banner(ui, error, Tone::Warning);
                ui.add_space(8.0);
            }
            if found.is_empty() {
                device_list::empty(
                    ui,
                    if first_scan {
                        text.searching_lan
                    } else if total == 0 {
                        text.no_nearby_devices
                    } else {
                        text.no_matching_devices
                    },
                );
                return;
            }
            for host in &found {
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
                    chips.push((text.this_account, GREEN));
                } else if let Some(device) = friend {
                    owner_chip = (text.shared_by)(
                        device
                            .owner
                            .as_ref()
                            .map_or(text.friend, |o| o.display_name()),
                    );
                    chips.push((owner_chip.as_str(), AMBER));
                } else if paired {
                    chips.push((text.paired, GREEN));
                } else if admits {
                    chips.push((text.trusts_this_device, BLUE));
                } else {
                    chips.push((
                        if methods(farwing_protocol::direct::method::ASSIST) {
                            text.code_or_approval
                        } else {
                            text.access_code_off
                        },
                        MUTED,
                    ));
                }
                let accepts_code = methods(farwing_protocol::direct::method::ASSIST);
                let detail = format!(
                    "ID {} · {target} · Farwing {}",
                    crate::account::api::device_id_text(&announce.device_id),
                    announce.version
                );
                ui.push_id(&announce.device_id, |ui| {
                    device_list::row(
                        ui,
                        &device_list::Row {
                            title: &announce.name,
                            tile: Tile::Platform(announce.platform),
                            status: Some((t().common.online, GREEN)),
                            chips: &chips,
                            grants: None,
                            detail: &detail,
                            selected: false,
                        },
                        |ui| {
                            if own_device || admits {
                                if ui.add(primary(t().common.connect)).clicked() {
                                    picked = Some(Pick::Key(target.clone(), announce.name.clone()));
                                }
                            } else if let Some(device) = friend {
                                // It takes friends' keys directly (also while the
                                // server cannot be reached); otherwise through it.
                                let direct = methods(farwing_protocol::direct::method::FRIEND);
                                if ui.add(primary(t().common.connect)).clicked() {
                                    picked = Some(if direct {
                                        Pick::Key(target.clone(), announce.name.clone())
                                    } else {
                                        Pick::Server(device.device_id.clone())
                                    });
                                }
                            } else if ui.add_enabled(accepts_code, primary(t().common.connect)).clicked() {
                                picked = Some(Pick::Code(target.clone(), announce.name.clone()));
                            }
                            if !own_device
                                && friend.is_none()
                                && !paired
                                && ui
                                    .add_enabled(!pairing, crate::ui::controls::secondary(text.pair))
                                    .clicked()
                            {
                                // Two devices may share a name: show which one.
                                picked = Some(Pick::Pair(
                                    target.clone(),
                                    announce.device_id.clone(),
                                    (text.name_with_id)(
                                        &announce.name,
                                        &crate::account::api::device_id_text(&announce.device_id),
                                    ),
                                ));
                            }
                        },
                    );
                });
            }
        });
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
                let done = (t().center.paired_with)(&host.name);
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
            Err(_) if cancel.is_cancelled() => self.direct.note(t().center.pairing_canceled),
            Err(error) => {
                self.direct.error = Some((t().center.pairing_failed)(&name, &error.to_string()))
            }
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
        let text = &t().center;
        cards::card().show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal(|ui| {
                ui.vertical(|ui| {
                    ui.label(RichText::new((text.pairing_with)(&name)).strong());
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
                                    text.waiting_for_confirmation
                                } else {
                                    text.compare_pairing_code
                                },
                            );
                        }
                        None => detail(ui, text.connecting_to_peer),
                    }
                });
                ui.with_layout(egui::Layout::right_to_left(Align::Center), |ui| {
                    stop = ui.add(crate::ui::controls::secondary(t().common.cancel)).clicked();
                    if code.is_some() && !agreed {
                        agree = ui.add(primary(text.pair)).clicked();
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
        let blocker = self.blocker();
        // Devices this one trusts: with one that trusts this device too, paired.
        let trusting: Vec<String> = self
            .direct_host_state()
            .map(|state| state.trusted.into_iter().map(|d| d.device_id).collect())
            .unwrap_or_default();
        let available = !self.assist.busy && !self.logout_pending && !self.mutation_pending;
        let lists = self.assist.lists.clone().unwrap_or_default();
        let text = &t().center;
        let mut add = false;
        device_list::toolbar(ui, &mut self.center_ui.device_list.recent, |ui| {
            if account {
                add = server_button(
                    ui,
                    blocker,
                    available,
                    crate::ui::controls::secondary(text.add_favorite),
                )
                .clicked();
            }
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
        let query = self.center_ui.device_list.recent.trim().to_lowercase();
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
        let recent_total = rows.len();
        rows.retain(|row| {
            device_list::matches(
                &query,
                &[
                    &row.title,
                    row.device_id.as_deref().unwrap_or_default(),
                    row.short_number.as_deref().unwrap_or_default(),
                    row.address.as_deref().unwrap_or_default(),
                ],
            )
        });
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
        let favorites_total = favorites.len();
        favorites.retain(|item| {
            device_list::matches(
                &query,
                &[item.title(), &item.publisher_device_id, &item.connect_id],
            )
        });
        enum Pick {
            Connect(RecentRow),
            ConnectFavorite(String),
            EditFavorite(SavedDevice),
            RemoveFavorite(SavedDevice),
            Favorite(SavedDevice),
            Forget(String, Option<SavedDevice>),
            ClearHistory,
        }
        let mut picked = None;
        crate::ui::controls::page_scroll("recent-devices").show(ui, |ui| {
            if account {
                device_list::heading(ui, text.favorites, Some(favorites_total), true, |_| {});
                if favorites.is_empty() {
                    device_list::empty(
                        ui,
                        if self.assist.lists.is_none() {
                            blocker.map_or(text.loading_favorites, |b| b.reason())
                        } else if favorites_total == 0 {
                            text.no_favorites
                        } else {
                            text.no_matching_devices
                        },
                    );
                }
                for item in &favorites {
                    let detail = (text.favorite_line)(
                        &assist_components::grouped_id(&item.connect_id),
                        &crate::account::api::key_id_text(&item.public_key),
                    );
                    ui.push_id(("favorite", &item.connect_id), |ui| {
                        device_list::row(
                            ui,
                            &device_list::Row {
                                title: item.title(),
                                tile: Tile::Generic,
                                status: None,
                                chips: &[],
                                grants: None,
                                detail: &detail,
                                selected: false,
                            },
                            |ui| {
                                if server_button(ui, blocker, available, primary(t().common.connect))
                                    .clicked()
                                {
                                    picked = Some(Pick::ConnectFavorite(item.connect_id.clone()));
                                }
                                if server_button(
                                    ui,
                                    blocker,
                                    available,
                                    crate::ui::controls::secondary(text.remove_favorite),
                                )
                                .clicked()
                                {
                                    picked = Some(Pick::RemoveFavorite(item.clone()));
                                }
                                if server_button(
                                    ui,
                                    blocker,
                                    available,
                                    crate::ui::controls::secondary(text.edit),
                                )
                                .clicked()
                                {
                                    picked = Some(Pick::EditFavorite(item.clone()));
                                }
                            },
                        );
                    });
                }
            }
            device_list::heading(ui, text.recent_connections, Some(recent_total), !account, |ui| {
                if account
                    && !lists.recent.is_empty()
                    && server_button(
                        ui,
                        blocker,
                        available,
                        crate::ui::controls::secondary(text.clear_server_history),
                    )
                    .clicked()
                {
                    picked = Some(Pick::ClearHistory);
                }
            });
            if rows.is_empty() {
                device_list::empty(
                    ui,
                    if recent_total == 0 {
                        text.no_history
                    } else {
                        text.no_matching_devices
                    },
                );
                return;
            }
            for row in rows {
                let mut chips: Vec<(&str, Color32)> = Vec::new();
                if row.trusted {
                    let paired = row.device_id.as_ref().is_some_and(|id| trusting.contains(id));
                    chips.push(if paired {
                        (text.paired, GREEN)
                    } else {
                        (text.trusts_this_device, BLUE)
                    });
                }
                if row.saved.as_ref().is_some_and(|s| s.is_favorite) {
                    chips.push((text.favorite, AMBER));
                }
                let mut parts = Vec::new();
                if let Some(id) = &row.device_id {
                    parts.push(format!("ID {}", crate::account::api::device_id_text(id)));
                }
                if let Some(address) = &row.address {
                    parts.push((text.address_line)(address));
                }
                if let Some(number) = &row.short_number {
                    parts.push((text.short_number_line)(&assist_components::grouped_id(number)));
                }
                parts.push((text.last_connected)(&cards::when(row.last)));
                let detail = parts.join(" · ");
                let key = row
                    .device_id
                    .clone()
                    .or_else(|| row.short_number.clone())
                    .unwrap_or_default();
                let title = if row.title.is_empty() {
                    row.short_number.clone().unwrap_or_else(|| text.unnamed_device.into())
                } else {
                    row.title.clone()
                };
                let mut action = None;
                ui.push_id(("recent", &key), |ui| {
                    device_list::row(
                        ui,
                        &device_list::Row {
                            title: &title,
                            tile: Tile::Generic,
                            status: None,
                            chips: &chips,
                            grants: None,
                            detail: &detail,
                            selected: false,
                        },
                        |ui| {
                            if ui.add_enabled(available, primary(t().common.connect)).clicked() {
                                action = Some(0);
                            }
                            if ui
                                .add_enabled(available, crate::ui::controls::secondary(t().common.delete))
                                .clicked()
                            {
                                action = Some(1);
                            }
                            if let Some(saved) = &row.saved
                                && account
                                && !saved.is_favorite
                                && server_button(
                                    ui,
                                    blocker,
                                    available,
                                    crate::ui::controls::secondary(text.add_to_favorites),
                                )
                                .clicked()
                            {
                                action = Some(2);
                            }
                        },
                    );
                });
                match action {
                    Some(0) => picked = Some(Pick::Connect(row)),
                    Some(1) => picked = Some(Pick::Forget(key, row.saved.clone())),
                    Some(2) => picked = row.saved.clone().map(Pick::Favorite),
                    _ => {}
                }
            }
        });
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
            Some(Pick::ConnectFavorite(number)) => self.request_assist_connect(number, String::new()),
            Some(Pick::EditFavorite(item)) => {
                self.assist.favorite_editor = Some(FavoriteEditor {
                    id: item.connect_id.clone(),
                    remark: item.remark.clone(),
                    editing: true,
                    code: item.saved_code.clone(),
                    code_changed: false,
                });
            }
            Some(Pick::RemoveFavorite(item)) => {
                self.assist.delete_prompt = Some(DeletePrompt::Device(SavedKind::Favorites, item));
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
                    self.direct.error = Some((t().center.delete_failed)(&format!("{error:#}")));
                }
                if let Some(item) = saved {
                    self.assist.delete_prompt = Some(DeletePrompt::Device(SavedKind::Recent, item));
                } else {
                    self.direct.note(t().center.record_deleted);
                }
            }
            Some(Pick::ClearHistory) => self.assist.delete_prompt = Some(DeletePrompt::Recent),
            None => {}
        }
    }
}
