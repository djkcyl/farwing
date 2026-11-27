//! Native Linux device center. Authentication and sessions use the Farwing client stack.
#[path = "linux/center.rs"]
mod center;
#[path = "linux/cli.rs"]
mod cli;
#[path = "linux/chat.rs"]
mod chat;
#[path = "linux/device_switch.rs"]
mod device_switch;
#[path = "linux/joins.rs"]
mod joins;
#[path = "linux/login.rs"]
mod login;
#[path = "linux/settings.rs"]
mod settings;
#[path = "app/updates.rs"]
mod updates;
#[path = "viewer_shortcuts.rs"]
pub(crate) mod viewer_shortcuts;
#[path = "viewer/linux.rs"]
pub(crate) mod viewer;
#[path = "linux/viewers.rs"]
mod viewers;
#[path = "wallpaper/mod.rs"]
pub(crate) mod wallpaper;

use crate::account::api::{DeviceInfo, DeviceList};
use crate::account::client::{AuthenticatedClient, NoSavedSession};
use crate::account::login::{self as account_login, Credentials};
use crate::account::server::ServerConfig;
use crate::i18n::t;
use crate::media::ConnectionMediaOptions;
use crate::session::controller::ControllerConnection;
use crate::ui::{branding, fonts, theme};
use anyhow::Result;
use clap::Parser;
use std::sync::{Arc, mpsc};

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
    #[command(subcommand)]
    command: Option<cli::Command>,
}

pub fn run() -> Result<()> {
    let args = Arguments::parse();
    crate::product::init_profile(args.profile.as_deref())?;
    crate::i18n::init();
    let _logging = crate::diagnostics::logging::init(args.log_level.as_deref(), None)?;
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    let launch = if let Some(command) = args.command {
        match runtime.block_on(cli::run(command))? {
            Some(launch) => Some(launch),
            None => return Ok(()),
        }
    } else {
        None
    };
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1000.0, 680.0])
            .with_min_inner_size([720.0, 560.0])
            .with_icon(branding::icon())
            .with_app_id("farwing"),
        ..Default::default()
    };
    let entered = runtime.enter();
    eframe::run_native(
        &crate::product::display_name(),
        options,
        Box::new(|cc| {
            fonts::install(&cc.egui_ctx);
            theme::configure(&cc.egui_ctx);
            let mut center = DeviceCenter::new(runtime.handle().clone(), cc.egui_ctx.clone());
            match launch {
                Some(cli::Launch::Join { request, options }) => center.open_join_media(request, options),
                Some(cli::Launch::Account { client, device, options }) => {
                    center.connect_media(client, device, options);
                }
                None => {}
            }
            Ok(Box::new(center))
        }),
    )
    .map_err(|error| anyhow::anyhow!("{error}"))?;
    drop(entered);
    runtime.block_on(crate::session::controller::shutdown_all());
    Ok(())
}

enum Event {
    Account(Result<Option<(Arc<AuthenticatedClient>, DeviceList)>>),
    LoginProgress(account_login::LoginProgress),
    UpdateServerConfigured,
    Devices(Result<DeviceList>),
    RefreshDevices,
    SwitchDevice { source: egui::ViewportId, device_id: String },
    SignedOut(Option<String>),
    Center(Result<center::Update>),
    NoticeCount(Arc<AuthenticatedClient>, Result<u32>),
}

struct DeviceCenter {
    ctx: egui::Context,
    runtime: tokio::runtime::Handle,
    sender: mpsc::Sender<Event>,
    events: mpsc::Receiver<Event>,
    sign_in: login::LoginScreen,
    login_open: bool,
    login_status: Option<&'static str>,
    account: Option<(Arc<AuthenticatedClient>, DeviceList)>,
    center: center::State,
    wallpapers: wallpaper::Wallpapers,
    viewers: viewers::Windows,
    joins: joins::JoinUi,
    settings: settings::SettingsUi,
    media: ConnectionMediaOptions,
    updates: updates::UpdateCheck,
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
        let mut settings = settings::SettingsUi::default();
        let media = crate::media::preferences::load().unwrap_or_else(|error| {
            settings.error = Some(format!("{error:#}"));
            ConnectionMediaOptions::default()
        });
        let mut state = Self {
            updates: updates::UpdateCheck::start(&ctx),
            ctx,
            runtime,
            sender,
            events,
            sign_in: login::LoginScreen::new(server, fingerprint),
            login_open: false,
            login_status: None,
            account: None,
            center: center::State::default(),
            wallpapers: wallpaper::Wallpapers::default(),
            viewers: viewers::Windows::default(),
            joins: joins::JoinUi::default(),
            settings,
            media,
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
        self.login_status = Some(if login.is_some() {
            t().app.signing_in
        } else {
            t().app.restoring_sign_in
        });
        let sender = self.sender.clone();
        let ctx = self.ctx.clone();
        self.runtime.spawn(async move {
            let result = async {
                if let Some((config, credentials)) = login {
                    if !ServerConfig::overridden() {
                        config.save()?;
                        let _ = sender.send(Event::UpdateServerConfigured);
                        ctx.request_repaint();
                    }
                    account_login::login_with_progress(credentials, |stage| {
                        let _ = sender.send(Event::LoginProgress(stage));
                        ctx.request_repaint();
                    })
                    .await?;
                }
                let client = match AuthenticatedClient::from_saved_session() {
                    Ok(client) => Arc::new(client),
                    Err(error) if error.is::<NoSavedSession>() => return Ok(None),
                    Err(error) => return Err(error),
                };
                let mut devices = client.list_devices().await?;
                devices.sort();
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
            let result = client.list_devices().await.map(|mut list| {
                list.sort();
                list
            });
            let _ = sender.send(Event::Devices(result));
            ctx.request_repaint();
        });
    }

    fn connect(&mut self, client: Arc<AuthenticatedClient>, device: DeviceInfo) {
        self.connect_media(client, device, self.media);
    }

    fn connect_media(
        &mut self,
        client: Arc<AuthenticatedClient>,
        device: DeviceInfo,
        options: ConnectionMediaOptions,
    ) -> egui::ViewportId {
        let target = device.device_id.clone();
        self.viewers.open(
            &self.ctx,
            &self.runtime,
            Some((&target, Arc::clone(&client))),
            device.alias.clone(),
            move |cancel, progress, recovering, visit, record| {
                let (client, mut device) = (client.clone(), device.clone());
                async move {
                    if recovering {
                        let list = tokio::select! {
                            _ = cancel.cancelled() => anyhow::bail!(t().viewer.cancel_connection),
                            result = client.list_devices() => result?,
                        };
                        device = list
                            .all()
                            .find(|candidate| candidate.device_id == device.device_id)
                            .ok_or_else(|| {
                                anyhow::anyhow!((t().center.device_not_found)(&device.device_id))
                            })?
                            .clone();
                    }
                    let alias = device.alias.clone();
                    let connection = ControllerConnection::connect_account_device(
                        client,
                        device,
                        options,
                        &visit,
                        &record,
                        Some(&progress),
                        &cancel,
                    )
                    .await?;
                    Ok((connection, alias))
                }
            },
        )
    }

    fn show_join(&mut self, ui: &mut egui::Ui) {
        match self.joins.show(ui, &self.runtime, self.account.as_ref().map(|(_, devices)| devices)) {
            Some(joins::JoinAction::Account(device)) => {
                if let Some((client, _)) = &self.account {
                    self.connect(Arc::clone(client), device);
                }
            }
            Some(joins::JoinAction::Join(request)) => self.open_join(request),
            None => {}
        }
    }

    fn open_join(&mut self, request: joins::JoinRequest) {
        self.open_join_media(request, self.media);
    }

    fn open_join_media(&mut self, request: joins::JoinRequest, options: ConnectionMediaOptions) {
        let client = self.account.as_ref().map(|(client, _)| Arc::clone(client));
        self.viewers.open(
            &self.ctx,
            &self.runtime,
            None,
            request.title(),
            move |cancel, progress, _, visit, record| {
                let (request, client) = (request.clone(), client.clone());
                async move {
                    request
                        .connect(
                            client,
                            options,
                            &visit,
                            &record,
                            Some(&progress),
                            &cancel,
                        )
                        .await
                }
            },
        );
    }

    fn signed_out(&mut self, client: Arc<AuthenticatedClient>) {
        self.viewers.close_all();
        self.busy = true;
        let sender = self.sender.clone();
        let ctx = self.ctx.clone();
        self.runtime.spawn(async move {
            let result = client.logout().await;
            let _ = sender.send(Event::SignedOut(result.local_error.or(result.remote_error)));
            ctx.request_repaint();
        });
    }

    fn show_error(&mut self, error: anyhow::Error) {
        self.error = Some(format!("{error:#}"));
    }
}

impl eframe::App for DeviceCenter {
    fn logic(&mut self, _ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.wallpapers.poll(&self.ctx);
        self.updates.poll();
        while let Ok(event) = self.events.try_recv() {
            match event {
                Event::Account(result) => {
                    self.busy = false;
                    self.login_status = None;
                    self.finish_account_check(&result);
                    match result {
                        Ok(Some(account)) => {
                            self.login_open = false;
                            self.start_chat(&account.0);
                            self.account = Some(account);
                        }
                        Ok(None) => {}
                        Err(error) => self.show_error(error),
                    }
                }
                Event::UpdateServerConfigured => self.updates.recheck_source(&self.ctx),
                Event::LoginProgress(stage) => {
                    use account_login::LoginProgress::*;
                    self.login_status = Some(match stage {
                        ValidatingSavedSession => t().app.restoring_sign_in,
                        RegisteringDevice => t().app.registering_device,
                        DeviceRegistered => t().app.device_registered,
                        SubmittingCredentials => t().app.signing_in,
                    });
                }
                Event::Devices(result) => {
                    self.busy = false;
                    self.finish_server_check(result.is_ok());
                    match result {
                        Ok(devices) => {
                            if let Some((_, list)) = &mut self.account {
                                *list = devices;
                            }
                        }
                        Err(error) => self.show_error(error),
                    }
                }
                Event::RefreshDevices => {
                    if !self.busy && let Some((client, _)) = &self.account {
                        self.refresh(Arc::clone(client));
                    }
                }
                Event::SwitchDevice { source, device_id } => {
                    if self.viewers.contains(source)
                        && let Some((client, list)) = &self.account
                        && let Ok(device) = list.with_capability(&device_id, farwing_protocol::caps::VIEW)
                    {
                        let target = self.connect_media(Arc::clone(client), device.clone(), self.media);
                        self.viewers.switch_when_ready(source, target);
                    }
                }
                Event::SignedOut(error) => {
                    self.account = None;
                    self.center = center::State::default();
                    self.wallpapers.clear();
                    self.busy = false;
                    self.error = error;
                }
                Event::Center(result) => self.finish_center(result),
                Event::NoticeCount(client, result) => self.finish_notice_count(&client, result),
            }
        }
        if !self.busy
            && self
                .account
                .as_ref()
                .is_some_and(|(client, _)| !client.is_active())
        {
            self.viewers.close_all();
            self.account = None;
            self.center = center::State::default();
            self.wallpapers.clear();
            self.error
                .get_or_insert_with(|| t().app.account_session_ended.into());
        }
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        if self.login_open {
            self.login_page(ui);
        } else {
            self.devices_page(ui);
        }
        self.show_center_tools(ui.ctx());
        self.viewers.show(
            ui.ctx(),
            self.account.as_ref().map(|(_, devices)| devices),
            &self.sender,
            self.busy,
        );
        crate::ui::controls::show_notices(ui.ctx());
    }
}
