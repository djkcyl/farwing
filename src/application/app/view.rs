//! Device-center presentation. Network/account ownership remains in app.rs.
use super::*;
use crate::ui::theme::{
    self, ACCENT as BLUE, AMBER, BG, GREEN, LINE, LOGO_LOADING, LOGO_LOGIN, LOGO_SIDEBAR, MUTED,
    RED, SIDEBAR, SURFACE, TEXT,
};
use crate::i18n::t;
use egui::{Align, Color32, FontId, RichText, Sense, Stroke, vec2};

mod about;
mod access;
use access::Access;
mod assist;
mod friends;
mod assist_components;
mod cards;
mod components;
mod connect;
mod device_details;
mod device_visuals;
mod device_list;
mod devices;
mod diagnostic_export;
mod diagnostics_panel;
mod host_assist;
mod host_settings;
mod logs;
mod port_mapping;
mod notices;
mod power;
mod records;
mod settings;
mod status_bar;
mod this_device;
mod remote_diagnostics;
mod update_dialog;
mod wol_setup;

fn singleline_input(value: &mut String) -> egui::TextEdit<'_> {
    crate::ui::controls::singleline(value, crate::ui::controls::HEIGHT)
}

#[derive(Clone, Copy, Default, PartialEq, Eq)]
enum Page {
    /// Every device to connect to, and the connect bar.
    #[default]
    Devices,
    Friends,
    /// Who may connect to this device.
    ThisDevice,
    /// Connections and account events.
    Records,
    /// The account's notifications and which it receives.
    Notices,
    DeviceDetails,
    Settings,
}
impl Page {
    fn title(self) -> &'static str {
        let text = t();
        match self {
            Self::Devices => text.center.page_devices,
            Self::Friends => text.center.page_friends,
            Self::ThisDevice => text.center.page_this_device,
            Self::Records => text.records.page,
            Self::Notices => text.notices.page,
            Self::DeviceDetails => text.center.page_device_details,
            Self::Settings => text.common.settings,
        }
    }
}

#[derive(Default)]
pub(super) struct CenterUi {
    pub(super) components: components::Manager,
    page: Page,
    /// The category shown on the settings page.
    settings_tab: settings::SettingsTab,
    /// Asking whether to sign out to change the server.
    change_server: bool,
    device_list: devices::ListUi,
    detail_id: Option<String>,
    detail_parent: Page,
    wallpapers: crate::application::wallpaper::Wallpapers,
    edit: Option<DeviceEdit>,
    power: Option<power::PowerConfirmation>,
    legal_document: Option<about::LegalDocument>,
    logs: logs::LogUi,
    diagnostics: diagnostics_panel::ViewState,
    wol_setup: wol_setup::ViewState,
    host_assist_password: Option<host_assist::PasswordEditor>,
    host_assist_settings_open: bool,
    host_assist_show_code: bool,
    /// Using the device center without signing in (see `access`).
    pub(super) browsing: bool,
    /// The saved server's address and when it was read.
    server_cache: Option<(Instant, Option<String>)>,
    /// This device's name, ID and fingerprint, for use without an account.
    local_identity: Option<(String, String)>,
    pub(super) shortcuts: crate::application::viewer_shortcuts::Editor,

    plugins: crate::plugins::Manager,
}

struct DeviceEdit {
    device: DeviceInfo,
    alias: String,
    action: EditAction,
}
enum EditAction {
    Rename,
    Remove,
}

impl CenterUi {
    pub(super) fn clear_host_assist(&mut self) {
        self.host_assist_password = None;
        self.host_assist_settings_open = false;
        self.host_assist_show_code = false;
    }
    pub(super) fn open_details(&mut self, id: String) {
        if self.page != Page::DeviceDetails {
            self.detail_parent = self.page;
        }
        self.detail_id = Some(id);
        self.page = Page::DeviceDetails;
    }
    /// The device whose details are open.
    pub(super) fn detail_id(&self) -> Option<&str> {
        self.detail_id.as_deref()
    }
    pub(super) fn close_details(&mut self) {
        if self.page == Page::DeviceDetails {
            self.page = self.detail_parent;
        }
        self.detail_id = None;
        self.edit = None;
        self.power = None;
    }
    pub(super) fn finish_device_operation(&mut self) {
        self.edit = None;
        self.power = None;
    }
    pub(super) fn clear_wallpapers(&mut self) {
        self.wallpapers.clear();
    }
}

#[derive(Clone, Copy)]
enum Icon {
    Monitor,
    Settings,
    Refresh,
    Info,
    Assist,
    Edit,
    Logout,
    Logs,
    Plugins,
    Keyboard,
    Diagnostics,
    Friends,
    /// This device and who may connect to it.
    Host,
    /// Records: a clock.
    Records,
    /// Notifications: a bell.
    Notices,
}

pub(super) fn configure_visuals(ctx: &egui::Context) {
    theme::configure(ctx);
}

fn paint_icon(p: &egui::Painter, rect: egui::Rect, icon: Icon, color: Color32) {
    let c = rect.center();
    let q = |x, y| c + vec2(x, y);
    let s = Stroke::new(1.5, color);
    match icon {
        Icon::Diagnostics => {
            p.add(egui::Shape::line(
                vec![
                    q(-10.0, 0.0),
                    q(-5.0, 0.0),
                    q(-2.0, -8.0),
                    q(2.0, 8.0),
                    q(5.0, 0.0),
                    q(10.0, 0.0),
                ],
                s,
            ));
        }
        Icon::Keyboard => crate::ui::controls::paint_keyboard(p, rect, color),
        Icon::Notices => {
            p.add(egui::Shape::closed_line(
                vec![
                    q(-7.0, 5.0),
                    q(-5.5, 2.0),
                    q(-5.5, -3.0),
                    q(-3.0, -7.0),
                    q(0.0, -8.0),
                    q(3.0, -7.0),
                    q(5.5, -3.0),
                    q(5.5, 2.0),
                    q(7.0, 5.0),
                ],
                s,
            ));
            p.line_segment([q(-2.0, 8.0), q(2.0, 8.0)], s);
        }
        Icon::Records => {
            p.circle_stroke(c, 8.5, s);
            p.add(egui::Shape::line(vec![q(0.0, -5.0), q(0.0, 0.0), q(4.0, 3.0)], s));
        }
        Icon::Friends => {
            p.circle_stroke(q(-4.0, -4.0), 3.5, s);
            p.circle_stroke(q(5.0, -2.0), 3.0, s);
            p.add(egui::Shape::line(
                vec![q(-11.0, 8.0), q(-9.0, 2.0), q(-4.0, 1.0), q(1.0, 2.0), q(3.0, 8.0)],
                s,
            ));
            p.add(egui::Shape::line(vec![q(4.0, 3.0), q(8.0, 3.5), q(10.0, 8.0)], s));
        }
        Icon::Plugins => {
            crate::plugins::paint_plugin_icon(p, rect, color);
        }
        Icon::Logs => {
            p.rect_stroke(
                egui::Rect::from_center_size(c, vec2(15.0, 19.0)),
                2.0,
                s,
                egui::StrokeKind::Inside,
            );
            for y in [-5.0, 0.0, 5.0] {
                p.line_segment([q(-4.0, y), q(4.0, y)], s);
            }
        }
        Icon::Host => {
            // A shield with a check: who may connect here.
            p.add(egui::Shape::closed_line(
                vec![
                    q(0.0, -10.0),
                    q(8.0, -7.0),
                    q(8.0, 0.0),
                    q(5.0, 6.0),
                    q(0.0, 10.0),
                    q(-5.0, 6.0),
                    q(-8.0, 0.0),
                    q(-8.0, -7.0),
                ],
                s,
            ));
            p.add(egui::Shape::line(vec![q(-4.0, 0.0), q(-1.0, 3.5), q(4.5, -3.0)], s));
        }
        Icon::Assist => {
            p.circle_stroke(q(-4.0, -5.0), 3.0, s);
            p.circle_stroke(q(6.0, -3.0), 2.5, s);
            p.add(egui::Shape::line(
                vec![
                    q(-10.0, 7.0),
                    q(-8.0, 1.0),
                    q(-3.0, 0.0),
                    q(2.0, 3.0),
                    q(3.0, 7.0),
                ],
                s,
            ));
            p.add(egui::Shape::line(
                vec![q(5.0, 2.0), q(9.0, 3.0), q(11.0, 7.0)],
                s,
            ));
        }
        Icon::Edit => {
            p.add(egui::Shape::closed_line(
                vec![
                    q(-8.0, 8.0),
                    q(-6.0, 2.0),
                    q(5.0, -9.0),
                    q(9.0, -5.0),
                    q(-2.0, 6.0),
                ],
                s,
            ));
        }
        Icon::Monitor => {
            p.rect_stroke(
                egui::Rect::from_center_size(q(0.0, -2.0), vec2(20.0, 14.0)),
                2.0,
                s,
                egui::StrokeKind::Inside,
            );
            p.line_segment([q(0.0, 5.0), q(0.0, 9.0)], s);
            p.line_segment([q(-5.0, 9.0), q(5.0, 9.0)], s);
        }
        Icon::Settings => {
            for (y, knob) in [(-6.0, -3.0), (0.0, 4.0), (6.0, -1.0)] {
                p.line_segment([q(-9.0, y), q(9.0, y)], s);
                p.circle_filled(q(knob, y), 2.5, color);
            }
        }
        Icon::Refresh => {
            let points = (0..=32)
                .map(|i| {
                    let angle = std::f32::consts::FRAC_PI_4
                        + i as f32 / 32.0 * (std::f32::consts::TAU - std::f32::consts::FRAC_PI_4);
                    q(-2.0, 0.0) + vec2(angle.cos(), angle.sin()) * 8.0
                })
                .collect();
            p.add(egui::Shape::line(points, s));
            p.add(egui::Shape::line(
                vec![q(2.0, -4.0), q(6.0, 0.0), q(10.0, -4.0)],
                s,
            ));
        }
        Icon::Info => {
            p.circle_stroke(c, 8.0, s);
            p.circle_filled(q(0.0, -3.5), 1.0, color);
            p.line_segment([q(0.0, 0.0), q(0.0, 4.0)], s);
        }
        Icon::Logout => {
            p.add(egui::Shape::line(
                vec![q(-1.0, -8.0), q(-8.0, -8.0), q(-8.0, 8.0), q(-1.0, 8.0)],
                s,
            ));
            p.line_segment([q(-2.0, 0.0), q(9.0, 0.0)], s);
            p.add(egui::Shape::line(
                vec![q(5.0, -4.0), q(9.0, 0.0), q(5.0, 4.0)],
                s,
            ));
        }
    }
}

fn primary(label: &str) -> egui::Button<'_> {
    crate::ui::controls::primary(label).min_size(vec2(84.0, crate::ui::controls::HEIGHT))
}

fn login_button(label: &str) -> egui::Button<'_> {
    crate::ui::controls::centered_button(label)
}

fn dialog_frame() -> egui::Frame {
    crate::ui::controls::dialog_frame()
}

fn nav_item(
    ui: &mut egui::Ui,
    icon: Icon,
    title: &str,
    count: Option<usize>,
    locked: bool,
    selected: bool,
) -> bool {
    let (rect, response) = ui.allocate_exact_size(
        vec2(ui.available_width(), theme::NAV_HEIGHT),
        Sense::click(),
    );
    response.widget_info(|| {
        egui::WidgetInfo::selected(
            egui::WidgetType::SelectableLabel,
            ui.is_enabled(),
            selected,
            title,
        )
    });
    if selected || response.hovered() || response.has_focus() {
        ui.painter()
            .rect_filled(rect, 5.0, if selected { theme::SELECTED } else { SURFACE });
    }
    if response.has_focus() {
        ui.painter().rect_stroke(
            rect,
            theme::CONTROL_RADIUS,
            Stroke::new(1.0, theme::BORDER_FOCUS),
            egui::StrokeKind::Inside,
        );
    }
    paint_icon(
        ui.painter(),
        egui::Rect::from_center_size(rect.left_center() + vec2(20.0, 0.0), vec2(22.0, 22.0)),
        icon,
        if selected { BLUE } else { MUTED },
    );
    ui.painter().text(
        rect.left_center() + vec2(42.0, 0.0),
        egui::Align2::LEFT_CENTER,
        title,
        FontId::proportional(crate::ui::theme::BODY),
        if locked && !selected { MUTED } else { TEXT },
    );
    if locked {
        cards::paint_lock(
            ui.painter(),
            rect.right_center() - vec2(16.0, 0.0),
            12.0,
            MUTED,
        );
    } else if let Some(count) = count {
        ui.painter().text(
            rect.right_center() - vec2(12.0, 0.0),
            egui::Align2::RIGHT_CENTER,
            count.to_string(),
            FontId::proportional(crate::ui::theme::SMALL),
            MUTED,
        );
    }
    response.clicked()
}


fn form_row(ui: &mut egui::Ui, label: &str, hint: &str, content: impl FnOnce(&mut egui::Ui)) {
    form_row_content(ui, label, hint, content);
    ui.separator();
}

fn form_row_content(
    ui: &mut egui::Ui,
    label: &str,
    hint: &str,
    content: impl FnOnce(&mut egui::Ui),
) {
    ui.horizontal(|ui| {
        let label_width = (ui.available_width() - 258.0).max(160.0);
        let measure = |text: &str, size: f32| {
            ui.painter()
                .layout(text.to_owned(), FontId::proportional(size), TEXT, label_width)
                .size()
                .y
        };
        let mut text_height = measure(label, crate::ui::theme::BODY);
        if !hint.is_empty() {
            text_height += ui.spacing().item_spacing.y + measure(hint, crate::ui::theme::SMALL);
        }
        let height = (text_height + 16.0).max(52.0);
        ui.allocate_ui_with_layout(
            vec2(label_width, height),
            egui::Layout::top_down(Align::Min),
            |ui| {
                ui.set_min_size(vec2(label_width, height));
                ui.add_space(((height - text_height) / 2.0).floor());
                ui.label(RichText::new(label).size(crate::ui::theme::BODY));
                if !hint.is_empty() {
                    ui.label(
                        RichText::new(hint)
                            .size(crate::ui::theme::SMALL)
                            .color(MUTED),
                    );
                }
            },
        );
        ui.with_layout(egui::Layout::right_to_left(Align::Center), content);
    });
}

fn section(ui: &mut egui::Ui, title: &str) {
    ui.add_space(14.0);
    ui.label(
        RichText::new(title)
            .size(crate::ui::theme::SECTION)
            .strong(),
    );
}

/// The sign-in card: the brand on top, the current step below.
fn login_card(
    root: &mut egui::Ui,
    height: f32,
    content: impl FnOnce(&mut egui::Ui),
) {
    egui::CentralPanel::default()
        .frame(egui::Frame::new().fill(BG).inner_margin(24))
        .show(root, |ui| {
            let bounds = ui.available_rect_before_wrap();
            // The card ends where its content does, measured on the last
            // frame; `height` is the first guess.
            let measured = egui::Id::new("sign-in-content-height");
            let content_height = ui.ctx().data(|d| d.get_temp::<f32>(measured));
            let height = content_height.map_or(height, |h| 98.0 + h + 40.0);
            let card = egui::Rect::from_center_size(
                bounds.center(),
                vec2(
                    460.0_f32.min(bounds.width()),
                    height.min(bounds.height()),
                ),
            );
            let painter = ui.painter();
            painter.rect_filled(
                card.translate(vec2(0.0, 8.0)),
                theme::PANEL_RADIUS,
                Color32::from_black_alpha(28),
            );
            painter.rect_filled(card, theme::PANEL_RADIUS, crate::ui::theme::SURFACE);
            painter.rect_stroke(
                card,
                theme::PANEL_RADIUS,
                Stroke::new(1.0, LINE),
                egui::StrokeKind::Inside,
            );
            let logo = crate::ui::branding::logo_size(LOGO_LOGIN);
            crate::ui::branding::paint_logo(
                painter,
                card.center().x - logo.x * 0.5,
                card.top() + 50.0,
                LOGO_LOGIN,
                TEXT,
            );
            let column = egui::Rect::from_min_max(
                egui::pos2(card.left() + 40.0, card.top() + 98.0),
                egui::pos2(card.right() - 40.0, card.bottom() - 24.0),
            );
            let used = ui
                .scope_builder(
                    egui::UiBuilder::new()
                        .id_salt("sign-in")
                        .max_rect(column)
                        .layout(egui::Layout::top_down(Align::Min)),
                    |ui| {
                        ui.spacing_mut().item_spacing.y = 6.0;
                        content(ui);
                    },
                )
                .response
                .rect
                .height();
            if content_height.is_none_or(|h| (h - used).abs() > 0.5) {
                ui.ctx().data_mut(|d| d.insert_temp(measured, used));
                ui.ctx().request_repaint();
            }
        });
}

/// Signed out, the device center opens on its pages rather than the sign-in
/// steps once the user chose to go on without an account or a server.
fn signed_out_view_path() -> Option<std::path::PathBuf> {
    crate::product::local_data_dir().ok().map(|dir| dir.join("signed-out-view"))
}

impl CenterUi {
    pub(super) fn new() -> Self {
        Self {
            browsing: signed_out_view(),
            ..Self::default()
        }
    }
}

fn signed_out_view() -> bool {
    signed_out_view_path().is_some_and(|path| path.is_file())
}

fn remember_signed_out_view() {
    if let Some(path) = signed_out_view_path() {
        let _ = std::fs::create_dir_all(path.parent().unwrap_or(&path));
        let _ = std::fs::write(path, b"1");
    }
}

impl DeviceCenterApp {
    pub(super) fn needs_login(&self) -> bool {
        self.devices.is_none() && self.catalog.is_none()
    }

    pub(super) fn draw_center(&mut self, ui: &mut egui::Ui) {
        self.center_ui.wallpapers.poll(ui.ctx());
        self.finish_pairing(ui.ctx());
        if self.needs_login() {
            if self.login_restoring {
                self.loading_page(ui);
                return;
            }
            if !self.center_ui.browsing {
                self.login_page(ui);
                return;
            }
        }
        let access = self.access();
        // Reconcile navigation before choosing the panel margins. A removed
        // device must not leave an empty, edge-to-edge detail page behind.
        self.reconcile_device_details();
        // First, so it spans the whole width under the sidebar too.
        self.draw_status_bar(ui);
        self.draw_navigation(ui, access);
        let locked = self.center_ui.page.needs_account() && access != Access::Account;
        if self.center_ui.page != Page::Settings
            || self.center_ui.settings_tab != settings::SettingsTab::Shortcuts
        {
            self.center_ui.shortcuts.reset();
        }
        if self.center_ui.page != Page::DeviceDetails {
            self.center_ui.detail_id = None;
        }
        egui::CentralPanel::default()
            .frame(egui::Frame::new().fill(BG).inner_margin(
                if self.center_ui.page == Page::DeviceDetails && !locked {
                    egui::Margin::ZERO
                } else {
                    egui::Margin {
                        left: 28,
                        right: 28,
                        top: 24,
                        bottom: 0,
                    }
                },
            ))
            .show(ui, |ui| {
                if locked {
                    self.locked_page(ui);
                } else if self.center_ui.page == Page::DeviceDetails {
                    self.device_details_page(ui);
                } else if self.center_ui.page == Page::Settings {
                    self.settings_page(ui);
                } else if self.center_ui.page == Page::Friends {
                    self.friends_page(ui);
                } else if self.center_ui.page == Page::ThisDevice {
                    self.this_device_page(ui);
                } else if self.center_ui.page == Page::Records {
                    self.records_page(ui);
                } else if self.center_ui.page == Page::Notices {
                    self.notices_page(ui);
                } else {
                    self.devices_page(ui);
                }
            });
    }

    fn draw_navigation(&mut self, ui: &mut egui::Ui, access: Access) {
        let locked = |page: Page| page.needs_account() && access != Access::Account;
        // An exact size: a remembered one shrinks for good after any frame
        // narrower than the sidebar (a minimised or restored window).
        egui::Panel::left("center-navigation")
            .resizable(false)
            .exact_size(theme::SIDEBAR_WIDTH)
            .frame(
                egui::Frame::new()
                    .fill(SIDEBAR)
                    .inner_margin(egui::Margin::symmetric(12, 20)),
            )
            .show(ui, |ui| {
                ui.spacing_mut().item_spacing.y = 6.0;
                let (row, response) =
                    ui.allocate_exact_size(vec2(ui.available_width(), 30.0), Sense::click());
                let response = response
                    .on_hover_cursor(egui::CursorIcon::PointingHand)
                    .on_hover_text(t().center.upstream_project);
                response.widget_info(|| {
                    egui::WidgetInfo::labeled(
                        egui::WidgetType::Button,
                        true,
                        t().center.upstream_project_link,
                    )
                });
                let title_color = if response.hovered() { BLUE } else { TEXT };
                if response.clicked() {
                    ui.ctx()
                        .open_url(egui::OpenUrl::new_tab("https://github.com/djkcyl/openuuyc"));
                }
                let logo = crate::ui::branding::logo_size(LOGO_SIDEBAR);
                crate::ui::branding::paint_logo(
                    ui.painter(),
                    row.center().x - logo.x * 0.5,
                    row.center().y,
                    LOGO_SIDEBAR,
                    title_color,
                );
                ui.add_space(24.0);
                let count = self
                    .devices
                    .as_ref()
                    .map(|list| {
                        list.devices
                            .iter()
                            .filter(|d| self.show_in_watching_list(d))
                            .count()
                    })
                    .unwrap_or_default();
                if nav_item(
                    ui,
                    Icon::Monitor,
                    Page::Devices.title(),
                    Some(count).filter(|_| access == Access::Account),
                    false,
                    self.center_ui.page == Page::Devices
                        || (self.center_ui.page == Page::DeviceDetails
                            && self.center_ui.detail_parent == Page::Devices),
                ) {
                    self.center_ui.page = Page::Devices;
                }
                let pending = self
                    .friends
                    .data
                    .as_ref()
                    .map(|d| d.incoming.len())
                    .filter(|count| *count > 0);
                if nav_item(
                    ui,
                    Icon::Friends,
                    Page::Friends.title(),
                    pending,
                    locked(Page::Friends),
                    self.center_ui.page == Page::Friends,
                ) {
                    if self.center_ui.page != Page::Friends && !locked(Page::Friends) {
                        self.request_friends(super::friends::FriendOperation::Refresh);
                    }
                    self.center_ui.page = Page::Friends;
                }
                let connected = self
                    .host
                    .as_ref()
                    .map(|h| h.status().connections.len())
                    .filter(|count| *count > 0);
                if nav_item(
                    ui,
                    Icon::Host,
                    Page::ThisDevice.title(),
                    connected,
                    false,
                    self.center_ui.page == Page::ThisDevice,
                ) {
                    self.center_ui.page = Page::ThisDevice;
                }
                if nav_item(
                    ui,
                    Icon::Records,
                    Page::Records.title(),
                    None,
                    locked(Page::Records),
                    self.center_ui.page == Page::Records,
                ) {
                    if self.center_ui.page != Page::Records && !locked(Page::Records) {
                        // Opening the page shows the latest.
                        self.records.connections = None;
                        self.records.events = None;
                        self.records.error = None;
                    }
                    self.center_ui.page = Page::Records;
                }
                let unread = Some(self.notices.unread as usize)
                    .filter(|count| *count > 0 && access == Access::Account);
                if nav_item(
                    ui,
                    Icon::Notices,
                    Page::Notices.title(),
                    unread,
                    locked(Page::Notices),
                    self.center_ui.page == Page::Notices,
                ) {
                    if self.center_ui.page != Page::Notices && !locked(Page::Notices) {
                        // Opening the page shows the latest and reads them.
                        self.notices.list = None;
                        self.notices.settings = None;
                        self.notices.error = None;
                    }
                    self.center_ui.page = Page::Notices;
                }
                ui.add_space(14.0);
                ui.separator();
                ui.add_space(8.0);
                if nav_item(
                    ui,
                    Icon::Settings,
                    Page::Settings.title(),
                    None,
                    false,
                    self.center_ui.page == Page::Settings
                        || (self.center_ui.page == Page::DeviceDetails
                            && self.center_ui.detail_parent == Page::Settings),
                ) {
                    self.center_ui.page = Page::Settings;
                }
                ui.with_layout(egui::Layout::bottom_up(Align::Min), |ui| {
                    self.draw_account_footer(ui);
                    ui.add_space(8.0);
                    let active = self
                        .host
                        .as_ref()
                        .is_some_and(|h| h.status().session_active);
                    self.center_ui.components.button(ui, active);
                });
            });
    }

    fn draw_account_footer(&mut self, ui: &mut egui::Ui) {
        let (bounds, _) = ui.allocate_exact_size(vec2(ui.available_width(), 52.0), Sense::hover());
        let left = bounds.left() + 6.0;
        let right = bounds.right() - 6.0;
        ui.painter()
            .hline(left..=right, bounds.top(), Stroke::new(1.0, LINE));
        if self.needs_login() {
            self.signed_out_footer(ui, bounds);
            return;
        }

        let account = if self.logout_pending {
            t().center.signing_out
        } else if self.account_name.trim().is_empty() {
            t().center.signed_in
        } else {
            &self.account_name
        };
        let account_row = egui::Rect::from_min_max(
            egui::pos2(left, bounds.top() + 16.0),
            egui::pos2(right - 34.0, bounds.top() + 44.0),
        );
        ui.scope_builder(
            egui::UiBuilder::new()
                .max_rect(account_row)
                .layout(egui::Layout::left_to_right(Align::Center)),
            |ui| {
                ui.add(
                    egui::Label::new(
                        RichText::new(account)
                            .size(crate::ui::theme::BODY)
                            .color(TEXT),
                    )
                    .truncate(),
                )
                .on_hover_text(account);
            },
        );

        let exit = egui::Rect::from_center_size(
            egui::pos2(right - 12.0, account_row.center().y),
            vec2(28.0, 28.0),
        );
        if self.logout_pending {
            egui::Spinner::new().paint_at(ui, exit.shrink(5.0));
        } else {
            let response = ui
                .interact(exit, ui.id().with("account-logout"), Sense::click())
                .on_hover_cursor(egui::CursorIcon::PointingHand)
                .on_hover_text(t().common.sign_out);
            response.widget_info(|| {
                egui::WidgetInfo::labeled(egui::WidgetType::Button, true, t().common.sign_out)
            });
            if response.hovered() {
                ui.painter().rect_filled(exit, 5.0, SURFACE);
            }
            paint_icon(
                ui.painter(),
                exit,
                Icon::Logout,
                if response.hovered() { TEXT } else { MUTED },
            );
            if response.clicked() {
                self.logout();
            }
        }

    }

    fn draw_version(&mut self, ui: &mut egui::Ui, rect: egui::Rect) {
        use super::updates::State;
        let text = &t().center;
        let current = format!("v{}", env!("CARGO_PKG_VERSION"));
        let prerelease_offered = self.updates.allow_prerelease();
        let (label, color, mut hint, destination) = match &self.updates.state {
            State::Checking => (
                text.checking.into(),
                MUTED,
                (text.current_version)(&current),
                None,
            ),
            State::Current => (
                current.clone(),
                MUTED,
                (text.up_to_date)(prerelease_offered).to_owned(),
                None,
            ),
            State::Ahead => (
                current.clone(),
                MUTED,
                (text.ahead_of_release)(prerelease_offered).to_owned(),
                None,
            ),
            State::NoRelease => (
                current.clone(),
                MUTED,
                (text.no_public_release)(prerelease_offered).to_owned(),
                None,
            ),
            // The server's state, not an update failure: no warning here.
            State::Unreachable => (
                current.clone(),
                MUTED,
                format!(
                    "{}\n{}",
                    (text.current_version)(&current),
                    text.status_unreachable
                ),
                None,
            ),
            State::Failed(error) => (
                format!("{current} !"),
                AMBER,
                (text.update_check_failed)(error),
                None,
            ),
            State::Available {
                version,
                url,
                prerelease,
                ..
            } => {
                let label = format!("↑ v{version}");
                (
                    if label.chars().count() <= 10 {
                        label
                    } else {
                        text.new_version.into()
                    },
                    BLUE,
                    (text.update_available)(*prerelease, version, &current),
                    Some(url.clone()),
                )
            }
        };
        let checking = matches!(self.updates.state, State::Checking);
        let wait = self.updates.retry_wait();
        let enabled = !checking && (destination.is_some() || wait == 0);
        if !checking && destination.is_none() && wait > 0 {
            hint.push('\n');
            hint.push_str(&(text.check_again_in)(wait));
        }
        let mut job = egui::text::LayoutJob::simple_singleline(
            label,
            FontId::proportional(crate::ui::theme::TINY),
            Color32::PLACEHOLDER,
        );
        job.wrap.max_width = rect.width().max(0.0);
        job.wrap.max_rows = 1;
        job.wrap.break_anywhere = true;
        let text = ui.painter().layout_job(job);
        let text_size = text.size();
        let hit_rect = egui::Rect::from_min_size(
            rect.right_center() - vec2(text_size.x, text_size.y * 0.5),
            text_size,
        )
        .expand(2.0)
        .intersect(rect)
        .intersect(ui.clip_rect());
        let response = ui
            .interact(
                hit_rect,
                ui.id().with("check-release"),
                if enabled {
                    Sense::click()
                } else {
                    Sense::hover()
                },
            )
            .on_hover_text(hint);
        response.widget_info(|| {
            egui::WidgetInfo::labeled(
                egui::WidgetType::Button,
                enabled,
                t().center.check_for_updates,
            )
        });
        let response = if enabled {
            response.on_hover_cursor(egui::CursorIcon::PointingHand)
        } else {
            response
        };
        ui.painter()
            .with_clip_rect(rect.intersect(ui.clip_rect()))
            .galley(
                rect.right_center() - vec2(text_size.x, text_size.y * 0.5),
                text,
                if enabled && response.hovered() {
                    BLUE
                } else {
                    color
                },
            );
        if response.clicked() {
            if destination.is_some() {
                self.updates.dialog_open = true;
            } else {
                self.updates.request(ui.ctx());
            }
        }
    }

    fn alert(&mut self, ui: &mut egui::Ui) {
        self.power_results(ui);
        if self.status.kind.is_alert() {
            let icon = match self.status.kind {
                StatusKind::Notice => crate::ui::controls::DialogIcon::Info,
                StatusKind::Error => crate::ui::controls::DialogIcon::Error,
                _ => crate::ui::controls::DialogIcon::Warning,
            };
            crate::ui::controls::notice(
                ui.ctx(),
                "device-center-status",
                t().center.notice,
                icon,
                self.status.text.clone(),
            );
            self.status = StatusMessage::info("");
        }
    }

    fn active_view(&mut self, ui: &mut egui::Ui) {
        if self.viewers.active.len() > 1 {
            let mut close = None;
            ui.menu_button(
                (t().center.viewer_windows)(self.viewers.active.len()),
                |ui| {
                    for session in &self.viewers.active {
                        ui.horizontal(|ui| {
                            if ui
                                .add_enabled(
                                    !session.closing,
                                    crate::ui::controls::secondary(&format!(
                                        "{} · {}",
                                        session.alias,
                                        session.device_id.as_deref().unwrap_or(t().center.via_server)
                                    )),
                                )
                                .clicked()
                            {
                                session.handle.focus();
                                ui.close();
                            }
                            if ui
                                .add_enabled(
                                    !session.closing,
                                    crate::ui::controls::secondary(if session.closing {
                                        t().center.closing
                                    } else {
                                        t().common.disconnect
                                    }),
                                )
                                .clicked()
                            {
                                close = Some(session.key.clone());
                                ui.close();
                            }
                        });
                    }
                },
            );
            if let Some(key) = close {
                if let Some(session) = self.viewers.active.iter_mut().find(|s| s.key == key) {
                    session.handle.request_close();
                    session.closing = true;
                }
            }
            if ui
                .add_enabled(
                    self.viewers.active.iter().any(|s| !s.closing),
                    crate::ui::controls::secondary(t().center.stop_all_viewing),
                )
                .clicked()
            {
                self.stop_viewer();
            }
            return;
        }
        let Some(session) = self.viewers.active.first() else {
            return;
        };
        let closing = session.closing;
        let key = session.key.clone();
        let alias = session.alias.clone();
        // Inline in the existing right-aligned header; never add a session row
        // above the device list or assistance form.
        if ui
            .add_enabled(!closing, crate::ui::controls::secondary(t().center.stop_viewing))
            .clicked()
        {
            if let Some(session) = self.viewers.active.iter_mut().find(|s| s.key == key) {
                session.handle.request_close();
                session.closing = true;
            }
        }
        let text = if closing {
            (t().center.closing_viewer)(&alias)
        } else {
            (t().center.viewer_open)(&alias)
        };
        ui.add_sized(
            [
                ui.available_width()
                    .min(crate::ui::theme::SESSION_STATUS_WIDTH)
                    .max(0.),
                crate::ui::theme::CONTROL_HEIGHT,
            ],
            egui::Label::new(
                RichText::new(&text)
                    .size(crate::ui::theme::SMALL)
                    .color(MUTED),
            )
            .truncate(),
        )
        .on_hover_text(text);
    }

    pub(super) fn draw_dialogs(&mut self, ctx: &egui::Context) {
        if self.exit_requested {
            return;
        }
        if self.host_assist_dialog(ctx) {
            return;
        }
        if self.needs_login() {
            self.logout_confirmation = false;
            return;
        }
        if self.center_ui.power.is_some() {
            self.power_confirmation(ctx);
        } else if self.center_ui.edit.is_some() {
            self.device_edit_dialog(ctx);
        }
        if self.logout_confirmation {
            self.logout_dialog(ctx);
        }
        if self.center_ui.change_server {
            match cards::confirm(
                ctx,
                "change-server",
                t().center.change_server_title,
                t().center.change_server_body,
                t().center.change_server_action,
            ) {
                Some(true) => {
                    self.center_ui.change_server = false;
                    self.login_form.step = super::login_form::LoginStep::Server;
                    self.begin_logout();
                }
                Some(false) => self.center_ui.change_server = false,
                None => {}
            }
        }
        if self.center_ui.edit.is_none()
            && self.center_ui.power.is_none()
            && self.center_ui.page != Page::DeviceDetails
            && !self.logout_confirmation
        {
            self.assist_dialogs(ctx);
        }
    }

    fn device_edit_dialog(&mut self, ctx: &egui::Context) {
        let Some(mut edit) = self.center_ui.edit.take() else {
            return;
        };
        let mut commit = false;
        let mut cancel = false;
        let rename = matches!(edit.action, EditAction::Rename);
        let response = egui::Modal::new(egui::Id::new("edit-account-device"))
            .frame(dialog_frame())
            .show(ctx, |ui| {
                ui.set_width(450.0);
                cancel = crate::ui::controls::dialog_header(
                    ui,
                    if rename {
                        t().center.rename_device
                    } else {
                        t().center.remove_device_title
                    },
                    if rename {
                        crate::ui::controls::DialogIcon::Edit
                    } else {
                        crate::ui::controls::DialogIcon::Warning
                    },
                    true,
                );
                ui.add(egui::Label::new(display_alias(&edit.device)).wrap());
                ui.label(
                    RichText::new(&edit.device.device_id)
                        .monospace()
                        .small()
                        .color(MUTED),
                );
                ui.add_space(12.0);
                if rename {
                    ui.add_sized(
                        [ui.available_width(), theme::CONTROL_HEIGHT],
                        singleline_input(&mut edit.alias).hint_text(t().center.device_name),
                    );
                    crate::ui::controls::observe_form_notice(
                        ui.ctx(),
                        "device-name-validation",
                        t().center.invalid_name,
                        crate::ui::controls::DialogIcon::Warning,
                        edit.alias
                            .chars()
                            .any(char::is_control)
                            .then_some(t().center.name_control_characters),
                    );
                    if let Some(catalog) = &self.catalog
                        && edit.device.device_id == catalog.groups.current_device_id
                        && ui
                            .button((t().center.use_short_name)(&catalog.suggested_name))
                            .clicked()
                    {
                        edit.alias = catalog.suggested_name.clone();
                    }
                } else {
                    if self.viewer_for_device(&edit.device.device_id).is_some() {
                        ui.colored_label(AMBER, t().center.viewing_will_end);
                    }
                }
                let (accept, dismiss) = crate::ui::controls::dialog_actions(
                    ui,
                    Some(
                        crate::ui::controls::DialogAction::new(if rename {
                            t().center.save_name
                        } else {
                            t().common.remove
                        })
                        .enabled(
                            !self.mutation_pending
                                && (!rename
                                    || (!edit.alias.trim().is_empty()
                                        && edit.alias != edit.device.alias
                                        && !edit.alias.chars().any(char::is_control))),
                        )
                        .danger(!rename),
                    ),
                    Some(t().common.cancel),
                );
                commit = accept;
                cancel |= dismiss;
            });
        if commit {
            let change = match edit.action {
                EditAction::Rename => DeviceMutation::Rename {
                    id: edit.device.device_id,
                    alias: edit.alias.trim().to_owned(),
                },
                EditAction::Remove => DeviceMutation::Remove {
                    id: edit.device.device_id,
                },
            };
            self.queue_mutation(change);
        } else if !cancel && !response.should_close() {
            self.center_ui.edit = Some(edit);
        }
    }

    fn loading_page(&self, root: &mut egui::Ui) {
        egui::CentralPanel::default()
            .frame(egui::Frame::new().fill(BG))
            .show(root, |ui| {
                let bounds = ui.available_rect_before_wrap();
                let center = bounds.center();
                let logo = crate::ui::branding::logo_size(LOGO_LOADING);
                crate::ui::branding::paint_logo(
                    ui.painter(),
                    center.x - logo.x * 0.5,
                    center.y - 160.0,
                    LOGO_LOADING,
                    TEXT,
                );
                let width = (bounds.width() - 48.0).min(440.0);
                let rect = egui::Rect::from_min_size(
                    egui::pos2(center.x - width / 2.0, center.y - 88.0),
                    vec2(width, 330.0),
                );
                let mut content = ui.new_child(
                    egui::UiBuilder::new()
                        .id_salt("startup-progress")
                        .max_rect(rect)
                        .layout(egui::Layout::top_down(Align::Min)),
                );
                let current = self.startup_stage.index();
                content.horizontal(|ui| {
                    ui.strong(self.startup_stage.title());
                    ui.with_layout(egui::Layout::right_to_left(Align::Center), |ui| {
                        ui.weak((t().center.startup_step)(
                            current + 1,
                            StartupStage::ALL.len(),
                        ));
                    });
                });
                content.add_space(6.0);
                // Completed stages, not an estimate of remaining time.
                content.add(
                    egui::ProgressBar::new(current as f32 / StartupStage::ALL.len() as f32)
                        .desired_width(width)
                        .desired_height(7.0)
                        .fill(BLUE),
                );
                content.add_space(12.0);
                for (index, stage) in StartupStage::ALL.iter().enumerate() {
                    let active = index == current;
                    content.horizontal(|ui| {
                        let (mark, _) = ui.allocate_exact_size(vec2(14.0, 14.0), Sense::hover());
                        if index < current {
                            let c = mark.center();
                            ui.painter().line_segment(
                                [c + vec2(-4.0, 0.0), c + vec2(-1.0, 3.0)],
                                Stroke::new(1.5, GREEN),
                            );
                            ui.painter().line_segment(
                                [c + vec2(-1.0, 3.0), c + vec2(5.0, -4.0)],
                                Stroke::new(1.5, GREEN),
                            );
                        } else {
                            ui.painter().circle_stroke(
                                mark.center(),
                                4.0,
                                Stroke::new(1.3, if active { BLUE } else { LINE }),
                            );
                            if active {
                                ui.painter().circle_filled(mark.center(), 2.0, BLUE);
                            }
                        }
                        ui.label(
                            RichText::new(stage.title())
                                .size(crate::ui::theme::COMPACT_TEXT)
                                .color(if active { TEXT } else { MUTED }),
                        );
                        ui.with_layout(egui::Layout::right_to_left(Align::Center), |ui| {
                            let text = &t().center;
                            let status = if index < current {
                                text.stage_done.into()
                            } else if active {
                                let elapsed = self.startup_stage_since.elapsed().as_secs();
                                if elapsed >= 3 {
                                    (text.stage_running_for)(elapsed)
                                } else {
                                    text.stage_running.into()
                                }
                            } else {
                                text.stage_waiting.into()
                            };
                            ui.label(
                                RichText::new(status)
                                    .size(crate::ui::theme::SMALL)
                                    .color(if active { BLUE } else { MUTED }),
                            );
                        });
                    });
                }
                ui.ctx().request_repaint_after(Duration::from_millis(200));
            });
    }

    fn login_page(&mut self, root: &mut egui::Ui) {
        let locked = self.login_restoring
            || self.logout_pending
            || self.has_viewers()
            || self.mutation_pending;
        self.login_form.poll_probe();
        // A finished certificate check continues once the certificate is
        // trusted; otherwise the step shows it for the user to decide.
        if self.login_form.advance && !self.login_form.probing() {
            if self.login_form.certificate_trusted() {
                self.use_server();
            } else if self.login_form.probe.is_some() {
                self.login_form.advance = false;
            }
        }
        let mut action = LoginAction::default();
        let busy = locked || self.login_form.submitting || self.login_form.saving_server;
        let can_cancel = self.login_form.submitting && !locked;
        let step = self.login_form.step;
        // The card fits its step; a certificate check adds its result.
        let height = match step {
            LoginStep::Server if self.login_form.probe.is_some() => 565.0,
            LoginStep::Server => 465.0,
            LoginStep::Account => 525.0,
        };
        login_card(root, height, |ui| match step {
            LoginStep::Server => server_step(ui, &mut self.login_form, busy, &mut action),
            LoginStep::Account => {
                account_step(ui, &mut self.login_form, busy, can_cancel, &mut action)
            }
        });
        if action.cancel {
            self.cancel_password_login();
        } else if action.probe {
            self.login_form.start_probe(root.ctx());
        } else if action.trust {
            if let Some(Ok(probe)) = &self.login_form.probe {
                self.login_form.pin = probe.sha256.clone();
            }
            self.use_server();
        } else if action.next {
            // A typed fingerprint is checked when connecting; otherwise
            // the certificate is checked first.
            if self.login_form.certificate_trusted() || !self.login_form.pin.trim().is_empty() {
                self.use_server();
            } else {
                self.login_form.advance = true;
                self.login_form.start_probe(root.ctx());
            }
        } else if action.change_server {
            self.login_form.step = LoginStep::Server;
            self.login_form.error = None;
        } else if action.submit {
            self.submit_login();
        } else if action.guest {
            self.center_ui.browsing = true;
            remember_signed_out_view();
            self.center_ui.page = Page::ThisDevice;
        } else if action.direct {
            self.center_ui.browsing = true;
            remember_signed_out_view();
            self.center_ui.page = Page::Devices;
            self.direct.tab = super::direct::DeviceTab::Nearby;
            self.direct.tab_chosen = true;
            self.direct.scan(root.ctx());
        }
    }
}

#[derive(Default)]
struct LoginAction {
    probe: bool,
    next: bool,
    trust: bool,
    change_server: bool,
    submit: bool,
    cancel: bool,
    guest: bool,
    direct: bool,
}

fn step_title(ui: &mut egui::Ui, title: &str) {
    ui.label(RichText::new(title).size(theme::DIALOG_TITLE).strong());
    ui.add_space(12.0);
}

/// A thin line with a word in the middle.
fn divider(ui: &mut egui::Ui, text: &str) {
    ui.add_space(10.0);
    let (rect, _) = ui.allocate_exact_size(vec2(ui.available_width(), 18.0), Sense::hover());
    let galley = ui.painter().layout_no_wrap(
        text.into(),
        FontId::proportional(theme::SMALL),
        MUTED,
    );
    let half = galley.size().x / 2.0 + 10.0;
    let y = rect.center().y;
    for (from, to) in [
        (rect.left(), rect.center().x - half),
        (rect.center().x + half, rect.right()),
    ] {
        ui.painter()
            .hline(from..=to, y, Stroke::new(1.0, LINE));
    }
    ui.painter()
        .galley(rect.center() - galley.size() / 2.0, galley, MUTED);
    ui.add_space(6.0);
}

/// Step one: which Farwing Server to use, or none at all.
fn server_step(ui: &mut egui::Ui, form: &mut LoginForm, busy: bool, action: &mut LoginAction) {
    let text = &t().center;
    let width = ui.available_width();
    step_title(ui, text.connect_server);
    ui.label(RichText::new(text.server_address).color(MUTED));
    let before = form.server.clone();
    ui.add_enabled_ui(!busy, |ui| {
        let response = ui.add_sized(
            [width, theme::CONTROL_HEIGHT],
            singleline_input(&mut form.server)
                .hint_text(text.server_address_hint)
                .char_limit(256),
        );
        if response.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
            action.next = !form.server.trim().is_empty();
        }
    });
    if form.server != before {
        form.probe = None;
        form.advance = false;
    }
    ui.add_space(6.0);
    ui.label(RichText::new(text.fingerprint_optional).color(MUTED));
    ui.add_enabled_ui(!busy, |ui| {
        ui.add_sized(
            [width, theme::CONTROL_HEIGHT],
            singleline_input(&mut form.pin)
                .hint_text("SHA-256")
                .char_limit(128),
        );
    });
    ui.add_space(10.0);
    let ready = !busy && !form.probing() && !form.server.trim().is_empty();
    ui.horizontal(|ui| {
        let half = (width - ui.spacing().item_spacing.x) / 2.0;
        action.probe |= ui
            .add_enabled(
                ready,
                login_button(if form.probing() && !form.advance {
                    text.checking
                } else {
                    text.check_certificate
                })
                .min_size(vec2(half, theme::CONTROL_HEIGHT)),
            )
            .clicked();
        action.next |= ui
            .add_enabled(
                ready,
                login_button(if form.saving_server || (form.advance && form.probing()) {
                    text.connecting
                } else {
                    t().common.next
                })
                .fill(BLUE)
                .stroke(Stroke::NONE)
                .min_size(vec2(half, theme::CONTROL_HEIGHT)),
            )
            .clicked();
    });
    match &form.probe {
        Some(Ok(probe)) => {
            ui.add_space(6.0);
            let trusted = form.certificate_trusted();
            ui.label(
                RichText::new(if probe.trusted_by_system {
                    text.certificate_trusted_by_system
                } else if trusted {
                    text.certificate_matches
                } else {
                    text.certificate_untrusted
                })
                .size(theme::COMPACT_TEXT)
                .color(if trusted { MUTED } else { AMBER }),
            );
            ui.add(
                egui::Label::new(
                    RichText::new(&probe.sha256)
                        .monospace()
                        .size(crate::ui::theme::SMALL),
                )
                .wrap(),
            );
            if !trusted {
                action.trust |= ui
                    .add_enabled(
                        !busy,
                        login_button(text.trust_and_continue)
                            .min_size(vec2(width, theme::CONTROL_HEIGHT)),
                    )
                    .clicked();
            }
        }
        Some(Err(error)) => {
            ui.add_space(6.0);
            ui.colored_label(AMBER, error);
        }
        None => {}
    }
    if let Some(error) = &form.error
        && !form.saving_server
    {
        ui.add_space(4.0);
        ui.colored_label(AMBER, error);
    }
    divider(ui, text.or);
    action.direct |= ui
        .add_enabled(
            !busy,
            login_button(text.without_server).min_size(vec2(width, theme::CONTROL_HEIGHT)),
        )
        .clicked();
}

/// Step two: the account on the chosen server, or no account at all.
fn account_step(
    ui: &mut egui::Ui,
    form: &mut LoginForm,
    busy: bool,
    can_cancel: bool,
    action: &mut LoginAction,
) {
    let text = &t().center;
    let width = ui.available_width();
    step_title(ui, text.sign_in_title);
    let origin = LoginForm::saved_origin().unwrap_or_else(|| form.server.clone());
    ui.horizontal(|ui| {
        ui.label(RichText::new(text.server).color(MUTED));
        ui.add(egui::Label::new(RichText::new(&origin).color(TEXT)).truncate())
            .on_hover_text(&origin);
        ui.with_layout(egui::Layout::right_to_left(Align::Center), |ui| {
            action.change_server |= ui.add_enabled(!busy, egui::Link::new(text.change)).clicked();
        });
    });
    ui.add_space(8.0);
    ui.label(RichText::new(text.username).color(MUTED));
    ui.add_enabled_ui(!busy, |ui| {
        ui.add_sized(
            [width, theme::CONTROL_HEIGHT],
            singleline_input(&mut form.username).char_limit(128),
        );
    });
    ui.add_space(6.0);
    ui.label(RichText::new(text.password).color(MUTED));
    ui.add_enabled_ui(!busy, |ui| {
        let response = ui.add_sized(
            [width, theme::CONTROL_HEIGHT],
            singleline_input(&mut form.password)
                .password(true)
                .char_limit(1024),
        );
        if response.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
            action.submit = form.can_submit();
        }
    });
    ui.add_space(14.0);
    action.submit |= ui
        .add_enabled_ui(!busy && form.can_submit(), |ui| {
            ui.add_sized(
                [width, theme::CONTROL_HEIGHT],
                login_button(if form.submitting {
                    text.signing_in
                } else {
                    t().common.sign_in
                })
                    .fill(BLUE)
                    .stroke(Stroke::NONE),
            )
        })
        .inner
        .clicked();
        if form.submitting && !form.status.is_empty() {
            ui.add_space(6.0);
            ui.label(
                RichText::new(&form.status)
                    .size(theme::COMPACT_TEXT)
                    .color(MUTED),
            );
        } else if !busy && form.error.is_some() && !form.status.is_empty() {
            crate::ui::controls::notice(
                ui.ctx(),
                "password-login-result",
                text.sign_in_failed,
                crate::ui::controls::DialogIcon::Error,
                std::mem::take(&mut form.status),
            );
        }
    if can_cancel {
        ui.add_space(8.0);
        action.cancel = ui
            .add_sized(
                [width, theme::CONTROL_HEIGHT],
                login_button(t().common.cancel).frame(false),
            )
            .clicked();
    }
    divider(ui, text.without_account);
    ui.horizontal(|ui| {
        let half = (width - ui.spacing().item_spacing.x) / 2.0;
        action.guest |= ui
            .add_enabled(
                !busy,
                login_button(text.allow_connections_here)
                    .min_size(vec2(half, theme::CONTROL_HEIGHT)),
            )
            .clicked();
        action.direct |= ui
            .add_enabled(
                !busy,
                login_button(text.connect_to_device).min_size(vec2(half, theme::CONTROL_HEIGHT)),
            )
            .clicked();
    });
}

impl DeviceCenterApp {
    fn logout_dialog(&mut self, ctx: &egui::Context) {
        let mut confirm = false;
        let mut cancel = false;
        let response = egui::Modal::new(egui::Id::new("center-logout"))
            .frame(dialog_frame())
            .show(ctx, |ui| {
                ui.set_width(410.0);
                cancel = crate::ui::controls::dialog_header(
                    ui,
                    t().center.sign_out_title,
                    crate::ui::controls::DialogIcon::Warning,
                    true,
                );
                ui.label(t().center.sign_out_body);
                let (accept, dismiss) = crate::ui::controls::dialog_actions(
                    ui,
                    Some(crate::ui::controls::DialogAction::new(t().common.sign_out).danger(true)),
                    Some(t().common.cancel),
                );
                confirm = accept;
                cancel |= dismiss;
            });
        if confirm {
            self.begin_logout();
        } else if cancel || response.should_close() {
            self.logout_confirmation = false;
        }
    }

    /// Sign out once confirmed: viewers end, then the account.
    fn begin_logout(&mut self) {
        self.logout_confirmation = false;
        self.logout_pending = true;
        self.cancel_login();
        self.stop_viewer();
        self.status = StatusMessage::info(t().center.ending_viewing_and_signing_out);
    }
}
