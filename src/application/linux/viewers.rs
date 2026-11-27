use super::viewer::{ViewerWindow, viewport_builder};
use super::{Event, device_switch};
use crate::account::api::DeviceList;
use crate::account::client::AuthenticatedClient;
use crate::connection_progress::{ConnectionProgress, ConnectionProgressState, RetryNow};
use crate::features::remote_upgrade::{
    UPDATE_WAIT_LIMIT, progress_prompt, wait_for_update_readiness,
};
use crate::i18n::t;
use crate::session::controller::{
    ConnectionProgressReporter, ControllerConnection, RECOVERY_LIMIT, Recovery, VisitRecord,
    new_visit, recovery, recovery_backoff, recovery_for,
};
use anyhow::{Context, Result, anyhow};
use egui::{ViewportCommand, ViewportId};
use std::time::{Duration, Instant};
use std::{
    collections::HashMap,
    sync::{Arc, Mutex, MutexGuard},
    sync::mpsc,
};
use tokio_util::sync::CancellationToken;

#[derive(Default)]
pub(super) struct Windows {
    windows: HashMap<ViewportId, Arc<Mutex<ViewerWindow>>>,
    switches: HashMap<ViewportId, ViewportId>,
    next: u64,
}

fn window(state: &Mutex<ViewerWindow>) -> MutexGuard<'_, ViewerWindow> {
    state
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

pub(super) fn account_viewport(target: &str) -> ViewportId {
    ViewportId::from_hash_of(("account-viewer", target))
}

impl Windows {
    pub(super) fn open<F, FutureConnection>(
        &mut self,
        ctx: &egui::Context,
        runtime: &tokio::runtime::Handle,
        account_target: Option<(&str, Arc<AuthenticatedClient>)>,
        title: String,
        connect: F,
    ) -> ViewportId
    where
        F: Fn(
                CancellationToken,
                ConnectionProgressReporter,
                bool,
                String,
                Arc<VisitRecord>,
            ) -> FutureConnection
            + Send
            + Sync
            + 'static,
        FutureConnection: Future<Output = Result<(ControllerConnection, String)>> + Send,
    {
        let viewport = if let Some((target, _)) = &account_target {
            account_viewport(target)
        } else {
            self.next += 1;
            ViewportId::from_hash_of(("viewer", self.next))
        };
        if self.windows.get(&viewport).is_some_and(|state| {
            let state = window(state);
            !state.cancel.is_cancelled()
                && !matches!(state.progress.state, ConnectionProgressState::Failed)
        }) {
            ctx.send_viewport_cmd_to(viewport, ViewportCommand::Focus);
            return viewport;
        }
        let cancel = CancellationToken::new();
        let state = Arc::new(Mutex::new(ViewerWindow {
            alias: title,
            cancel: cancel.clone(),
            playback: None,
            progress: ConnectionProgress::working(
                1,
                t().viewer.preparing_connection,
                t().viewer.reading_local_settings,
            ),
        }));
        self.windows.insert(viewport, Arc::clone(&state));
        let account_target = account_target.map(|(id, client)| (id.to_owned(), client));
        let ctx = ctx.clone();
        runtime.spawn(async move {
            let progress: ConnectionProgressReporter = {
                let ctx = ctx.clone();
                let state = Arc::clone(&state);
                Arc::new(move |progress| {
                    let mut state = window(&state);
                    // Keep one update prompt through the new room's handshake and media startup.
                    if matches!(state.progress.state, ConnectionProgressState::Updating(_))
                        && !matches!(progress.state, ConnectionProgressState::Updating(_))
                    {
                        return;
                    }
                    state.progress = progress;
                    drop(state);
                    ctx.request_repaint_of(viewport);
                })
            };
            let mut recovering = None;
            let mut update_deadline = None;
            let mut attempts = 0;
            let (monitor, samples) = tokio::sync::watch::channel(None);
            let record = Arc::new(VisitRecord::start(samples));
            let visit = new_visit();
            let result = loop {
                if let Some(deadline) = update_deadline {
                    if tokio::time::Instant::now() >= deadline {
                        break Err(anyhow!("{}", t().session.error_update_wait_timeout));
                    }
                    if let Some((id, client)) = &account_target {
                        let ready = wait_for_update_readiness(
                            deadline,
                            &cancel,
                            || async {
                                let devices = client.list_devices().await?;
                                let device = devices
                                    .all()
                                    .find(|device| &device.device_id == id)
                                    .context(t().session.error_updated_device_missing)?;
                                device.validated_device_id()?;
                                Ok(device.is_connected()
                                    && device.controlled_support
                                    && device.controllable)
                            },
                            || {},
                        )
                        .await;
                        if let Err(error) = ready {
                            break if cancel.is_cancelled() {
                                Ok(())
                            } else {
                                Err(error)
                            };
                        }
                    }
                }
                let mut host_updating = false;
                let attempt = async {
                    let (mut connection, alias) = connect(
                        cancel.clone(),
                        Arc::clone(&progress),
                        recovering.is_some() || update_deadline.is_some(),
                        visit.clone(),
                        Arc::clone(&record),
                    )
                    .await?;
                    let control = connection.stream_control_handle();
                    let upgrade = control.remote_upgrade();
                    let live = async {
                        let ended = connection.linux_session_ended();
                        tokio::pin!(ended);
                        let playback = tokio::select! {
                            result = &mut ended => Err(result.err().unwrap_or_else(|| {
                                anyhow!("{}", t().viewer.connection_ended)
                            })),
                            result = connection.start_linux_viewer(ctx.clone(), viewport) => result,
                        }?;
                        monitor.send_replace(Some(connection.performance_monitor()));
                        record.connected(&control);
                        if update_deadline.take().is_some() {
                            tracing::info!("media resumed after controlled update");
                        }
                        recovering = None;
                        attempts = 0;
                        {
                            let mut state = window(&state);
                            state.alias = alias.clone();
                            state.playback = Some(playback);
                            state.progress = ConnectionProgress::ready("");
                        }
                        ctx.request_repaint_of(viewport);
                        let mut modes = control.audio_mode_requests();
                        loop {
                            tokio::select! {
                                result = &mut ended => break result,
                                changed = modes.changed() => {
                                    if changed.is_err() { break Ok(()); }
                                    let audio_only = *modes.borrow_and_update();
                                    window(&state).playback = None;
                                    progress(ConnectionProgress::working(
                                        1,
                                        if audio_only {
                                            t().session.progress_switching_to_audio
                                        } else {
                                            t().session.progress_restoring_picture
                                        },
                                        t().session.progress_switching_media,
                                    ));
                                    let playback = connection
                                        .switch_linux_audio(audio_only, ctx.clone(), viewport)
                                        .await?;
                                    window(&state).playback = Some(playback);
                                    ctx.request_repaint_of(viewport);
                                }
                            }
                        }
                    };
                    let result = tokio::select! {
                        biased;
                        _ = cancel.cancelled() => Ok(()),
                        _ = upgrade.wait_for_announcement() => Ok(()),
                        result = live => result,
                    };
                    if let Some(started) = upgrade.started_at().filter(|_| !cancel.is_cancelled()) {
                        {
                            let mut state = window(&state);
                            state.alias = alias;
                            state.playback = None;
                        }
                        progress(ConnectionProgress::updating(started));
                        tokio::select! {
                            biased;
                            _ = cancel.cancelled() => {},
                            _ = connection.linux_session_ended() => {},
                            _ = upgrade.wait_for_disconnect_grace() => {},
                        }
                        if !cancel.is_cancelled() {
                            // The viewer may be the only lease, or a file/port
                            // owner may retain the peer after a cancelled install.
                            // Either way this announcement has been acted on.
                            upgrade.clear_announcement();
                            host_updating = true;
                        }
                    }
                    result.and(connection.close().await)
                }
                .await;
                monitor.send_replace(None);
                if cancel.is_cancelled() {
                    break Ok(());
                }
                if host_updating {
                    record.reconnecting();
                    update_deadline = Some(tokio::time::Instant::now() + UPDATE_WAIT_LIMIT);
                    recovering = None;
                    attempts = 0;
                    continue;
                }
                let Err(error) = attempt else { break attempt };
                if let Some(deadline) = update_deadline {
                    // Short-number hosts may temporarily disappear from lookup;
                    // direct hosts report DeviceOffline until their listener is back.
                    if recovery_for(&error, account_target.is_none()) == Recovery::Fatal {
                        break Err(error);
                    }
                    attempts += 1;
                    let retry_at = tokio::time::Instant::now() + recovery_backoff(attempts);
                    tokio::select! {
                        _ = cancel.cancelled() => break Ok(()),
                        _ = tokio::time::sleep_until(deadline.min(retry_at)) => {},
                    }
                    continue;
                }
                if recovery(&error) == Recovery::Fatal
                    || (window(&state).playback.is_none() && recovering.is_none())
                {
                    break Err(error);
                }
                if recovering.is_none() {
                    record.reconnecting();
                }
                let since = *recovering.get_or_insert_with(Instant::now);
                let remaining = RECOVERY_LIMIT.saturating_sub(since.elapsed());
                if remaining.is_zero() {
                    break Err(error);
                }
                attempts += 1;
                window(&state).playback = None;
                let retry = RetryNow::default();
                progress(ConnectionProgress::working(
                    4, t().session.progress_reconnecting, format!("{error:#}"),
                ).with_retry(&retry));
                tokio::select! {
                    _ = cancel.cancelled() => break Ok(()),
                    _ = retry.requested() => {},
                    _ = tokio::time::sleep(recovery_backoff(attempts).min(remaining)) => {},
                }
            };
            record.finish().await;
            if !cancel.is_cancelled() {
                let mut state = window(&state);
                state.playback = None;
                state.progress = match result {
                    Ok(()) => ConnectionProgress::ended(t().viewer.connection_ended),
                    Err(error) => ConnectionProgress::from_error(&error),
                };
                ctx.request_repaint_of(viewport);
                ctx.request_repaint_of(ViewportId::ROOT);
            }
        });
        viewport
    }

    pub(super) fn contains(&self, viewport: ViewportId) -> bool {
        self.windows.get(&viewport).is_some_and(|state| !window(state).cancel.is_cancelled())
    }

    pub(super) fn switch_when_ready(&mut self, from: ViewportId, to: ViewportId) {
        if from != to {
            self.switches.insert(from, to);
        }
    }

    pub(super) fn show(
        &mut self,
        ctx: &egui::Context,
        devices: Option<&DeviceList>,
        events: &mpsc::Sender<Event>,
        busy: bool,
    ) {
        let parent = ctx.viewport_id();
        self.switches.retain(|from, to| {
            let Some(destination) = self.windows.get(to) else { return false };
            let destination = window(destination);
            if destination.cancel.is_cancelled()
                || matches!(destination.progress.state, ConnectionProgressState::Failed)
            {
                return false;
            }
            if destination.is_presenting() {
                if let Some(source) = self.windows.get(from) {
                    window(source).cancel.cancel();
                }
                ctx.send_viewport_cmd_to(*to, ViewportCommand::Focus);
                return false;
            }
            self.windows.get(from).is_some_and(|source| !window(source).cancel.is_cancelled())
        });
        self.windows
            .retain(|_, state| !window(state).cancel.is_cancelled());
        let devices = devices.cloned().map(Arc::new);
        for (&id, state) in &self.windows {
            let title = format!("{}{}", crate::VIEWER_TITLE_PREFIX, window(state).alias);
            let state = Arc::clone(state);
            let devices = devices.clone();
            let events = events.clone();
            ctx.show_viewport_deferred(id, viewport_builder(title), move |ui, _| {
                let mut state = window(&state);
                state.show(ui, parent, |ui| {
                    if let Some(devices) = &devices {
                        device_switch::show(ui, devices, &events, parent, id, busy);
                    }
                });
                if !state.cancel.is_cancelled()
                    && let ConnectionProgressState::Updating(started) = state.progress.state
                {
                    let ctx = ui.ctx();
                    if progress_prompt(ctx, started) {
                        state.cancel.cancel();
                        ctx.request_repaint_of(parent);
                    } else {
                        ctx.request_repaint_after(Duration::from_millis(200));
                    }
                }
            });
        }
    }

    pub(super) fn close_all(&mut self) {
        self.switches.clear();
        for state in self.windows.drain().map(|(_, state)| state) {
            window(&state).cancel.cancel();
        }
    }
}

impl Drop for Windows {
    fn drop(&mut self) {
        self.close_all();
    }
}
