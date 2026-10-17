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
use tokio::{sync::oneshot, task::JoinHandle};
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

pub(crate) struct ActivePresence {
    cancel: CancellationToken,
    _cancel_on_drop: tokio_util::sync::DropGuard,
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
                if crate::platform::host_service::resident::managed() {
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
                        let _ = events.send(PresenceEvent::Warning(format!("清理旧账号凭据失败：{error:#}")));
                    }
                    let _ = events.send(PresenceEvent::AccountEnded);
                    run.await
                },
                result = &mut run => result,
            }
        });
        Self {
            _cancel_on_drop: cancel.clone().drop_guard(),
            cancel,
            task,
            events: receiver,
        }
    }

    pub(crate) async fn close(self) {
        self.cancel.cancel();
        if let Err(error) = self.task.await {
            tracing::warn!(%error, "host presence task ended unexpectedly");
        }
    }
}

async fn run_presence(
    client: HostClient,
    events: Sender<PresenceEvent>,
    task_cancel: CancellationToken,
) -> Result<()> {
    // Reservation lifetime (not mutex thread ownership) spans the whole room.
    // During installation the new background waits for the portable room's
    // normal close; two account owners must never kick each other off the server.
    let _owner = loop {
        if let Some(reservation) =
            crate::platform::host_service::reserve_presence(&client.presence_key())?
        {
            break reservation;
        }
        tokio::select! { _ = task_cancel.cancelled() => return Ok(()), _ = tokio::time::sleep(Duration::from_millis(100)) => () }
    };
    let _assistance = crate::features::host::assist::Running::start(client.clone());
    let _wol = crate::features::host::wol::Running::start(client.clone());
    let _wol_setup = crate::features::host::wol::setup::Running::start(client.clone());
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
        // 39C5B0: a device that has never been controlled sends -1.
        let request = tokio::select! {
            _ = task_cancel.cancelled() => return Ok(()),
            result = client.create_host_room(client.host.last_controlled_interval()) => result,
        };
        let room = match request {
            Ok(room) => {
                policy.room_response(&room);
                room
            }
            Err(error) => {
                if error
                    .downcast_ref::<ApiFailure>()
                    .is_some_and(|failure| failure.code == 1120)
                {
                    return Err(error);
                }
                let delay = if backoff {
                    policy.failed_backoff_request()
                } else {
                    initial_requests += 1;
                    if initial_requests >= 3 {
                        return Err(error);
                    }
                    Duration::from_millis(500)
                };
                let _ = events.send(PresenceEvent::Warning(format!(
                    "在线房间请求失败：{error:#}；{} ms 后重试",
                    delay.as_millis()
                )));
                tokio::select! { _ = task_cancel.cancelled() => return Ok(()), _ = tokio::time::sleep(delay) => {} }
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
            if push_client.host.assistance.push(push, &push_cancel) {
                return;
            }
            if push_client.is_guest() {
                return;
            }
            if push.get("type").and_then(|v| v.as_str()) == Some("wake_on_lan") {
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
                // Server 398A50 -> 3B5540: exact current-device match.
                // No credential I/O on the Socket.IO input worker.
                tracing::info!("current virtual device was unbound; retiring account generation");
                push_client.retire();
            } else {
                let _ = push_events.send(PresenceEvent::DeviceChanged(change));
            }
        });
        let session =
            SignalSession::connect_observed(room, SignalRole::Host, &room_cancel, Some(observer))
                .await;
        let result = match session {
            Ok(session) => {
                client.host.wol.online(true);
                let _ = events.send(PresenceEvent::State(PresenceState::Online));
                let mut socket_state = session.socket_state();
                let mut state_open = true;
                let (shutdown, shutdown_receiver) = oneshot::channel();
                let alive = session.keep_alive_host(shutdown_receiver, client.clone());
                tokio::pin!(alive);
                loop {
                    tokio::select! {
                        result = &mut alive => break result,
                        _ = task_cancel.cancelled() => { let _ = shutdown.send(()); break alive.await; }
                        state = socket_state.changed(), if state_open => {
                            if state.is_err() { state_open = false; continue; }
                            let socket=*socket_state.borrow_and_update();
                            client.host.wol.online(socket==SocketState::Connected);
                            let presence = match socket {
                                SocketState::Connected => PresenceState::Online,
                                SocketState::Connecting | SocketState::Reconnecting => PresenceState::Reconnecting,
                                SocketState::Closed => PresenceState::Offline,
                            };
                            let _ = events.send(PresenceEvent::State(presence));
                        }
                    }
                }
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
            Some(SignalFailure::Kicked)
        ) {
            return result;
        }
        let delay = policy.lost_room();
        backoff = true;
        initial_requests = 0;
        if let Err(error) = result {
            let _ = events.send(PresenceEvent::Warning(format!(
                "在线房间失联：{error:#}；{} 秒后重建",
                delay.as_secs()
            )));
        }
        tokio::select! { _ = task_cancel.cancelled() => return Ok(()), _ = tokio::time::sleep(delay) => {} }
    }
}

// GameViewerServer 2D5DE0, 3CA500..3CA680 and 3BB060/3BB490.
// HTTP-response and socket-open time are distinct: >=30s starts a random
// 1..5s backoff; repeated short failures double delay up to the server limit.
struct HostRoomRetry {
    delay_seconds: u32,
    maximum_seconds: u32,
    last_room_response: Option<Instant>,
}
impl Default for HostRoomRetry {
    fn default() -> Self {
        Self {
            delay_seconds: 0,
            maximum_seconds: 300,
            last_room_response: None,
        }
    }
}
impl HostRoomRetry {
    fn room_response(&mut self, room: &crate::account::api::RoomSession) {
        if room.max_reconnect_delta > 0 {
            self.maximum_seconds = room.max_reconnect_delta as u32;
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
    use crate::platform::host_service::resident::{self, Reply, Request};
    // Reuse the credential handle, not its contents: each poll must still see
    // logout/account changes and portable/resident storage transitions.
    let sessions = KeyringSessionStore::new()?;
    let mut device_cursor = None;
    let mut tick = tokio::time::interval(Duration::from_millis(500));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    while !cancel.is_cancelled() {
        tokio::select! { _ = cancel.cancelled() => break, _ = tick.tick() => () }
        if crate::platform::host_service::maintaining() {
            continue;
        }
        let saved = sessions.load()?.map(|s| s.generation());
        if !client.matches_saved(saved.as_deref().unwrap_or_default()) {
            client.retire();
            break;
        }
        if let Some(action) = client.host.take_remote_action() {
            let action = match action {
                crate::features::host::RemoteAction::Retry => Request::Retry {
                    account: client.generation(),
                },
                crate::features::host::RemoteAction::Disconnect(session) => Request::Disconnect {
                    account: client.generation(),
                    session,
                },
            };
            if let Err(e) = resident::request(action).await {
                let _ = events.send(PresenceEvent::Warning(format!("后台操作失败：{e:#}")));
            }
        }
        match resident::request(Request::Snapshot {
            ui: client.host.assistance.ui_present(),
            device_cursor: device_cursor.clone(),
        })
        .await
        {
            Ok(Reply::Snapshot(mut snapshot)) if snapshot.account == client.generation() => {
                crate::account::reporting::replace(snapshot.publication.clone());
                let _ = events.send(PresenceEvent::State(snapshot.online.clone()));
                if let Some(batch) = snapshot.device_changes.take() {
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
                device_cursor = None;
                let _ = events.send(PresenceEvent::State(PresenceState::Connecting));
            }
            Ok(_) => (),
            Err(e) => {
                // Installation intentionally replaces the endpoint; uninstall
                // hands ownership back to portable presence. Do not publish a
                // stale RPC failure after that handoff.
                if crate::platform::host_service::maintaining() || !resident::managed() {
                    continue;
                }
                client.host.remote_failed(format!("{e:#}"));
                let _ = events.send(PresenceEvent::State(PresenceState::Offline));
            }
        }
    }
    Ok(())
}
