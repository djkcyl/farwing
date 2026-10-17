//! Retire uncertain input before restoring a temporarily stalled activation.
use super::*;

pub(super) struct RecoveryState {
    mode: MouseMode,
    pub(super) confirmed: bool,
    dispatched: bool,
}

pub(crate) struct InputRecovery {
    pub epoch: u64,
    pub releases: Vec<InputEvent>,
}

pub(crate) enum InputWork {
    Event(QueuedInputEvent),
    Recovery(InputRecovery),
}

impl RemoteInput {
    // Input delivery is paused, but cursor/microphone/clipboard policy still
    // belongs to the user's active control intent until it is explicitly revoked.
    pub(crate) fn policy_mode(&self) -> MouseMode {
        let s = self.lock();
        s.transport_recovery.as_ref().map_or(s.mode, |r| r.mode)
    }

    pub(crate) fn transport_recovering(&self) -> bool {
        self.lock().transport_recovery.is_some()
    }

    pub(crate) fn recovered_mode(&self) -> Option<MouseMode> {
        self.lock()
            .transport_recovery
            .as_ref()
            .filter(|r| r.confirmed)
            .map(|r| r.mode)
    }

    pub(crate) fn recovery_current(&self, epoch: u64) -> bool {
        let s = self.lock();
        s.ready && !s.stopping && s.epoch == epoch && s.transport_recovery.is_some()
    }

    pub(crate) fn pause_transport(&self, event: &QueuedInputEvent) -> Option<InputRecovery> {
        let mut s = self.lock();
        if !s.ready
            || s.stopping
            || s.epoch != event.epoch
            || s.mode == MouseMode::View
            || !Self::pending_current(&s, &event.event)
        {
            return None;
        }
        Self::begin_recovery_locked(&mut s);
        let recovery = Self::take_recovery_locked(&mut s);
        drop(s);
        self.drained.notify_waiters();
        self.repaint();
        recovery
    }

    // Producers retire uncertain input immediately. The sole transport sender
    // takes the release pass before dequeuing any new input, even with no keys
    // held and an otherwise empty queue.
    #[cfg_attr(
        not(windows),
        allow(
            dead_code,
            reason = "Only the Windows viewer reads raw input and checks key ordering; winit has neither failure."
        )
    )]
    pub(crate) fn pause_for_recovery(&self, reason: &str) {
        let mut s = self.lock();
        if !Self::begin_recovery_locked(&mut s) {
            return;
        }
        tracing::warn!(reason, "remote input paused; automatic recovery pending");
        drop(s);
        self.wake.notify_one();
        self.drained.notify_waiters();
        self.repaint();
    }

    pub(super) fn begin_recovery_locked(s: &mut State) -> bool {
        if !s.ready || s.stopping || s.mode == MouseMode::View {
            return false;
        }
        let mode = s.mode;
        Self::release_locked(s);
        s.mode = MouseMode::View;
        s.waiting_for_neutral = false;
        s.error = None;
        s.transport_recovery = Some(RecoveryState {
            mode,
            confirmed: false,
            dispatched: false,
        });
        true
    }

    pub(super) fn take_recovery_locked(s: &mut State) -> Option<InputRecovery> {
        let pending = s.transport_recovery.as_mut()?;
        if pending.dispatched || !s.ready || s.stopping {
            return None;
        }
        pending.dispatched = true;
        // Only release obligations survive. The sender owns this finite pass;
        // a later explicit stop rebuilds releases in its own input epoch.
        Some(InputRecovery {
            epoch: s.epoch,
            releases: s.queue.drain(..).collect(),
        })
    }

    pub(crate) fn finish_transport_recovery(&self, epoch: u64, result: Result<()>) {
        let mut s = self.lock();
        if s.epoch != epoch || s.transport_recovery.is_none() {
            return;
        }
        match result {
            Ok(()) => {
                // Reliable acknowledgement establishes ordering of the release
                // pass before future input, not proof of OS input injection.
                s.remote_keys.clear();
                s.remote_held = [false; 5];
                s.transport_recovery.as_mut().unwrap().confirmed = true;
                tracing::info!(
                    "remote input transport drained; awaiting focused neutral activation"
                );
            }
            Err(error) => {
                s.transport_recovery = None;
                Self::release_locked(&mut s);
                s.error = Some(format!("键鼠通道恢复失败，已停止控制：{error}"));
                tracing::warn!(%error, "remote input transport recovery failed");
            }
        }
        drop(s);
        self.wake.notify_one();
        self.repaint();
    }
}
