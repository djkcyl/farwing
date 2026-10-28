//! Logged-in device presence and account push ownership, shared by GUI and CLI.
//! Network retry policy is independent of account/device revocation.
//!
//! The same authenticated room now also owns configured local controlled-device access.

use super::host_client::HostClient;
use crate::account::api::ApiFailure;
use crate::account::device_change::{ChangeKind, DeviceChange};
use crate::transport::signal::{
    SignalFailure, SignalPushHandler, SignalRole, SignalSession, SocketState,
};
use anyhow::Result;
use std::{
    sync::{
        Arc,
        mpsc::{self, Receiver, Sender},
    },
    time::{Duration, Instant},
};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

#[derive(Clone, serde::Serialize, serde::Deserialize)]
pub(crate) enum PresenceState {
    Connecting,
    Online,
    Reconnecting,
    Offline,
}

pub(crate) enum PresenceEvent {
    State(PresenceState),
    Warning(String),
    DeviceChanged(DeviceChange),
    DevicesResync,
    AccountEnded,
}

static DEVICE_PUSHES: std::sync::OnceLock<tokio::sync::watch::Sender<u64>> = std::sync::OnceLock::new();

fn device_push_sender() -> &'static tokio::sync::watch::Sender<u64> {
    DEVICE_PUSHES.get_or_init(|| tokio::sync::watch::channel(0).0)
}

/// Changes on every device push any presence connection of this process
/// receives (including a device coming online), so waiting for a device
/// need not poll.
pub(crate) fn device_pushes() -> tokio::sync::watch::Receiver<u64> {
    device_push_sender().subscribe()
}

fn note_device_push() {
    device_push_sender().send_modify(|count| *count = count.wrapping_add(1));
}

/// How long closing waits before the presence is stopped where it is.
const CLOSE_WAIT: Duration = Duration::from_secs(6);

pub(crate) struct ActivePresence {
    cancel: CancellationToken,
    pub(crate) task: JoinHandle<Result<()>>,
    pub(crate) events: Receiver<PresenceEvent>,
}

impl ActivePresence {
    pub(crate) fn start(client: impl Into<HostClient>) -> Self {
        let client = client.into();
        let cancel = CancellationToken::new();
        let task_cancel = cancel.clone();
        let (events, receiver) = mpsc::channel();
        let task = tokio::spawn(async move {
            let ended = client.ended();
            let run = async {
                if crate::platform::windows::host_service::resident::managed() {
                    run_remote(client.clone(), events.clone(), task_cancel.clone()).await
                } else {
                    run_presence(client.clone(), events.clone(), task_cancel.clone()).await
                }
            };
            tokio::pin!(run);
            tokio::select! {
                biased;
                _ = ended.cancelled() => {
                    task_cancel.cancel();
                    if let Err(error) = client.clear_saved_generation() {
                        let _ = events.send(PresenceEvent::Warning((crate::i18n::t().session.warning_clear_credentials_failed)(&format!("{error:#}"))));
                    }
                    let _ = events.send(PresenceEvent::AccountEnded);
                    run.await
                },
                result = &mut run => result,
            }
        });
        Self {
            cancel,
            task,
            events: receiver,
        }
    }

    pub(crate) async fn close(mut self) {
        self.cancel.cancel();
        // Exit, pause and sign-out wait here. A handler still waiting on an
        // unreachable server must not keep them waiting for good.
        match tokio::time::timeout(CLOSE_WAIT, &mut self.task).await {
            Ok(Err(error)) => tracing::warn!(%error, "host presence task ended unexpectedly"),
            Ok(Ok(_)) => (),
            Err(_) => {
                tracing::warn!(
                    seconds = CLOSE_WAIT.as_secs(),
                    "host presence did not close in time; stopping it"
                );
                self.task.abort();
                let _ = (&mut self.task).await;
            }
        }
    }
}

// This mutex is never acquired: its HANDLE only reserves the object name.
struct Reservation {
    _handle: crate::platform::windows::host_service::pipe::Handle,
}
unsafe impl Send for Reservation {}

async fn run_presence(
    client: HostClient,
    events: Sender<PresenceEvent>,
    task_cancel: CancellationToken,
) -> Result<()> {
    // This process hosts: its participants' records go up with its account.
    if let Some(account) = client.account() {
        let cancel = task_cancel.clone();
        tokio::spawn(async move { crate::account::records::upload(&account, &cancel).await });
    }
    // Reservation lifetime (not mutex thread ownership) spans the whole room.
    // During installation the new background waits for the portable room's
    // normal close; two account owners must never kick each other off the server.
    let _owner = loop {
        use windows::{
            Win32::{
                Foundation::{ERROR_ALREADY_EXISTS, GetLastError},
                System::Threading::CreateMutexW,
            },
            core::PCWSTR,
        };
        let name: Vec<u16> =
            crate::product::global_object(&format!("Presence.{}", client.presence_key()))
            .encode_utf16()
            .chain(Some(0))
            .collect();
        let handle = Reservation {
            _handle: crate::platform::windows::host_service::pipe::Handle(unsafe {
                CreateMutexW(None, false, PCWSTR(name.as_ptr()))?
            }),
        };
        if unsafe { GetLastError() } != ERROR_ALREADY_EXISTS {
            break handle;
        }
        drop(handle);
        tokio::select! { _ = task_cancel.cancelled() => return Ok(()), _ = tokio::time::sleep(Duration::from_millis(100)) => () }
    };
    let _assistance = crate::features::host::assist::Running::start(client.clone());
    let offline = client.offline();
    let _wol = (!offline).then(|| crate::features::host::wol::Running::start(client.clone()));
    let _wol_setup =
        (!offline).then(|| crate::features::host::wol::setup::Running::start(client.clone()));
    // One host session serves server and direct participants
    // (`docs/protocol/v2-design.md` §12). It outlives each server connection,
    // and runs on its own while the server is unreachable or not used.
    let router = crate::transport::signal::router::Router::new();
    let direct_enabled = crate::transport::nat::settings::current().direct;
    let direct = crate::transport::direct::host::Supervisor::start(client.clone(), Arc::clone(&router));
    let mut host = crate::transport::signal::host::Session::new(client.clone());
    let mut local = SignalSession::offline(Arc::clone(&router));
    let result = if offline {
        let _ = events.send(PresenceEvent::State(PresenceState::Offline));
        local
            .run_host(&mut host, task_cancel.cancelled())
            .await
            .map(|_| ())
    } else {
        serve_presence(&client, &events, &task_cancel, &mut host, &mut local, &router, direct_enabled).await
    };
    tracing::info!("host presence closing: host session");
    host.close().await;
    tracing::info!("host presence closing: local signalling");
    let _ = local.close().await;
    tracing::info!("host presence closing: direct endpoint");
    drop(direct);
    tracing::info!("host presence closed");
    result
}

#[allow(clippy::too_many_arguments)]
async fn serve_presence(
    client: &HostClient,
    events: &Sender<PresenceEvent>,
    task_cancel: &CancellationToken,
    host: &mut crate::transport::signal::host::Session,
    local: &mut SignalSession,
    router: &Arc<crate::transport::signal::router::Router>,
    direct: bool,
) -> Result<()> {
    let mut policy = HostRoomRetry::default();
    let mut initial_requests = 0;
    let mut backoff = false;
    loop {
        if task_cancel.is_cancelled() {
            return Ok(());
        }
        if client.is_guest() {
            let mut desired = client.host.subscribe();
            while !client.host.requested() {
                tokio::select! { _ = task_cancel.cancelled() => return Ok(()), result = desired.changed() => { result?; } }
            }
        }
        let _ = events.send(PresenceEvent::State(if backoff {
            PresenceState::Reconnecting
        } else {
            PresenceState::Connecting
        }));
        // Direct participants are served while the server is reached.
        let request = local
            .run_host(host, async {
                tokio::select! {
                    _ = task_cancel.cancelled() => None,
                    result = client.presence_ticket() => Some(result),
                }
            })
            .await?;
        let Some(request) = request else {
            return Ok(());
        };
        let room = match request {
            Ok(room) => {
                policy.room_response(&room);
                room
            }
            Err(error) => {
                if error
                    .downcast_ref::<ApiFailure>()
                    .is_some_and(|failure| failure.retires_account() || failure.invalid_device())
                {
                    return Err(error);
                }
                let delay = if backoff {
                    policy.failed_backoff_request()
                } else {
                    initial_requests += 1;
                    if initial_requests >= 3 {
                        if !direct {
                            return Err(error);
                        }
                        // Keep trying in the background: direct connections
                        // work meanwhile.
                        backoff = true;
                    }
                    Duration::from_millis(500)
                };
                let _ = events.send(PresenceEvent::Warning((crate::i18n::t().session.warning_presence_request_failed)(&format!("{error:#}"))));
                let cancelled = local
                    .run_host(host, async {
                        tokio::select! { _ = task_cancel.cancelled() => true, _ = super::reconnect::wait(delay) => false }
                    })
                    .await?;
                if cancelled {
                    return Ok(());
                }
                continue;
            }
        };
        let room_cancel = task_cancel.child_token();
        let push_client = client.clone();
        let push_cancel = room_cancel.clone();
        let push_events = events.clone();
        let observer: SignalPushHandler = Arc::new(move |push| {
            if push_cancel.is_cancelled() || !push_client.is_active() {
                return;
            }
            if crate::account::server_changed::push(push) {
                return;
            }
            // Notices, and friends or requests changing: the device centre
            // reads them again.
            if push.kind == farwing_protocol::realtime::types::NOTICE
                || push.kind == farwing_protocol::realtime::types::FRIEND_CHANGED
            {
                crate::account::notices::pushed();
                return;
            }
            if push_client.host.assistance.push(push) {
                return;
            }
            if push_client.is_guest() {
                return;
            }
            if push.kind == farwing_protocol::realtime::types::WOL_WAKE {
                if push_client.host.wol_permitted() {
                    push_client.host.wol.push(push, &push_cancel);
                }
                return;
            }
            if push_client.host.power.push(push) {
                return;
            }
            let change = match DeviceChange::parse(push) {
                Ok(Some(change)) => change,
                Ok(None) => return,
                Err(_) => {
                    tracing::warn!("discarded malformed device push");
                    return;
                }
            };
            if change.kind == ChangeKind::Removed && change.id == push_client.device_id() {
                // Exact current-device match. No credential I/O on the
                // realtime socket worker.
                tracing::info!("current device was removed from the account; retiring account generation");
                push_client.retire();
            } else {
                note_device_push();
                let _ = push_events.send(PresenceEvent::DeviceChanged(change));
            }
        });
        let session = local
            .run_host(
                host,
                SignalSession::connect_observed(room, SignalRole::Host, &room_cancel, Some(observer)),
            )
            .await?;
        // Cancelled while connecting: the connection is closed below.
        let result = match session {
            Ok(mut session) => {
                session.attach_router(Arc::clone(router));
                // This process's controller sessions of the same account
                // ride on this connection (`docs/protocol/v2-design.md` §11).
                let _carrier = client.account_user_id().and_then(|user| {
                    session.carry_controller_sessions(&client.device_id(), &user)
                });
                host.server_connected();
                // The hosting process's caches of the account's devices and
                // of the friends' devices that may control it authorise them
                // for direct connections, also later while the server is
                // unreachable (`docs/access-model.md` §2.2).
                if direct && client.account_user_id().is_some() {
                    let refresh = client.clone();
                    tokio::spawn(async move {
                        if let Err(error) = refresh.refresh_account_cache().await {
                            tracing::debug!(error = %format!("{error:#}"), "account device cache not refreshed");
                        }
                    });
                }
                client.host.wol.online(true);
                let _ = events.send(PresenceEvent::State(PresenceState::Online));
                let mut socket_state = session.socket_state();
                let watch = async {
                    let mut state_open = true;
                    loop {
                        tokio::select! {
                            _ = task_cancel.cancelled() => break,
                            state = socket_state.changed(), if state_open => {
                                if state.is_err() { state_open = false; continue; }
                                let socket = *socket_state.borrow_and_update();
                                client.host.wol.online(socket == SocketState::Connected);
                                let presence = match socket {
                                    SocketState::Connected => PresenceState::Online,
                                    SocketState::Connecting | SocketState::Reconnecting => PresenceState::Reconnecting,
                                    SocketState::Closed => PresenceState::Offline,
                                };
                                let _ = events.send(PresenceEvent::State(presence));
                            }
                        }
                    }
                };
                let result = session.run_host(host, watch).await;
                // Participants of the lost server connection cannot be
                // reached any more; direct ones continue.
                if !task_cancel.is_cancelled() {
                    host.retain_direct(local).await;
                }
                tracing::debug!("host presence: closing the server connection");
                let closed = session.close().await;
                result.and(closed)
            }
            Err(error) => Err(error),
        };
        room_cancel.cancel();
        client.host.wol.online(false);
        if task_cancel.is_cancelled() {
            return Ok(());
        }
        if matches!(
            result
                .as_ref()
                .err()
                .and_then(|error| error.downcast_ref::<SignalFailure>()),
            Some(SignalFailure::Kicked(_))
        ) {
            return result;
        }
        let delay = policy.lost_room();
        backoff = true;
        initial_requests = 0;
        if let Err(error) = result {
            let _ = events.send(PresenceEvent::Warning((crate::i18n::t().session.warning_presence_lost)(&format!("{error:#}"))));
        }
        let _ = events.send(PresenceEvent::State(PresenceState::Reconnecting));
        let cancelled = local
            .run_host(host, async {
                tokio::select! { _ = task_cancel.cancelled() => true, _ = super::reconnect::wait(delay) => false }
            })
            .await?;
        if cancelled {
            return Ok(());
        }
    }
}

// Presence rebuild backoff. Ticket-response and socket-open time are
// distinct: >=30s starts a random 1..5s backoff; repeated short failures
// double the delay up to the server limit.
struct HostRoomRetry {
    delay_seconds: u32,
    maximum_seconds: u32,
    last_room_response: Option<Instant>,
}
impl Default for HostRoomRetry {
    fn default() -> Self {
        Self {
            delay_seconds: 0,
            maximum_seconds: farwing_protocol::timing::MAX_RECONNECT_DELAY_S,
            last_room_response: None,
        }
    }
}
impl HostRoomRetry {
    fn room_response(&mut self, room: &crate::account::api::RealtimeTicket) {
        if room.max_reconnect_delay_s > 0 {
            self.maximum_seconds = room.max_reconnect_delay_s;
        }
        self.last_room_response = Some(Instant::now());
    }
    fn lost_room(&mut self) -> Duration {
        if self
            .last_room_response
            .is_none_or(|time| time.elapsed() >= Duration::from_secs(30))
        {
            self.delay_seconds = rand::random_range(1..=5);
        } else {
            self.delay_seconds = self
                .delay_seconds
                .saturating_mul(2)
                .min(self.maximum_seconds);
        }
        self.delay_seconds = self.delay_seconds.max(1);
        Duration::from_secs(self.delay_seconds as u64)
    }
    fn failed_backoff_request(&mut self) -> Duration {
        self.delay_seconds = self
            .delay_seconds
            .saturating_mul(2)
            .min(self.maximum_seconds)
            .max(1);
        Duration::from_secs(self.delay_seconds as u64)
    }
}

async fn run_remote(
    client: HostClient,
    events: Sender<PresenceEvent>,
    cancel: CancellationToken,
) -> Result<()> {
    use crate::account::auth::{KeyringSessionStore, SessionStore};
    use crate::platform::windows::host_service::resident::{self, Reply, Request};
    // Reuse the credential handle, not its contents: each check must still see
    // logout/account changes and portable/resident storage transitions.
    let sessions = KeyringSessionStore::new()?;
    let mut device_cursor = None;
    // The service's notification pushes, as last seen.
    let mut notices = None::<u64>;
    // The background host answers a watch when its state changes from the
    // revision we have (at the latest after `resident::WATCH`); actions go
    // to it as they are queued.
    let mut revision = None::<u64>;
    let actions = client.host.remote_actions();
    let mut check = tokio::time::interval(Duration::from_secs(1));
    check.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut watch: Option<std::pin::Pin<Box<dyn std::future::Future<Output = Result<Reply>> + Send>>> = None;
    // After a failed watch, the next one waits a moment.
    let mut next_watch = Instant::now();
    while !cancel.is_cancelled() {
        let maintaining = crate::platform::windows::components::maintaining();
        if watch.is_none() && !maintaining && Instant::now() >= next_watch {
            watch = Some(Box::pin(resident::request(Request::Snapshot {
                ui: client.host.assistance.ui_present(),
                device_cursor: device_cursor.clone(),
                language: Some(crate::i18n::language().tag().to_owned()),
                seen: revision,
            })));
        }
        let reply = tokio::select! {
            _ = cancel.cancelled() => break,
            _ = actions.notified() => None,
            _ = check.tick() => None,
            reply = async { watch.as_mut().expect("guarded").await }, if watch.is_some() => {
                watch = None;
                Some(reply)
            }
        };
        if maintaining {
            continue;
        }
        let saved = sessions.load()?.map(|s| s.generation());
        if !client.matches_saved(saved.as_deref().unwrap_or_default()) {
            client.retire();
            break;
        }
        // Several participants may be disconnected at once.
        while let Some(action) = client.host.take_remote_action() {
            let action = match action {
                crate::features::host::RemoteAction::Retry => Request::Retry {
                    account: client.generation(),
                },
                crate::features::host::RemoteAction::Disconnect(session) => Request::Disconnect {
                    account: client.generation(),
                    session,
                },
                crate::features::host::RemoteAction::BlockControl(session, blocked) => {
                    Request::BlockControl {
                        account: client.generation(),
                        session,
                        blocked,
                    }
                }
            };
            if let Err(e) = resident::request(action).await {
                let _ = events.send(PresenceEvent::Warning((crate::i18n::t().session.warning_background_failed)(&format!("{e:#}"))));
            }
        }
        let Some(reply) = reply else {
            continue;
        };
        match reply {
            Ok(Reply::Snapshot(mut snapshot)) if snapshot.account == client.generation() => {
                revision = Some(snapshot.revision);
                super::reconnect::set_remote(snapshot.retry_at_ms.map(|at| {
                    let left = (at - chrono::Utc::now().timestamp_millis()).max(0) as u64;
                    Instant::now() + Duration::from_millis(left)
                }));
                crate::account::reporting::replace(snapshot.publication.clone());
                if notices.is_some_and(|seen| seen != snapshot.notices) {
                    crate::account::notices::pushed();
                }
                notices = Some(snapshot.notices);
                crate::transport::direct::managed::sync_local_settings(&snapshot.direct.settings);
                crate::transport::direct::managed::remember(snapshot.direct.clone());
                let _ = events.send(PresenceEvent::State(snapshot.online.clone()));
                if let Some(batch) = snapshot.device_changes.take() {
                    if batch.resync || !batch.changes.is_empty() {
                        note_device_push();
                    }
                    if batch.resync {
                        let _ = events.send(PresenceEvent::DevicesResync);
                    }
                    for change in batch.changes {
                        let _ = events.send(PresenceEvent::DeviceChanged(change));
                    }
                    device_cursor = Some(batch.cursor);
                }
                client.host.apply_remote(*snapshot).await;
            }
            Ok(Reply::Snapshot(_)) => {
                revision = None;
                next_watch = Instant::now() + Duration::from_millis(500);
                super::reconnect::set_remote(None);
                device_cursor = None;
                let _ = events.send(PresenceEvent::State(PresenceState::Connecting));
            }
            Ok(_) => (),
            Err(e) => {
                revision = None;
                next_watch = Instant::now() + Duration::from_millis(500);
                // Installation intentionally replaces the endpoint; uninstall
                // hands ownership back to portable presence. Do not publish a
                // stale RPC failure after that handoff.
                if crate::platform::windows::components::maintaining() || !resident::managed() {
                    continue;
                }
                client.host.remote_failed(format!("{e:#}"));
                let _ = events.send(PresenceEvent::State(PresenceState::Offline));
            }
        }
    }
    Ok(())
}
