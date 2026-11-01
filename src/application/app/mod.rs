//! Native graphical device center and host-presence lifecycle.
mod media_preferences;

use crate::account::api::{DeviceInfo, DeviceList};
use crate::account::client::AuthenticatedClient;
use crate::account::login::LoginProgress;
use crate::i18n::t;
use crate::media::{
    CodecPreference, ConnectionMediaOptions, FrameRateChoice, LocalDisplayInfo, TransportChoice,
    detect_local_display,
};
use crate::session::presence::PresenceState;
use anyhow::{Context, Result};
use assist::{AssistOperation, AssistUi};
use messages::{DeviceMutation, GuiCommand, GuiEvent, MutationOutcome};
use login_form::{LoginForm, LoginStep};
use runtime::GuiWorker;
use std::sync::Arc;
use std::sync::mpsc::Sender;
use std::time::{Duration, Instant};
use tokio::task::JoinHandle;
use view::{CenterUi, configure_visuals};

mod assist;
mod direct;
pub(crate) mod friends;
mod notices;
mod recents;
mod records;
mod catalog;
mod controlled_caption;
pub(crate) mod device_status;
mod device_sync;
mod diagnostics;
pub(crate) mod notifications;

pub mod instance;
pub(crate) mod maintenance;
mod login_form;
mod power;
mod updates;
mod view;

const WORKER_TICK: Duration = Duration::from_millis(250);

pub struct GuiOptions {
    pub background: bool,
    pub media: ConnectionMediaOptions,
    pub startup_warning: Option<String>,
}

pub fn run(options: GuiOptions) -> Result<()> {
    let _user_backend = crate::platform::windows::host_service::user_backend::Server::start()?;
    let _shortcuts = crate::application::viewer_shortcuts::Watcher::new()?;
    crate::features::host::displays::fallback::start_background();
    let (local_display, display_warning) = match detect_local_display() {
        Ok(display) => (display, None),
        Err(error) => (
            LocalDisplayInfo::FALLBACK,
            Some((t().app.display_detect_failed)(&format!("{error:#}"))),
        ),
    };
    let display_warning = match (display_warning, options.startup_warning) {
        (Some(a), Some(b)) => Some((t().app.join_warnings)(&a, &b)),
        (a, b) => a.or(b),
    };
    options
        .media
        .resolve(local_display)
        .context("invalid initial GUI media options")?;

    let viewport = egui::ViewportBuilder::default()
        .with_visible(!options.background)
        .with_title(window_title())
        .with_icon(crate::ui::branding::icon())
        .with_inner_size([1180.0, 760.0])
        .with_min_inner_size([1024.0, 720.0]);
    crate::ui::run(
        crate::ui::WindowConfig {
            viewport,
            centered: true,
            notification: false,
        },
        Box::new(move |ctx, graphics| {
            configure_visuals(ctx);
            ctx.request_repaint();
            let mut app = DeviceCenterApp::new(ctx, local_display, options.media, display_warning);
            if let Some(graphics) = graphics {
                app.diagnostics.graphics.push(graphics);
            }
            Box::new(app)
        }),
    )
}
/// The device centre's window title in the current language.
fn window_title() -> String {
    (t().app.window_title)(crate::APP_NAME)
}

pub fn notification_activation(uri: &str) -> Result<()> {
    notifications::Action::parse(uri)?;
    instance::deliver_notification(uri)
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum StartupStage {
    #[default]
    Identity,
    Credentials,
    Device,
    Account,
    Devices,
}
impl StartupStage {
    const ALL: [Self; 5] = [
        Self::Identity,
        Self::Credentials,
        Self::Device,
        Self::Account,
        Self::Devices,
    ];
    fn index(self) -> usize {
        match self {
            Self::Identity => 0,
            Self::Credentials => 1,
            Self::Device => 2,
            Self::Account => 3,
            Self::Devices => 4,
        }
    }
    fn title(self) -> &'static str {
        let text = &t().app;
        match self {
            Self::Identity => text.stage_identity,
            Self::Credentials => text.stage_credentials,
            Self::Device => text.stage_device,
            Self::Account => text.stage_account,
            Self::Devices => text.stage_devices,
        }
    }
}

struct DeviceCenterApp {
    updates: updates::UpdateCheck,
    center_ui: CenterUi,
    assist: AssistUi,
    friends: friends::FriendsUi,
    chat: Option<crate::features::chat::ChatService>,
    chat_ui: view::ChatUi,
    records: records::RecordsUi,
    notices: notices::NoticesUi,
    direct: direct::DirectUi,
    worker: GuiWorker,
    /// Whether a background host is managed, and when that was checked.
    full_exit: std::cell::Cell<(Option<std::time::Instant>, bool)>,
    devices: Option<DeviceList>,
    catalog: Option<catalog::Catalog>,
    catalog_error: Option<String>,
    extra_details: std::collections::HashMap<
        String,
        std::result::Result<crate::account::api::DeviceDetail, String>,
    >,
    detail_pending: Option<String>,
    account_name: String,
    /// The signed-in server's name (empty until it answers).
    server_name: String,
    /// The server was out of reach at the account's last attempt.
    server_unreachable: bool,
    diagnostics: diagnostics::LocalDiagnostics,
    mutation_pending: bool,
    queued_mutation: Option<DeviceMutation>,
    power_progress: std::collections::BTreeMap<String, power::PowerProgress>,
    pending_power: Option<power::PendingPower>,
    selected_device_id: Option<String>,
    local_display: LocalDisplayInfo,
    media: ConnectionMediaOptions,
    media_inventory: media_preferences::Inventory,
    presence: PresenceState,
    host: Option<crate::features::host::Handle>,
    notifications: notifications::Center,
    status: StatusMessage,
    refreshed_at: Option<Instant>,
    refresh_pending: bool,
    viewers: viewing::Registry,
    exit_requested: bool,
    exit_pending: bool,
    exit_ready: bool,
    logout_confirmation: bool,
    logout_pending: bool,
    logout_sent: bool,
    login_generation: u64,
    login_form: LoginForm,
    login_restoring: bool,
    /// The device list shown is the cached one from before this start.
    cached_devices: bool,
    startup_stage: StartupStage,
    startup_stage_since: Instant,
    login_running: bool,
    login_status: String,
    login_error: Option<String>,
}

impl DeviceCenterApp {
    fn new(
        ctx: &egui::Context,
        local_display: LocalDisplayInfo,
        media: ConnectionMediaOptions,
        display_warning: Option<String>,
    ) -> Self {
        Self {
            updates: updates::UpdateCheck::start(ctx),
            center_ui: CenterUi::new(),
            viewers: Default::default(),
            assist: AssistUi::default(),
            friends: Default::default(),
            chat: None,
            chat_ui: Default::default(),
            records: Default::default(),
            notices: Default::default(),
            direct: Default::default(),
            worker: GuiWorker::spawn(),
            full_exit: std::cell::Cell::new((None, false)),
            devices: None,
            catalog: None,
            catalog_error: None,
            extra_details: Default::default(),
            detail_pending: None,
            account_name: String::new(),
            server_name: String::new(),
            server_unreachable: false,
            diagnostics: diagnostics::LocalDiagnostics::start(),
            mutation_pending: false,
            queued_mutation: None,
            power_progress: Default::default(),
            pending_power: None,
            selected_device_id: None,
            local_display,
            media,
            media_inventory: media_preferences::Inventory::new(),
            presence: PresenceState::Connecting,
            host: None,
            notifications: notifications::Center::new(ctx),
            status: display_warning.map_or_else(
                || StatusMessage::info(t().app.loading_devices),
                StatusMessage::warning,
            ),
            refreshed_at: None,
            refresh_pending: true,
            exit_requested: false,
            exit_pending: false,
            exit_ready: false,
            logout_confirmation: false,
            logout_pending: false,
            logout_sent: false,
            login_generation: 0,
            login_form: LoginForm::default(),
            login_restoring: true,
            cached_devices: false,
            startup_stage: StartupStage::Identity,
            startup_stage_since: Instant::now(),
            login_running: false,
            login_status: String::new(),
            login_error: None,
        }
    }

    fn drain_events(&mut self, ctx: &egui::Context) {
        self.updates.poll();
        self.diagnostics.poll();
        while let Ok(event) = self.worker.events.try_recv() {
            match event {
                GuiEvent::ExitReady(result) => {
                    self.exit_pending = false;
                    match result {
                        Ok(()) => {
                            self.exit_ready = true;
                            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                        }
                        Err(error) => {
                            self.exit_requested = false;
                            let text = &t().app;
                            self.status =
                                StatusMessage::error((text.exit_failed_with)(&error.to_string()));
                            ctx.send_viewport_cmd(egui::ViewportCommand::Visible(true));
                            // The status line is not on every page (startup, login).
                            crate::ui::controls::notice(
                                ctx,
                                "exit-failed",
                                text.cant_exit,
                                crate::ui::controls::DialogIcon::Error,
                                (text.host_not_stopped)(&error.to_string()),
                            );
                        }
                    }
                }
                GuiEvent::Viewer(generation, key, alias, device_id, result) => {
                    let pending = self.viewers.completed_open(&key, generation);
                    match result {
                        Ok(handle) => {
                            let closing = generation != self.login_generation
                                || self.logout_pending
                                || self.exit_requested
                                || !pending;
                            if closing {
                                handle.request_close();
                            }
                            self.viewers.active.push(viewing::Session {
                                generation,
                                key,
                                device_id,
                                alias,
                                handle,
                                closing,
                            });
                        }
                        Err(error)
                            if pending
                                && generation == self.login_generation
                                && !self.logout_pending =>
                        {
                            self.status = StatusMessage::error(error)
                        }
                        Err(_) => (),
                    }
                }
                GuiEvent::Startup(generation, stage) => {
                    if generation == self.login_generation
                        && self.login_restoring
                        && !self.logout_pending
                        && self.startup_stage != stage
                    {
                        tracing::info!(stage = ?stage, "device center startup");
                        self.startup_stage = stage;
                        self.startup_stage_since = Instant::now();
                    }
                }
                GuiEvent::Presence(presence) => self.presence = presence,
                GuiEvent::Host(generation, host) => {
                    if generation == self.login_generation {
                        self.host = Some(host);
                    }
                }
                GuiEvent::PowerDispatched(generation, id, action) => {
                    if generation == self.login_generation
                        && self.mutation_pending
                        && !self.logout_pending
                    {
                        self.pending_power = Some(power::PendingPower {
                            id,
                            action,
                            saw_offline: false,
                        });
                    }
                }
                GuiEvent::CachedDevices(generation, mut devices, name) => {
                    if generation != self.login_generation
                        || !self.login_restoring
                        || self.devices.is_some()
                        || self.logout_pending
                        || self.login_running
                    {
                        continue;
                    }
                    // Whether they are online is unknown until the server answers.
                    for device in std::iter::once(&mut devices.current_device)
                        .chain(&mut devices.devices)
                        .chain(&mut devices.shared)
                    {
                        device.status = "offline".into();
                    }
                    tracing::info!("device center startup: cached devices shown");
                    if self.account_name.trim().is_empty() {
                        self.account_name = name;
                    }
                    self.cached_devices = true;
                    self.devices = Some(devices);
                    self.normalize_selection();
                    self.login_restoring = false;
                }
                GuiEvent::Devices(generation, devices) => {
                    if generation != self.login_generation
                        || self.logout_pending
                        || self.login_running
                    {
                        continue;
                    }
                    self.observe_power(&devices);
                    self.devices = Some(devices);
                    self.normalize_selection();
                    self.refreshed_at = Some(Instant::now());
                    self.refresh_pending = false;
                    self.cached_devices = false;
                    if !self.status.kind.is_alert() {
                        self.status = StatusMessage::success(t().app.devices_refreshed);
                    }
                    if self.login_restoring {
                        tracing::info!("device center startup: devices shown");
                    }
                    self.login_restoring = false;
                }
                GuiEvent::Catalog(generation, result, account_name) => {
                    if generation != self.login_generation
                        || self.logout_pending
                        || self.login_running
                    {
                        continue;
                    }
                    self.account_name = account_name;
                    match result {
                        Ok(catalog) => {
                            // The worker owns hardware freshness and has already merged deltas.
                            self.extra_details.retain(|id, _| {
                                !catalog.groups.entries().any(|(_, d)| &d.device_id == id)
                            });
                            if self.detail_pending.as_ref().is_some_and(|id| {
                                !catalog.groups.entries().any(|(_, d)| &d.device_id == id)
                            }) {
                                self.detail_pending = None;
                            }
                            self.catalog = Some(catalog);
                            self.normalize_selection();
                            self.catalog_error = None;
                            self.login_restoring = false;
                        }
                        Err(error) => self.catalog_error = Some(error),
                    }
                }
                GuiEvent::MutationFinished(generation, result) => {
                    if generation != self.login_generation
                        || self.logout_pending
                        || self.login_running
                    {
                        continue;
                    }
                    self.mutation_pending = false;
                    self.status = match result {
                        Ok(MutationOutcome::Changed { message, .. }) => {
                            StatusMessage::success(message)
                        }
                        Ok(MutationOutcome::Power(accepted)) => {
                            self.accept_power(*accepted);
                            StatusMessage::info(t().app.power_accepted)
                        }
                        Err(message) => StatusMessage::warning(message),
                    };
                    self.center_ui.finish_device_operation();
                    self.pending_power = None;
                }
                GuiEvent::Detail(generation, id, result) => {
                    if generation == self.login_generation
                        && !self.logout_pending
                        && !self.login_running
                    {
                        if self.detail_pending.as_deref() == Some(id.as_str()) {
                            self.detail_pending = None;
                        }
                        // Unanswered while the server is away: asked again
                        // once it is back.
                        if result.is_err() && self.server_unreachable {
                            continue;
                        }
                        self.extra_details.insert(id, result);
                    }
                }
                GuiEvent::Working(message) => {
                    if !self.logout_pending && !self.status.kind.is_alert() {
                        self.status = StatusMessage::info(message);
                    }
                }
                GuiEvent::Warning(message) => {
                    self.login_restore_error(&message);
                    self.refresh_pending = false;
                    self.status = StatusMessage::warning(message);
                }
                GuiEvent::Error(message) => {
                    self.login_restore_error(&message);
                    self.refresh_pending = false;
                    self.status = StatusMessage::error(message);
                }
                GuiEvent::HostAssistFailed { generation, error } => {
                    if generation == self.login_generation {
                        self.notifications.report(error);
                    }
                }
                GuiEvent::SessionUnavailable(message) => {
                    self.clear_catalog();
                    self.devices = None;
                    self.cached_devices = false;
                    self.selected_device_id = None;
                    self.refresh_pending = false;
                    self.status = StatusMessage::warning(message);
                    self.login_restoring = false;
                    self.login_error = Some(self.status.text.clone());
                    self.login_status = t().app.cant_restore_sign_in.into();
                }
                GuiEvent::ServerSaved(result) => self.finish_server(result),
                GuiEvent::SignedOut => {
                    self.clear_catalog();
                    self.devices = None;
                    self.cached_devices = false;
                    self.selected_device_id = None;
                    self.refresh_pending = false;
                    self.login_restoring = false;
                    self.login_error = None;
                    self.login_status.clear();
                    self.status = StatusMessage::info("");
                }
                GuiEvent::LoginProgress(_method, generation, attempt, progress) => {
                    if generation != self.login_generation
                        || attempt != self.login_form.generation
                        || !self.login_form.submitting
                    {
                        continue;
                    }
                    self.login_form.error = None;
                    let text = &t().app;
                    self.login_status = match progress {
                        LoginProgress::ValidatingSavedSession => text.restoring_sign_in,
                        LoginProgress::RegisteringDevice => text.registering_device,
                        LoginProgress::DeviceRegistered => text.device_registered,
                        LoginProgress::SubmittingCredentials => text.verifying_account,
                    }
                    .to_owned();
                    self.login_form.status = self.login_status.clone();
                }
                GuiEvent::LoginFinished(_method, generation, attempt, result) => {
                    if generation != self.login_generation {
                        continue;
                    }
                    match result {
                        Ok(()) => {
                            // The worker accepted this session and invalidated
                            // the login flow before exposing the new epoch.
                            self.login_generation = generation.wrapping_add(1);
                            self.records = Default::default();
                            self.notices = Default::default();
                            self.login_form.clear_private();
                            self.sync_login_running();
                            self.login_restoring = true;
                            self.startup_stage = StartupStage::Device;
                            self.startup_stage_since = Instant::now();
                            self.login_status = t().app.loading_devices.to_owned();
                            self.status = StatusMessage::success(t().app.signed_in);
                            self.refresh_pending = true;
                        }
                        Err(error) => {
                            if attempt != self.login_form.generation || !self.login_form.submitting {
                                continue;
                            }
                            let message = format!("{error:#}");
                            self.login_form.submitting = false;
                            self.sync_login_running();
                            self.login_form.status = message.clone();
                            self.login_form.error = Some(message.clone());
                            self.login_restoring = false;
                            self.login_error = Some(message.clone());
                            self.login_status = t().app.sign_in_incomplete.to_owned();
                            self.status = StatusMessage::error((t().app.sign_in_failed)(&message));
                        }
                    }
                }
                GuiEvent::AccountEnded(message) => {
                    self.cancel_login();
                    self.login_form.clear_private();
                    self.clear_catalog();
                    for session in &self.viewers.active {
                        session.handle.request_close();
                    }
                    self.devices = None;
                    self.cached_devices = false;
                    self.selected_device_id = None;
                    self.presence = PresenceState::Offline;
                    self.status = StatusMessage::warning(message);
                    self.refresh_pending = false;
                    self.login_status = t().app.sign_in_expired.into();
                }
                GuiEvent::Chat(account) => {
                    if let Some(old) = self.chat.take() {
                        old.stop();
                    }
                    self.chat_ui = Default::default();
                    self.chat = account.map(|(client, runtime)| {
                        crate::features::chat::ChatService::start(client, runtime, ctx.clone())
                    });
                }
                GuiEvent::LoggedOut(outcome) => {
                    if let Ok(dir) = crate::product::local_data_dir() {
                        crate::features::chat::cache::Cache::clear_all(&dir);
                    }
                    self.login_form.clear_private();
                    self.clear_catalog();
                    self.logout_pending = false;
                    self.logout_sent = false;
                    self.devices = None;
                    self.cached_devices = false;
                    self.selected_device_id = None;
                    self.presence = PresenceState::Offline;
                    let text = &t().app;
                    self.status = match (outcome.remote_error, outcome.local_error) {
                        (_, Some(error)) => {
                            StatusMessage::error((text.delete_credentials_failed)(&error.to_string()))
                        }
                        (Some(error), None) => {
                            StatusMessage::warning((text.sign_out_unconfirmed)(&error.to_string()))
                        }
                        (None, None) => StatusMessage::success(text.signed_out),
                    };
                    self.login_restoring = false;
                    self.login_running = false;
                    self.login_error = self
                        .status
                        .kind
                        .is_alert()
                        .then(|| self.status.text.clone());
                    self.login_status = if self.login_error.is_some() {
                        text.sign_out_incomplete.into()
                    } else {
                        String::new()
                    };
                }
                GuiEvent::ServerName(generation, name) => {
                    if generation == self.login_generation {
                        self.server_name = name;
                    }
                }
                GuiEvent::Reachability(generation, away) => {
                    if generation != self.login_generation {
                        continue;
                    }
                    let back = self.server_unreachable && !away;
                    self.server_unreachable = away;
                    // What failed while the server was away comes back by itself.
                    if back {
                        if self.friends.error.is_some() {
                            self.request_friends(friends::FriendOperation::Refresh);
                        }
                        if self.assist.last_list_error.is_some() {
                            self.request_assist_refresh();
                        }
                        self.updates.retry_failed(ctx);
                        if let Some(id) = self.center_ui.detail_id() {
                            self.request_details(id.to_owned());
                        }
                    }
                }
                GuiEvent::AssistLists(generation, result) => {
                    if generation != self.login_generation
                        || self.logout_pending
                        || self.login_running
                    {
                        continue;
                    }
                    self.assist.loading = false;
                    match result {
                        Ok(lists) => {
                            if let Some(error) = &lists.code_error {
                                self.assist.list_failure(error.clone());
                            } else {
                                self.assist.last_list_error = None;
                            }
                            self.assist.lists = Some(lists);
                        }
                        // The status bar already says the server is away.
                        Err(error) if self.server_unreachable => {
                            self.assist.last_list_error = Some(error);
                        }
                        Err(error) => {
                            self.assist.list_failure(error);
                        }
                    }
                }
                GuiEvent::Friends(generation, result) => {
                    if generation != self.login_generation || self.logout_pending {
                        continue;
                    }
                    self.finish_friends(result);
                }
                GuiEvent::Records(generation, result) => {
                    if generation != self.login_generation || self.logout_pending {
                        continue;
                    }
                    self.finish_records(result);
                }
                GuiEvent::Notices(generation, result) => {
                    if generation != self.login_generation || self.logout_pending {
                        continue;
                    }
                    self.finish_notices(result);
                }
                GuiEvent::AssistOperation(generation, sequence, result) => {
                    if generation != self.login_generation
                        || sequence != self.assist.sequence
                        || self.logout_pending
                    {
                        continue;
                    }
                    self.finish_assist_operation(result);
                }
            }
            self.sync_login_running();
        }

        for (generation, alias, result) in self.viewers.finished() {
            tracing::info!(device = %alias, outcome = ?result, "viewing window ended");
            if generation != self.login_generation {
                continue;
            }
            let text = &t().app;
            self.status = match result {
                Ok(crate::session::controller::windows::ViewerEnd::Closed) => {
                    StatusMessage::success((text.viewer_closed)(&alias))
                }
                Ok(crate::session::controller::windows::ViewerEnd::RoomReleased(reason)) => {
                    StatusMessage::notice((text.connection_ended)(&alias, &reason.to_string()))
                }
                Err(error) => {
                    StatusMessage::error((text.viewer_crashed)(&alias, &error.to_string()))
                }
            };
            if self.devices.is_some() && !self.logout_pending {
                self.refresh_pending = self.worker.commands.send(GuiCommand::Refresh).is_ok();
                self.request_assist_refresh();
            }
        }
        if self.logout_pending && !self.logout_sent && !self.has_viewers() {
            self.logout_sent = self.worker.commands.send(GuiCommand::Logout).is_ok();
            if !self.logout_sent {
                self.logout_pending = false;
                self.status = StatusMessage::error(t().app.sign_out_failed);
            }
        }
        if let Some(id) = self
            .queued_mutation
            .as_ref()
            .and_then(DeviceMutation::close_target)
            .map(str::to_owned)
        {
            self.stop_device_viewers(&id);
        }
        if self
            .queued_mutation
            .as_ref()
            .is_some_and(|change| !self.mutation_waits(change))
        {
            let change = self.queued_mutation.take().unwrap();
            self.send_mutation(change);
        }
    }

    fn cancel_login(&mut self) {
        self.login_form.cancel();
        self.login_generation = self.login_generation.wrapping_add(1);
        let _ = self
            .worker
            .commands
            .send(GuiCommand::CancelLogin(self.login_generation));
        self.login_restoring = false;
        self.login_running = false;
        self.login_error = None;
        self.login_status.clear();
    }

    fn sync_login_running(&mut self) {
        self.login_running = self.login_form.submitting;
    }


    fn login_restore_error(&mut self, message: &str) {
        if self.devices.is_none() && self.catalog.is_none() && !self.login_running {
            self.login_restoring = false;
            self.login_status = t().app.connection_failed.into();
            self.login_error = Some(message.to_owned());
        }
    }

    fn logout(&mut self) {
        if self.can_logout() {
            self.logout_confirmation = true;
        }
    }

    /// Signing out waits for device operations in flight.
    fn can_logout(&mut self) -> bool {
        if self.center_ui.components.busy()
            || self.mutation_pending
            || (self.assist.busy && !self.assist.querying)
        {
            self.status = StatusMessage::warning(t().app.device_operation_pending);
            return false;
        }
        true
    }

    fn normalize_selection(&mut self) {
        if self.selected_device_id.as_ref().is_some_and(|id| {
            self.catalog
                .as_ref()
                .is_some_and(|c| c.groups.entries().any(|(_, d)| &d.device_id == id))
        }) {
            return;
        }
        let Some(devices) = self.devices.as_ref() else {
            self.selected_device_id = None;
            return;
        };
        let selected_exists = self.selected_device_id.as_ref().is_some_and(|selected| {
            all_devices(devices).any(|(_, device)| {
                device.device_id == *selected && self.show_in_watching_list(device)
            })
        });
        if !selected_exists {
            self.selected_device_id = all_devices(devices)
                .filter(|(_, device)| self.show_in_watching_list(device))
                .next()
                .map(|(_, device)| device.device_id.clone());
        }
    }

    fn selected_device(&self) -> Option<&DeviceInfo> {
        let selected = self.selected_device_id.as_deref()?;
        self.devices
            .as_ref()
            .and_then(|devices| {
                all_devices(devices).find(|(_, device)| device.device_id == selected)
            })
            .map(|(_, device)| device)
            .or_else(|| {
                self.catalog
                    .as_ref()?
                    .groups
                    .entries()
                    .find(|(_, d)| d.device_id == selected)
                    .map(|(_, d)| d)
            })
    }

    /// Seconds until the next attempt to reach the server; `None` while one
    /// is under way.
    fn next_reconnect(&self, ctx: &egui::Context) -> Option<u64> {
        ctx.request_repaint_after(Duration::from_secs(1));
        crate::session::reconnect::next()
            .filter(|at| *at > Instant::now() + Duration::from_millis(500))
            .map(|at| at.saturating_duration_since(Instant::now()).as_secs_f32().ceil() as u64)
    }

    fn reconnect(&mut self) {
        let _ = self.worker.commands.send(GuiCommand::Reconnect);
    }

    fn clear_catalog(&mut self) {
        self.server_unreachable = false;
        if !self.host.as_ref().is_some_and(|h| h.is_guest()) {
            self.host = None;
        }
        self.center_ui.clear_host_assist();
        self.center_ui.clear_wallpapers();
        self.assist = AssistUi::default();
        self.extra_details.clear();
        self.detail_pending = None;
        self.queued_mutation = None;
        self.power_progress.clear();
        self.pending_power = None;
        self.mutation_pending = false;
        self.catalog = None;
        self.catalog_error = None;
        self.account_name.clear();
        self.server_name.clear();
        self.center_ui.close_details();
    }

    fn is_viewing_target(&self, id: &str) -> bool {
        self.devices.as_ref().is_some_and(|list| {
            all_devices(list).any(|(_, d)| d.device_id == id && self.show_in_watching_list(d))
        })
    }

    fn show_in_watching_list(&self, device: &DeviceInfo) -> bool {
        device.platform.is_desktop()
            && self
                .devices
                .as_ref()
                .is_some_and(|list| list.current_device.device_id != device.device_id)
            && !self.catalog.as_ref().is_some_and(|c| {
                c.groups
                    .mobile_devices
                    .iter()
                    .any(|d| d.device_id == device.device_id)
            })
    }

    fn open_details(&mut self, id: String) {
        self.selected_device_id = Some(id.clone());
        self.center_ui.open_details(id.clone());
        self.request_details(id);
    }

    /// Ask the server for a device's details unless they are known.
    fn request_details(&mut self, id: String) {
        if self
            .catalog
            .as_ref()
            .is_none_or(|c| !c.details.contains_key(&id))
            && !self.extra_details.contains_key(&id)
            && self.detail_pending.as_deref() != Some(id.as_str())
        {
            self.detail_pending = Some(id.clone());
            if self.worker.commands.send(GuiCommand::Detail(id)).is_err() {
                self.detail_pending = None;
            }
        }
    }

    fn queue_mutation(&mut self, change: DeviceMutation) {
        if self.mutation_pending || self.logout_pending {
            return;
        }
        if let DeviceMutation::Power { device, action } = &change
            && let Err(error) = self.power_available(device, *action)
        {
            self.status = StatusMessage::warning(error.to_string());
            return;
        }
        self.mutation_pending = true;
        if self.mutation_waits(&change) {
            if let Some(id) = change.close_target() {
                self.stop_device_viewers(id);
            }
            self.status = StatusMessage::info(t().app.waiting_for_connections);
            self.queued_mutation = Some(change);
        } else {
            self.send_mutation(change);
        }
    }

    fn send_mutation(&mut self, change: DeviceMutation) {
        self.pending_power = None;
        if self
            .worker
            .commands
            .send(GuiCommand::Mutate {
                generation: self.login_generation,
                change,
            })
            .is_err()
        {
            self.mutation_pending = false;
            self.status = StatusMessage::error(t().app.device_operation_send_failed);
            self.pending_power = None;
        } else {
            self.status = StatusMessage::info(t().app.processing_device_operation);
        }
    }

    fn request_refresh(&mut self) {
        if self.worker.commands.send(GuiCommand::Refresh).is_ok() {
            self.refresh_pending = true;
            self.status = StatusMessage::info(t().app.refreshing_devices);
        } else {
            self.status = StatusMessage::error(t().app.refresh_failed);
        }
    }

    fn start_viewer(&mut self) {
        self.start_viewer_mode(false);
    }
    /// A window for the selected device's cameras only.
    fn start_camera_viewer(&mut self) {
        if self.logout_pending || self.mutation_pending {
            return;
        }
        let Some(device) = self.selected_device().cloned() else {
            self.status = StatusMessage::warning(t().app.no_device_selected);
            return;
        };
        if let Err(message) = self.connection_issue(&device) {
            self.status = StatusMessage::warning(message);
            return;
        }
        self.spawn_viewer_with(
            display_alias(&device).to_owned(),
            Some(device.device_id.clone()),
            None,
            ConnectionMediaOptions {
                camera_only: true,
                ..self.media
            },
        );
    }
    fn start_viewer_mode(&mut self, audio_only: bool) {
        if self.logout_pending || self.mutation_pending {
            return;
        }
        let Some(device) = self.selected_device().cloned() else {
            self.status = StatusMessage::warning(t().app.no_device_selected);
            return;
        };
        if !self.is_viewing_target(&device.device_id) {
            self.status = StatusMessage::warning(t().app.device_not_viewable);
            return;
        }
        if let Err(message) = self.connection_issue(&device) {
            self.status = StatusMessage::warning(message);
            return;
        }
        if let Some(session) = self.viewer_for_device(&device.device_id) {
            if !session.closing {
                session.handle.request_audio_only(audio_only);
                session.handle.focus();
            }
            return;
        }

        self.spawn_viewer(
            display_alias(&device).to_owned(),
            Some(device.device_id.clone()),
            None,
            audio_only,
        );
    }

    fn viewer_action_issue(&self, device: &DeviceInfo) -> Option<String> {
        let text = &t().app;
        if self.logout_pending {
            Some(text.signing_out.into())
        } else if self.mutation_pending {
            Some(text.processing_device_operation.into())
        } else if self
            .viewers
            .opening
            .contains_key(&viewing::Key::Device(device.device_id.clone()))
        {
            Some(text.opening_viewer.into())
        } else if self
            .viewer_for_device(&device.device_id)
            .is_some_and(|s| s.closing)
        {
            Some(text.closing_viewer.into())
        } else {
            self.connection_issue(device).err()
        }
    }

    fn connection_issue(&self, device: &DeviceInfo) -> std::result::Result<(), String> {
        if self.devices.as_ref().is_some_and(|list| {
            crate::session::controller::has_gui_connection(
                &list.current_device.device_id,
                &device.device_id,
            )
        }) && device.is_connected()
            && device.controllable
            && device.controlled_support
        {
            return Ok(());
        }
        connectability_error(device)
    }

    fn spawn_viewer(
        &mut self,
        alias: String,
        device_id: Option<String>,
        assist_request: Option<crate::account::assist::AssistRequest>,
        audio_only: bool,
    ) {
        self.spawn_viewer_with(
            alias,
            device_id,
            assist_request,
            ConnectionMediaOptions {
                audio_only,
                ..self.media
            },
        );
    }
    fn spawn_viewer_with(
        &mut self,
        alias: String,
        device_id: Option<String>,
        assist_request: Option<crate::account::assist::AssistRequest>,
        options: ConnectionMediaOptions,
    ) {
        let audio_only = options.audio_only;
        let camera = options.camera_only;
        let Some(key) = viewing::Key::new(device_id.as_deref(), assist_request.as_ref())
            .map(|key| if camera { key.camera() } else { key })
        else {
            return;
        };
        if let Some(session) = self.viewers.active.iter().find(|s| {
            s.key == key
                || (!camera
                    && !s.key.is_camera()
                    && device_id
                        .as_ref()
                        .is_some_and(|id| s.device_id.as_ref() == Some(id)))
        }) {
            if !session.closing {
                session.handle.request_audio_only(audio_only);
                session.handle.focus();
            }
            return;
        }
        if !self.viewers.begin(key.clone(), self.login_generation) {
            return;
        }
        self.diagnostics.cancel_probe();
        let background = device_id.as_ref().and_then(|id| {
            self.devices.as_ref().and_then(|list| {
                list.devices
                    .iter()
                    .find(|d| &d.device_id == id)
                    .map(|d| {
                        crate::application::wallpaper::Source::new(&d.device_id, &d.wallpaper_url)
                    })
            })
        });
        if self
            .worker
            .commands
            .send(GuiCommand::View {
                generation: self.login_generation,
                key: key.clone(),
                alias,
                device_id,
                assist: assist_request,
                direct: None,
                options,
                background,
            })
            .is_err()
        {
            self.viewers.opening.remove(&key);
            self.status = StatusMessage::error(t().app.cant_open_viewer);
        }
    }
    /// Open a viewer for a host reached directly (§12), logged in or not.
    fn spawn_direct_viewer(&mut self, request: crate::session::controller::DirectRequest) {
        let key = viewing::Key::Direct(request.label());
        if let Some(session) = self.viewers.active.iter().find(|s| s.key == key) {
            if !session.closing {
                session.handle.focus();
            }
            return;
        }
        if !self.viewers.begin(key.clone(), self.login_generation) {
            return;
        }
        if self
            .worker
            .commands
            .send(GuiCommand::View {
                generation: self.login_generation,
                key: key.clone(),
                alias: request.title(),
                device_id: None,
                assist: None,
                direct: Some(request),
                options: self.media,
                background: None,
            })
            .is_err()
        {
            self.viewers.opening.remove(&key);
            self.status = StatusMessage::error(t().app.cant_open_viewer);
        }
    }
    fn prepare_exit(&mut self) {
        if !self.exit_requested
            || self.exit_pending
            || self.exit_ready
            || self.center_ui.components.busy()
        {
            return;
        }
        self.exit_pending = self.worker.commands.send(GuiCommand::PrepareExit).is_ok();
        if !self.exit_pending {
            self.exit_requested = false;
            self.status = StatusMessage::error(t().app.exit_failed);
        }
    }
    fn has_viewers(&self) -> bool {
        self.viewers.has_viewers()
    }
    fn viewer_for_device(&self, id: &str) -> Option<&viewing::Session> {
        self.viewers.for_device(id)
    }
    fn mutation_waits(&self, change: &DeviceMutation) -> bool {
        change
            .close_target()
            .is_some_and(|id| self.viewers.waits_for_device(id))
    }
    fn stop_device_viewers(&mut self, id: &str) {
        self.viewers.stop_device(id);
    }
    fn stop_viewer(&mut self) {
        self.viewers.stop_all();
    }
}

impl crate::ui::App for DeviceCenterApp {
    fn title_bar_alert(&self) -> Option<crate::ui::chrome::TitleBarAlert> {
        self.controlled_caption()
    }
    fn exit_ready(&self) -> bool {
        self.exit_ready
    }
    fn on_close_requested(&mut self) -> bool {
        self.exit_requested = true;
        self.prepare_exit();
        self.exit_ready
    }

    fn full_exit(&self) -> bool {
        let (at, managed) = self.full_exit.get();
        if at.is_some_and(|at| at.elapsed() < std::time::Duration::from_secs(5)) {
            return managed;
        }
        let managed = crate::platform::windows::host_service::resident::managed();
        self.full_exit.set((Some(std::time::Instant::now()), managed));
        managed
    }

    fn tray_state(&self) -> crate::ui::tray::TrayState {
        use crate::ui::tray::TrayState;
        match &self.host {
            Some(host) if host.status().connected => TrayState::Controlled,
            Some(host) if !host.allowed() => TrayState::Disabled,
            _ => TrayState::Idle,
        }
    }

    fn on_focus_changed(&mut self, focused: bool) {
        if !focused {
            self.center_ui.shortcuts.cancel_recording();
        }
        self.worker.focus.send_if_modified(|current| {
            if *current == focused {
                return false;
            }
            *current = focused;
            tracing::debug!(focused, "device center refresh focus changed");
            true
        });
    }

    fn ui(&mut self, ui: &mut egui::Ui) {
        let ctx = ui.ctx().clone();
        self.drain_events(&ctx);
        self.tick_notifications();
        self.tick_chat();
        let signed_in = !self.needs_login();
        self.tick_notices(signed_in);
        self.tick_power();
        self.draw_center(ui);
        self.draw_dialogs(&ctx);
        self.chat_overlays(&ctx);
        self.component_dialogs(&ctx);
        self.prepare_exit();
        if !self.exit_requested && !self.login_restoring && !self.login_running {
            self.update_dialog(&ctx);
        }
        ui.ctx().request_repaint_after(WORKER_TICK);
    }

    fn on_exit(&mut self) {
        for session in &self.viewers.active {
            session.handle.request_close();
        }
        if self.logout_pending && !self.logout_sent {
            let _ = self.worker.commands.send(GuiCommand::Logout);
        }
        let _ = self.worker.commands.send(GuiCommand::Shutdown);
    }
}

struct StatusMessage {
    text: String,
    kind: StatusKind,
}

#[derive(Clone, Copy)]
enum StatusKind {
    Info,
    Notice,
    Success,
    Warning,
    Error,
}

impl StatusKind {
    fn is_alert(self) -> bool {
        matches!(self, Self::Notice | Self::Warning | Self::Error)
    }
}

impl StatusMessage {
    fn notice(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            kind: StatusKind::Notice,
        }
    }
    fn info(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            kind: StatusKind::Info,
        }
    }

    fn success(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            kind: StatusKind::Success,
        }
    }

    fn warning(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            kind: StatusKind::Warning,
        }
    }

    fn error(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            kind: StatusKind::Error,
        }
    }
}

fn all_devices(devices: &DeviceList) -> impl Iterator<Item = (&'static str, &DeviceInfo)> {
    devices
        .devices
        .iter()
        .map(|device| (t().app.my_devices, device))
        .chain(
            devices
                .shared
                .iter()
                .map(|device| (t().app.friend_devices, device)),
        )
}

fn connectability_error(device: &DeviceInfo) -> std::result::Result<(), String> {
    let text = &t().app;
    if !device.is_connected() {
        return Err((text.device_offline)(display_alias(device)));
    }
    if !device.controlled_support || !device.controllable {
        return Err((text.remote_control_off)(display_alias(device)));
    }
    if device.is_shared() && !device.granted(farwing_protocol::caps::VIEW) {
        return Err((text.view_not_granted)(display_alias(device)));
    }
    Ok(())
}

fn display_alias(device: &DeviceInfo) -> &str {
    if device.alias.is_empty() {
        t().app.unnamed_device
    } else {
        &device.alias
    }
}


mod messages;
mod runtime;
mod viewing;
