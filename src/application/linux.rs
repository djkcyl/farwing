//! Native Linux device center. Authentication and sessions use the Farwing client stack.
#[path = "../ui/fonts.rs"]
mod fonts;
#[path = "viewer/linux.rs"]
pub(crate) mod viewer;

use crate::account::api::{DeviceInfo, device_id_text, platform_label};
use crate::account::client::{AuthenticatedClient, NoSavedSession};
use crate::account::login::{self, Credentials};
use crate::account::server::ServerConfig;
use crate::media::{CodecPreference, ConnectionMediaOptions, FrameRateChoice};
use crate::session::controller::ControllerConnection;
use anyhow::{Result, ensure};
use clap::Parser;
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
            .with_app_id("farwing"),
        ..Default::default()
    };
    eframe::run_native(
        &crate::product::display_name(),
        options,
        Box::new(|cc| {
            fonts::install(&cc.egui_ctx);
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
    Account(Result<Option<(Arc<AuthenticatedClient>, Vec<DeviceInfo>)>>),
    Devices(Result<Vec<DeviceInfo>>),
    Playback(viewer::Playback),
    Closed(Result<()>),
    SignedOut(Option<String>),
}

struct DeviceCenter {
    ctx: egui::Context,
    runtime: tokio::runtime::Handle,
    sender: mpsc::Sender<Event>,
    events: mpsc::Receiver<Event>,
    server: String,
    fingerprint: String,
    username: String,
    password: String,
    client: Option<Arc<AuthenticatedClient>>,
    devices: Vec<DeviceInfo>,
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
            server,
            fingerprint,
            username: String::new(),
            password: String::new(),
            client: None,
            devices: Vec::new(),
            alias: String::new(),
            playback: None,
            stop: None,
            busy: false,
            error,
        };
        if !state.server.is_empty() {
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
                    login::login_with_progress(credentials, |_| {}).await?;
                }
                let client = match AuthenticatedClient::from_saved_session() {
                    Ok(client) => Arc::new(client),
                    Err(error) if error.is::<NoSavedSession>() => return Ok(None),
                    Err(error) => return Err(error),
                };
                let devices = client.list_devices().await?.all().cloned().collect();
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
            let result = client
                .list_devices()
                .await
                .map(|devices| devices.all().cloned().collect());
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
        while let Ok(event) = self.events.try_recv() {
            match event {
                Event::Account(result) => {
                    self.busy = false;
                    match result {
                        Ok(Some((client, devices))) => {
                            self.client = Some(client);
                            self.devices = devices;
                        }
                        Ok(None) => {}
                        Err(error) => self.show_error(error),
                    }
                }
                Event::Devices(result) => {
                    self.busy = false;
                    match result {
                        Ok(devices) => self.devices = devices,
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
                    self.client = None;
                    self.devices.clear();
                    self.busy = false;
                    self.error = error;
                }
            }
        }
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let text = crate::i18n::t();
        egui::CentralPanel::default().show(ui, |ui| {
            if let Some(error) = &self.error {
                ui.colored_label(ui.visuals().error_fg_color, error);
            }
            if self.stop.is_some() {
                let mut disconnect = false;
                let mut error = None;
                ui.horizontal(|ui| {
                    ui.heading(&self.alias);
                    disconnect = ui.button(text.common.disconnect).clicked();
                    if let Some(playback) = &mut self.playback {
                        error = playback.toolbar(ui).err();
                    }
                });
                if disconnect {
                    self.disconnect();
                    return;
                }
                if let Some(error) = error {
                    self.show_error(error);
                }
                if let Some(playback) = &mut self.playback {
                    if let Err(error) = playback.picture(ui) {
                        self.show_error(error);
                        self.disconnect();
                    }
                } else {
                    ui.spinner();
                    ui.label(text.viewer.connecting);
                }
                return;
            }
            ui.heading("Farwing");
            if self.busy {
                ui.spinner();
            }
            if let Some(client) = &self.client {
                let client = client.clone();
                ui.horizontal(|ui| {
                    ui.label(&self.server);
                    if ui
                        .add_enabled(!self.busy, egui::Button::new(text.common.refresh))
                        .clicked()
                    {
                        self.refresh(client.clone());
                    }
                    if ui
                        .add_enabled(!self.busy, egui::Button::new(text.common.sign_out))
                        .clicked()
                    {
                        self.signed_out(client.clone());
                    }
                });
                let mut selected = None;
                let self_id = client.device_id();
                egui::ScrollArea::vertical().show(ui, |ui| {
                    for device in &self.devices {
                        ui.horizontal(|ui| {
                            ui.strong(&device.alias);
                            ui.label(platform_label(device.platform));
                            ui.label(device_id_text(&device.device_id));
                            ui.label(if device.is_connected() {
                                text.common.online
                            } else {
                                text.common.offline
                            });
                            if device.device_id != self_id
                                && device.controlled_support
                                && ui
                                    .add_enabled(
                                        !self.busy && device.is_connected(),
                                        egui::Button::new(text.common.connect),
                                    )
                                    .clicked()
                            {
                                selected = Some(device.clone());
                            }
                        });
                    }
                });
                if let Some(device) = selected {
                    self.connect(client, device);
                }
            } else {
                ui.add_enabled_ui(!self.busy, |ui| {
                    ui.label(text.center.server_address);
                    ui.text_edit_singleline(&mut self.server);
                    ui.label(text.center.fingerprint_optional);
                    ui.text_edit_singleline(&mut self.fingerprint);
                    ui.label(text.center.username);
                    ui.text_edit_singleline(&mut self.username);
                    ui.label(text.center.password);
                    ui.add(egui::TextEdit::singleline(&mut self.password).password(true));
                    if ui.button(text.common.sign_in).clicked() {
                        let result = (|| {
                            let config =
                                ServerConfig::new(&self.server, Some(&self.fingerprint), None)?;
                            ensure!(
                                !ServerConfig::overridden(),
                                "Unset FARWING_SERVER_URL to change the configured server"
                            );
                            let credentials = Credentials::new(
                                &self.username,
                                std::mem::take(&mut self.password),
                            )?;
                            Ok((config, credentials))
                        })();
                        match result {
                            Ok(login) => self.account(Some(login)),
                            Err(error) => self.show_error(error),
                        }
                    }
                });
            }
        });
    }
}
