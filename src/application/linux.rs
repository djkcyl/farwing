//! Native Linux device center. Authentication and sessions use the Farwing client stack.
#[path = "linux/login.rs"]
mod login;
#[path = "wallpaper/mod.rs"]
pub(crate) mod wallpaper;
#[path = "viewer/linux.rs"]
pub(crate) mod viewer;

use crate::account::api::{DeviceInfo, DeviceList, device_id_text, platform_label};
use crate::account::client::{AuthenticatedClient, NoSavedSession};
use crate::account::login::{self as account_login, Credentials};
use crate::account::server::ServerConfig;
use crate::media::{CodecPreference, ConnectionMediaOptions, FrameRateChoice};
use crate::session::controller::ControllerConnection;
use crate::i18n::t;
use crate::ui::center_icon::{Icon, paint_icon};
use crate::ui::controls::{self, ViewerCaptionIcon, viewer_caption_button};
use crate::ui::{branding, device_list, fonts, theme};
use anyhow::Result;
use clap::Parser;
use egui::{Align, Layout, RichText, Sense, Stroke, vec2};
use std::sync::{Arc, mpsc};
use tokio::sync::oneshot;
use tokio_util::sync::CancellationToken;

#[derive(Parser)]
#[command(
    name = "Farwing",
    version,
    about = "Farwing remote desktop client for Linux"
)]
struct Arguments {
    #[arg(long)]
    profile: Option<String>,
    #[arg(long)]
    log_level: Option<String>,
}

pub fn run() -> Result<()> {
    let args = Arguments::parse();
    crate::product::init_profile(args.profile.as_deref())?;
    crate::i18n::init();
    let _logging = crate::diagnostics::logging::init(args.log_level.as_deref(), None)?;
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1000.0, 680.0])
            .with_min_inner_size([720.0, 560.0])
            .with_icon(branding::icon())
            .with_app_id("farwing"),
        ..Default::default()
    };
    eframe::run_native(
        &crate::product::display_name(),
        options,
        Box::new(|cc| {
            fonts::install(&cc.egui_ctx);
            theme::configure(&cc.egui_ctx);
            Ok(Box::new(DeviceCenter::new(
                runtime.handle().clone(),
                cc.egui_ctx.clone(),
            )))
        }),
    )
    .map_err(|error| anyhow::anyhow!("{error}"))?;
    runtime.block_on(crate::session::controller::shutdown_all());
    Ok(())
}

enum Event {
    Account(Result<Option<(Arc<AuthenticatedClient>, DeviceList)>>),
    Devices(Result<DeviceList>),
    Playback(viewer::Playback),
    Closed(Result<()>),
    SignedOut(Option<String>),
}

struct DeviceCenter {
    ctx: egui::Context,
    runtime: tokio::runtime::Handle,
    sender: mpsc::Sender<Event>,
    events: mpsc::Receiver<Event>,
    sign_in: login::LoginScreen,
    account: Option<(Arc<AuthenticatedClient>, DeviceList)>,
    search: String,
    wallpapers: wallpaper::Wallpapers,
    alias: String,
    playback: Option<viewer::Playback>,
    stop: Option<CancellationToken>,
    busy: bool,
    error: Option<String>,
}

impl DeviceCenter {
    fn new(runtime: tokio::runtime::Handle, ctx: egui::Context) -> Self {
        let (sender, events) = mpsc::channel();
        let (server, fingerprint, error) = match ServerConfig::load() {
            Ok(Some(config)) => (
                config.origin().to_owned(),
                config.pinned_fingerprint().unwrap_or_default(),
                None,
            ),
            Ok(None) => (String::new(), String::new(), None),
            Err(error) => (String::new(), String::new(), Some(format!("{error:#}"))),
        };
        let mut state = Self {
            ctx,
            runtime,
            sender,
            events,
            sign_in: login::LoginScreen::new(server, fingerprint),
            account: None,
            search: String::new(),
            wallpapers: wallpaper::Wallpapers::default(),
            alias: String::new(),
            playback: None,
            stop: None,
            busy: false,
            error,
        };
        if !state.sign_in.server().is_empty() {
            state.account(None);
        }
        state
    }

    fn account(&mut self, login: Option<(ServerConfig, Credentials)>) {
        self.busy = true;
        self.error = None;
        let sender = self.sender.clone();
        let ctx = self.ctx.clone();
        self.runtime.spawn(async move {
            let result = async {
                if let Some((config, credentials)) = login {
                    config.save()?;
                    account_login::login_with_progress(credentials, |_| {}).await?;
                }
                let client = match AuthenticatedClient::from_saved_session() {
                    Ok(client) => Arc::new(client),
                    Err(error) if error.is::<NoSavedSession>() => return Ok(None),
                    Err(error) => return Err(error),
                };
                let devices = client.list_devices().await?;
                Ok(Some((client, devices)))
            }
            .await;
            let _ = sender.send(Event::Account(result));
            ctx.request_repaint();
        });
    }

    fn refresh(&mut self, client: Arc<AuthenticatedClient>) {
        self.busy = true;
        self.error = None;
        let sender = self.sender.clone();
        let ctx = self.ctx.clone();
        self.runtime.spawn(async move {
            let result = client.list_devices().await;
            let _ = sender.send(Event::Devices(result));
            ctx.request_repaint();
        });
    }

    fn connect(&mut self, client: Arc<AuthenticatedClient>, device: DeviceInfo) {
        let cancel = CancellationToken::new();
        self.stop = Some(cancel.clone());
        self.alias = device.alias.clone();
        self.error = None;
        let sender = self.sender.clone();
        let ctx = self.ctx.clone();
        self.runtime.spawn(async move {
            let policy = client.feature_catalog().policy(&device.device_id);
            let options = ConnectionMediaOptions {
                codec: CodecPreference::H264,
                frame_rate: FrameRateChoice::Fps30,
                ..Default::default()
            };
            let result =
                ControllerConnection::connect_headless(&client, &device, policy, options, &cancel)
                    .await;
            let mut connection = match result {
                Ok(connection) => connection,
                Err(error) => {
                    let _ = sender.send(Event::Closed(if cancel.is_cancelled() {
                        Ok(())
                    } else {
                        Err(error)
                    }));
                    ctx.request_repaint();
                    return;
                }
            };
            let result = tokio::select! {
                _ = cancel.cancelled() => Err(anyhow::anyhow!("Connection cancelled")),
                result = connection.start_linux_viewer(ctx.clone()) => result,
            };
            let result = match result {
                Ok(playback) => {
                    if sender.send(Event::Playback(playback)).is_ok() {
                        ctx.request_repaint();
                        let (shutdown, terminated) = oneshot::channel();
                        let request = tokio::spawn(async move {
                            cancel.cancelled().await;
                            let _ = shutdown.send(());
                        });
                        let result = connection.keep_alive(terminated).await;
                        request.abort();
                        result
                    } else {
                        connection.close().await
                    }
                }
                Err(error) => {
                    let closed = connection.close().await;
                    if cancel.is_cancelled() {
                        closed
                    } else {
                        Err(error)
                    }
                }
            };
            let _ = sender.send(Event::Closed(result));
            ctx.request_repaint();
        });
    }

    fn signed_out(&mut self, client: Arc<AuthenticatedClient>) {
        self.busy = true;
        let sender = self.sender.clone();
        let ctx = self.ctx.clone();
        self.runtime.spawn(async move {
            let result = client.logout().await;
            let _ = sender.send(Event::SignedOut(result.local_error.or(result.remote_error)));
            ctx.request_repaint();
        });
    }

    fn disconnect(&mut self) {
        if let Some(cancel) = &self.stop {
            self.playback.take();
            cancel.cancel();
        }
    }

    fn show_error(&mut self, error: anyhow::Error) {
        self.error = Some(format!("{error:#}"));
    }
}

impl Drop for DeviceCenter {
    fn drop(&mut self) {
        self.disconnect();
    }
}

impl eframe::App for DeviceCenter {
    fn logic(&mut self, _ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.wallpapers.poll(&self.ctx);
        while let Ok(event) = self.events.try_recv() {
            match event {
                Event::Account(result) => {
                    self.busy = false;
                    match result {
                        Ok(Some(account)) => self.account = Some(account),
                        Ok(None) => {}
                        Err(error) => self.show_error(error),
                    }
                }
                Event::Devices(result) => {
                    self.busy = false;
                    match result {
                        Ok(devices) => {
                            if let Some((_, list)) = &mut self.account {
                                *list = devices;
                            }
                        }
                        Err(error) => self.show_error(error),
                    }
                }
                Event::Playback(playback) => {
                    if self.stop.as_ref().is_some_and(|stop| !stop.is_cancelled()) {
                        self.playback = Some(playback);
                    }
                }
                Event::Closed(result) => {
                    self.playback = None;
                    self.stop = None;
                    if let Err(error) = result {
                        self.show_error(error);
                    }
                }
                Event::SignedOut(error) => {
                    self.account = None;
                    self.search.clear();
                    self.wallpapers.clear();
                    self.busy = false;
                    self.error = error;
                }
            }
        }
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        if self.stop.is_some() {
            self.viewer_page(ui);
        } else if self.account.is_some() {
            self.devices_page(ui);
        } else if let Some(result) = self.sign_in.show(ui, self.busy, &mut self.error) {
            match result {
                Ok(login) => self.account(Some(login)),
                Err(error) => self.show_error(error),
            }
        }
    }
}

impl DeviceCenter {
    fn devices_page(&mut self, root: &mut egui::Ui) {
        let Some((client, devices)) = &self.account else {
            return;
        };
        let text = t();
        let account = client.account_name();
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
                    ui.label(RichText::new(text.center.server).small().color(theme::MUTED));
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

        let mut sign_out = false;
        egui::Panel::left("linux-device-navigation")
            .resizable(false)
            .exact_size(theme::SIDEBAR_WIDTH)
            .frame(
                egui::Frame::new()
                    .fill(theme::SIDEBAR)
                    .inner_margin(egui::Margin::symmetric(12, 18)),
            )
            .show(root, |ui| {
                let (row, _) =
                    ui.allocate_exact_size(vec2(ui.available_width(), 30.0), Sense::hover());
                let logo = branding::logo_size(theme::LOGO_SIDEBAR);
                branding::paint_logo(
                    ui.painter(),
                    row.center().x - logo.x * 0.5,
                    row.center().y,
                    theme::LOGO_SIDEBAR,
                    theme::TEXT,
                );
                ui.add_space(22.0);
                let (row, _) = ui.allocate_exact_size(
                    vec2(ui.available_width(), theme::NAV_HEIGHT),
                    Sense::hover(),
                );
                ui.painter().rect_filled(row, theme::CONTROL_RADIUS, theme::SELECTED);
                paint_icon(
                    ui.painter(),
                    egui::Rect::from_center_size(
                        row.left_center() + vec2(20.0, 0.0),
                        vec2(22.0, 22.0),
                    ),
                    Icon::Monitor,
                    theme::ACCENT,
                );
                ui.painter().text(
                    row.left_center() + vec2(42.0, 0.0),
                    egui::Align2::LEFT_CENTER,
                    text.center.page_devices,
                    egui::FontId::proportional(theme::BODY),
                    theme::TEXT,
                );
                ui.with_layout(Layout::bottom_up(Align::Min), |ui| {
                    ui.allocate_ui_with_layout(
                        vec2(ui.available_width(), theme::VIEWER_CAPTION_BUTTON),
                        Layout::right_to_left(Align::Center),
                        |ui| {
                            sign_out = ui
                                .add_enabled_ui(!self.busy, |ui| {
                                    let (rect, response) =
                                        controls::icon_button_area(ui, theme::VIEWER_CAPTION_BUTTON);
                                    paint_icon(
                                        ui.painter(),
                                        rect,
                                        Icon::Logout,
                                        if response.hovered() { theme::TEXT } else { theme::MUTED },
                                    );
                                    response.widget_info(|| {
                                        egui::WidgetInfo::labeled(
                                            egui::WidgetType::Button,
                                            ui.is_enabled(),
                                            text.common.sign_out,
                                        )
                                    });
                                    response.on_hover_text(text.common.sign_out)
                                })
                                .inner
                                .clicked();
                            ui.with_layout(Layout::left_to_right(Align::Center), |ui| {
                                let name = if account.is_empty() {
                                    text.center.signed_in
                                } else {
                                    &account
                                };
                                ui.add(egui::Label::new(name).truncate()).on_hover_text(name);
                            });
                        },
                    );
                    ui.separator();
                });
            });

        let mut refresh = false;
        let mut selected = None;
        egui::CentralPanel::default()
            .frame(egui::Frame::new().fill(theme::BG).inner_margin(egui::Margin {
                left: 28,
                right: 28,
                top: 24,
                bottom: 0,
            }))
            .show(root, |ui| {
                ui.heading(text.center.page_devices);
                ui.add_space(10.0);
                device_list::toolbar(ui, &mut self.search, |ui| {
                    refresh = ui
                        .add_enabled(!self.busy, controls::secondary(text.common.refresh))
                        .clicked();
                    if self.busy {
                        ui.spinner();
                    }
                });
                if let Some(error) = &self.error {
                    ui.colored_label(theme::RED, error);
                }

                let query = self.search.trim().to_lowercase();
                controls::page_scroll("linux-devices").show(ui, |ui| {
                    let mut any = false;
                    for (title, group, local) in [
                        (text.center.tab_mine, devices.devices.as_slice(), false),
                        (text.center.tab_friends, devices.shared.as_slice(), false),
                        (
                            text.center.page_this_device,
                            std::slice::from_ref(&devices.current_device),
                            true,
                        ),
                    ] {
                        let rows: Vec<_> = group
                            .iter()
                            .filter(|device| {
                                device_list::matches(
                                    &query,
                                    &[
                                        &device.alias,
                                        &device_id_text(&device.device_id),
                                        platform_label(device.platform),
                                        &device.version_name,
                                    ],
                                )
                            })
                            .collect();
                        if rows.is_empty() {
                            continue;
                        }
                        device_list::heading(ui, title, (!local).then_some(rows.len()), !any, |_| {});
                        any = true;
                        for device in rows {
                            ui.push_id(&device.device_id, |ui| {
                                let texture = self.wallpapers.texture(
                                    ui.ctx(),
                                    &device.device_id,
                                    &device.wallpaper_url,
                                );
                                let alias = if device.alias.trim().is_empty() {
                                    text.center.unnamed_device
                                } else {
                                    &device.alias
                                };
                                let detail = format!(
                                    "{} · {}",
                                    platform_label(device.platform),
                                    device_id_text(&device.device_id),
                                );
                                let can_host = !local && device.controlled_support;
                                device_list::row(
                                    ui,
                                    &device_list::Row {
                                        title: alias,
                                        tile: device_list::Tile::of(texture.as_ref(), device.platform),
                                        status: (!local).then(|| {
                                            let color = if device.is_connected() {
                                                theme::GREEN
                                            } else {
                                                theme::MUTED
                                            };
                                            (device.status_label(), color)
                                        }),
                                        chips: &[],
                                        extra_badges: None,
                                        detail: &detail,
                                        selected: None,
                                    },
                                    |ui| {
                                        if can_host
                                            && ui
                                                .add_enabled(
                                                    !self.busy && device.is_connected(),
                                                    controls::primary(text.common.connect)
                                                        .min_size(vec2(84.0, controls::HEIGHT)),
                                                )
                                                .clicked()
                                        {
                                            selected = Some(device.clone());
                                        }
                                    },
                                );
                            });
                        }
                    }
                    if !any {
                        device_list::empty(ui, text.center.no_matching_devices);
                    }
                });
            });

        if sign_out {
            self.signed_out(client.clone());
        } else if refresh {
            self.refresh(client.clone());
        } else if let Some(device) = selected {
            self.connect(client.clone(), device);
        }
    }

    fn viewer_page(&mut self, root: &mut egui::Ui) {
        let text = t();
        let mut disconnect = false;
        let mut toolbar_error = None;
        egui::Panel::top("linux-viewer-caption")
            .resizable(false)
            .frame(
                egui::Frame::new()
                    .fill(theme::SIDEBAR)
                    .stroke(Stroke::new(1.0, theme::LINE))
                    .inner_margin(theme::WINDOW_TITLE_MARGIN),
            )
            .show(root, |ui| {
                ui.horizontal_centered(|ui| {
                    ui.set_height(theme::WINDOW_TITLE_CONTENT_HEIGHT);
                    let (icon, _) = ui.allocate_exact_size(
                        egui::Vec2::splat(theme::WINDOW_LOGO_SIZE),
                        Sense::hover(),
                    );
                    branding::paint(ui.painter(), icon, branding::Art::App, egui::Color32::WHITE);
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        disconnect = viewer_caption_button(
                            ui,
                            ViewerCaptionIcon::Close,
                            false,
                            text.common.disconnect,
                        )
                        .clicked();
                        if let Some(playback) = &mut self.playback {
                            toolbar_error = playback.toolbar(ui).err();
                        }
                        ui.with_layout(Layout::left_to_right(Align::Center), |ui| {
                            ui.add(egui::Label::new(RichText::new(&self.alias).strong()).truncate())
                                .on_hover_text(&self.alias);
                        });
                    });
                });
            });

        if disconnect {
            self.disconnect();
        }
        if let Some(error) = toolbar_error {
            self.show_error(error);
        }

        let notice = self
            .error
            .as_ref()
            .map(|error| (error.clone(), true))
            .or_else(|| self.playback.as_ref().and_then(viewer::Playback::notice));
        if let Some((notice, is_error)) = notice {
            egui::Panel::top("linux-viewer-notice")
                .frame(egui::Frame::new().fill(theme::SURFACE).inner_margin(8))
                .show(root, |ui| {
                    ui.colored_label(if is_error { theme::RED } else { theme::MUTED }, notice);
                });
        }

        egui::CentralPanel::default()
            .frame(egui::Frame::new().fill(egui::Color32::BLACK))
            .show(root, |ui| {
                if let Some(playback) = &mut self.playback {
                    if let Err(error) = playback.picture(ui) {
                        self.show_error(error);
                        self.disconnect();
                    }
                } else {
                    let closing = self.stop.as_ref().is_some_and(CancellationToken::is_cancelled);
                    viewer::waiting(
                        ui,
                        if closing { text.center.closing } else { text.viewer.connecting },
                    );
                }
            });
    }
}
