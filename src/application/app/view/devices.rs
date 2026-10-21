//! Shared device-list presentation. Selection and actions always use device IDs.
use super::*;

#[derive(Clone, Copy, Default, PartialEq, Eq)]
enum Filter {
    #[default]
    All,
    Online,
    Offline,
}

#[derive(Clone, Copy, Default, PartialEq, Eq)]
enum Sort {
    Status,
    #[default]
    Name,
}

impl Sort {
    fn label(self) -> &'static str {
        match self {
            Self::Status => "在线优先",
            Self::Name => "名称排序",
        }
    }
}

#[derive(Default)]
pub(super) struct ListUi {
    search: String,
    filter: Filter,
    sort: Sort,
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
        devices.sort_by(|(a, a_current), (b, b_current)| {
            b_current
                .cmp(a_current)
                .then_with(|| a.list_order(b))
        });
        devices
    }

    /// The "我的设备" tab of the device page: this account's computers.
    pub(super) fn own_devices_tab(&mut self, ui: &mut egui::Ui) {
        let rows = self.device_entries();
        let count = rows.as_ref().map_or(0, Vec::len);
        let online = rows
            .as_ref()
            .map_or(0, |r| r.iter().filter(|e| e.device.is_connected()).count());
        let offline = rows.as_ref().map_or(0, |r| {
            r.iter()
                .filter(|e| e.device.is_offline())
                .count()
        });
        let state = &mut self.center_ui.device_list;
        ui.horizontal(|ui| {
            let width = (ui.available_width() - 182.0).max(140.0);
            ui.add_sized(
                [width, 36.0],
                singleline_input(&mut state.search).hint_text("搜索设备名称、ID、系统或版本"),
            );
            if ui
                .add_enabled(!state.search.is_empty(), egui::Button::new("清除"))
                .clicked()
            {
                state.search.clear();
            }
            egui::ComboBox::from_id_salt("device-sort")
                .width(108.0)
                .selected_text(state.sort.label())
                .show_ui(ui, |ui| {
                    for sort in [Sort::Status, Sort::Name] {
                        ui.selectable_value(&mut state.sort, sort, sort.label());
                    }
                });
        });
        ui.add_space(8.0);
        ui.horizontal(|ui| {
            for (filter, label, n) in [
                (Filter::All, "全部", count),
                (Filter::Online, "在线", online),
                (Filter::Offline, "离线", offline),
            ] {
                ui.selectable_value(&mut state.filter, filter, format!("{label}  {n}"));
            }
        });
        let query = state.search.trim().to_lowercase();
        let filter = state.filter;
        let sort = state.sort;
        ui.add_space(16.0);
        self.alert(ui);
        crate::ui::controls::observe_notice(
            ui.ctx(),
            "catalog-error",
            "设备清单未更新",
            crate::ui::controls::DialogIcon::Warning,
            self.catalog_error.as_deref(),
        );
        let Some(mut rows) = rows else {
            self.empty_state(ui, "正在读取设备清单…", None);
            return;
        };
        rows.retain(|e| {
            let d = &e.device;
            let visible = match filter {
                Filter::All => true,
                Filter::Online => d.is_connected(),
                Filter::Offline => d.is_offline(),
            };
            visible
                && (query.is_empty()
                    || [
                        display_alias(d),
                        &d.device_id,
                        &d.platform_label(),
                        &d.version_name,
                    ]
                    .iter()
                    .any(|value| value.to_lowercase().contains(&query)))
        });
        rows.sort_by(|a, b| {
            (if sort == Sort::Status {
                b.device.is_connected().cmp(&a.device.is_connected())
            } else {
                std::cmp::Ordering::Equal
            })
            .then_with(|| a.device.list_order(&b.device))
        });
        let others = self.other_devices();
        let mut picked = None;
        let mut clear_filters = false;
        crate::ui::controls::page_scroll("device-list").show(ui, |ui| {
            if rows.is_empty() {
                clear_filters = self.empty_state(
                    ui,
                    if count == 0 {
                        "暂无可连接的电脑"
                    } else {
                        "没有符合条件的设备"
                    },
                    (count > 0).then_some("清除搜索和筛选"),
                );
            }
            for entry in &rows {
                let selected = self.selected_device_id.as_deref() == Some(&entry.device.device_id);
                if let Some(action) = ui
                    .push_id(&entry.device.device_id, |ui| {
                        row(ui, entry, selected, &mut self.center_ui.wallpapers)
                    })
                    .inner
                {
                    picked = Some((entry.device.device_id.clone(), action));
                }
                ui.add_space(6.0);
            }
            if !others.is_empty() {
                ui.add_space(18.0);
                if let Some(id) = self.other_devices_section(ui, &others) {
                    picked = Some((id, Action::Details));
                }
            }
        });
        if clear_filters {
            let state = &mut self.center_ui.device_list;
            state.search.clear();
            state.filter = Filter::All;
        }
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
        let (rect, response) =
            ui.allocate_exact_size(vec2(ui.available_width(), 30.0), Sense::click());
        let response = response.on_hover_cursor(egui::CursorIcon::PointingHand);
        response.widget_info(|| {
            egui::WidgetInfo::selected(
                egui::WidgetType::CollapsingHeader,
                true,
                *open,
                "本机与其他登录设备",
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
            "本机与其他登录设备",
            FontId::proportional(crate::ui::theme::BODY),
            color,
        );
        ui.painter().text(
            title.right_center() + vec2(8.0, 0.0),
            egui::Align2::LEFT_CENTER,
            format!("{} 台 · 改名或移除请进入详情", devices.len()),
            FontId::proportional(crate::ui::theme::SMALL),
            MUTED,
        );
        if !*open {
            return None;
        }
        ui.add_space(4.0);
        let mut picked = None;
        for (device, current) in devices {
            let status = self.device_status(device);
            let clicked = ui
                .push_id(&device.device_id, |ui| {
                    super::cards::card().show(ui, |ui| {
                        ui.set_width(ui.available_width());
                        ui.horizontal(|ui| {
                            let (icon, _) = ui.allocate_exact_size(vec2(30.0, 30.0), Sense::hover());
                            device_visuals::system_icon(ui.painter(), icon, device.platform);
                            ui.vertical(|ui| {
                                ui.spacing_mut().item_spacing.y = 3.0;
                                ui.horizontal(|ui| {
                                    ui.add(
                                        egui::Label::new(RichText::new(display_alias(device)).strong())
                                            .truncate(),
                                    );
                                    if *current {
                                        super::cards::chip(ui, "本机", theme::ACCENT);
                                    }
                                });
                                ui.horizontal(|ui| {
                                    ui.label(
                                        RichText::new(format!(
                                            "{} · ID {} · {}",
                                            device.platform_label(),
                                            crate::account::api::device_id_text(&device.device_id),
                                            if device.version_name.is_empty() {
                                                "版本未知"
                                            } else {
                                                &device.version_name
                                            }
                                        ))
                                        .size(crate::ui::theme::SMALL)
                                        .color(MUTED),
                                    );
                                    if !*current {
                                        ui.label(
                                            RichText::new(status.label())
                                                .size(crate::ui::theme::SMALL)
                                                .color(status.color()),
                                        );
                                    }
                                });
                            });
                            ui.with_layout(egui::Layout::right_to_left(Align::Center), |ui| {
                                ui.add(crate::ui::controls::secondary("详情")).clicked()
                            })
                            .inner
                        })
                        .inner
                    })
                    .inner
                })
                .inner;
            if clicked {
                picked = Some(device.device_id.clone());
            }
            ui.add_space(6.0);
        }
        picked
    }
}

fn row(
    ui: &mut egui::Ui,
    entry: &Entry,
    selected: bool,
    wallpapers: &mut crate::application::wallpaper::Wallpapers,
) -> Option<Action> {
    let device = &entry.device;
    let wide = ui.available_width() >= 760.0;
    let (rect, response) = ui.allocate_exact_size(
        vec2(ui.available_width(), if wide { 88.0 } else { 124.0 }),
        Sense::click(),
    );
    if !ui.is_rect_visible(rect) {
        return None;
    }
    let texture = wallpapers.texture(ui.ctx(), &device.device_id, &device.wallpaper_url);
    let fill = if selected {
        crate::ui::theme::SELECTED
    } else if response.hovered() {
        crate::ui::theme::HOVER
    } else {
        SURFACE
    };
    let p = ui.painter();
    p.rect_filled(rect, 8.0, fill);
    p.rect_stroke(
        rect,
        8.0,
        Stroke::new(
            1.0,
            if selected {
                crate::ui::theme::BORDER_FOCUS
            } else {
                LINE
            },
        ),
        egui::StrokeKind::Inside,
    );
    let preview = egui::Rect::from_min_size(rect.min + vec2(14.0, 12.0), vec2(116.0, 64.0));
    device_visuals::wallpaper(ui, preview, texture.as_ref(), device.platform);
    let icon =
        egui::Rect::from_min_size(preview.right_bottom() - vec2(29.0, 29.0), vec2(26.0, 26.0));
    ui.painter().rect_filled(icon, 5.0, BG);
    device_visuals::system_icon(ui.painter(), icon.shrink(3.0), device.platform);
    let actions_x = rect.right() - if entry.viewing { 210.0 } else { 92.0 };
    let status_x = if wide {
        actions_x - 120.0
    } else {
        rect.left() + 148.0
    };
    let metadata_x = if wide {
        status_x - 154.0
    } else {
        rect.left() + 260.0
    };
    let name_right = if wide {
        metadata_x - 16.0
    } else {
        actions_x - 12.0
    };
    let mut name = ui.new_child(egui::UiBuilder::new().max_rect(egui::Rect::from_min_max(
        rect.min + vec2(148.0, 13.0),
        egui::pos2(name_right, rect.top() + 82.0),
    )));
    name.add(
        egui::Label::new(
            RichText::new(display_alias(device))
                .size(crate::ui::theme::SECTION)
                .strong(),
        )
        .truncate(),
    )
    .on_hover_text(display_alias(device));
    name.add(
        egui::Label::new(
            RichText::new(format!(
                "{}ID {}",
                if entry.current { "本机 · " } else { "" },
                crate::account::api::device_id_text(&device.device_id)
            ))
            .size(crate::ui::theme::TINY)
            .color(MUTED),
        )
        .truncate(),
    )
    .on_hover_text(&device.device_id);
    let status_rect = egui::Rect::from_min_size(
        egui::pos2(status_x, rect.top() + if wide { 30.0 } else { 89.0 }),
        crate::ui::theme::DEVICE_STATUS_SIZE.into(),
    );
    crate::ui::controls::device_status_badge(
        ui,
        status_rect,
        entry.status.label(),
        entry.status.color(),
    );
    let mut metadata = ui.new_child(egui::UiBuilder::new().max_rect(egui::Rect::from_min_max(
        egui::pos2(metadata_x, rect.top() + if wide { 22.0 } else { 94.0 }),
        egui::pos2(
            if wide {
                status_x - 12.0
            } else {
                rect.right() - 14.0
            },
            rect.bottom() - 8.0,
        ),
    )));
    if wide {
        metadata.label(RichText::new(device.platform_label()).size(crate::ui::theme::COMPACT_TEXT));
        metadata.add(
            egui::Label::new(
                RichText::new(if device.version_name.is_empty() {
                    "版本未知"
                } else {
                    &device.version_name
                })
                .size(crate::ui::theme::TINY)
                .color(MUTED),
            )
            .truncate(),
        );
    } else {
        metadata.add(
            egui::Label::new(
                RichText::new(format!(
                    "{} · {}",
                    device.platform_label(),
                    if device.version_name.is_empty() {
                        "版本未知"
                    } else {
                        &device.version_name
                    }
                ))
                .size(crate::ui::theme::TINY)
                .color(MUTED),
            )
            .truncate(),
        );
    }
    let mut action = None;
    let mut buttons = ui.new_child(
        egui::UiBuilder::new()
            .max_rect(egui::Rect::from_min_max(
                egui::pos2(actions_x, rect.top() + 12.0),
                egui::pos2(
                    rect.right() - 14.0,
                    rect.top() + if wide { 76.0 } else { 70.0 },
                ),
            ))
            .layout(egui::Layout::right_to_left(Align::Center)),
    );
    if entry.viewing {
        let button = buttons.add_enabled(
            entry.connect_issue.is_none(),
            primary(if entry.own_session {
                "已打开"
            } else {
                "连接"
            }),
        );
        if button.clicked() {
            action = Some(Action::Connect);
        }
        let audio = buttons
            .add_enabled_ui(entry.connect_issue.is_none(), |ui| {
                crate::ui::controls::audio_output_button(
                    ui,
                    "仅音频连接",
                    crate::ui::theme::CONTROL_HEIGHT,
                )
            })
            .inner;
        if audio.clicked() {
            action = Some(Action::Audio);
        }
        if let Some(issue) = &entry.connect_issue {
            audio.on_disabled_hover_text(issue);
        }
        if let Some(issue) = &entry.connect_issue {
            button.on_disabled_hover_text(issue);
        }
    }
    if buttons.button("详情").clicked() {
        action = Some(Action::Details);
    }
    action.or_else(|| response.clicked().then_some(Action::Details))
}
