//! Device-center asynchronous worker and cancellation ownership.
mod guest;
use super::assist::AssistResult;
use super::messages::{DeviceMutation, GuiCommand, GuiEvent, MutationOutcome};
use super::phone::LoginMethod;
use super::{StartupStage, WORKER_TICK, assist, device_sync, power};
use crate::account::client::AuthenticatedClient;
use crate::account::login::{self, LoginProgress};
use crate::session::presence::{ActivePresence, PresenceEvent, PresenceState};
use anyhow::{Result, anyhow};
use std::sync::Arc;
use std::sync::mpsc::{Receiver, Sender};
use std::time::Instant;
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
                        .send(GuiEvent::Error(format!("无法启动 GUI 后台服务：{error}")));
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

pub(super) struct ActiveSmsCode {
    pub(super) generation: u64,
    pub(super) attempt: u64,
    pub(super) phone: login::sms::PhoneNumber,
    pub(super) task: JoinHandle<login::sms::CodeOutcome>,
}

pub(super) async fn cancel_sms_task(task: &mut Option<ActiveSmsCode>) {
    if let Some(active) = task.take() {
        active.task.abort();
        let _ = active.task.await;
    }
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

struct StartupMedia {
    generation: u64,
    cancel: tokio_util::sync::CancellationToken,
    task: JoinHandle<Result<()>>,
}
async fn cancel_startup_media(task: &mut Option<StartupMedia>) {
    if let Some(active) = task.take() {
        active.cancel.cancel();
        // The native probe observes cancellation and drops GPU/COM owners on
        // its own thread. Await it rather than abandoning a blocking task.
        let _ = active.task.await;
    }
}

pub(super) async fn gui_worker_loop(
    mut commands: mpsc::UnboundedReceiver<GuiCommand>,
    events: Sender<GuiEvent>,
    mut foreground: watch::Receiver<bool>,
) {
    if crate::platform::host_service::resident::managed() {
        if let Err(error) = crate::platform::host_service::resident::request(
            crate::platform::host_service::resident::Request::Resume,
        )
        .await
        {
            let _ = events.send(GuiEvent::Warning(format!("后台启动失败：{error:#}")));
        }
    }
    let device_runtime = match crate::session::device_session::DeviceRuntime::start() {
        Ok(runtime) => runtime,
        Err(error) => {
            let _ = events.send(GuiEvent::SessionUnavailable(format!(
                "无法打开本机设备身份：{error:#}"
            )));
            return;
        }
    };
    let _ = events.send(GuiEvent::Startup(0, StartupStage::Credentials));
    let mut startup_last = None;
    let mut startup_media: Option<StartupMedia> = None;
    let mut media_ready = None;
    let mut client: Option<Arc<AuthenticatedClient>> = None;
    let mut allow_load = true;
    let mut device_sync = device_sync::DeviceSync::default();
    let mut presence_online = false;
    let mut mutation_is_power = false;
    let mut login_task: Option<ActiveLogin> = None;
    let mut sms_login_task: Option<ActiveLogin> = None;
    let mut commit_gate = login::LoginCommitGate::default();
    let mut sms_task: Option<ActiveSmsCode> = None;
    let mut sms_gate = login::sms::SmsGate::default();
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
    let mut resident_mode = crate::platform::host_service::resident::managed();
    let mut guest = guest::Guest::default();
    let mut guest_allowed = false;

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
        let mode = crate::platform::host_service::resident::managed();
        if mode != resident_mode {
            stop_active_signal(&mut host_signal).await;
            guest.close().await;
            resident_mode = mode;
            presence_stopped = false;
        }
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
                        let result = if crate::platform::host_service::resident::managed() {
                            crate::platform::host_service::resident::request(
                                crate::platform::host_service::resident::Request::Assist {
                                    account: current.generation(),
                                    action,
                                },
                            )
                            .await
                            .map(|_| ())
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
                    options,
                    background,
                    takeover,
                } => {
                    let result = if generation == catalog_generation && logout_task.is_none() {
                        client
                            .as_ref()
                            .map(|client| {
                                crate::session::controller::windows::start(
                                    Arc::clone(client),
                                    alias.clone(),
                                    device_id.clone(),
                                    assist,
                                    options,
                                    background,
                                    takeover,
                                )
                            })
                            .ok_or_else(|| "请先登录".to_owned())
                    } else {
                        Err("账号状态已改变".into())
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
                    if generation != catalog_generation
                        || logout_task.is_some()
                        || assist_operation_task.is_some()
                        || client.is_none()
                    {
                        let _ = events.send(GuiEvent::AssistOperation(
                            generation,
                            sequence,
                            Err("当前无法处理远程协助操作".into()),
                        ));
                    } else if let Some(client) = &client {
                        let client = Arc::clone(client);
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
                GuiCommand::RequestSms {
                    generation,
                    attempt,
                    phone,
                    agreed,
                } => {
                    if generation != catalog_generation
                        || logout_task.is_some()
                        || sms_login_task.is_some()
                        || sms_task.is_some()
                    {
                        let _ = events.send(GuiEvent::SmsCooldown(sms_gate.resend_at()));
                        let _ = events.send(GuiEvent::SmsCodeFinished {
                            generation,
                            attempt,
                            phone,
                            dispatched: false,
                            result: Err("请等待当前操作完成".into()),
                        });
                        continue;
                    }
                    let deadline = match sms_gate.begin_request(Instant::now(), agreed) {
                        Ok(deadline) => deadline,
                        Err(error) => {
                            let _ = events.send(GuiEvent::SmsCooldown(sms_gate.resend_at()));
                            let _ = events.send(GuiEvent::SmsCodeFinished {
                                generation,
                                attempt,
                                phone,
                                dispatched: false,
                                result: Err(error.to_string()),
                            });
                            continue;
                        }
                    };
                    catalog_generation = generation;

                    device_sync.reset().await;
                    presence_online = false;
                    stop_active_signal(&mut host_signal).await;
                    client = None;
                    allow_load = false;
                    let _ = events.send(GuiEvent::SmsCooldown(Some(deadline)));
                    sms_task = Some(ActiveSmsCode {
                        generation,
                        attempt,
                        phone: phone.clone(),
                        task: tokio::spawn(login::sms::request_code(
                            device_runtime.handle(),
                            phone,
                        )),
                    });
                }
                GuiCommand::LoginSms {
                    generation,
                    attempt,
                    phone,
                    code,
                    agreed,
                } => {
                    let ready = if generation != catalog_generation
                        || logout_task.is_some()
                        || sms_login_task.is_some()
                        || sms_task.is_some()
                    {
                        Err(anyhow!("登录操作已改变，请重新获取验证码"))
                    } else {
                        sms_gate.validate_submission(&phone, &code, agreed)
                    };
                    if let Err(error) = ready {
                        let _ = events.send(GuiEvent::LoginFinished(
                            LoginMethod::Phone,
                            generation,
                            attempt,
                            Err(error),
                        ));
                        continue;
                    }
                    let device = device_runtime.handle();
                    let (progress, receiver) = std::sync::mpsc::channel();
                    let gate = commit_gate.clone();
                    sms_login_task = Some(ActiveLogin {
                        generation,
                        attempt,
                        progress: receiver,
                        task: tokio::spawn(async move {
                            let result = login::sms::prepare_login(
                                device,
                                gate,
                                phone.clone(),
                                code.clone(),
                                move |event| {
                                    let _ = progress.send(event);
                                },
                            )
                            .await;
                            result.map_err(|error| {
                                anyhow!(login::sms::error_message(
                                    &error,
                                    Some(&phone),
                                    Some(&code)
                                ))
                            })
                        }),
                    });
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
                            Err("当前无法执行设备操作，未发送请求".into()),
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
                    let update_exit = super::instance::take_update_exit();
                    let result = if crate::platform::host_service::resident::managed() {
                        crate::platform::host_service::resident::pause_for_exit()
                            .await
                            .map_err(|e| format!("{e:#}"))
                    } else if update_exit && let Some(current) = &client {
                        current
                            .host
                            .prepare_update()
                            .await
                            .map_err(|e| format!("{e:#}"))
                    } else {
                        Ok(())
                    };
                    let _ = events.send(GuiEvent::ExitReady(result));
                }
                GuiCommand::Shutdown => break,
                GuiCommand::Refresh => {
                    device_sync.refresh(false);
                    allow_load = true;
                }
                GuiCommand::Login {
                    generation,
                    attempt,
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
                    login_task = Some(ActiveLogin {
                        generation,
                        attempt,
                        progress: receiver,
                        task: tokio::spawn(login::prepare_login_with_gate(
                            device_runtime.handle(),
                            commit_gate.clone(),
                            move |event| {
                                let _ = progress.send(event);
                            },
                        )),
                    });
                }
                GuiCommand::Login { .. } => {}
                GuiCommand::CancelQr(generation) => {
                    if generation == catalog_generation {
                        cancel_login_task(&mut login_task).await;
                    }
                }
                GuiCommand::CancelSms(generation) => {
                    if generation == catalog_generation {
                        cancel_sms_task(&mut sms_task).await;
                        cancel_login_task(&mut sms_login_task).await;
                        sms_gate.cancel();
                    }
                }
                GuiCommand::CancelLogin(generation) => {
                    cancel_sms_task(&mut sms_task).await;
                    sms_gate.cancel();
                    catalog_generation = generation;
                    cancel_login_task(&mut login_task).await;
                    cancel_login_task(&mut sms_login_task).await;
                    commit_gate = login::LoginCommitGate::default();
                }
                GuiCommand::Logout if logout_task.is_none() => {
                    cancel_operation(&mut assist_lists_task).await;
                    cancel_operation(&mut assist_operation_task).await;
                    cancel_sms_task(&mut sms_task).await;
                    sms_gate.cancel();

                    cancel_login_task(&mut login_task).await;
                    cancel_login_task(&mut sms_login_task).await;
                    commit_gate = login::LoginCommitGate::default();
                    device_sync.reset().await;
                    presence_online = false;
                    allow_load = false;
                    if let Some(client) = client.take() {
                        logout_task = Some(tokio::spawn(async move { client.logout().await }));
                    } else {
                        let _ = events.send(GuiEvent::LoggedOut(
                            crate::account::client::LogoutOutcome {
                                remote_error: Some("没有可用账号会话，未发送服务端退出请求".into()),
                                local_error: None,
                            },
                        ));
                    }
                    stop_active_signal(&mut host_signal).await;
                }
                GuiCommand::Logout => {}
            }
        }

        if sms_task.as_ref().is_some_and(|s| s.task.is_finished()) {
            let active = sms_task.take().expect("finished SMS request");
            let outcome = active
                .task
                .await
                .unwrap_or_else(|error| login::sms::CodeOutcome {
                    dispatched: false,
                    result: Err(error.into()),
                });
            if active.generation == catalog_generation {
                if outcome.dispatched {
                    sms_gate.dispatched(active.phone.clone());
                }
                let result = outcome
                    .result
                    .map_err(|error| login::sms::error_message(&error, Some(&active.phone), None));
                tracing::info!(
                    success = result.is_ok(),
                    dispatched = outcome.dispatched,
                    "SMS code request completed"
                );
                let _ = events.send(GuiEvent::SmsCodeFinished {
                    generation: active.generation,
                    attempt: active.attempt,
                    phone: active.phone,
                    dispatched: outcome.dispatched,
                    result,
                });
            }
        }

        for method in [LoginMethod::Qr, LoginMethod::Phone] {
            let task = match method {
                LoginMethod::Qr => &mut login_task,
                LoginMethod::Phone => &mut sms_login_task,
            };
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
            if !task.as_ref().is_some_and(|login| login.task.is_finished()) {
                continue;
            }
            let login = task.take().expect("finished login");
            if login.generation != catalog_generation {
                login.task.abort();
                let _ = login.task.await;
                continue;
            }
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
                // A single accepted result closes this login epoch. Errors
                // leave the other method alive; queued old events cannot win.
                cancel_login_task(&mut login_task).await;
                cancel_login_task(&mut sms_login_task).await;
                cancel_sms_task(&mut sms_task).await;
                sms_gate.cancel();
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
                        Err(format!("远程协助操作中断：{error}")),
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
                    let _ = events.send(GuiEvent::AssistLists(
                        generation,
                        result.map_err(assist::operation_error),
                    ));
                }
                Err(error) => {
                    let _ = events.send(GuiEvent::AssistLists(
                        assist_lists_generation,
                        Err(format!("记录读取中断：{error}")),
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
                            remote_error: Some(format!("退出后台任务异常：{error}")),
                            local_error: Some("后台退出未完成，凭据删除结果未知".into()),
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
            cancel_sms_task(&mut sms_task).await;
            cancel_login_task(&mut login_task).await;
            cancel_login_task(&mut sms_login_task).await;
            commit_gate = login::LoginCommitGate::default();
            sms_gate.cancel();

            cancel_operation(&mut mutation_task).await;

            device_sync.reset().await;
            presence_online = false;
            client = None;
            allow_load = false;
            let _ = events.send(GuiEvent::AccountEnded(
                "本机设备的账号会话已结束，正在关闭观看连接；请重新登录".into(),
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
                let _ = events.send(GuiEvent::Warning(format!("本机在线会话已停止：{error:#}")));
            }
            let _ = events.send(GuiEvent::Presence(PresenceState::Offline));
        }

        if client.is_none()
            || logout_task.is_some()
            || startup_media
                .as_ref()
                .is_some_and(|task| task.generation != catalog_generation)
        {
            cancel_startup_media(&mut startup_media).await;
            media_ready = None;
        }
        if client.is_none()
            && allow_load
            && login_task.is_none()
            && sms_login_task.is_none()
            && sms_task.is_none()
            && logout_task.is_none()
        {
            let _ = events.send(GuiEvent::Startup(
                catalog_generation,
                StartupStage::Credentials,
            ));
            match AuthenticatedClient::from_saved_session_with_device(device_runtime.handle()) {
                Ok(loaded) => {
                    client = Some(Arc::new(loaded));
                    presence_stopped = false;
                }
                Err(error) => {
                    allow_load = false;
                    let event = if error
                        .downcast_ref::<crate::account::client::NoSavedSession>()
                        .is_some()
                    {
                        guest_allowed = true;
                        GuiEvent::SignedOut
                    } else {
                        tracing::warn!(%error, "saved account could not be opened; credentials retained");
                        GuiEvent::SessionUnavailable(format!("无法打开账号会话：{error:#}"))
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
            if media_ready != Some(catalog_generation) {
                if startup_media.is_none() {
                    let host = active_client.host.clone();
                    let cancel = tokio_util::sync::CancellationToken::new();
                    let task_cancel = cancel.clone();
                    startup_media = Some(StartupMedia {
                        generation: catalog_generation,
                        cancel,
                        task: tokio::spawn(async move { host.prepare_startup(task_cancel).await }),
                    });
                    startup_last = Some((catalog_generation, StartupStage::Media));
                    let _ = events.send(GuiEvent::Startup(catalog_generation, StartupStage::Media));
                }
                if !startup_media
                    .as_ref()
                    .is_some_and(|task| task.task.is_finished())
                {
                    continue;
                }
                let prepared = startup_media.take().expect("completed media preparation");
                if let Err(error) = prepared
                    .task
                    .await
                    .unwrap_or_else(|error| Err(error.into()))
                {
                    let _ = events.send(GuiEvent::Warning(format!(
                        "本机媒体能力检查失败：{error:#}；连接时将重新检查"
                    )));
                }
                media_ready = Some(catalog_generation);
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
                    let _ = events.send(GuiEvent::SessionUnavailable(format!(
                        "登录恢复未完成，凭据已保留，可点击登录重试：{error:#}"
                    )));
                    device_sync.reset().await;
                    client = None;
                    stop_active_signal(&mut host_signal).await;
                    continue;
                }
                let _ = events.send(GuiEvent::Warning(format!(
                    "设备同步失败：{error:#}；可手动刷新重试"
                )));
            }
            if host_signal.is_none() && !presence_stopped {
                let _ = events.send(GuiEvent::Host(
                    catalog_generation,
                    active_client.host.clone(),
                ));
                host_signal = Some(ActivePresence::start(Arc::clone(active_client)));
            }
        }
    }
    cancel_startup_media(&mut startup_media).await;
    cancel_login_task(&mut login_task).await;
    crate::session::controller::windows::shutdown().await;
    cancel_login_task(&mut sms_login_task).await;
    cancel_operation(&mut assist_lists_task).await;
    cancel_operation(&mut assist_operation_task).await;
    cancel_sms_task(&mut sms_task).await;
    device_sync.reset().await;

    // A user-confirmed mutation may already have reached the service. Let its
    // acknowledgement/reconciliation and own-name persistence finish on exit.
    if let Some(task) = mutation_task {
        let _ = task.await;
    }
    // Once explicitly requested, logout still commits its local cleanup even
    // if the device-center window closes while the HTTP response is pending.
    if let Some(task) = logout_task {
        let _ = task.await;
    }
    stop_active_signal(&mut host_signal).await;
    guest.close().await;
    device_runtime.close().await;
}

pub(super) async fn stop_active_signal(active_signal: &mut Option<ActivePresence>) {
    if let Some(signal) = active_signal.take() {
        signal.close().await;
    }
}
