#[path = "activity.rs"]
mod activity;
#[path = "device_details.rs"]
mod device_details;
#[path = "friends.rs"]
mod friends;

use super::{DeviceCenter, Event};
use crate::account::api::{
    DeviceDetail, DeviceGroups, DeviceInfo, DeviceList, device_id_text, platform_label,
};
use crate::account::client::AuthenticatedClient;
use crate::i18n::t;
use crate::ui::center_icon::{Icon, paint_icon};
use crate::ui::controls::{self, DialogAction, DialogIcon};
use crate::ui::{branding, device_list, theme};
use anyhow::Result;
use egui::{Align, Layout, RichText, Sense, vec2};
use farwing_protocol::api::{AuthSession, Me};
use std::{future::Future, sync::Arc};

#[derive(Clone, Copy, Default, PartialEq, Eq)]
enum Page {
    #[default]
    Devices,
    Friends,
    Account,
    Records,
    Notices,
    Chat,
}

impl Page {
    fn title(self) -> &'static str {
        match self {
            Self::Devices => t().center.page_devices,
            Self::Friends => t().center.page_friends,
            Self::Account => t().center.this_account,
            Self::Records => t().records.page,
            Self::Notices => t().notices.page,
            Self::Chat => t().chat.page,
        }
    }
}

#[derive(Default)]
pub(super) struct State {
    page: Page,
    load: bool,
    search: String,
    detail: Option<device_details::Details>,
    friends: friends::State,
    activity: activity::State,
    account: Option<(Me, Vec<AuthSession>)>,
    dialog: Option<Dialog>,
    message: Option<String>,
    ports: std::collections::BTreeMap<String, crate::features::port_mapping::ui::PortWindow>,
    notice_checked: Option<std::time::Instant>,
    unread: u32,
    chat: Option<super::chat::LinuxChat>,
}

pub(super) enum Update {
    Detail(String, DeviceDetail, DeviceList),
    Friends(friends::Update),
    Activity(activity::Update),
    Account(Me, Vec<AuthSession>),
    Revoked(String),
    Renamed(String, String, String),
    Removed(String, String),
    Message(String),
}

enum Dialog {
    SignOut,
    Revoke(AuthSession),
    RemoveDevice(DeviceInfo),
    Rename(DeviceInfo, String),
    Friend(friends::Dialog),
    Power(DeviceInfo, crate::account::power::PowerAction),
}

pub(super) fn alias(device: &DeviceInfo) -> &str {
    if device.alias.trim().is_empty() {
        t().center.unnamed_device
    } else {
        &device.alias
    }
}

fn time(seconds: i64) -> String {
    chrono::DateTime::from_timestamp(seconds, 0)
        .filter(|_| seconds > 0)
        .map(|date| {
            date.with_timezone(&chrono::Local)
                .format("%Y-%m-%d %H:%M")
                .to_string()
        })
        .unwrap_or_else(|| t().center.never.to_owned())
}

fn modal(
    ctx: &egui::Context,
    id: &'static str,
    title: &str,
    icon: DialogIcon,
    width: f32,
    contents: impl FnOnce(&mut egui::Ui) -> Option<bool>,
) -> Option<bool> {
    let response = egui::Modal::new(egui::Id::new(id))
        .frame(controls::dialog_frame())
        .show(ctx, |ui| {
            ui.set_width(width.min(ctx.content_rect().width() - 48.0));
            let close = controls::dialog_header(ui, title, icon, true);
            let answer = contents(ui);
            if close { Some(false) } else { answer }
        });
    response
        .inner
        .or_else(|| response.should_close().then_some(false))
}

fn actions(ui: &mut egui::Ui, label: &str, enabled: bool, danger: bool) -> Option<bool> {
    let (yes, no) = controls::dialog_actions(
        ui,
        Some(DialogAction::new(label).enabled(enabled).danger(danger)),
        Some(t().common.cancel),
    );
    if yes { Some(true) } else { no.then_some(false) }
}

impl DeviceCenter {
    fn request_center<F, Fut>(&mut self, request: F)
    where
        F: FnOnce(Arc<AuthenticatedClient>) -> Fut + Send + 'static,
        Fut: Future<Output = Result<Update>> + Send + 'static,
    {
        let Some((client, _)) = &self.account else {
            return;
        };
        if self.busy {
            return;
        }
        let client = client.clone();
        self.busy = true;
        self.error = None;
        let sender = self.sender.clone();
        let ctx = self.ctx.clone();
        self.runtime.spawn(async move {
            let result = request(client).await;
            let _ = sender.send(Event::Center(result));
            ctx.request_repaint();
        });
    }

    pub(super) fn finish_center(&mut self, result: Result<Update>) {
        use crate::account::device_change::DeviceChange;
        self.busy = false;
        match result {
            Ok(Update::Detail(id, device, devices)) => {
                if let Some((_, list)) = &mut self.account {
                    *list = devices;
                }
                if let Some(detail) = self.center.detail.as_mut().filter(|detail| detail.id == id) {
                    detail.data = Some(device);
                }
            }
            Ok(Update::Friends(update)) => self.finish_friends(update),
            Ok(Update::Activity(update)) => {
                if matches!(&update, activity::Update::Notices(None, ..)) {
                    self.center.unread = 0;
                    self.center.notice_checked = Some(std::time::Instant::now());
                }
                self.center.activity.finish(update);
            }
            Ok(Update::Account(me, sessions)) => self.center.account = Some((me, sessions)),
            Ok(Update::Revoked(id)) => {
                if let Some((_, sessions)) = &mut self.center.account {
                    sessions.retain(|session| session.device_id != id);
                }
            }
            Ok(Update::Renamed(id, alias, message)) => {
                if let Some((_, list)) = &mut self.account {
                    DeviceChange::renamed(id.clone(), alias).apply_list(list);
                    list.sort();
                }
                if let Some(detail) = &mut self.center.detail {
                    detail.data = None;
                    self.center.load = true;
                }
                self.center.message = Some(message);
            }
            Ok(Update::Removed(id, message)) => {
                if let Some((_, list)) = &mut self.account {
                    DeviceChange::removed(id).apply_list(list);
                }
                self.center.detail = None;
                self.center.message = Some(message);
            }
            Ok(Update::Message(message)) => self.center.message = Some(message),
            Err(error) => {
                self.center.message = None;
                self.show_error(error);
            }
        }
    }

    pub(super) fn finish_notice_count(
        &mut self,
        client: &Arc<AuthenticatedClient>,
        result: Result<u32>,
    ) {
        if self.center.page != Page::Notices
            && self
                .account
                .as_ref()
                .is_some_and(|(active, _)| Arc::ptr_eq(client, active))
            && let Ok(unread) = result
        {
            self.center.unread = unread;
        }
    }

    fn check_notices(&mut self, client: &Arc<AuthenticatedClient>) {
        let interval = std::time::Duration::from_secs(60);
        let remaining = self
            .center
            .notice_checked
            .map_or(std::time::Duration::ZERO, |checked| {
                interval.saturating_sub(checked.elapsed())
            });
        if remaining.is_zero() && self.center.page != Page::Notices {
            self.center.notice_checked = Some(std::time::Instant::now());
            let (client, sender, ctx) = (client.clone(), self.sender.clone(), self.ctx.clone());
            self.runtime.spawn(async move {
                let result = client
                    .request(|api| async move { api.notices(None, 0).await })
                    .await
                    .map(|list| list.unread);
                let _ = sender.send(Event::NoticeCount(client, result));
                ctx.request_repaint();
            });
            self.ctx.request_repaint_after(interval);
        } else if !remaining.is_zero() {
            self.ctx.request_repaint_after(remaining);
        }
    }

    pub(super) fn show_center_tools(&mut self, ctx: &egui::Context) {
        self.center.ports.retain(|_, window| window.show(ctx));
    }

    fn navigate(&mut self, page: Page) {
        if page != Page::Chat && let Some(chat) = &self.center.chat {
            chat.set_visible(false);
        }
        self.center.page = page;
        self.center.detail = None;
        self.center.load = true;
        self.center.message = None;
        self.error = None;
    }

    fn open_details(&mut self, device: &DeviceInfo) {
        self.center.detail = Some(device_details::Details {
            id: device.device_id.clone(),
            data: None,
        });
        self.center.load = true;
        self.center.message = None;
        self.error = None;
    }

    fn refresh_page(&mut self, client: &Arc<AuthenticatedClient>) {
        if let Some(id) = self.center.detail.as_ref().map(|d| d.id.clone()) {
            self.wallpapers.refresh(&id);
            self.request_center(move |client| async move {
                let (details, mut devices) =
                    tokio::try_join!(client.device_detail(&id), client.list_devices())?;
                devices.sort();
                Ok(Update::Detail(id, details, devices))
            });
            return;
        }
        match self.center.page {
            Page::Devices => self.refresh(client.clone()),
            Page::Friends => self.refresh_friends(),
            Page::Records => self.refresh_records(None),
            Page::Notices => self.refresh_notices(None),
            Page::Chat => {}
            Page::Account => self.request_center(|client| async move {
                let (me, sessions) =
                    tokio::try_join!(client.account_info(), client.auth_sessions())?;
                Ok(Update::Account(me, sessions.sessions))
            }),
        }
    }

    pub(super) fn devices_page(&mut self, root: &mut egui::Ui) {
        let Some((client, devices)) = self.account.as_ref() else {
            return;
        };
        let (client, devices) = (client.clone(), devices.clone());
        self.check_notices(&client);
        let text = t();
        let origin = self.sign_in.server();
        egui::Panel::bottom("linux-device-status")
            .resizable(false)
            .frame(
                egui::Frame::new()
                    .fill(theme::SIDEBAR)
                    .inner_margin(egui::Margin::symmetric(16, 6)),
            )
            .show(root, |ui| {
                ui.horizontal(|ui| {
                    ui.label(
                        RichText::new(text.center.server)
                            .small()
                            .color(theme::MUTED),
                    );
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        ui.label(
                            RichText::new(concat!("v", env!("CARGO_PKG_VERSION")))
                                .small()
                                .color(theme::MUTED),
                        );
                        ui.with_layout(Layout::left_to_right(Align::Center), |ui| {
                            ui.add(egui::Label::new(RichText::new(origin).small()).truncate())
                                .on_hover_text(origin);
                        });
                    });
                });
            });
        egui::Panel::left("linux-device-navigation")
            .resizable(false)
            .exact_size(theme::SIDEBAR_WIDTH)
            .frame(
                egui::Frame::new()
                    .fill(theme::SIDEBAR)
                    .inner_margin(egui::Margin::symmetric(12, 18)),
            )
            .show(root, |ui| self.navigation(ui, &client));

        if self.center.load && !self.busy {
            self.center.load = false;
            self.refresh_page(&client);
        }
        egui::CentralPanel::default()
            .frame(
                egui::Frame::new()
                    .fill(theme::BG)
                    .inner_margin(egui::Margin {
                        left: 28,
                        right: 28,
                        top: 24,
                        bottom: 0,
                    }),
            )
            .show(root, |ui| {
                let is_details = self.center.detail.is_some();
                ui.horizontal(|ui| {
                    if is_details
                        && controls::back_button(
                            ui,
                            &(text.center.back_to)(self.center.page.title()),
                        )
                        .clicked()
                    {
                        self.center.detail = None;
                        self.error = None;
                        self.center.message = None;
                    }
                    ui.heading(if is_details {
                        text.center.page_device_details
                    } else {
                        self.center.page.title()
                    });
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        if self.busy {
                            ui.spinner();
                        }
                        if !is_details && self.center.page == Page::Devices {
                            self.show_join(ui);
                        }
                        if (is_details || self.center.page != Page::Chat)
                            && ui.add_enabled(
                                !self.busy,
                                controls::secondary(if is_details {
                                    text.center.refresh_details
                                } else {
                                    text.common.refresh
                                }),
                            )
                            .clicked()
                        {
                            self.refresh_page(&client);
                        }
                    });
                });
                if let Some(error) = &self.error {
                    ui.colored_label(theme::RED, error);
                }
                if let Some(message) = &self.center.message {
                    ui.colored_label(theme::GREEN, message);
                }
                ui.add_space(10.0);
                if !is_details && self.center.page == Page::Chat {
                    let page = self.center.chat.get_or_insert_with(|| {
                        super::chat::LinuxChat::new(client.clone(), self.runtime.clone(), self.ctx.clone())
                    });
                    if let Some(device) = page.show_with_devices(ui, Some(&devices)) {
                        self.connect(client.clone(), device);
                    }
                    return;
                }
                controls::page_scroll("linux-center-page")
                    .id_salt((
                        self.center.page.title(),
                        self.center.detail.as_ref().map(|detail| &detail.id),
                    ))
                    .show(ui, |ui| {
                        if self.center.detail.is_some() {
                            self.details_page(ui, &client, &devices);
                        } else {
                            match self.center.page {
                                Page::Devices => self.device_groups(ui, &client, &devices),
                                Page::Friends => self.friends_page(ui, &client, &devices),
                                Page::Account => self.account_page(ui, &client),
                                Page::Records => self.records_page(ui),
                                Page::Notices => self.notices_page(ui),
                                Page::Chat => {}
                            }
                        }
                    });
            });
        if let Some(dialog) = self.center.dialog.take() {
            self.center_dialog(root.ctx(), &client, &devices, dialog);
        }
    }

    fn navigation(&mut self, ui: &mut egui::Ui, client: &Arc<AuthenticatedClient>) {
        let (row, _) = ui.allocate_exact_size(vec2(ui.available_width(), 30.0), Sense::hover());
        let logo = branding::logo_size(theme::LOGO_SIDEBAR);
        branding::paint_logo(
            ui.painter(),
            row.center().x - logo.x * 0.5,
            row.center().y,
            theme::LOGO_SIDEBAR,
            theme::TEXT,
        );
        ui.add_space(22.0);
        for (page, icon) in [
            (Page::Devices, Icon::Monitor),
            (Page::Friends, Icon::Friends),
            (Page::Chat, Icon::Chat),
            (Page::Records, Icon::Records),
            (Page::Notices, Icon::Notices),
            (Page::Account, Icon::Info),
        ] {
            let selected = self.center.page == page;
            let (row, response) = ui
                .add_enabled_ui(!self.busy, |ui| {
                    ui.allocate_exact_size(
                        vec2(ui.available_width(), theme::NAV_HEIGHT),
                        Sense::click(),
                    )
                })
                .inner;
            let response = response.on_hover_cursor(egui::CursorIcon::PointingHand);
            response.widget_info(|| {
                egui::WidgetInfo::selected(
                    egui::WidgetType::SelectableLabel,
                    !self.busy,
                    selected,
                    page.title(),
                )
            });
            if selected || response.hovered() {
                ui.painter().rect_filled(
                    row,
                    theme::CONTROL_RADIUS,
                    if selected {
                        theme::SELECTED
                    } else {
                        theme::HOVER
                    },
                );
            }
            paint_icon(
                ui.painter(),
                egui::Rect::from_center_size(row.left_center() + vec2(20.0, 0.0), vec2(22.0, 22.0)),
                icon,
                if selected {
                    theme::ACCENT
                } else {
                    theme::MUTED
                },
            );
            ui.painter().text(
                row.left_center() + vec2(42.0, 0.0),
                egui::Align2::LEFT_CENTER,
                page.title(),
                egui::FontId::proportional(theme::BODY),
                theme::TEXT,
            );
            if page == Page::Notices && self.center.unread > 0 {
                ui.painter().text(
                    row.right_center() - vec2(10.0, 0.0),
                    egui::Align2::RIGHT_CENTER,
                    self.center.unread.to_string(),
                    egui::FontId::proportional(theme::SMALL),
                    theme::ACCENT,
                );
            }
            if response.clicked() && (!selected || self.center.detail.is_some()) {
                self.navigate(page);
            }
        }
        ui.with_layout(Layout::bottom_up(Align::Min), |ui| {
            ui.allocate_ui_with_layout(
                vec2(ui.available_width(), theme::VIEWER_CAPTION_BUTTON),
                Layout::right_to_left(Align::Center),
                |ui| {
                    if ui
                        .add_enabled_ui(!self.busy, |ui| {
                            let (rect, response) =
                                controls::icon_button_area(ui, theme::VIEWER_CAPTION_BUTTON);
                            paint_icon(
                                ui.painter(),
                                rect,
                                Icon::Logout,
                                if response.hovered() {
                                    theme::TEXT
                                } else {
                                    theme::MUTED
                                },
                            );
                            response.widget_info(|| {
                                egui::WidgetInfo::labeled(
                                    egui::WidgetType::Button,
                                    ui.is_enabled(),
                                    t().common.sign_out,
                                )
                            });
                            response.on_hover_text(t().common.sign_out)
                        })
                        .inner
                        .clicked()
                    {
                        self.center.dialog = Some(Dialog::SignOut);
                    }
                    ui.with_layout(Layout::left_to_right(Align::Center), |ui| {
                        let account = client.account_name();
                        let name = if account.is_empty() {
                            t().center.signed_in
                        } else {
                            &account
                        };
                        ui.add(egui::Label::new(name).truncate())
                            .on_hover_text(name);
                    });
                },
            );
            ui.separator();
        });
    }

    fn device_groups(
        &mut self,
        ui: &mut egui::Ui,
        client: &Arc<AuthenticatedClient>,
        devices: &DeviceList,
    ) {
        device_list::toolbar(ui, &mut self.center.search, |_| {});
        let query = self.center.search.trim().to_lowercase();
        let groups = DeviceGroups::from_list(devices);
        let current = &devices.current_device;
        let mut any = false;
        for (title, rows, exclude_current) in [
            (
                t().session.group_computers,
                groups.desktop_devices.as_slice(),
                true,
            ),
            (
                t().session.group_mobile,
                groups.mobile_devices.as_slice(),
                true,
            ),
            (
                t().center.page_this_device,
                std::slice::from_ref(current),
                false,
            ),
        ] {
            let shown: Vec<_> = rows
                .iter()
                .filter(|d| {
                    (!exclude_current || d.device_id != current.device_id)
                        && device_list::matches(
                            &query,
                            &[
                                alias(d),
                                &device_id_text(&d.device_id),
                                platform_label(d.platform),
                                &d.version_name,
                            ],
                        )
                })
                .collect();
            if !shown.is_empty() {
                device_list::heading(
                    ui,
                    title,
                    exclude_current.then_some(shown.len()),
                    !any,
                    |_| {},
                );
                any = true;
                for device in shown {
                    self.device_row(ui, client, device, !exclude_current);
                }
            }
        }
        if !any {
            device_list::empty(ui, t().center.no_matching_devices);
        }
    }

    fn device_row(
        &mut self,
        ui: &mut egui::Ui,
        client: &Arc<AuthenticatedClient>,
        device: &DeviceInfo,
        local: bool,
    ) {
        ui.push_id(&device.device_id, |ui| {
            let texture =
                self.wallpapers
                    .texture(ui.ctx(), &device.device_id, &device.wallpaper_url);
            let detail = format!(
                "{} · {}",
                platform_label(device.platform),
                device_id_text(&device.device_id)
            );
            let mut details = false;
            let mut connect = false;
            let selected = device_list::row(
                ui,
                &device_list::Row {
                    title: alias(device),
                    tile: device_list::Tile::of(texture.as_ref(), device.platform),
                    status: (!local).then(|| {
                        (
                            device.status_label(),
                            if device.is_connected() {
                                theme::GREEN
                            } else {
                                theme::MUTED
                            },
                        )
                    }),
                    chips: &[],
                    extra_badges: None,
                    detail: &detail,
                    selected: Some(false),
                },
                |ui| {
                    details = ui
                        .add_enabled(!self.busy, controls::secondary(t().common.details))
                        .clicked();
                    if !local && can_view(device) {
                        connect = ui
                            .add_enabled(
                                !self.busy && device.is_connected(),
                                controls::primary(t().common.connect),
                            )
                            .clicked();
                    }
                },
            );
            if connect {
                self.connect(client.clone(), device.clone());
            } else if !self.busy && (selected || details) {
                self.open_details(device);
            }
        });
    }

    fn account_page(&mut self, ui: &mut egui::Ui, client: &Arc<AuthenticatedClient>) {
        let Some((me, sessions)) = &self.center.account else {
            return;
        };
        ui.label(format!("{} · {}", t().center.username, me.username));
        if !me.nickname.is_empty() && me.nickname != me.username {
            ui.label(&me.nickname);
        }
        ui.add_space(12.0);
        device_list::heading(
            ui,
            t().center.this_and_other_devices,
            Some(sessions.len()),
            true,
            |_| {},
        );
        for session in sessions {
            let current = session.current || session.device_id == client.device_id();
            controls::card().show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.vertical(|ui| {
                        let name = if session.device_alias.is_empty() {
                            t().center.unnamed_device
                        } else {
                            &session.device_alias
                        };
                        ui.label(RichText::new(name).strong());
                        ui.label(format!(
                            "{} · {}",
                            platform_label(session.platform),
                            device_id_text(&session.device_id)
                        ));
                        ui.label(
                            RichText::new(format!(
                                "{} · {}",
                                time(session.created_at),
                                time(session.last_used_at)
                            ))
                            .small()
                            .color(theme::MUTED),
                        );
                    });
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        if current {
                            ui.label(t().center.page_this_device);
                        } else if ui
                            .add_enabled(
                                !self.busy,
                                controls::secondary((t().records.event)("auth.revoke")),
                            )
                            .clicked()
                        {
                            self.center.dialog = Some(Dialog::Revoke(session.clone()));
                        }
                    });
                });
            });
        }
    }

    fn center_dialog(
        &mut self,
        ctx: &egui::Context,
        client: &Arc<AuthenticatedClient>,
        devices: &DeviceList,
        dialog: Dialog,
    ) {
        let text = t();
        match dialog {
            Dialog::SignOut => {
                let answer = modal(
                    ctx,
                    "linux-sign-out",
                    text.center.sign_out_title,
                    DialogIcon::Warning,
                    420.0,
                    |ui| {
                        ui.label(text.center.sign_out_body);
                        actions(ui, text.common.sign_out, !self.busy, true)
                    },
                );
                match answer {
                    None => self.center.dialog = Some(Dialog::SignOut),
                    Some(true) => self.signed_out(client.clone()),
                    Some(false) => {}
                }
            }
            Dialog::Revoke(session) => {
                let label = (text.records.event)("auth.revoke");
                let answer = modal(
                    ctx,
                    "linux-revoke-login",
                    label,
                    DialogIcon::Warning,
                    420.0,
                    |ui| {
                        ui.label(&session.device_alias);
                        ui.label(device_id_text(&session.device_id));
                        actions(
                            ui,
                            label,
                            !self.busy
                                && !session.current
                                && session.device_id != client.device_id(),
                            true,
                        )
                    },
                );
                match answer {
                    None => self.center.dialog = Some(Dialog::Revoke(session)),
                    Some(true) => self.request_center(move |client| async move {
                        client.revoke_auth_session(&session.device_id).await?;
                        Ok(Update::Revoked(session.device_id))
                    }),
                    Some(false) => {}
                }
            }
            Dialog::Friend(dialog) => self.friend_dialog(ctx, devices, dialog),
            dialog => self.details_dialog(ctx, client, devices, dialog),
        }
    }
}

fn can_view(device: &DeviceInfo) -> bool {
    device.platform.is_desktop()
        && device.controlled_support
        && device.controllable
        && (!device.is_shared() || device.granted(farwing_protocol::caps::VIEW))
}
