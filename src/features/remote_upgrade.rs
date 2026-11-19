//! Update notification and reconnect for an authenticated Windows target.
//! Local OpenUUYC installation shares the official ReportError(-6) lifecycle.
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use egui::RichText;
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;
use winit::window::WindowId;

use crate::account::client::AuthenticatedClient;
use crate::account::feature_ability::{Feature, FeaturePolicy};
use crate::features::stream_control::StreamControlHandle;
use crate::ui::{controls, theme};

// The official widget always waits 20..0. Here that duration is only a fallback
// for an old connection that never ends (for example a cancelled installation).
// A real remote disconnect moves directly to readiness checks and reconnect.
const DISCONNECT_GRACE: Duration = Duration::from_secs(21);
const PREPARED_NOTICE: Duration = Duration::from_secs(10);

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
    prompt: Option<(WindowId, &'static str)>,
    posting: bool,
    notice: Option<(WindowId, String)>,
    started: Option<Instant>,
    prepared: Option<Instant>,
}

struct Inner {
    client: Arc<AuthenticatedClient>,
    device_id: String,
    alias: String,
    version: String,
    policy: FeaturePolicy,
    state: Mutex<State>,
    changed: Notify,
    runtime: tokio::runtime::Handle,
    cancel: CancellationToken,
}

#[derive(Clone)]
pub(crate) struct RemoteUpgrade(Arc<Inner>);

impl RemoteUpgrade {
    pub(crate) fn new(
        client: Arc<AuthenticatedClient>,
        device_id: String,
        alias: String,
        version: String,
        platform: i32,
        cancel: &CancellationToken,
    ) -> Self {
        let policy = client.feature_catalog().policy(platform, &version);
        Self(Arc::new(Inner {
            client,
            device_id,
            alias,
            version,
            policy,
            state: Mutex::new(State::default()),
            changed: Notify::new(),
            runtime: tokio::runtime::Handle::current(),
            cancel: cancel.child_token(),
        }))
    }

    fn state(&self) -> std::sync::MutexGuard<'_, State> {
        self.0.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub(crate) fn prompt(&self, window: WindowId, feature: &'static str) {
        let mut state = self.state();
        if !state.posting && state.started.is_none() && !self.0.cancel.is_cancelled() {
            state.prompt = Some((window, feature));
        }
    }

    pub(crate) fn retire(&self) {
        self.0.cancel.cancel();
        self.state().prompt = None;
    }

    pub(crate) fn started_at(&self) -> Option<Instant> {
        self.state().started
    }

    pub(crate) fn owns_input(&self, window: WindowId) -> bool {
        let state = self.state();
        state.started.is_some()
            || state.prompt.is_some_and(|(owner, _)| owner == window)
            || state
                .notice
                .as_ref()
                .is_some_and(|(owner, _)| *owner == window)
    }

    pub(crate) fn receive(&self, code: i32) {
        if self.0.cancel.is_cancelled() {
            return;
        }
        let mut state = self.state();
        match code {
            -6 => {
                if state.started.is_none() {
                    tracing::info!("remote update start received; waiting for update reconnect");
                }
                state.started.get_or_insert_with(Instant::now);
                state.prompt = None;
                state.notice = None;
                state.prepared = None;
                self.0.changed.notify_waiters();
            }
            -8 if state.started.is_none() => state.prepared = Some(Instant::now()),
            _ => {}
        }
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

    fn submit(&self, ctx: &egui::Context, window: WindowId, immediate: bool) {
        {
            let mut state = self.state();
            if state.posting || state.started.is_some() || self.0.cancel.is_cancelled() {
                return;
            }
            state.posting = true;
            state.prompt = None;
        }
        let this = self.clone();
        let ctx = ctx.clone();
        self.0.runtime.spawn(async move {
            let result = tokio::select! {
                biased;
                _ = this.0.cancel.cancelled() => return,
                result = this.0.client.update_owned_device(&this.0.device_id, immediate) => result,
            };
            let mut state = this.state();
            state.posting = false;
            if state.started.is_none() {
                match result {
                    Ok(()) => tracing::info!(immediate, "remote update request accepted"),
                    Err(error) => state.notice = Some((window, format!("{error:#}"))),
                }
            }
            // HTTP acceptance is not an installation acknowledgement. Only
            // ReportError(-6) starts the update overlay and reconnect deadline.
            ctx.request_repaint();
        });
    }

    pub(crate) fn show(
        &self,
        ctx: &egui::Context,
        window: WindowId,
        control: &StreamControlHandle,
    ) {
        let (prompt, notice, started, prepared) = {
            let state = self.state();
            (
                state.prompt,
                state.notice.clone(),
                state.started,
                state.prepared,
            )
        };
        if let Some((owner, feature)) = prompt.filter(|(owner, _)| *owner == window) {
            let supported = self.0.policy.supports(Feature::ControlledUpdate);
            let (chosen, dismiss) =
                version_prompt(ctx, &self.0.alias, &self.0.version, feature, supported);
            if let Some(immediate) = chosen {
                control.mouse().disable();
                self.submit(ctx, owner, immediate);
            } else if dismiss {
                self.state().prompt = None;
            }
        } else if let Some((_, message)) = notice.filter(|(owner, _)| *owner == window) {
            if result_prompt(ctx, &message) {
                self.state().notice = None;
            }
        }
        if let Some(started) = started {
            if progress_prompt(ctx, started) {
                ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            }
            ctx.request_repaint_after(Duration::from_millis(200));
        } else if prepared.is_some_and(|at| at.elapsed() < PREPARED_NOTICE) {
            if prepared_notice(ctx) {
                self.state().prepared = None;
                let control = control.clone();
                let this = self.clone();
                let ctx = ctx.clone();
                self.0.runtime.spawn(async move {
                    let result = tokio::select! {
                        biased;
                        _ = this.0.cancel.cancelled() => return,
                        result = control.stop_acquire_update() => result,
                    };
                    if let Err(error) = result {
                        this.state().notice =
                            Some((window, format!("延后安装结果未确认：{error:#}")));
                    }
                    ctx.request_repaint();
                });
            }
            ctx.request_repaint_after(Duration::from_millis(200));
        }
    }
}

fn version_prompt(
    ctx: &egui::Context,
    alias: &str,
    version: &str,
    feature: &str,
    supported: bool,
) -> (Option<bool>, bool) {
    let mut chosen = None;
    let mut dismiss = false;
    let response = egui::Modal::new(egui::Id::new("remote-version-too-low"))
        .frame(controls::dialog_frame())
        .show(ctx, |ui| {
            ui.set_width(
                theme::REMOTE_UPGRADE_WIDTH.min((ctx.content_rect().width() - 80.0).max(260.0)),
            );
            dismiss =
                controls::dialog_header(ui, "被控端需要更新", controls::DialogIcon::Required, true);
            ui.label(format!("当前被控端不支持{feature}，请先更新。"));
            ui.add_space(12.0);
            controls::update_device_row(ui, alias, version);
            if supported {
                let (now, later) = controls::update_actions(
                    ui,
                    Some("立即更新"),
                    Some(("稍后再说", "通知被控端延后更新，不立即安装")),
                );
                if now {
                    chosen = Some(true);
                } else if later {
                    chosen = Some(false);
                }
            } else {
                ui.add_space(12.0);
                ui.label(
                    RichText::new("请在被控端手动更新 UU 远程后重试。")
                        .size(theme::COMPACT_TEXT)
                        .color(theme::MUTED),
                );
                dismiss |= controls::update_actions(ui, Some("知道了"), None).0;
            }
        });
    (chosen, dismiss || response.should_close())
}

fn result_prompt(ctx: &egui::Context, message: &str) -> bool {
    let mut close = false;
    let response = egui::Modal::new(egui::Id::new("remote-update-request-result"))
        .frame(controls::dialog_frame())
        .show(ctx, |ui| {
            ui.set_width(
                theme::REMOTE_UPGRADE_WIDTH.min((ctx.content_rect().width() - 80.0).max(260.0)),
            );
            close =
                controls::dialog_header(ui, "未能确认更新结果", controls::DialogIcon::Error, true);
            egui::ScrollArea::vertical()
                .id_salt(("remote-update-error-message", message))
                .max_height(theme::UPDATE_MESSAGE_HEIGHT)
                .show(ui, |ui| {
                    ui.add(egui::Label::new(message).wrap().selectable(true));
                });
            close |= controls::update_actions(ui, Some("知道了"), None).0;
        });
    close || response.should_close()
}

pub(crate) fn progress_prompt(ctx: &egui::Context, started: Instant) -> bool {
    let mut close = false;
    egui::Modal::new(egui::Id::new("remote-upgrade-progress"))
        .frame(controls::dialog_frame())
        .show(ctx, |ui| {
            ui.set_width(
                theme::REMOTE_UPGRADE_WIDTH.min((ctx.content_rect().width() - 80.0).max(260.0)),
            );
            controls::dialog_header(ui, "正在更新被控端", controls::DialogIcon::Waiting, false);
            ui.label(RichText::new("正在等待更新完成并恢复画面，请稍候。").color(theme::MUTED));
            ui.add_space(16.0);
            ui.horizontal(|ui| {
                ui.spinner();
                ui.label(format!(
                    "已等待 {} 秒 · 就绪后自动恢复",
                    started.elapsed().as_secs()
                ));
            });
            close = controls::update_actions(
                ui,
                None,
                Some(("关闭观看", "仅关闭观看窗口，被控端会继续更新")),
            )
            .1;
        });
    close
}

fn prepared_notice(ctx: &egui::Context) -> bool {
    controls::update_prepared_notice(ctx)
}
