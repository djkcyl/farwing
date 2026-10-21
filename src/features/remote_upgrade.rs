//! A viewer's handling of a host that updates Farwing locally: the host
//! announces it with `HOST_EVENT{UPDATING}`; the viewer shows the progress,
//! waits for the host to come back and reconnects. Updates are never
//! triggered remotely.
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;
use winit::window::WindowId;

use crate::ui::{controls, theme};

// Fallback for a connection that never ends after the announcement (for
// example a cancelled installation). A real disconnect moves directly to
// readiness checks and reconnect.
const DISCONNECT_GRACE: Duration = Duration::from_secs(21);

pub(crate) const UPDATE_PROBE_INTERVAL: Duration = Duration::from_secs(1);

/// Device-list online state is a hint, not proof that a new media peer exists.
/// Prefer an observed unavailable -> ready transition. When a fast restart's
/// offline interval was missed, confirm readiness across two spaced snapshots.
#[derive(Default)]
pub(crate) struct UpdateReadiness {
    unavailable_seen: bool,
    ready_since: Option<Instant>,
}
impl UpdateReadiness {
    pub(crate) fn observe(&mut self, ready: bool, now: Instant) -> bool {
        if !ready {
            self.unavailable_seen = true;
            self.ready_since = None;
            return false;
        }
        if self.unavailable_seen {
            return true;
        }
        now.saturating_duration_since(*self.ready_since.get_or_insert(now)) >= UPDATE_PROBE_INTERVAL
    }
}

#[derive(Default)]
struct State {
    started: Option<Instant>,
}

struct Inner {
    state: Mutex<State>,
    changed: Notify,
    cancel: CancellationToken,
}

#[derive(Clone)]
pub(crate) struct RemoteUpgrade(Arc<Inner>);

impl RemoteUpgrade {
    pub(crate) fn new(cancel: &CancellationToken) -> Self {
        Self(Arc::new(Inner {
            state: Mutex::new(State::default()),
            changed: Notify::new(),
            cancel: cancel.child_token(),
        }))
    }

    fn state(&self) -> std::sync::MutexGuard<'_, State> {
        self.0.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub(crate) fn retire(&self) {
        self.0.cancel.cancel();
    }

    pub(crate) fn started_at(&self) -> Option<Instant> {
        self.state().started
    }

    pub(crate) fn owns_input(&self, _window: WindowId) -> bool {
        self.state().started.is_some()
    }

    /// `HOST_EVENT{UPDATING}`: the host installs an update and restarts.
    pub(crate) fn host_updating(&self) {
        if self.0.cancel.is_cancelled() {
            return;
        }
        let mut state = self.state();
        if state.started.is_none() {
            tracing::info!("host update announced; waiting for the host to come back");
        }
        state.started.get_or_insert_with(Instant::now);
        self.0.changed.notify_waiters();
    }

    pub(crate) async fn wait_for_disconnect_grace(&self) {
        loop {
            let notified = self.0.changed.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            let started = self.state().started;
            if let Some(started) = started {
                tokio::time::sleep(DISCONNECT_GRACE.saturating_sub(started.elapsed())).await;
                return;
            }
            notified.await;
        }
    }

    pub(crate) fn show(&self, ctx: &egui::Context) {
        if let Some(started) = self.state().started {
            if progress_prompt(ctx, started) {
                ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            }
            ctx.request_repaint_after(Duration::from_millis(200));
        }
    }
}

pub(crate) fn progress_prompt(ctx: &egui::Context, started: Instant) -> bool {
    let mut close = false;
    egui::Modal::new(egui::Id::new("remote-upgrade-progress"))
        .frame(controls::dialog_frame())
        .show(ctx, |ui| {
            ui.set_width(
                theme::REMOTE_UPGRADE_WIDTH.min((ctx.content_rect().width() - 80.0).max(260.0)),
            );
            controls::dialog_header(ui, "被控端正在更新", controls::DialogIcon::Waiting, false);
            ui.add_space(16.0);
            ui.horizontal(|ui| {
                ui.spinner();
                ui.label(format!("已等待 {} 秒", started.elapsed().as_secs()));
            });
            close = controls::update_actions(ui, None, Some(("关闭观看", ""))).1;
        });
    close
}
