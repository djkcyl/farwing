//! Device-center asynchronous worker and cancellation ownership.
mod guest;
use super::assist::AssistResult;
use super::messages::{DeviceMutation, GuiCommand, GuiEvent, MutationOutcome};
use super::login_form::LoginMethod;
use super::{StartupStage, WORKER_TICK, assist, device_sync, power};
use crate::account::client::AuthenticatedClient;
use crate::account::login::{self, LoginProgress};
use crate::account::server::ServerConfig;
use crate::i18n::t;
use crate::session::presence::{ActivePresence, PresenceEvent, PresenceState};
use anyhow::{Result, anyhow};
use std::sync::Arc;
use std::sync::mpsc::{Receiver, Sender};
use std::time::Instant;

/// How long a starting device center waits for the server before showing
/// the cached device list.
const CACHED_LIST_AFTER: std::time::Duration = std::time::Duration::from_millis(1500);
use tokio::sync::{mpsc, watch};
use tokio::task::JoinHandle;

pub(super) struct GuiWorker {
    pub(super) commands: mpsc::UnboundedSender<GuiCommand>,
    pub(super) focus: watch::Sender<bool>,
    pub(super) events: Receiver<GuiEvent>,
    pub(super) thread: Option<std::thread::JoinHandle<()>>,
}

impl GuiWorker {
    pub(super) fn spawn() -> Self {
        let (commands, command_receiver) = mpsc::unbounded_channel();
        let (focus, foreground) = watch::channel(false);
        let (event_sender, events) = std::sync::mpsc::channel();
        let thread = std::thread::spawn(move || {
            let runtime = match tokio::runtime::Runtime::new() {
                Ok(runtime) => runtime,
                Err(error) => {
                    let _ = event_sender
                        .send(GuiEvent::Error((t().app.start_failed)(&error.to_string())));
                    return;
                }
            };
            runtime.block_on(gui_worker_loop(command_receiver, event_sender, foreground));
        });
        Self {
            commands,
            focus,
            events,
            thread: Some(thread),
        }
    }
}

impl Drop for GuiWorker {
    fn drop(&mut self) {
        let _ = self.commands.send(GuiCommand::Shutdown);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

pub(super) struct ActiveLogin {
    pub(super) generation: u64,
    pub(super) attempt: u64,
    pub(super) task: JoinHandle<Result<login::PreparedLogin>>,
    pub(super) progress: Receiver<LoginProgress>,
}

pub(super) async fn cancel_login_task(task: &mut Option<ActiveLogin>) {
    if let Some(login) = task.take() {
        login.task.abort();
        let _ = login.task.await;
        // PreparedLogin is not committed by the network worker. Both late
        // progress and a completed-but-cancelled result die with this owner.
    }
}

pub(super) async fn cancel_operation<T>(task: &mut Option<JoinHandle<T>>) {
    if let Some(task) = task.take() {
        task.abort();
        let _ = task.await;
    }
}

/// The account host's probe, for one catalog generation.
async fn cancel_probe(probe: &mut Option<(u64, crate::session::startup::Probe)>) {
    if let Some((_, probe)) = probe.take() {
        probe.cancel().await;
    }
}

pub(super) async fn gui_worker_loop(
    mut commands: mpsc::UnboundedReceiver<GuiCommand>,
    events: Sender<GuiEvent>,
    mut foreground: watch::Receiver<bool>,
) {
    if crate::platform::windows::host_service::resident::managed() {
        if let Err(error) = crate::platform::windows::host_service::resident::request(
            crate::platform::windows::host_service::resident::Request::Resume,
        )
        .await
        {
            let _ = events.send(GuiEvent::Warning((t().app.service_start_failed)(&format!(
                "{error:#}"
            ))));
        }
    }
    let device_runtime = match crate::session::device_session::DeviceRuntime::start() {
        Ok(runtime) => runtime,
        Err(error) => {
            let _ = events.send(GuiEvent::SessionUnavailable((t().app.cant_read_identity)(
                &format!("{error:#}"),
            )));
            return;
        }
    };
    let _ = events.send(GuiEvent::Startup(0, StartupStage::Credentials));
    let mut startup_last = None;
    // When the saved account was opened, until its device list was offered
    // from the cache (the server may be slow or unreachable).
    let mut restoring: Option<(u64, Instant)> = None;
    // This device goes online once the encoders are known (a session must
    // not wait out the probe against its connection deadline), or after
    // this long whatever the probe does.
    let mut host_shown: Option<u64> = None;
    let mut reachability: Option<(u64, bool)> = None;
    let mut probe: Option<(u64, crate::session::startup::Probe)> = None;
    let mut client: Option<Arc<AuthenticatedClient>> = None;
    let mut allow_load = true;
    let mut device_sync = device_sync::DeviceSync::default();
    let mut presence_online = false;
    let mut mutation_is_power = false;
    let mut login_task: Option<ActiveLogin> = None;
    let mut commit_gate = login::LoginCommitGate::default();
    let mut catalog_generation = 0;
    let mut mutation_task: Option<JoinHandle<Result<MutationOutcome>>> = None;
    let mut mutation_generation = 0;
    let mut logout_task: Option<JoinHandle<crate::account::client::LogoutOutcome>> = None;
    let mut assist_lists_task: Option<
        JoinHandle<(u64, Result<crate::account::assist::SavedLists>)>,
    > = None;
    let mut assist_operation_task: Option<JoinHandle<(u64, u64, Result<AssistResult>)>> = None;
    let mut assist_operation_is_query = false;
    let mut assist_operation_context = (0_u64, 0_u64);
    let mut assist_lists_generation = 0_u64;
    let mut next_assist_refresh = Some(Instant::now());
    let mut tick = tokio::time::interval(WORKER_TICK);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut host_signal: Option<ActivePresence> = None;
    let mut presence_stopped = false;
    let mut resident_mode = crate::platform::windows::host_service::resident::managed();
    // "Direct only" decides at start whether the in-process host connects
    // to the server; a change rebuilds it, as the background service does.
    let mut serverless = crate::transport::nat::settings::current().serverless;
    let mut guest = guest::Guest::default();
    let mut guest_allowed = false;
    // Connects through the server without an account (`docs/access-model.md`
    // §2.5); built on first use, dropped when the server changes.
    let mut guest_controller: Option<Arc<AuthenticatedClient>> = None;

    loop {
        let command = tokio::select! {
            biased;
            command = commands.recv() => match command {
                Some(command) => Some(command),
                None => break,
            },
            changed = foreground.changed() => {
                if changed.is_err() { break; }
                None
            },
            _ = tick.tick() => None,
        };
        if let Some(host) = client
            .as_ref()
            .filter(|c| c.is_active())
            .map(|c| &c.host)
            .or_else(|| guest.client.as_ref().map(|c| &c.host))
        {
            host.assistance.touch_ui();
        }
        let mode = crate::platform::windows::host_service::resident::managed();
        let direct_only = crate::transport::nat::settings::current().serverless;
        if mode != resident_mode || (!mode && direct_only != serverless) {
            if direct_only != serverless {
                tracing::info!(direct_only, "direct-only setting changed; rebuilding the host presence");
            }
            stop_active_signal(&mut host_signal).await;
            guest.close().await;
            resident_mode = mode;
            presence_stopped = false;
        }
        serverless = direct_only;
        if let Some(command) = command {
            match command {
                GuiCommand::HostAssist { generation, action } => {
                    if generation == catalog_generation
                        && logout_task.is_none()
                        && let Some(current) = client
                            .as_ref()
                            .map(|c| crate::session::host_client::HostClient::from(c.clone()))
                            .or_else(|| guest.client.clone())
                    {
                        let result =
                            if crate::platform::windows::host_service::resident::managed() {
                                crate::platform::windows::host_service::resident::request(
                                crate::platform::windows::host_service::resident::Request::Assist {
                                    account: current.generation(), action,
                                }).await.map(|_| ())
                            } else {
                                current.host.assistance.act(action)
                            };
                        if let Err(error) = result {
                            let _ = events.send(GuiEvent::HostAssistFailed {
                                generation,
                                error: error.to_string(),
                            });
                        }
                    }
                }
                GuiCommand::RefreshHostAudioDevices { generation } => {
                    if generation == catalog_generation
                        && logout_task.is_none()
                        && let Some(client) = client.as_ref().filter(|c| c.is_active())
                    {
                        client.host.refresh_audio_devices().await;
                    }
                }
                GuiCommand::SaveHostSettings { generation } => {
                    if generation == catalog_generation
                        && logout_task.is_none()
                        && let Some(host) = client
                            .as_ref()
                            .filter(|c| c.is_active())
                            .map(|c| c.host.clone())
                            .or_else(|| guest.client.as_ref().map(|c| c.host.clone()))
                    {
                        host.persist_settings().await;
                    }
                }
                GuiCommand::View {
                    generation,
                    key,
                    alias,
                    device_id,
                    assist,
                    direct,
                    options,
                    background,
                } => {
                    let result = if let Some(direct) = direct {
                        // No account needed: the host's direct endpoint authorises.
                        Ok(crate::session::controller::windows::start_direct(
                            alias.clone(),
                            direct,
                            options,
                        ))
                    } else if generation == catalog_generation && logout_task.is_none() {
                        // Without an account, a host reached through the
                        // server admits this device as a guest (trusted
                        // device or visitor).
                        if client.is_none() && assist.is_some() && guest_controller.is_none() {
                            match AuthenticatedClient::guest(device_runtime.handle()) {
                                Ok(guest) => guest_controller = Some(Arc::new(guest)),
                                Err(error) => tracing::warn!(error = %format!("{error:#}"), "guest controller unavailable"),
                            }
                        }
                        client
                            .as_ref()
                            .or(guest_controller.as_ref().filter(|_| assist.is_some()))
                            .map(|client| {
                                crate::session::controller::windows::start(
                                    Arc::clone(client),
                                    alias.clone(),
                                    device_id.clone(),
                                    assist,
                                    options,
                                    background,
                                )
                            })
                            .ok_or_else(|| t().common.signed_out.to_owned())
                    } else {
                        Err(t().app.account_changed.into())
                    };
                    let _ =
                        events.send(GuiEvent::Viewer(generation, key, alias, device_id, result));
                }
                GuiCommand::Ports {
                    generation,
                    device,
                    options,
                } => {
                    if generation == catalog_generation
                        && logout_task.is_none()
                        && let Some(client) = &client
                    {
                        if let Err(error) = crate::features::port_mapping::ui::open(
                            Arc::clone(client),
                            device,
                            options,
                        ) {
                            let _ = events.send(GuiEvent::Error(error.to_string()));
                        }
                    }
                }
                GuiCommand::Files {
                    generation,
                    device,
                    options,
                } => {
                    if generation == catalog_generation
                        && logout_task.is_none()
                        && let Some(client) = &client
                    {
                        if let Err(error) = crate::features::file_transfer::ui::open(
                            Arc::clone(client),
                            device,
                            options,
                        ) {
                            let _ = events.send(GuiEvent::Error(error.to_string()));
                        }
                    }
                }
                GuiCommand::Diagnostics {
                    generation,
                    device,
                    options,
                } => {
                    if generation == catalog_generation
                        && logout_task.is_none()
                        && let Some(client) = &client
                    {
                        crate::diagnostics::remote::service::start(
                            Arc::clone(client),
                            device,
                            options,
                        );
                    }
                }
                GuiCommand::RefreshAssist => {
                    if assist_lists_task.is_none() {
                        next_assist_refresh = Some(Instant::now());
                    }
                }
                GuiCommand::Friends {
                    generation,
                    operation,
                } => {
                    if generation != catalog_generation || logout_task.is_some() {
                        let _ = events.send(GuiEvent::Friends(
                            generation,
                            Err(t().app.friends_unavailable.into()),
                        ));
                    } else if let Some(client) = &client {
                        let client = Arc::clone(client);
                        let events = events.clone();
                        tokio::spawn(async move {
                            let result = super::friends::execute(client, operation)
                                .await
                                .map_err(assist::operation_error);
                            let _ = events.send(GuiEvent::Friends(generation, result));
                        });
                    } else {
                        let _ = events.send(GuiEvent::Friends(
                            generation,
                            Err(t().common.signed_out.into()),
                        ));
                    }
                }
                GuiCommand::Records {
                    generation,
                    operation,
                } => {
                    if generation != catalog_generation || logout_task.is_some() {
                        let _ = events.send(GuiEvent::Records(
                            generation,
                            Err(t().common.signed_out.into()),
                        ));
                    } else if let Some(client) = &client {
                        let client = Arc::clone(client);
                        let events = events.clone();
                        tokio::spawn(async move {
                            let result = super::records::execute(client, operation)
                                .await
                                .map_err(assist::operation_error);
                            let _ = events.send(GuiEvent::Records(generation, result));
                        });
                    } else {
                        let _ = events.send(GuiEvent::Records(
                            generation,
                            Err(t().common.signed_out.into()),
                        ));
                    }
                }
                GuiCommand::Notices {
                    generation,
                    operation,
                } => {
                    if generation != catalog_generation || logout_task.is_some() {
                        let _ = events.send(GuiEvent::Notices(
                            generation,
                            Err(t().common.signed_out.into()),
                        ));
                    } else if let Some(client) = &client {
                        let client = Arc::clone(client);
                        let events = events.clone();
                        tokio::spawn(async move {
                            let result = super::notices::execute(client, operation)
                                .await
                                .map_err(assist::operation_error);
                            let _ = events.send(GuiEvent::Notices(generation, result));
                        });
                    } else {
                        let _ = events.send(GuiEvent::Notices(
                            generation,
                            Err(t().common.signed_out.into()),
                        ));
                    }
                }
                GuiCommand::CancelAssistCheck => {
                    if assist_operation_is_query {
                        cancel_operation(&mut assist_operation_task).await;
                    }
                }
                GuiCommand::AssistOperation {
                    generation,
                    sequence,
                    operation,
                } => {
                    // Without an account only connecting is possible (as a
                    // guest device); saved records need the account.
                    if client.is_none() && operation.is_query() && guest_controller.is_none() {
                        match AuthenticatedClient::guest(device_runtime.handle()) {
                            Ok(guest) => guest_controller = Some(Arc::new(guest)),
                            Err(error) => tracing::warn!(error = %format!("{error:#}"), "guest controller unavailable"),
                        }
                    }
                    let acting = client
                        .clone()
                        .or_else(|| guest_controller.clone().filter(|_| operation.is_query()));
                    if generation != catalog_generation
                        || logout_task.is_some()
                        || assist_operation_task.is_some()
                        || acting.is_none()
                    {
                        let _ = events.send(GuiEvent::AssistOperation(
                            generation,
                            sequence,
                            Err(t().app.cant_connect.into()),
                        ));
                    } else if let Some(client) = acting {
                        assist_operation_is_query = operation.is_query();
                        if !assist_operation_is_query {
                            cancel_operation(&mut assist_lists_task).await;
                        }
                        assist_operation_context = (generation, sequence);
                        assist_operation_task = Some(tokio::spawn(async move {
                            (
                                generation,
                                sequence,
                                assist::execute(client, operation).await,
                            )
                        }));
                    }
                }
                GuiCommand::Detail(id) => {
                    device_sync.detail(id);
                }
                GuiCommand::Mutate { generation, change } => {
                    if generation != catalog_generation
                        || mutation_task.is_some()
                        || logout_task.is_some()
                        || client.is_none()
                    {
                        let _ = events.send(GuiEvent::MutationFinished(
                            generation,
                            Err(t().app.device_operations_unavailable.into()),
                        ));
                    } else if let Some(client) = &client {
                        let client = Arc::clone(client);
                        mutation_generation = generation;
                        mutation_is_power = matches!(&change, DeviceMutation::Power { .. });
                        let report = events.clone();
                        mutation_task = Some(tokio::spawn(async move {
                            match change {
                                DeviceMutation::Rename { id, alias } => {
                                    let (actual, message) =
                                        client.rename_owned_device(&id, &alias).await?;
                                    Ok(MutationOutcome::Changed {
                                        message,
                                        change:
                                            crate::account::device_change::DeviceChange::renamed(
                                                id, actual,
                                            ),
                                    })
                                }
                                DeviceMutation::Remove { id } => {
                                    let message = client.remove_account_device(&id).await?;
                                    Ok(MutationOutcome::Changed {
                                        message,
                                        change:
                                            crate::account::device_change::DeviceChange::removed(id),
                                    })
                                }
                                DeviceMutation::Power { device, action } => {
                                    let receipt = client
                                        .power_owned_device(&device, action, || {
                                            let _ = report.send(GuiEvent::PowerDispatched(
                                                generation,
                                                device.device_id.clone(),
                                                action,
                                            ));
                                        })
                                        .await?;
                                    Ok(MutationOutcome::Power(Box::new(power::AcceptedPower {
                                        device,
                                        action,
                                        receipt,
                                    })))
                                }
                            }
                        }));
                    }
                }
                GuiCommand::PrepareExit => {
                    tracing::info!("device center preparing to exit");
                    let update_exit = super::instance::take_update_exit();
                    let result = if crate::platform::windows::host_service::resident::managed() {
                        use crate::platform::windows::host_service::resident;
                        let paused = resident::pause_for_exit().await;
                        if paused.is_err() {
                            // This window stays: so does hosting, unpaused,
                            // and every request of this window works again.
                            if let Err(error) = resident::request(resident::Request::Resume).await {
                                tracing::warn!(error = %format!("{error:#}"), "background host not resumed after a failed exit");
                            }
                        }
                        paused.map_err(|e| format!("{e:#}"))
                    } else if update_exit && let Some(current) = &client {
                        current
                            .host
                            .prepare_update()
                            .await
                            .map_err(|e| format!("{e:#}"))
                    } else {
                        Ok(())
                    };
                    tracing::info!(ready = result.is_ok(), "device center exit prepared");
                    let _ = events.send(GuiEvent::ExitReady(result));
                }
                GuiCommand::Reconnect => {
                    crate::session::reconnect::now();
                    if crate::platform::windows::host_service::resident::managed() {
                        tokio::spawn(async {
                            use crate::platform::windows::host_service::resident;
                            if let Err(error) = resident::request(resident::Request::Reconnect).await {
                                tracing::debug!(error = %format!("{error:#}"), "background host not asked to reconnect");
                            }
                        });
                    }
                }
                GuiCommand::Shutdown => break,
                GuiCommand::UseServer { server } => {
                    // A guest presence on the previous server ends first; it
                    // starts again on the new one.
                    let result = async {
                        if ServerConfig::load()?.as_ref() != Some(&server) {
                            guest.close().await;
                            guest_controller = None;
                            server.save()?;
                        }
                        anyhow::Ok(())
                    }
                    .await
                    .map_err(|error| format!("{error:#}"));
                    let _ = events.send(GuiEvent::ServerSaved(result));
                }
                GuiCommand::Refresh => {
                    device_sync.refresh(false);
                    allow_load = true;
                }
                GuiCommand::Login {
                    generation,
                    attempt,
                    server,
                    credentials,
                } if logout_task.is_none() && generation == catalog_generation => {
                    cancel_operation(&mut assist_lists_task).await;
                    cancel_operation(&mut assist_operation_task).await;

                    catalog_generation = generation;

                    cancel_login_task(&mut login_task).await;
                    device_sync.reset().await;
                    presence_online = false;
                    stop_active_signal(&mut host_signal).await;
                    client = None;
                    allow_load = false;
                    let (progress, receiver) = std::sync::mpsc::channel();
                    let device = device_runtime.handle();
                    let gate = commit_gate.clone();
                    login_task = Some(ActiveLogin {
                        generation,
                        attempt,
                        progress: receiver,
                        task: tokio::spawn(async move {
                            // The server setting is saved only when this login
                            // is submitted; a changed server re-registers the
                            // device there.
                            if ServerConfig::load()?.as_ref() != Some(&server) {
                                server.save()?;
                            }
                            login::prepare_login_with_gate(device, gate, credentials, move |event| {
                                let _ = progress.send(event);
                            })
                            .await
                            .map_err(|error| anyhow!(login::error_message(&error)))
                        }),
                    });
                }
                GuiCommand::Login { .. } => {}
                GuiCommand::CancelAttempt(generation) => {
                    if generation == catalog_generation {
                        cancel_login_task(&mut login_task).await;
                    }
                }
                GuiCommand::CancelLogin(generation) => {
                    catalog_generation = generation;
                    cancel_login_task(&mut login_task).await;
                    commit_gate = login::LoginCommitGate::default();
                }
                GuiCommand::Logout if logout_task.is_none() => {
                    cancel_operation(&mut assist_lists_task).await;
                    cancel_operation(&mut assist_operation_task).await;

                    cancel_login_task(&mut login_task).await;
                    commit_gate = login::LoginCommitGate::default();
                    device_sync.reset().await;
                    presence_online = false;
                    allow_load = false;
                    if let Some(client) = client.take() {
                        logout_task = Some(tokio::spawn(async move { client.logout().await }));
                    } else {
                        let _ = events.send(GuiEvent::LoggedOut(
                            crate::account::client::LogoutOutcome {
                                remote_error: Some(t().app.no_account_session.into()),
                                local_error: None,
                            },
                        ));
                    }
                    stop_active_signal(&mut host_signal).await;
                }
                GuiCommand::Logout => {}
            }
        }

        {
            let method = LoginMethod::Password;
            let task = &mut login_task;
            if let Some(login) = task.as_ref() {
                while let Ok(progress) = login.progress.try_recv() {
                    let _ = events.send(GuiEvent::LoginProgress(
                        method,
                        login.generation,
                        login.attempt,
                        progress,
                    ));
                }
            }
            if let Some(login) = task
                .as_ref()
                .is_some_and(|login| login.task.is_finished())
                .then(|| task.take())
                .flatten()
            {
            if login.generation != catalog_generation {
                login.task.abort();
                let _ = login.task.await;
            } else {
            let result = match login.task.await {
                Ok(Ok(prepared)) => {
                    guest.close().await;
                    prepared.commit().map(|_| ())
                }
                Ok(Err(error)) => Err(error),
                Err(error) => Err(error.into()),
            };
            if result.is_ok() {
                guest_allowed = false;
                // An accepted result closes this login epoch; queued old
                // events cannot win.
                cancel_login_task(&mut login_task).await;
                catalog_generation = catalog_generation.wrapping_add(1);
                commit_gate = login::LoginCommitGate::default();
                allow_load = true;
                device_sync.refresh(false);
            }
            let _ = events.send(GuiEvent::LoginFinished(
                method,
                login.generation,
                login.attempt,
                result,
            ));
            }
            }
        }

        if assist_operation_task
            .as_ref()
            .is_some_and(JoinHandle::is_finished)
        {
            let query = assist_operation_is_query;
            match assist_operation_task
                .take()
                .expect("finished assist operation")
                .await
            {
                Ok((generation, sequence, result)) => {
                    if !query {
                        cancel_operation(&mut assist_lists_task).await;
                        next_assist_refresh = Some(Instant::now());
                    }
                    let _ = events.send(GuiEvent::AssistOperation(
                        generation,
                        sequence,
                        result.map_err(assist::operation_error),
                    ));
                }
                Err(error) => {
                    let _ = events.send(GuiEvent::AssistOperation(
                        assist_operation_context.0,
                        assist_operation_context.1,
                        Err((t().app.connection_interrupted)(&error.to_string())),
                    ));
                }
            }
        }
        if assist_lists_task
            .as_ref()
            .is_some_and(JoinHandle::is_finished)
        {
            match assist_lists_task
                .take()
                .expect("finished assist list")
                .await
            {
                Ok((generation, result)) => {
                    // The window learns the server is away before the error.
                    let away = result
                        .as_ref()
                        .err()
                        .and_then(crate::account::api::failure_code)
                        .is_some_and(crate::account::session_restore::out_of_reach);
                    if away && reachability != Some((generation, true)) {
                        reachability = Some((generation, true));
                        let _ = events.send(GuiEvent::Reachability(generation, true));
                    }
                    let _ = events.send(GuiEvent::AssistLists(
                        generation,
                        result.map_err(assist::operation_error),
                    ));
                }
                Err(error) => {
                    let _ = events.send(GuiEvent::AssistLists(
                        assist_lists_generation,
                        Err((t().app.records_interrupted)(&error.to_string())),
                    ));
                }
            }
        }
        if logout_task.as_ref().is_some_and(JoinHandle::is_finished) {
            let result = logout_task.take().expect("finished logout").await;
            match result {
                Ok(outcome) => {
                    guest_allowed = outcome.local_error.is_none();
                    let _ = events.send(GuiEvent::LoggedOut(outcome));
                }
                Err(error) => {
                    let _ =
                        events.send(GuiEvent::LoggedOut(crate::account::client::LogoutOutcome {
                            remote_error: Some((t().app.sign_out_error)(&error.to_string())),
                            local_error: Some((t().app.sign_out_error)(&error.to_string())),
                        }));
                }
            }
        }

        if let Some(signal) = &host_signal {
            while let Ok(event) = signal.events.try_recv() {
                match event {
                    PresenceEvent::State(state) => {
                        let online = matches!(state, PresenceState::Online);
                        if online && !presence_online {
                            device_sync.resynchronize();
                            next_assist_refresh = Some(Instant::now());
                        }
                        presence_online = online;
                        let _ = events.send(GuiEvent::Presence(state));
                    }
                    PresenceEvent::Warning(message) => {
                        let _ = events.send(GuiEvent::Warning(message));
                    }
                    PresenceEvent::DeviceChanged(change) => {
                        let name = client
                            .as_ref()
                            .map(|c| c.account_name())
                            .unwrap_or_default();
                        device_sync.change(change, &events, catalog_generation, &name);
                    }
                    PresenceEvent::DevicesResync => device_sync.resynchronize(),
                    PresenceEvent::AccountEnded => {}
                }
            }
        }
        if client.as_ref().is_some_and(|client| !client.is_active()) {
            cancel_operation(&mut assist_lists_task).await;
            cancel_operation(&mut assist_operation_task).await;
            cancel_login_task(&mut login_task).await;
            commit_gate = login::LoginCommitGate::default();

            cancel_operation(&mut mutation_task).await;

            device_sync.reset().await;
            presence_online = false;
            client = None;
            allow_load = false;
            let _ = events.send(GuiEvent::AccountEnded(
                t().app.account_session_ended.into(),
            ));
            stop_active_signal(&mut host_signal).await;
        }

        if mutation_task.as_ref().is_some_and(JoinHandle::is_finished) {
            let result = mutation_task
                .take()
                .expect("finished mutation")
                .await
                .unwrap_or_else(|e| Err(e.into()))
                .map_err(|e| format!("{e:#}"));
            if let Ok(MutationOutcome::Changed { change, .. }) = &result {
                let name = client
                    .as_ref()
                    .map(|c| c.account_name())
                    .unwrap_or_default();
                device_sync.change(change.clone(), &events, catalog_generation, &name);
            } else if result.is_err() && mutation_is_power {
                // Observe once after an uncertain power result; never replay the command.
                device_sync.refresh_status();
            }
            let _ = events.send(GuiEvent::MutationFinished(mutation_generation, result));
        }

        if let Some(signal) = host_signal.as_ref()
            && signal.task.is_finished()
        {
            let signal = host_signal.take().expect("finished presence");
            presence_stopped = true;
            if let Err(error) = signal.task.await.unwrap_or_else(|error| Err(error.into())) {
                let _ = events.send(GuiEvent::Warning((t().app.presence_stopped)(&format!(
                    "{error:#}"
                ))));
            }
            let _ = events.send(GuiEvent::Presence(PresenceState::Offline));
        }

        if client.is_none()
            || logout_task.is_some()
            || probe
                .as_ref()
                .is_some_and(|(generation, _)| *generation != catalog_generation)
        {
            cancel_probe(&mut probe).await;
        }
        if client.is_none()
            && allow_load
            && login_task.is_none()
            && logout_task.is_none()
        {
            let _ = events.send(GuiEvent::Startup(
                catalog_generation,
                StartupStage::Credentials,
            ));
            match AuthenticatedClient::from_saved_session_with_device(device_runtime.handle()) {
                Ok(loaded) => {
                    restoring = Some((catalog_generation, Instant::now()));
                    host_shown = None;
                    client = Some(Arc::new(loaded));
                    presence_stopped = false;
                }
                Err(error) => {
                    allow_load = false;
                    let event = if error
                        .downcast_ref::<crate::account::client::NoSavedSession>()
                        .is_some()
                    {
                        tracing::info!("no saved account; guest assistance available");
                        guest_allowed = true;
                        GuiEvent::SignedOut
                    } else {
                        tracing::warn!(%error, "saved account could not be opened; credentials retained");
                        guest_allowed = true;
                        GuiEvent::SessionUnavailable((t().app.cant_open_account)(&format!(
                            "{error:#}"
                        )))
                    };
                    let _ = events.send(event);
                }
            }
        }
        if client.is_none() && guest_allowed && logout_task.is_none() {
            guest
                .poll(device_runtime.handle(), catalog_generation, &events)
                .await;
        } else if guest.client.is_some() {
            guest.close().await;
        }
        if let Some(active_client) = &client {
            if logout_task.is_some() {
                continue;
            }
            // The probe runs alongside the account and device list
            // (`session::startup`).
            if probe.is_none() {
                probe = Some((
                    catalog_generation,
                    crate::session::startup::Probe::start(&active_client.host),
                ));
            }
            if let Some((_, running)) = &mut probe
                && let Some(Err(error)) = running.ended().await
            {
                let _ = events.send(GuiEvent::Warning((t().app.media_check_failed)(&format!(
                    "{error:#}"
                ))));
            }
            let stage = match active_client.restoration_stage() {
                crate::account::client::RestorationStage::Device => StartupStage::Device,
                crate::account::client::RestorationStage::Account => StartupStage::Account,
                crate::account::client::RestorationStage::Ready => StartupStage::Devices,
            };
            if startup_last != Some((catalog_generation, stage)) {
                startup_last = Some((catalog_generation, stage));
                let _ = events.send(GuiEvent::Startup(catalog_generation, stage));
            }
            if stage == StartupStage::Devices {
                restoring = None;
            } else if let Some((generation, since)) = restoring
                && since.elapsed() >= CACHED_LIST_AFTER
            {
                restoring = None;
                if generation == catalog_generation
                    && let Some(list) =
                        crate::transport::direct::store::device_list(active_client.user_id())
                            .or_else(this_device_only)
                {
                    let _ = events.send(GuiEvent::CachedDevices(
                        generation,
                        list,
                        active_client.account_name(),
                    ));
                }
            }
            let refresh_active = *foreground.borrow();
            if refresh_active
                && assist_lists_task.is_none()
                && next_assist_refresh.is_some()
                && logout_task.is_none()
                && (assist_operation_task.is_none() || assist_operation_is_query)
            {
                let client = Arc::clone(active_client);
                let generation = catalog_generation;
                assist_lists_generation = generation;
                assist_lists_task = Some(tokio::spawn(async move {
                    (generation, client.assist_lists().await)
                }));
                next_assist_refresh = None;
            }
            if let Some(error) = device_sync
                .poll(active_client, &events, catalog_generation)
                .await
            {
                if active_client.restoration_failed().await {
                    allow_load = false;
                    tracing::warn!(%error, "saved account restoration failed");
                    let _ = events.send(GuiEvent::SessionUnavailable((t().app.restore_failed)(
                        &format!("{error:#}"),
                    )));
                    device_sync.reset().await;
                    client = None;
                    stop_active_signal(&mut host_signal).await;
                    // The login page still lets others connect to this device.
                    guest_allowed = true;
                    continue;
                }
                let _ = events.send(GuiEvent::Warning((t().app.device_sync_failed)(&format!(
                    "{error:#}"
                ))));
            }
            let away = active_client.server_unreachable();
            if reachability != Some((catalog_generation, away)) {
                reachability = Some((catalog_generation, away));
                let _ = events.send(GuiEvent::Reachability(catalog_generation, away));
            }
            // The "this device" page works with the host before it is online.
            if host_shown != Some(catalog_generation) {
                host_shown = Some(catalog_generation);
                let _ = events.send(GuiEvent::Host(
                    catalog_generation,
                    active_client.host.clone(),
                ));
            }
            if host_signal.is_none()
                && !presence_stopped
                && probe.as_ref().is_some_and(|(_, probe)| probe.online())
            {
                host_signal = Some(ActivePresence::start(Arc::clone(active_client)));
            }
        }
    }
    // Each step is logged: a step that never finishes keeps the process
    // alive after its window is gone.
    tracing::info!("device center shutting down");
    cancel_probe(&mut probe).await;
    cancel_login_task(&mut login_task).await;
    tracing::info!("device center shutdown: viewers");
    crate::session::controller::windows::shutdown().await;
    cancel_operation(&mut assist_lists_task).await;
    cancel_operation(&mut assist_operation_task).await;
    tracing::info!("device center shutdown: device list");
    device_sync.reset().await;

    // A user-confirmed mutation may already have reached the service. Let its
    // acknowledgement/reconciliation and own-name persistence finish on exit.
    // Neither waits for good on an unreachable server.
    if let Some(task) = mutation_task {
        let _ = tokio::time::timeout(EXIT_TASK_WAIT, task).await;
    }
    // Once explicitly requested, logout still commits its local cleanup even
    // if the device-center window closes while the HTTP response is pending.
    if let Some(task) = logout_task {
        let _ = tokio::time::timeout(EXIT_TASK_WAIT, task).await;
    }
    tracing::info!("device center shutdown: presence");
    stop_active_signal(&mut host_signal).await;
    tracing::info!("device center shutdown: guest host");
    guest.close().await;
    tracing::info!("device center shutdown: device runtime");
    device_runtime.close().await;
    tracing::info!("device center shut down");
}

/// How long exiting waits for a confirmed change or a sign-out to finish.
const EXIT_TASK_WAIT: std::time::Duration = std::time::Duration::from_secs(10);

/// What the device list shows before the server first answered and with no
/// list saved from before: this device alone.
fn this_device_only() -> Option<crate::account::api::DeviceList> {
    let device = crate::transport::direct::local_device().ok()?;
    Some(crate::account::api::DeviceList {
        current_device: crate::account::api::DeviceInfo {
            device_id: device.identity.device_id.clone(),
            alias: device.name.clone(),
            status: "offline".into(),
            controlled_support: true,
            version_name: env!("CARGO_PKG_VERSION").into(),
            ..Default::default()
        },
        devices: Vec::new(),
        shared: Vec::new(),
    })
}

pub(super) async fn stop_active_signal(active_signal: &mut Option<ActivePresence>) {
    if let Some(signal) = active_signal.take() {
        signal.close().await;
    }
}
