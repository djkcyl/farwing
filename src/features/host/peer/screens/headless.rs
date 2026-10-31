//! Capture handoff for the machine-owned headless display. The connection may
//! retire it only after a replacement produces frames; disconnect leaves it alive.
use super::*;
use crate::i18n::t;
use std::time::{Duration, Instant};

pub(super) fn same_source(a: &capture::Screen, b: &capture::Screen) -> bool {
    a.id == b.id && a.identity.is_some() && a.identity == b.identity
}

/// After a failed fallback, the next try (the topology may settle).
const RETRY: Duration = Duration::from_secs(10);

#[derive(Default)]
pub(super) struct State {
    empty_since: Option<Instant>,
    /// When the fallback was last tried in this spell without a usable
    /// screen.
    attempted: Option<Instant>,
    unused_returning: Option<(String, Instant)>,
    pub(super) recovery: Option<Recovery>,
}
pub(super) struct Recovery {
    slot: usize,
    original: capture::Screen,
    fallback: i32,
    automatic: bool,
    returning: Option<Instant>,
    retry_at: Instant,
    probe: Option<Probe>,
    replacement_since: Option<(String, Instant)>,
}
struct Probe {
    cancel: CancellationToken,
    task: Option<tokio::task::JoinHandle<Result<capture::Screen>>>,
}
impl Probe {
    fn start(screen: capture::Screen, lease: Lease, cancel: CancellationToken) -> Self {
        let task_cancel = cancel.clone();
        let task = tokio::task::spawn_blocking(move || {
            ensure!(
                lease.requested() && !task_cancel.is_cancelled(),
                "屏幕恢复已取消"
            );
            let _runtime = crate::features::host::encoder::Runtime::new()?;
            let mut desktop = capture::Desktop::open_selected(&screen)?;
            let deadline = Instant::now() + Duration::from_secs(2);
            while lease.requested() && !task_cancel.is_cancelled() && Instant::now() < deadline {
                if desktop.next(20, crate::media::geometry::Tier::P720, false, false, (1280, 720))?.is_some()
                    && desktop.available
                {
                    ensure!(
                        same_source(&desktop.screen, &screen),
                        "恢复期间屏幕身份变化"
                    );
                    return Ok(desktop.screen.clone());
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            anyhow::bail!("原显示器尚未恢复可采集画面")
        });
        Self {
            cancel,
            task: Some(task),
        }
    }
    async fn close(&mut self) {
        self.cancel.cancel();
        if let Some(task) = self.task.take() {
            let _ = task.await;
        }
    }
}
impl Drop for Probe {
    fn drop(&mut self) {
        self.cancel.cancel();
        // The closure owns its capture resources and checks cancellation. During
        // normal close we join it; abnormal drop still arranges owned cleanup.
        if let Some(task) = self.task.take() {
            if let Ok(runtime) = tokio::runtime::Handle::try_current() {
                runtime.spawn(async move {
                    let _ = task.await;
                });
            }
        }
    }
}
impl State {
    pub(super) fn knows_screen(&self, id: i32) -> bool {
        self.recovery
            .as_ref()
            .is_some_and(|r| id == r.original.id || id == r.fallback)
    }
    pub(super) fn owns_slot(&self, slot: usize) -> bool {
        self.recovery.as_ref().is_some_and(|r| r.slot == slot)
    }
    pub(super) fn choose(&mut self, id: i32) {
        if let Some(r) = &mut self.recovery {
            // Controllers can echo our newly published fallback selection.
            if id != r.fallback && !(r.returning.is_some() && id == r.original.id) {
                r.automatic = false;
                r.returning = None;
                if let Some(probe) = &r.probe {
                    probe.cancel.cancel();
                }
            }
        }
    }
    pub(super) fn stop(&mut self, id: i32) {
        if let Some(r) = &mut self.recovery {
            if id == -1 || id == r.fallback || (r.returning.is_some() && id == r.original.id) {
                r.automatic = false;
                r.returning = None;
                if let Some(probe) = &r.probe {
                    probe.cancel.cancel();
                }
            }
        }
    }
    /// `empty`: no screen anyone sees (none, or only other products'
    /// virtual screens).
    fn should_create(&mut self, empty: bool, now: Instant) -> bool {
        if !empty {
            self.empty_since = None;
            self.attempted = None;
            return false;
        }
        let since = *self.empty_since.get_or_insert(now);
        if self.attempted.is_some_and(|at| now.saturating_duration_since(at) < RETRY)
            || now.saturating_duration_since(since) < Duration::from_millis(200)
        {
            return false;
        }
        self.attempted = Some(now);
        true
    }
    pub(super) async fn close(&mut self) {
        if let Some(mut recovery) = self.recovery.take() {
            if let Some(probe) = &mut recovery.probe {
                probe.close().await;
            }
        }
    }
}

impl Screens {
    /// No screen anyone sees: none, or only other products' virtual screens.
    fn unseen(&self) -> bool {
        lock(&self.reports.catalog)
            .iter()
            .all(|i| i.target.as_ref().is_some_and(|t| t.foreign()))
    }
    /// A screen waiting for its lost monitor while only the
    /// fallback `fallback` (or nothing anyone sees) is there.
    fn waiting_for(&self, fallback: i32) -> Option<usize> {
        let alone = lock(&self.reports.catalog).iter().all(|i| {
            i.screen.id == fallback || i.target.as_ref().is_some_and(|t| t.foreign())
        });
        if !alone {
            return None;
        }
        let current = self.reports.current.load(Ordering::Acquire);
        self.slots
            .iter()
            .enumerate()
            .filter(|(_, slot)| {
                slot.awaiting_source && slot.worker.is_none() && slot.suspended.is_some()
            })
            .min_by_key(|(_, slot)| slot.suspended.as_ref().is_none_or(|s| s.id != current))
            .map(|(index, _)| index)
    }
    fn delivered(&self, index: usize) -> bool {
        self.slots
            .get(index)
            .and_then(|s| s.worker.as_ref())
            .is_some_and(media::Worker::has_frame)
    }
    pub(super) async fn maintain_headless(&mut self) -> Result<()> {
        if self.headless.recovery.is_none() {
            if let Some(fallback) = self.displays.fallback_screen()? {
                if let Some(slot) = self.active_slot(fallback.id) {
                    self.headless.recovery = Some(Recovery {
                        slot,
                        original: fallback.clone(),
                        fallback: fallback.id,
                        automatic: true,
                        returning: None,
                        retry_at: Instant::now(),
                        probe: None,
                        replacement_since: None,
                    });
                } else if let Some(index) = self.waiting_for(fallback.id) {
                    // The fallback stayed (an earlier connection, a start that
                    // failed): a screen that lost its monitor takes it while
                    // nothing else is seen.
                    let original = self.slots[index]
                        .suspended
                        .clone()
                        .unwrap_or_else(|| fallback.clone());
                    self.headless.recovery = Some(Recovery {
                        slot: index,
                        original,
                        fallback: fallback.id,
                        automatic: true,
                        returning: None,
                        retry_at: Instant::now(),
                        probe: None,
                        replacement_since: None,
                    });
                    let config = *lock(&self.slots[index].config);
                    self.start_at(index, fallback, config).await?;
                    return Ok(());
                } else if self.displays.allows_fallback_retirement() {
                    // A retained screen may already be unused at reconnect, or
                    // video was paused before the first topology maintenance.
                    let idle = self
                        .slots
                        .iter()
                        .all(|s| s.worker.is_none() && !s.awaiting_source);
                    let replacement = lock(&self.reports.catalog)
                        .iter()
                        .find(|i| {
                            i.screen.id != fallback.id
                                && i.target.as_ref().is_some_and(|t| {
                                    t.active && t.available && !t.foreign()
                                })
                                && (idle
                                    || self.slots.iter().enumerate().any(|(n, s)| {
                                        self.delivered(n)
                                            && s.screen()
                                                .is_some_and(|s| same_source(s, &i.screen))
                                    }))
                        })
                        .and_then(|i| i.screen.identity.clone());
                    if let Some(identity) = replacement {
                        let now = Instant::now();
                        let (id, since) = self
                            .headless
                            .unused_returning
                            .get_or_insert((identity.clone(), now));
                        if *id != identity {
                            *id = identity.clone();
                            *since = now;
                        }
                        if now.duration_since(*since) >= Duration::from_secs(2)
                            && self.displays.retire_fallback(identity).await?
                        {
                            self.headless.unused_returning = None;
                            self.refresh()?;
                        }
                    } else {
                        self.headless.unused_returning = None;
                    }
                }
            }
        }
        if self.headless.recovery.is_none() {
            if self.unseen() && self.headless.attempted.is_none() {
                self.headless.empty_since.get_or_insert_with(Instant::now);
                // Confirm the empty topology before publishing ScreenSources.
                // Otherwise a controller could close its last tab in the gap.
                tokio::select! { _ = self.cancel.cancelled() => return Ok(()), _ = tokio::time::sleep(Duration::from_millis(200)) => {} }
                self.refresh()?;
            }
            let empty = self.unseen();
            if !self.headless.should_create(empty, Instant::now()) {
                return Ok(());
            }
            let current = self.reports.current.load(Ordering::Acquire);
            let candidate = self
                .slots
                .iter()
                .enumerate()
                .filter_map(|(index, slot)| {
                    slot.suspended
                        .clone()
                        .or_else(|| slot.screen().cloned())
                        .map(|s| (index, s))
                })
                .min_by_key(|(_, s)| s.id != current);
            let Some((index, original)) = candidate else {
                return Ok(());
            };
            self.lease
                .update(true, true, t().features.screen_preparing_persistent);
            let Some(fallback) = self
                .displays
                .ensure_fallback(original.clone())
                .await
                .context(t().features.screen_persistent_failed)?
            else {
                return Ok(());
            };
            // Record ownership before starting media: failure must still retain
            // a cleanup owner and must never create another monitor next tick.
            self.headless.recovery = Some(Recovery {
                slot: index,
                original,
                fallback: fallback.id,
                automatic: true,
                returning: None,
                retry_at: Instant::now(),
                probe: None,
                replacement_since: None,
            });
            self.refresh()?;
            let config = *lock(&self.slots[index].config);
            self.start_at(index, fallback, config).await?;
            return Ok(());
        }

        // Temporarily take the state so transitions may borrow the screen pool.
        // Always put it back on error; the driver keeps the fallback alive.
        let mut recovery = self.headless.recovery.take().unwrap();
        let result = self.advance_headless(&mut recovery, Instant::now()).await;
        match result {
            Ok(true) => {
                if let Some(probe) = &mut recovery.probe {
                    probe.close().await;
                }
                self.headless.empty_since = None;
                self.headless.attempted = None;
                Ok(())
            }
            result => {
                self.headless.recovery = Some(recovery);
                result.map(|_| ())
            }
        }
    }
    async fn advance_headless(&mut self, r: &mut Recovery, now: Instant) -> Result<bool> {
        if !self.displays.allows_fallback_retirement() {
            if let Some(probe) = &mut r.probe {
                probe.close().await;
            }
            r.probe = None;
            r.returning = None;
            return Ok(false);
        }
        let fallback = self.info(r.fallback).ok().map(|i| i.screen);
        let original = lock(&self.reports.catalog)
            .iter()
            .filter(|i| {
                i.screen.id != r.fallback
                    && i.target.as_ref().is_some_and(|t| t.active && t.available && !t.foreign())
            })
            .min_by_key(|i| (i.screen.identity != r.original.identity, !i.screen.primary))
            .map(|i| i.screen.clone());
        if let Some(original) = &original {
            let identity = original
                .identity
                .clone()
                .context(t().features.screen_replacement_identity_missing)?;
            if r.replacement_since
                .as_ref()
                .is_none_or(|(id, _)| id != &identity)
            {
                r.replacement_since = Some((identity, now));
                r.retry_at = now + Duration::from_secs(2);
            }
        } else {
            r.replacement_since = None;
        }
        if r.returning.is_none() && r.probe.is_none() {
            if let Some(original) = &original {
                r.original = original.clone();
            }
        }
        if original.is_some()
            && now >= r.retry_at
            && self
                .slots
                .iter()
                .all(|s| s.worker.is_none() && !s.awaiting_source)
        {
            // Audio-only / paused video has no capture to migrate or probe.
            return self.remove_fallback(r.fallback).await;
        }
        if !r.automatic {
            if let Some(probe) = &mut r.probe {
                probe.close().await;
            }
            r.probe = None;
            // Do not destroy a manually retained/paused fallback screen. A new
            // selection must actually deliver a frame before automatic cleanup.
            if self.active_slot(r.fallback).is_none()
                && self
                    .slots
                    .iter()
                    .enumerate()
                    .any(|(i, _)| self.delivered(i))
            {
                return self.remove_fallback(r.fallback).await;
            }
            return Ok(false);
        }
        if fallback.is_none() && original.is_none() && r.returning.is_none() && now >= r.retry_at {
            r.retry_at = now + Duration::from_secs(10);
            if let Some(screen) = self.displays.ensure_fallback(r.original.clone()).await? {
                r.fallback = screen.id;
                self.refresh()?;
                let config = *lock(&self.slots[r.slot].config);
                self.start_at(r.slot, screen, config).await?;
            }
            return Ok(false);
        }
        if let Some(started) = r.returning {
            if !self.connected.load(Ordering::Acquire) {
                r.returning = Some(now);
                return Ok(false);
            }
            if original.is_some()
                && self.slots[r.slot]
                    .screen()
                    .is_some_and(|s| same_source(s, &r.original))
                && self.delivered(r.slot)
            {
                if self.remove_fallback(r.fallback).await? {
                    tracing::info!("replacement display resumed; persistent fallback retired");
                    return Ok(true);
                }
                return Ok(false);
            }
            let ended = self.slots[r.slot]
                .worker
                .as_ref()
                .is_none_or(media::Worker::ended);
            if original.is_none()
                || ended
                || (self.connected.load(Ordering::Acquire)
                    && now.duration_since(started) >= Duration::from_secs(8))
            {
                r.returning = None;
                r.retry_at = now + Duration::from_secs(10);
                if let Some(fallback) = fallback {
                    let config = *lock(&self.slots[r.slot].config);
                    self.start_at(r.slot, fallback, config).await?;
                }
            }
            return Ok(false);
        }
        if let Some(probe) = &mut r.probe {
            if original.is_none() {
                probe.cancel.cancel();
            }
            if probe.task.as_ref().is_some_and(|t| t.is_finished()) {
                let outcome = probe.task.take().unwrap().await;
                r.probe = None;
                r.retry_at = now + Duration::from_secs(10);
                let outcome = outcome.context(t().features.screen_recovery_check_interrupted)?;
                if let Ok(screen) = outcome {
                    if original.as_ref().is_some_and(|s| same_source(s, &screen)) {
                        r.returning = Some(now);
                        let config = *lock(&self.slots[r.slot].config);
                        self.start_at(r.slot, screen, config).await?;
                    }
                }
            }
            return Ok(false);
        }
        if !self.connected.load(Ordering::Acquire) {
            return Ok(false);
        }
        // Keep fallback playback healthy while a returning physical display is
        // still failing its preflight. A present-but-unusable original must not
        // starve recovery of the usable fallback capture.
        if self.slots[r.slot].worker.is_none() && now >= r.retry_at {
            if let Some(fallback) = fallback {
                r.retry_at = now + Duration::from_secs(10);
                let config = *lock(&self.slots[r.slot].config);
                self.start_at(r.slot, fallback, config).await?;
                return Ok(false);
            }
        }
        if let Some(original) = original {
            if now >= r.retry_at {
                r.probe = Some(Probe::start(
                    original,
                    self.lease.clone(),
                    self.cancel.child_token(),
                ));
            }
        }
        Ok(false)
    }
    async fn remove_fallback(&mut self, id: i32) -> Result<bool> {
        let replacement = self
            .slots
            .iter()
            .enumerate()
            .filter(|(i, _)| self.delivered(*i))
            .filter_map(|(_, slot)| slot.screen())
            .filter(|s| s.id != id)
            .find_map(|s| s.identity.clone());
        let replacement = replacement.or_else(|| {
            if self
                .slots
                .iter()
                .any(|s| s.worker.is_some() || s.awaiting_source)
            {
                return None;
            }
            lock(&self.reports.catalog)
                .iter()
                .find(|i| {
                    i.screen.id != id
                        && i.target.as_ref().is_some_and(|t| t.active && t.available && !t.foreign())
                })
                .and_then(|i| i.screen.identity.clone())
        });
        let Some(replacement) = replacement else {
            return Ok(false);
        };
        if !self.displays.retire_fallback(replacement).await? {
            return Ok(false);
        }
        for slot in &mut self.slots {
            if slot.suspended.as_ref().is_some_and(|s| s.id == id) {
                slot.suspended = None;
                slot.awaiting_source = false;
            }
        }
        self.refresh()?;
        Ok(true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_fallback_is_tried_again_while_nothing_is_seen() {
        let mut state = State::default();
        let start = Instant::now();
        let at = |ms| start + Duration::from_millis(ms);
        assert!(!state.should_create(true, at(0)));
        assert!(state.should_create(true, at(200)));
        assert!(!state.should_create(true, at(5_000)));
        assert!(state.should_create(true, at(10_200)));
        // A screen someone sees ends the spell.
        assert!(!state.should_create(false, at(10_300)));
        assert!(!state.should_create(true, at(10_400)));
        assert!(state.should_create(true, at(10_600)));
    }
}
