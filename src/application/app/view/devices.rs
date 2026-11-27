//! The "我的设备" tab: this account's computers, then this device and the
//! account's other logins. Selection and actions always use device IDs.
use super::device_list::{self, Order, StateFilter, Tile};
use super::*;

/// Search, filter and order of one device list.
#[derive(Default)]
pub(super) struct TabState {
    pub search: String,
    pub filter: StateFilter,
    pub order: Order,
}

#[derive(Default)]
pub(super) struct ListUi {
    pub mine: TabState,
    pub shared: TabState,
    pub nearby: String,
    pub recent: String,
    /// The section with this device and the account's other logins.
    others_open: bool,
}

struct Entry {
    device: DeviceInfo,
    current: bool,
    viewing: bool,
    own_session: bool,
    connect_issue: Option<String>,
    status: crate::application::app::device_status::DeviceStatus,
}

enum Action {
    Details,
    Connect,
    Audio,
}

impl DeviceCenterApp {
    fn device_entries(&self) -> Option<Vec<Entry>> {
        // Friends' devices are listed under each friend.
        let devices = self
            .devices
            .as_ref()?
            .devices
            .iter()
            .filter(|d| self.show_in_watching_list(d));
        Some(
            devices
                .map(|device| {
                    let id = device.device_id.as_str();
                    Entry {
                        device: device.clone(),
                        current: self
                            .catalog
                            .as_ref()
                            .is_some_and(|c| c.groups.current_device_id == id),
                        viewing: self.is_viewing_target(id),
                        own_session: self.viewer_for_device(id).is_some(),
                        connect_issue: self.viewer_action_issue(device),
                        status: self.device_status(device),
                    }
                })
                .collect(),
        )
    }

    /// The account's devices that are not offered for connecting: this
    /// device, phones and tablets. Their details rename or remove them.
    fn other_devices(&self) -> Vec<(DeviceInfo, bool)> {
        let Some(catalog) = &self.catalog else {
            return Vec::new();
        };
        let mut devices: Vec<_> = catalog
            .groups
            .entries()
            .filter(|(_, d)| !self.show_in_watching_list(d))
            .map(|(_, d)| (d.clone(), d.device_id == catalog.groups.current_device_id))
            .collect();
        // This device first, then by name.
        devices.sort_by(|(a, a_current), (b, b_current)| b_current.cmp(a_current).then_with(|| a.list_order(b)));
        devices
    }

    /// The "我的设备" tab of the device page: this account's computers.
    pub(super) fn own_devices_tab(&mut self, ui: &mut egui::Ui) {
        let rows = self.device_entries();
        let text = &t().center;
        self.alert(ui);
        crate::ui::controls::observe_notice(
            ui.ctx(),
            "catalog-error",
            text.device_list_not_updated,
            crate::ui::controls::DialogIcon::Warning,
            self.catalog_error.as_deref(),
        );
        let Some(mut rows) = rows else {
            match self.blocker() {
                Some(blocker) => self.blocked_state(ui, blocker),
                None => device_list::empty(ui, text.loading_devices),
            }
            return;
        };
        {
            let all: Vec<&DeviceInfo> = rows.iter().map(|e| &e.device).collect();
            let state = &mut self.center_ui.device_list.mine;
            device_list::toolbar(ui, &mut state.search, |ui| {
                Order::combo(ui, "mine-order", &mut state.order);
                StateFilter::combo(ui, "mine-filter", &mut state.filter, &all);
            });
        }
        let count = rows.len();
        let state = &self.center_ui.device_list.mine;
        let query = state.search.trim().to_lowercase();
        let (filter, order) = (state.filter, state.order);
        rows.retain(|e| {
            let d = &e.device;
            filter.keeps(d)
                && device_list::matches(
                    &query,
                    &[display_alias(d), &d.device_id, &d.platform_label(), &d.version_name],
                )
        });
        rows.sort_by(|a, b| order.compare(&a.device, &b.device));
        let others = self.other_devices();
        let mut picked = None;
        crate::ui::controls::page_scroll("device-list").show(ui, |ui| {
            if rows.is_empty() {
                device_list::empty(
                    ui,
                    if count == 0 {
                        text.no_computers
                    } else {
                        text.no_matching_devices
                    },
                );
            }
            for entry in &rows {
                let selected = self.selected_device_id.as_deref() == Some(&entry.device.device_id);
                if let Some(action) = ui
                    .push_id(&entry.device.device_id, |ui| {
                        own_row(ui, entry, selected, &mut self.center_ui.wallpapers)
                    })
                    .inner
                {
                    picked = Some((entry.device.device_id.clone(), action));
                }
            }
            if !others.is_empty() {
                ui.add_space(10.0);
                if let Some(id) = self.other_devices_section(ui, &others) {
                    picked = Some((id, Action::Details));
                }
            }
        });
        if let Some((id, action)) = picked {
            match action {
                Action::Details => self.open_details(id),
                Action::Connect => {
                    self.selected_device_id = Some(id);
                    self.start_viewer();
                }
                Action::Audio => {
                    self.selected_device_id = Some(id);
                    self.start_viewer_mode(true);
                }
            }
        }
    }

    /// This device and the logins that are not computers to connect to,
    /// folded by default. Returns the device whose details were asked for.
    fn other_devices_section(
        &mut self,
        ui: &mut egui::Ui,
        devices: &[(DeviceInfo, bool)],
    ) -> Option<String> {
        let open = &mut self.center_ui.device_list.others_open;
        let text = &t().center;
        let (rect, response) =
            ui.allocate_exact_size(vec2(ui.available_width(), 28.0), Sense::click());
        let response = response.on_hover_cursor(egui::CursorIcon::PointingHand);
        response.widget_info(|| {
            egui::WidgetInfo::selected(
                egui::WidgetType::CollapsingHeader,
                true,
                *open,
                text.this_and_other_devices,
            )
        });
        if response.clicked() {
            *open = !*open;
        }
        let color = if response.hovered() { TEXT } else { MUTED };
        let c = rect.left_center() + vec2(7.0, 0.0);
        let stroke = Stroke::new(1.5, color);
        let points = if *open {
            vec![c + vec2(-4.0, -2.0), c + vec2(0.0, 2.0), c + vec2(4.0, -2.0)]
        } else {
            vec![c + vec2(-2.0, -4.0), c + vec2(2.0, 0.0), c + vec2(-2.0, 4.0)]
        };
        ui.painter().add(egui::Shape::line(points, stroke));
        let title = ui.painter().text(
            rect.left_center() + vec2(20.0, 0.0),
            egui::Align2::LEFT_CENTER,
            text.this_and_other_devices,
            FontId::proportional(crate::ui::theme::BODY),
            color,
        );
        ui.painter().text(
            title.right_center() + vec2(8.0, 0.0),
            egui::Align2::LEFT_CENTER,
            (text.device_count)(devices.len()),
            FontId::proportional(crate::ui::theme::SMALL),
            MUTED,
        );
        if !*open {
            return None;
        }
        let mut picked = None;
        for (device, current) in devices {
            let texture = self.center_ui.wallpapers.texture(
                ui.ctx(),
                &device.device_id,
                &device.wallpaper_url,
            );
            let status = self.device_status(device);
            let chips: Vec<(&str, Color32)> = if *current {
                vec![(text.page_this_device, theme::ACCENT)]
            } else {
                Vec::new()
            };
            let detail = detail_line(device);
            let mut details = false;
            let clicked = ui
                .push_id(&device.device_id, |ui| {
                    device_list::row(
                        ui,
                        &device_list::Row {
                            title: display_alias(device),
                            tile: Tile::of(texture.as_ref(), device.platform),
                            status: (!*current).then(|| (status.label(), status.color())),
                            chips: &chips,
                            extra_badges: None,
                            detail: &detail,
                            selected: Some(false),
                        },
                        |ui| {
                            details = ui
                                .add(crate::ui::controls::secondary(t().common.details))
                                .clicked();
                        },
                    )
                })
                .inner;
            if clicked || details {
                picked = Some(device.device_id.clone());
            }
        }
        picked
    }
}

/// "ID … · Windows · 0.1.0": the line under a device's name.
pub(super) fn detail_line(device: &DeviceInfo) -> String {
    format!(
        "ID {} · {} · {}",
        crate::account::api::device_id_text(&device.device_id),
        device.platform_label(),
        if device.version_name.is_empty() {
            t().center.unknown_version
        } else {
            &device.version_name
        }
    )
}

fn own_row(
    ui: &mut egui::Ui,
    entry: &Entry,
    selected: bool,
    wallpapers: &mut crate::application::wallpaper::Wallpapers,
) -> Option<Action> {
    let device = &entry.device;
    let texture = wallpapers.texture(ui.ctx(), &device.device_id, &device.wallpaper_url);
    let text = &t().center;
    let chips: Vec<(&str, Color32)> = if entry.current {
        vec![(text.page_this_device, theme::ACCENT)]
    } else {
        Vec::new()
    };
    let detail = detail_line(device);
    let mut action = None;
    let clicked = device_list::row(
        ui,
        &device_list::Row {
            title: display_alias(device),
            tile: Tile::Wallpaper {
                texture: texture.as_ref(),
                platform: device.platform,
            },
            status: Some((entry.status.label(), entry.status.color())),
            chips: &chips,
            extra_badges: None,
            detail: &detail,
            selected: Some(selected),
        },
        |ui| {
            if entry.viewing {
                let connect = ui.add_enabled(
                    entry.connect_issue.is_none(),
                    primary(if entry.own_session {
                        text.opened
                    } else {
                        t().common.connect
                    }),
                );
                let audio = ui
                    .add_enabled_ui(entry.connect_issue.is_none(), |ui| {
                        crate::ui::controls::audio_output_button(
                            ui,
                            text.audio_only_connect,
                            crate::ui::theme::CONTROL_HEIGHT,
                        )
                    })
                    .inner;
                if let Some(issue) = &entry.connect_issue {
                    connect.clone().on_disabled_hover_text(issue);
                    audio.clone().on_disabled_hover_text(issue);
                }
                if connect.clicked() {
                    action = Some(Action::Connect);
                }
                if audio.clicked() {
                    action = Some(Action::Audio);
                }
            }
            if ui
                .add(crate::ui::controls::secondary(t().common.details))
                .clicked()
            {
                action = Some(Action::Details);
            }
        },
    );
    action.or_else(|| clicked.then_some(Action::Details))
}
