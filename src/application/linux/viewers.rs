use super::viewer::{ViewerWindow, viewport_builder};
use crate::connection_progress::{ConnectionProgress, ConnectionProgressState, RetryNow};
use crate::i18n::t;
use crate::session::controller::{
    ConnectionProgressReporter, ControllerConnection, RECOVERY_LIMIT, Recovery, VisitRecord,
    new_visit, recovery, recovery_backoff,
};
use anyhow::Result;
use egui::{ViewportCommand, ViewportId};
use std::time::Instant;
use std::{
    collections::HashMap,
    future::Future,
    sync::{Arc, Mutex, MutexGuard},
};
use tokio_util::sync::CancellationToken;

#[derive(Default)]
pub(super) struct Windows {
    windows: HashMap<ViewportId, Arc<Mutex<ViewerWindow>>>,
    next: u64,
}

fn window(state: &Mutex<ViewerWindow>) -> MutexGuard<'_, ViewerWindow> {
    state
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

impl Windows {
    pub(super) fn open<F, FutureConnection>(
        &mut self,
        ctx: &egui::Context,
        runtime: &tokio::runtime::Handle,
        target: Option<&str>,
        title: String,
        connect: F,
    ) where
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
        let viewport = if let Some(target) = target {
            ViewportId::from_hash_of(("account-viewer", target))
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
            return;
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
        let ctx = ctx.clone();
        runtime.spawn(async move {
            let progress: ConnectionProgressReporter = {
                let ctx = ctx.clone();
                let state = Arc::clone(&state);
                Arc::new(move |progress| {
                    window(&state).progress = progress;
                    ctx.request_repaint_of(viewport);
                })
            };
            let mut recovering = None;
            let mut attempts = 0;
            let (monitor, samples) = tokio::sync::watch::channel(None);
            let record = Arc::new(VisitRecord::start(samples));
            let visit = new_visit();
            let result = loop {
                let connecting = connect(cancel.clone(), Arc::clone(&progress), recovering.is_some(), visit.clone(), Arc::clone(&record));
                let attempt = async {
                let (mut connection, alias) = connecting.await?;
                let ended = connection.linux_session_ended();
                tokio::pin!(ended);
                let initial = tokio::select! {
                    _ = cancel.cancelled() => None,
                    result = &mut ended => Some(Err(result.err().unwrap_or_else(|| anyhow::anyhow!("{}", t().viewer.connection_ended)))),
                    result = connection.start_linux_viewer(ctx.clone(), viewport) => Some(result),
                };
                let playback = match initial {
                    Some(Ok(playback)) => playback,
                    Some(Err(error)) => { let _ = connection.close().await; return Err(error); }
                    None => return connection.close().await,
                };
                monitor.send_replace(Some(connection.performance_monitor()));
                record.connected(&connection.stream_control_handle());
                recovering = None;
                attempts = 0;
                {
                    let mut state = window(&state);
                    state.alias = alias;
                    state.playback = Some(playback);
                    state.progress = ConnectionProgress::ready("");
                }
                ctx.request_repaint_of(viewport);
                let control = connection.stream_control_handle();
                let mut modes = control.audio_mode_requests();
                let result = loop {
                    tokio::select! {
                        _ = cancel.cancelled() => break Ok(()),
                        result = &mut ended => break result,
                        changed = modes.changed() => {
                            if changed.is_err() { break Ok(()); }
                            let audio_only = *modes.borrow_and_update();
                            window(&state).playback = None;
                            progress(ConnectionProgress::working(1, if audio_only { t().session.progress_switching_to_audio } else { t().session.progress_restoring_picture }, t().session.progress_switching_media));
                            let playback = tokio::select! {
                                _ = cancel.cancelled() => break Ok(()),
                                result = connection.switch_linux_audio(audio_only, ctx.clone(), viewport) => match result {
                                    Ok(playback) => playback,
                                    Err(error) => break Err(error),
                                },
                            };
                            window(&state).playback = Some(playback);
                            ctx.request_repaint_of(viewport);
                        }
                    }
                };
                result.and(connection.close().await)
                }.await;
                monitor.send_replace(None);
                let Err(error) = attempt else { break attempt };
                if cancel.is_cancelled()
                    || recovery(&error) == Recovery::Fatal
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
            }
        });
    }

    pub(super) fn show(&mut self, ctx: &egui::Context) {
        let parent = ctx.viewport_id();
        self.windows
            .retain(|_, state| !window(state).cancel.is_cancelled());
        for (&id, state) in &self.windows {
            let title = format!("{}{}", crate::VIEWER_TITLE_PREFIX, window(state).alias);
            let state = Arc::clone(state);
            ctx.show_viewport_deferred(id, viewport_builder(title), move |ui, _| {
                window(&state).show(ui, parent)
            });
        }
    }

    pub(super) fn close_all(&mut self) {
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
