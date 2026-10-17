//! Device-center presentation. Network/account ownership remains in app.rs.
use super::*;
use crate::ui::theme::{
    self, ACCENT as BLUE, AMBER, BG, GREEN, LINE, MUTED, RED, SIDEBAR, SURFACE, TEXT,
};
use egui::{Align, Color32, FontId, RichText, Sense, Stroke, vec2};

mod about;
mod assist;
mod assist_components;
// Installing the service and drivers is part of the Windows setup.
#[cfg(windows)]
mod components;
mod device_details;
mod device_visuals;
mod devices;
mod diagnostic_export;
mod diagnostics_panel;
mod host_assist;
mod host_settings;
mod logs;
mod port_mapping;
mod power;
mod remote_diagnostics;
mod startup;
mod terminal_sessions;
mod update_dialog;
mod wol_setup;

fn singleline_input(value: &mut String) -> egui::TextEdit<'_> {
    crate::ui::controls::singleline(value, crate::ui::controls::HEIGHT)
}

#[derive(Clone, Copy, Default, PartialEq, Eq)]
enum Page {
    #[default]
    Mine,
    Assist,
    Favorites,
    Management,
    DeviceDetails,
    Settings,
    Diagnostics,
    Shortcuts,
    Logs,
    Plugins,
    TerminalSessions,
    About,
}
impl Page {
    fn title(self) -> &'static str {
        match self {
            Self::Mine => "我的设备",
            Self::Assist => "远程协助",
            Self::Favorites => "收藏设备",
            Self::Management => "全部设备",
            Self::DeviceDetails => "设备详情",
            Self::Settings => "连接设置",
            Self::Diagnostics => "本机诊断",
            Self::Shortcuts => "快捷键设置",
            Self::Logs => "日志设置",
            Self::Plugins => "插件管理",
            Self::TerminalSessions => "终端会话",
            Self::About => "关于",
        }
    }
}

#[derive(Default)]
pub(super) struct CenterUi {
    #[cfg(windows)]
    pub(super) components: components::Manager,
    page: Page,
    device_lists: [devices::ListUi; 2],
    detail_id: Option<String>,
    detail_parent: Page,
    wallpapers: crate::application::wallpaper::Wallpapers,
    edit: Option<DeviceEdit>,
    power: Option<power::PowerConfirmation>,
    legal_document: Option<about::LegalDocument>,
    logs: logs::LogUi,
    diagnostics: diagnostics_panel::ViewState,
    wol_setup: wol_setup::ViewState,
    startup: startup::ViewState,
    host_assist_password: Option<host_assist::PasswordEditor>,
    host_assist_settings_open: bool,
    host_assist_show_code: bool,
    guest_assist_open: bool,
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
    Close,
    Info,
    Assist,
    Star,
    Edit,
    Logout,
    Logs,
    Plugins,
    Keyboard,
    Diagnostics,
    Terminal,
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
        Icon::Terminal => {
            p.rect_stroke(
                egui::Rect::from_center_size(c, vec2(18.0, 14.0)),
                2.0,
                s,
                egui::StrokeKind::Inside,
            );
            p.add(egui::Shape::line(
                vec![q(-5.0, -3.0), q(-1.0, 0.0), q(-5.0, 3.0)],
                s,
            ));
            p.line_segment([q(1.0, 3.0), q(5.0, 3.0)], s);
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
        Icon::Star => {
            let points = (0..10)
                .map(|i| {
                    let angle =
                        -std::f32::consts::FRAC_PI_2 + i as f32 * std::f32::consts::PI / 5.0;
                    let radius = if i % 2 == 0 { 10.0 } else { 4.5 };
                    q(angle.cos() * radius, angle.sin() * radius)
                })
                .collect();
            p.add(egui::Shape::closed_line(points, s));
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
        Icon::Close => {
            crate::ui::controls::paint_close(p, rect, color);
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
        TEXT,
    );
    if let Some(count) = count {
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

fn presence_text(state: &PresenceState) -> (&'static str, Color32) {
    match state {
        PresenceState::Connecting => ("本机正在上线", MUTED),
        PresenceState::Online => ("本机在线", GREEN),
        PresenceState::Reconnecting => ("本机正在重连", AMBER),
        PresenceState::Offline => ("本机离线", MUTED),
    }
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
        ui.allocate_ui_with_layout(
            vec2(label_width, 52.0),
            egui::Layout::top_down(Align::Min),
            |ui| {
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
    ui.add_space(20.0);
    ui.label(
        RichText::new(title)
            .size(crate::ui::theme::SECTION)
            .strong(),
    );
    ui.add_space(8.0);
}

fn login_scan_placeholder(p: &egui::Painter, rect: egui::Rect) {
    let c = rect.center();
    let corner = Stroke::new(2.0, crate::ui::theme::BORDER_FOCUS);
    for (x, y) in [(-1.0, -1.0), (1.0, -1.0), (-1.0, 1.0), (1.0, 1.0)] {
        let edge = c + vec2(x * 48.0, y * 48.0);
        p.add(egui::Shape::line(
            vec![edge - vec2(x * 15.0, 0.0), edge, edge - vec2(0.0, y * 15.0)],
            corner,
        ));
    }
    p.rect_stroke(
        egui::Rect::from_center_size(c, vec2(38.0, 64.0)),
        6.0,
        Stroke::new(2.0, BLUE),
        egui::StrokeKind::Inside,
    );
    p.line_segment(
        [c + vec2(-6.0, -23.0), c + vec2(6.0, -23.0)],
        Stroke::new(2.0, BLUE),
    );
    p.circle_filled(c + vec2(0.0, 24.0), 2.0, BLUE);
}

fn login_qr_area(
    ui: &mut egui::Ui,
    texture: Option<&egui::TextureHandle>,
    loading: bool,
) -> egui::Rect {
    // Reserve the same square exactly once for every state. Drawing into it
    // must not advance the cursor again (Ui::put does), nor negotiate a Frame
    // width with the wider login column.
    let (rect, _) = ui.allocate_exact_size(
        vec2(theme::AUTH_QR_SIZE, theme::AUTH_QR_SIZE),
        Sense::hover(),
    );
    if let Some(texture) = texture {
        ui.painter()
            .rect_filled(rect, theme::CONTROL_RADIUS, Color32::WHITE);
        ui.painter().image(
            texture.id(),
            rect.shrink(12.0),
            egui::Rect::from_min_max(egui::Pos2::ZERO, egui::pos2(1.0, 1.0)),
            Color32::WHITE,
        );
    } else {
        ui.painter()
            .rect_filled(rect, theme::CONTROL_RADIUS, crate::ui::theme::SIDEBAR);
        ui.painter().rect_stroke(
            rect,
            theme::CONTROL_RADIUS,
            Stroke::new(1.0, LINE),
            egui::StrokeKind::Inside,
        );
        if loading {
            egui::Spinner::new().paint_at(
                ui,
                egui::Rect::from_center_size(rect.center(), vec2(28.0, 28.0)),
            );
        } else {
            login_scan_placeholder(ui.painter(), rect);
        }
    }
    rect
}

fn login_surface(
    root: &mut egui::Ui,
    brand_texture: &egui::TextureHandle,
    mut content: impl FnMut(&mut egui::Ui, LoginMethod),
) {
    egui::CentralPanel::default()
        .frame(egui::Frame::new().fill(BG).inner_margin(24))
        .show(root, |ui| {
            let bounds = ui.available_rect_before_wrap();
            let card = egui::Rect::from_center_size(
                bounds.center(),
                vec2(
                    800.0_f32.min(bounds.width()),
                    510.0_f32.min(bounds.height()),
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
            let title = painter.layout_no_wrap(
                crate::APP_NAME.into(),
                FontId::proportional(crate::ui::theme::TITLE),
                TEXT,
            );
            let brand_width = 36.0 + 12.0 + title.size().x;
            let brand = egui::Rect::from_min_size(
                egui::pos2(card.center().x - brand_width * 0.5, card.top() + 34.0),
                vec2(36.0, 36.0),
            );
            crate::ui::branding::paint(painter, brand, brand_texture);
            painter.galley(
                brand.right_center() + vec2(12.0, -title.size().y * 0.5),
                title,
                TEXT,
            );
            painter.vline(
                card.center().x,
                (card.top() + 106.0)..=(card.bottom() - 34.0),
                Stroke::new(1.0, LINE),
            );
            for (method, center_x, heading) in [
                (LoginMethod::Qr, card.center().x - 192.0, "扫码登录"),
                (LoginMethod::Phone, card.center().x + 192.0, "短信登录"),
            ] {
                let column = egui::Rect::from_min_max(
                    egui::pos2(center_x - 160.0, card.top() + 104.0),
                    egui::pos2(center_x + 160.0, card.bottom() - 24.0),
                );
                ui.scope_builder(
                    egui::UiBuilder::new()
                        .id_salt(heading)
                        .max_rect(column)
                        .layout(egui::Layout::top_down(Align::Center)),
                    |ui| {
                        ui.spacing_mut().item_spacing.y = 6.0;
                        ui.label(
                            RichText::new(heading)
                                .size(crate::ui::theme::SECTION)
                                .strong(),
                        );
                        ui.add_space(18.0);
                        content(ui, method);
                    },
                );
            }
        });
}

#[derive(Default, PartialEq, Eq)]
enum QrAction {
    #[default]
    None,
    Start,
}

fn qr_form(
    ui: &mut egui::Ui,
    texture: Option<&egui::TextureHandle>,
    running: bool,
    enabled: bool,
    status: &str,
    error: Option<&str>,
) -> QrAction {
    let rect = login_qr_area(ui, texture, running);
    let mut action = QrAction::None;
    if !running && error.is_some() {
        ui.painter()
            .rect_filled(rect, theme::CONTROL_RADIUS, Color32::from_black_alpha(205));
        let button = egui::Rect::from_center_size(
            rect.center(),
            vec2(theme::AUTH_CODE_ACTION_WIDTH, theme::CONTROL_HEIGHT),
        );
        let mut overlay = ui.new_child(
            egui::UiBuilder::new()
                .id_salt("qr-refresh-overlay")
                .max_rect(button)
                .layout(egui::Layout::top_down(Align::Center)),
        );
        if overlay
            .add_enabled(
                enabled,
                login_button(if texture.is_some() {
                    "刷新二维码"
                } else {
                    "重试登录"
                })
                .fill(BLUE)
                .stroke(Stroke::NONE)
                .min_size(button.size()),
            )
            .clicked()
        {
            action = QrAction::Start;
        }
    }
    ui.add_space(12.0);
    let message = error.unwrap_or(if status.is_empty() {
        "使用 UU 远程手机端扫码"
    } else {
        status
    });
    ui.add_sized(
        [ui.available_width(), 20.0],
        egui::Label::new(
            RichText::new(message)
                .size(theme::COMPACT_TEXT)
                .color(if error.is_some() { AMBER } else { MUTED }),
        )
        .truncate(),
    );
    action
}
impl DeviceCenterApp {
    fn needs_login(&self) -> bool {
        self.devices.is_none() && self.catalog.is_none()
    }

    pub(super) fn draw_center(&mut self, ui: &mut egui::Ui) {
        self.center_ui.wallpapers.poll(ui.ctx());
        if self.needs_login() {
            if self.login_restoring {
                self.loading_page(ui);
            } else {
                self.login_page(ui);
            }
            return;
        }
        // Reconcile navigation before choosing the panel margins. A removed
        // device must not leave an empty, edge-to-edge detail page behind.
        self.reconcile_device_details();
        let previous_page = self.center_ui.page;
        self.draw_navigation(ui);
        if self.center_ui.page != previous_page
            && matches!(self.center_ui.page, Page::Assist | Page::Favorites)
        {
            self.request_assist_refresh();
        }
        if self.center_ui.page != Page::Shortcuts {
            self.center_ui.shortcuts.reset();
        }
        if self.center_ui.page != Page::DeviceDetails {
            self.center_ui.detail_id = None;
        }
        egui::CentralPanel::default()
            .frame(egui::Frame::new().fill(BG).inner_margin(
                if self.center_ui.page == Page::DeviceDetails {
                    egui::Margin::ZERO
                } else {
                    egui::Margin::symmetric(28, 24)
                },
            ))
            .show(ui, |ui| {
                if self.center_ui.page == Page::DeviceDetails {
                    self.device_details_page(ui);
                } else if self.center_ui.page == Page::Settings {
                    self.settings_page(ui);
                } else if self.center_ui.page == Page::Diagnostics {
                    self.diagnostics_page(ui);
                } else if self.center_ui.page == Page::Shortcuts {
                    self.shortcuts_page(ui);
                } else if self.center_ui.page == Page::Logs {
                    self.logs_page(ui);
                } else if self.center_ui.page == Page::Plugins {
                    self.center_ui.plugins.show(ui);
                } else if self.center_ui.page == Page::About {
                    self.about_page(ui);
                } else if self.center_ui.page == Page::Management {
                    self.management_page(ui);
                } else if self.center_ui.page == Page::TerminalSessions {
                    self.terminal_sessions_page(ui);
                } else if matches!(self.center_ui.page, Page::Assist | Page::Favorites) {
                    self.assist_page(ui, self.center_ui.page == Page::Favorites);
                } else {
                    self.devices_page(ui);
                }
            });
    }

    fn draw_navigation(&mut self, ui: &mut egui::Ui) {
        egui::Panel::left("center-navigation")
            .resizable(false)
            .default_size(theme::SIDEBAR_WIDTH)
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
                    .on_hover_text("在 GitHub 查看 OpenUUYC");
                response.widget_info(|| {
                    egui::WidgetInfo::labeled(
                        egui::WidgetType::Button,
                        true,
                        "在 GitHub 查看 OpenUUYC",
                    )
                });
                let title_color = if response.hovered() { BLUE } else { TEXT };
                if response.clicked() {
                    ui.ctx()
                        .open_url(egui::OpenUrl::new_tab("https://github.com/djkcyl/openuuyc"));
                }
                let title = ui.painter().layout_no_wrap(
                    crate::APP_NAME.into(),
                    FontId::proportional(crate::ui::theme::BRAND),
                    title_color,
                );
                let width = 30.0 + 8.0 + title.size().x;
                let icon = egui::Rect::from_min_size(
                    egui::pos2(row.center().x - width * 0.5, row.center().y - 15.0),
                    vec2(30.0, 30.0),
                );
                crate::ui::branding::paint(ui.painter(), icon, &self.brand_texture);
                ui.painter().galley(
                    icon.right_center() + vec2(8.0, -title.size().y * 0.5),
                    title,
                    title_color,
                );
                ui.add_space(24.0);
                let count = self
                    .devices
                    .as_ref()
                    .map(|list| {
                        list.my_binded_devices
                            .iter()
                            .filter(|d| self.show_in_watching_list(d))
                            .count()
                    })
                    .unwrap_or_default();
                if nav_item(
                    ui,
                    Icon::Monitor,
                    "我的设备",
                    Some(count),
                    self.center_ui.page == Page::Mine
                        || (self.center_ui.page == Page::DeviceDetails
                            && self.center_ui.detail_parent == Page::Mine),
                ) {
                    self.center_ui.page = Page::Mine;
                }
                if nav_item(
                    ui,
                    Icon::Monitor,
                    "全部设备",
                    self.catalog.as_ref().map(|c| c.groups.entries().count()),
                    self.center_ui.page == Page::Management
                        || (self.center_ui.page == Page::DeviceDetails
                            && self.center_ui.detail_parent == Page::Management),
                ) {
                    self.center_ui.page = Page::Management;
                }
                let terminals = crate::features::host::terminal::running();
                ui.add_space(14.0);
                ui.label(RichText::new("本机被控").small().color(MUTED));
                if nav_item(
                    ui,
                    Icon::Terminal,
                    "终端会话",
                    (terminals > 0).then_some(terminals),
                    self.center_ui.page == Page::TerminalSessions,
                ) {
                    self.center_ui.page = Page::TerminalSessions;
                }
                ui.add_space(14.0);
                ui.label(RichText::new("远程协助").small().color(MUTED));
                if nav_item(
                    ui,
                    Icon::Assist,
                    "开始协助",
                    None,
                    self.center_ui.page == Page::Assist,
                ) {
                    self.center_ui.page = Page::Assist;
                    self.request_assist_refresh();
                }
                if nav_item(
                    ui,
                    Icon::Star,
                    "收藏设备",
                    self.assist.lists.as_ref().map(|l| l.favorites.len()),
                    self.center_ui.page == Page::Favorites,
                ) {
                    self.center_ui.page = Page::Favorites;
                    self.request_assist_refresh();
                }
                ui.add_space(14.0);
                ui.separator();
                ui.add_space(8.0);
                if nav_item(
                    ui,
                    Icon::Settings,
                    "连接设置",
                    None,
                    self.center_ui.page == Page::Settings,
                ) {
                    self.center_ui.page = Page::Settings;
                }
                if nav_item(
                    ui,
                    Icon::Keyboard,
                    "快捷键设置",
                    None,
                    self.center_ui.page == Page::Shortcuts,
                ) {
                    self.center_ui.page = Page::Shortcuts;
                }
                if nav_item(
                    ui,
                    Icon::Plugins,
                    "插件管理",
                    None,
                    self.center_ui.page == Page::Plugins,
                ) {
                    self.center_ui.page = Page::Plugins;
                }
                if nav_item(
                    ui,
                    Icon::Logs,
                    "日志设置",
                    None,
                    self.center_ui.page == Page::Logs,
                ) {
                    self.center_ui.page = Page::Logs;
                }
                if nav_item(
                    ui,
                    Icon::Diagnostics,
                    "本机诊断",
                    None,
                    self.center_ui.page == Page::Diagnostics,
                ) {
                    self.center_ui.page = Page::Diagnostics;
                }
                if nav_item(
                    ui,
                    Icon::Info,
                    "关于",
                    None,
                    self.center_ui.page == Page::About,
                ) {
                    self.center_ui.page = Page::About;
                    self.center_ui.legal_document = None;
                }
                ui.with_layout(egui::Layout::bottom_up(Align::Min), |ui| {
                    self.draw_account_footer(ui);
                    ui.add_space(8.0);
                    #[cfg(windows)]
                    {
                        let active = self
                            .host
                            .as_ref()
                            .is_some_and(|h| h.status().session_active);
                        self.center_ui.components.button(ui, active);
                    }
                });
            });
    }

    fn draw_account_footer(&mut self, ui: &mut egui::Ui) {
        let (bounds, _) = ui.allocate_exact_size(vec2(ui.available_width(), 76.0), Sense::hover());
        let left = bounds.left() + 6.0;
        let right = bounds.right() - 6.0;
        ui.painter()
            .hline(left..=right, bounds.top(), Stroke::new(1.0, LINE));

        let account = if self.logout_pending {
            "正在退出账号…"
        } else if self.account_name.trim().is_empty() {
            "已登录"
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
                .on_hover_text("退出账号");
            response.widget_info(|| {
                egui::WidgetInfo::labeled(egui::WidgetType::Button, true, "退出账号")
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

        let (presence, color) = if self.host.as_ref().is_some_and(|h| h.status().connected) {
            ("正在被控", GREEN)
        } else {
            presence_text(&self.presence)
        };
        let baseline = bounds.top() + 62.0;
        ui.painter()
            .circle_filled(egui::pos2(left + 3.0, baseline), 3.0, color);
        let presence_rect = ui.painter().text(
            egui::pos2(left + 13.0, baseline),
            egui::Align2::LEFT_CENTER,
            presence,
            FontId::proportional(crate::ui::theme::TINY),
            MUTED,
        );
        self.draw_version(
            ui,
            egui::Rect::from_min_max(
                egui::pos2((presence_rect.right() + 8.0).min(right), baseline - 11.0),
                egui::pos2(right, baseline + 11.0),
            ),
        );
    }

    fn draw_version(&mut self, ui: &mut egui::Ui, rect: egui::Rect) {
        use super::updates::State;
        let current = format!("v{}", env!("CARGO_PKG_VERSION"));
        let (label, color, mut hint, destination) = match &self.updates.state {
            State::Checking => (
                "检查中…".into(),
                MUTED,
                format!("当前版本 {current}，正在检查更新…"),
                None,
            ),
            State::Current => (
                current.clone(),
                MUTED,
                format!(
                    "当前版本 {current}，已是最新{}，点击重新检查",
                    self.updates.release_label()
                ),
                None,
            ),
            State::Ahead => (
                current.clone(),
                MUTED,
                format!(
                    "当前版本 {current} 高于 GitHub 最新{}，点击重新检查",
                    self.updates.release_label()
                ),
                None,
            ),
            State::NoRelease => (
                current.clone(),
                MUTED,
                format!(
                    "当前版本 {current}，暂无公开{}，点击重新检查",
                    self.updates.release_label()
                ),
                None,
            ),
            State::Failed(error) => (
                format!("{current} !"),
                AMBER,
                format!("当前版本 {current}\n检查更新失败：{error}\n点击重试"),
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
                        "有新版本".into()
                    },
                    BLUE,
                    format!(
                        "发现{} v{version}（当前 {current}）\n点击查看更新内容",
                        if *prerelease {
                            "测试版"
                        } else {
                            "新版本"
                        }
                    ),
                    Some(url.clone()),
                )
            }
        };
        let checking = matches!(self.updates.state, State::Checking);
        let wait = self.updates.retry_wait();
        let enabled = !checking && (destination.is_some() || wait == 0);
        if !checking && destination.is_none() && wait > 0 {
            hint.push_str(&format!("\n{wait} 秒后可重新检查"));
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
            egui::WidgetInfo::labeled(egui::WidgetType::Button, enabled, "检查更新")
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
                "操作提示",
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
                format!("观看窗口（{}）", self.viewers.active.len()),
                |ui| {
                    for session in &self.viewers.active {
                        ui.horizontal(|ui| {
                            if ui
                                .add_enabled(
                                    !session.closing,
                                    crate::ui::controls::secondary(&format!(
                                        "{} · {}",
                                        session.alias,
                                        session.device_id.as_deref().unwrap_or("协助连接")
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
                                        "正在关闭"
                                    } else {
                                        "断开"
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
                    crate::ui::controls::secondary("结束全部观看"),
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
            .add_enabled(!closing, crate::ui::controls::secondary("结束观看"))
            .clicked()
        {
            if let Some(session) = self.viewers.active.iter_mut().find(|s| s.key == key) {
                session.handle.request_close();
                session.closing = true;
            }
        }
        let text = if closing {
            format!("正在关闭  {alias}")
        } else {
            format!("观看窗口已打开  ·  {alias}")
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

    fn empty_state(&mut self, ui: &mut egui::Ui, title: &str, action: Option<&str>) -> bool {
        ui.add_space(56.0);
        ui.vertical_centered(|ui| {
            let (rect, _) = ui.allocate_exact_size(vec2(48.0, 48.0), Sense::hover());
            paint_icon(ui.painter(), rect, Icon::Monitor, MUTED);
            ui.add_space(10.0);
            ui.label(
                RichText::new(title)
                    .size(crate::ui::theme::DIALOG_TITLE)
                    .strong(),
            );
            let clicked = action.is_some_and(|label| {
                ui.add_space(16.0);
                ui.add(crate::ui::controls::secondary(label)).clicked()
            });
            if self.devices.is_none() && !self.refresh_pending {
                ui.add_space(16.0);
                if ui
                    .add_enabled(
                        !self.login_running && !self.logout_pending && !self.has_viewers(),
                        primary("刷新"),
                    )
                    .clicked()
                {
                    self.request_refresh();
                }
            }
            clicked
        })
        .inner
    }

    fn shortcuts_page(&mut self, ui: &mut egui::Ui) {
        ui.label(RichText::new("快捷键设置").size(theme::TITLE).strong());
        ui.add_space(18.0);
        crate::ui::controls::page_scroll("center-shortcuts-scroll").show(ui, |ui| {
            self.center_ui.shortcuts.draw(ui);
        });
    }

    fn settings_page(&mut self, ui: &mut egui::Ui) {
        let original_media = self.media;
        ui.label(
            RichText::new("连接设置")
                .size(crate::ui::theme::TITLE)
                .strong(),
        );
        ui.add_space(18.0);
        self.alert(ui);
        crate::ui::controls::page_scroll("center-settings-scroll").show(ui, |ui| {
            self.startup_settings(ui);
            self.host_settings(ui);
            self.notification_settings(ui);
            section(ui, "本机作为主控");
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
            #[cfg(windows)]
            {
                let active = self
                    .host
                    .as_ref()
                    .is_some_and(|h| h.status().session_active);
                self.center_ui.components.management(ui, active);
            }
        });
        if self.media != original_media {
            if let Err(error) = crate::media::preferences::save(self.media) {
                self.media = original_media;
                self.status = StatusMessage::error(format!("保存编解码设置失败：{error:#}"));
            }
        }
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
            self.takeover_confirmation = None;
            return;
        }
        if self.takeover_confirmation.is_some() {
            self.takeover_dialog(ctx);
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
        if self.center_ui.edit.is_none()
            && self.center_ui.power.is_none()
            && self.center_ui.page != Page::DeviceDetails
            && !self.logout_confirmation
        {
            self.assist_dialogs(ctx);
        }
    }

    fn takeover_dialog(&mut self, ctx: &egui::Context) {
        let Some((generation, device, audio_only)) = self.takeover_confirmation.take() else {
            return;
        };
        if generation != self.login_generation || self.logout_pending || self.mutation_pending {
            return;
        }
        match crate::session::controller::takeover::confirmation(ctx, &device) {
            Some(true) => {
                if !self.is_viewing_target(&device.device_id) {
                    self.status = StatusMessage::warning("设备已不在可观看清单中，请刷新后重试");
                } else if let Some(issue) = self.viewer_action_issue(&device) {
                    self.status = StatusMessage::warning(issue);
                } else {
                    let approval =
                        crate::session::controller::takeover::Approval::confirmed(&device);
                    self.spawn_viewer_with_takeover(
                        display_alias(&device).to_owned(),
                        Some(device.device_id),
                        None,
                        Some(approval),
                        audio_only,
                    );
                }
            }
            Some(false) => {}
            None => self.takeover_confirmation = Some((generation, device, audio_only)),
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
                        "重命名设备"
                    } else {
                        "从账号移除设备？"
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
                        singleline_input(&mut edit.alias).hint_text("设备名称"),
                    );
                    crate::ui::controls::observe_form_notice(
                        ui.ctx(),
                        "device-name-validation",
                        "名称无效",
                        crate::ui::controls::DialogIcon::Warning,
                        edit.alias
                            .chars()
                            .any(char::is_control)
                            .then_some("名称不能包含换行或控制字符"),
                    );
                    if let Some(catalog) = &self.catalog
                        && edit.device.device_id == catalog.groups.current_device_id
                        && ui
                            .button(format!("使用短名  {}", catalog.suggested_name))
                            .clicked()
                    {
                        edit.alias = catalog.suggested_name.clone();
                    }
                } else {
                    if self.viewer_for_device(&edit.device.device_id).is_some() {
                        ui.colored_label(AMBER, "当前观看将结束。");
                    }
                }
                let (accept, dismiss) = crate::ui::controls::dialog_actions(
                    ui,
                    Some(
                        crate::ui::controls::DialogAction::new(if rename {
                            "保存名称"
                        } else {
                            "确认移除"
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
                    Some("取消"),
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
                crate::ui::branding::paint(
                    ui.painter(),
                    egui::Rect::from_center_size(center - vec2(0.0, 192.0), vec2(88.0, 88.0)),
                    &self.brand_texture,
                );
                ui.painter().text(
                    center - vec2(0.0, 128.0),
                    egui::Align2::CENTER_CENTER,
                    crate::APP_NAME,
                    FontId::proportional(crate::ui::theme::TITLE),
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
                        ui.weak(format!(
                            "步骤 {} / {}",
                            current + 1,
                            StartupStage::ALL.len()
                        ));
                    });
                });
                let detail = if self.startup_stage == StartupStage::Devices
                    && !ui.ctx().input(|i| i.focused)
                {
                    "窗口不在前台，设备清单刷新已暂停；返回窗口后继续加载。"
                } else {
                    self.startup_stage.detail()
                };
                content.add(
                    egui::Label::new(
                        RichText::new(detail)
                            .size(crate::ui::theme::COMPACT_TEXT)
                            .color(MUTED),
                    )
                    .wrap(),
                );
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
                            let status = if index < current {
                                "已完成".into()
                            } else if active {
                                let elapsed = self.startup_stage_since.elapsed().as_secs();
                                if elapsed >= 3 {
                                    format!("进行中 · {elapsed} 秒")
                                } else {
                                    "进行中".into()
                                }
                            } else {
                                "等待".into()
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
        if self.center_ui.guest_assist_open {
            egui::CentralPanel::default()
                .frame(egui::Frame::new().fill(BG).inner_margin(32))
                .show(root, |ui| {
                    if ui.add(crate::ui::controls::secondary("返回登录")).clicked() {
                        self.center_ui.guest_assist_open = false;
                    }
                    ui.add_space(24.0);
                    ui.heading("远程协助");
                    ui.label(
                        RichText::new("无需登录，将设备 ID 和验证码分享给协助你的人。")
                            .color(MUTED),
                    );
                    ui.add_space(20.0);
                    if self.host.as_ref().is_some_and(|h| h.is_guest()) {
                        self.host_assist_controls(ui);
                        self.notification_settings(ui);
                        if let Some(host) = &self.host {
                            let status = host.status();
                            ui.add_space(12.0);
                            ui.label(if status.connected {
                                "正在接受远程协助"
                            } else if !host.allowed() {
                                "尚未开启远程协助"
                            } else {
                                &status.message
                            });
                            if status.connected
                                && ui.add(crate::ui::controls::secondary("断开连接")).clicked()
                            {
                                host.disconnect();
                            }
                            if let Some(error) = status.error {
                                ui.colored_label(AMBER, error);
                            }
                        }
                    } else {
                        ui.label("正在准备本机协助…");
                    }
                });
            return;
        }
        egui::Panel::bottom("guest-assist-entry")
            .frame(egui::Frame::new().fill(BG).inner_margin(16))
            .show(root, |ui| {
                ui.vertical_centered(|ui| {
                    if ui
                        .add(crate::ui::controls::secondary("无需登录 · 接受远程协助"))
                        .clicked()
                    {
                        self.center_ui.guest_assist_open = true;
                    }
                });
            });
        let locked = self.login_restoring
            || self.logout_pending
            || self.has_viewers()
            || self.mutation_pending;
        // First restore saved credentials. Once the login page is idle,
        // request QR automatically; failures require an explicit refresh.
        if !locked && !self.qr_running && self.login_qr.is_none() && self.login_error.is_none() {
            self.begin_login();
        }
        let mut qr_action = QrAction::None;
        let mut phone_action = PhoneAction::default();
        login_surface(root, &self.brand_texture, |ui, method| match method {
            LoginMethod::Qr => {
                qr_action = qr_form(
                    ui,
                    self.login_qr.as_ref(),
                    self.login_restoring || self.qr_running,
                    !locked,
                    &self.login_status,
                    self.login_error.as_deref(),
                );
            }
            LoginMethod::Phone => {
                let running = self.phone.sending || self.phone.submitting;
                phone_action =
                    phone_form(ui, &mut self.phone, locked || running, running && !locked);
            }
        });
        if phone_action.cancel {
            self.cancel_phone_login();
        } else if qr_action == QrAction::Start {
            self.begin_login();
        } else if phone_action.send {
            self.request_sms_code();
        } else if phone_action.submit {
            self.submit_sms_login();
        }
    }
}

#[derive(Default)]
struct PhoneAction {
    send: bool,
    submit: bool,
    cancel: bool,
}

fn phone_form(
    ui: &mut egui::Ui,
    phone: &mut PhoneForm,
    busy: bool,
    can_cancel: bool,
) -> PhoneAction {
    let before = phone.contact().ok();
    let mut action = PhoneAction::default();
    let width = ui.available_width();
    ui.allocate_ui_with_layout(vec2(width, 0.0), egui::Layout::top_down(Align::Min), |ui| {
        ui.spacing_mut().item_spacing = vec2(8.0, 6.0);
        ui.label(RichText::new("手机号").color(MUTED));
        ui.add_enabled_ui(!busy, |ui| {
            ui.horizontal(|ui| {
                ui.add_sized(
                    [theme::AUTH_COUNTRY_WIDTH, theme::CONTROL_HEIGHT],
                    singleline_input(&mut phone.country)
                        .hint_text("+86")
                        .char_limit(6)
                        .horizontal_align(Align::Center),
                );
                ui.add_sized(
                    [
                        width - theme::AUTH_COUNTRY_WIDTH - 8.0,
                        theme::CONTROL_HEIGHT,
                    ],
                    singleline_input(&mut phone.mobile)
                        .hint_text("请输入手机号")
                        .char_limit(24),
                );
            });
        });
        if phone.contact().ok() != before {
            phone.code.clear();
            phone.status.clear();
            phone.error = None;
        }
        ui.add_space(14.0);
        ui.label(RichText::new("验证码").color(MUTED));
        ui.horizontal(|ui| {
            ui.add_enabled_ui(!busy, |ui| {
                let response = ui.add_sized(
                    [
                        width - theme::AUTH_CODE_ACTION_WIDTH - 8.0,
                        theme::CONTROL_HEIGHT,
                    ],
                    singleline_input(&mut phone.code)
                        .hint_text("6位短信验证码")
                        .char_limit(6),
                );
                if response.has_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                    action.submit = phone.can_submit();
                }
            });
            let remaining = phone.remaining();
            let label = if phone.sending {
                "发送中…".into()
            } else if remaining > 0 {
                format!("{remaining}秒后重发")
            } else {
                "获取验证码".into()
            };
            action.send = ui
                .add_enabled(
                    !busy && remaining == 0 && phone.agreed && phone.contact().is_ok(),
                    login_button(&label)
                        .min_size(vec2(theme::AUTH_CODE_ACTION_WIDTH, theme::CONTROL_HEIGHT)),
                )
                .clicked();
        });
        ui.add_space(12.0);
        ui.add_enabled_ui(!busy, |ui| {
            ui.horizontal_wrapped(|ui| {
                ui.spacing_mut().item_spacing.x = 4.0;
                ui.spacing_mut().interact_size.y = 20.0;
                ui.checkbox(
                    &mut phone.agreed,
                    RichText::new("我已阅读并同意").size(crate::ui::theme::SMALL),
                );
                ui.hyperlink_to(
                    RichText::new("用户协议").size(crate::ui::theme::SMALL),
                    login::sms::TERMS_URL,
                );
                ui.label(RichText::new("和").size(crate::ui::theme::SMALL));
                ui.hyperlink_to(
                    RichText::new("隐私政策").size(crate::ui::theme::SMALL),
                    login::sms::PRIVACY_URL,
                );
            });
        });
        ui.add_space(18.0);
        action.submit |= ui
            .add_enabled_ui(!busy && phone.can_submit(), |ui| {
                ui.add_sized(
                    [width, theme::CONTROL_HEIGHT],
                    login_button(if busy && !phone.sending {
                        "正在登录…"
                    } else {
                        "登录"
                    })
                    .fill(BLUE)
                    .stroke(Stroke::NONE),
                )
            })
            .inner
            .clicked();
        if !busy && !phone.status.is_empty() {
            crate::ui::controls::notice(
                ui.ctx(),
                "phone-login-result",
                "登录提示",
                if phone.error.is_some() {
                    crate::ui::controls::DialogIcon::Error
                } else {
                    crate::ui::controls::DialogIcon::Info
                },
                std::mem::take(&mut phone.status),
            );
        }
        if can_cancel {
            ui.add_space(8.0);
            action.cancel = ui
                .add_sized(
                    [width, theme::CONTROL_HEIGHT],
                    login_button("取消").frame(false),
                )
                .clicked();
        }
    });
    action
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
                    "退出登录？",
                    crate::ui::controls::DialogIcon::Warning,
                    true,
                );
                ui.label("当前连接将结束，本机设备也会从账号中移除。");
                let (accept, dismiss) = crate::ui::controls::dialog_actions(
                    ui,
                    Some(crate::ui::controls::DialogAction::new("退出登录").danger(true)),
                    Some("取消"),
                );
                confirm = accept;
                cancel |= dismiss;
            });
        if confirm {
            self.logout_confirmation = false;
            self.logout_pending = true;
            self.cancel_login();
            self.stop_viewer();
            self.status = StatusMessage::info("正在结束观看并退出账号…");
        } else if cancel || response.should_close() {
            self.logout_confirmation = false;
        }
    }
}
