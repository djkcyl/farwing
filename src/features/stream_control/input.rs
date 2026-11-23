//! Remote input and cursor policy transitions.
use super::{
    StreamControlHandle, StreamControlState, ensure_ready, lock, negotiated,
};
use crate::features::remote_cursor::RemoteCursor;
use crate::features::remote_input::MouseMode;
use crate::i18n::t;
use anyhow::{Result, bail};

impl StreamControlHandle {
    pub(crate) fn set_mouse_transport_ready(&self, connected: bool) {
        let mut state = lock(&self.shared);
        state.mouse_transport_connected = connected;
        if !connected {
            state.restore_input_pending = state.device_preferences.control_enabled
                && state.viewing_enabled
                && !state.annotation.enabled
                && !state.annotation.toggling();
            self.microphone.disconnect();
            #[cfg(windows)]
            self.clipboard.suspend();
            state.annotation.disconnect();
            state.cursor_sync_needed = false;
            state.cursor_desired_capture = true;
            state.mouse_restore_point = None;
            // Revoke actual input now. A focused, ready viewer may later restore
            // the separately saved intent; held keys/clicks are never replayed.
            state.cursor_capture = true;
            if let Some(sequence) = state.cursor_pending {
                fail_cursor_request(
                    &mut state,
                    sequence,
                    t().features.stream_mouse_connection_lost.into(),
                );
            }
            state.mouse.set_ready(false);
        } else {
            state.mouse.set_ready(
                negotiated(&state)
                    && state.control_channel_open
                    && state.text_channel_open,
            );
            self.sync_stream_locked(&mut state);
        }
    }

    /// The host takes TOUCH (`touch`) or PEN (`pen`) frames.
    pub(crate) fn peer_supports(&self, feature: &str) -> bool {
        lock(&self.shared).peer.has(feature)
    }

    pub(crate) fn mouse_screen(&self, track: i32) -> Option<(i32, u32, u32)> {
        let s = lock(&self.shared);
        let screen = s
            .screens
            .iter()
            .find(|screen| screen.video_track_index == track && screen.id >= 0)?;
        Some((screen.id, screen.width, screen.height))
    }

    pub(crate) fn take_mouse_restore_point(&self, track: i32) -> Option<[f64; 2]> {
        let mut s = lock(&self.shared);
        let screen = s
            .screens
            .iter()
            .find(|screen| screen.video_track_index == track)?
            .id;
        if s.mouse.mode() == MouseMode::Smart
            && s.mouse_restore_point.is_some_and(|(id, _)| id == screen)
        {
            s.mouse_restore_point.take().map(|(_, point)| point)
        } else {
            None
        }
    }

    pub fn remote_cursor(&self) -> Option<RemoteCursor> {
        self.cursor.snapshot()
    }

    pub(crate) fn remote_cursor_hidden(&self) -> bool {
        self.cursor.hidden()
    }

    pub fn set_mouse_mode(&self, mode: MouseMode) -> Result<()> {
        self.change_mouse_mode(mode, true, false)
    }

    /// Turn control on, asking the host's user for it in this connection
    /// (after a refusal for lack of the grant).
    pub(crate) fn ask_control(&self) -> Result<()> {
        #[cfg(windows)]
        let mode = {
            let mode = lock(&self.shared).device_preferences.mouse_mode;
            if mode == MouseMode::View { MouseMode::Smart } else { mode }
        };
        #[cfg(target_os = "linux")]
        let mode = MouseMode::Remote;
        self.change_mouse_mode(mode, true, true)
    }

    pub(crate) fn suspend_mouse_control(&self) -> Result<()> {
        self.change_mouse_mode(MouseMode::View, false, false)
    }

    pub(crate) fn reconcile_mouse_failure(&self) {
        // RemoteInput already released the failed activation. Update cursor and
        // clipboard policy without canceling restoration armed by a concurrent
        // transport loss. Transient send recovery is owned by RemoteInput.
        let mut state = lock(&self.shared);
        state.cursor_sync_needed = true;
        self.refresh_mouse_policy(&mut state);
    }

    fn change_mouse_mode(&self, mode: MouseMode, user_choice: bool, ask: bool) -> Result<()> {
        let mut state = lock(&self.shared);
        state.mouse_restore_point = None;
        let was_controlling = state.mouse.mode() != MouseMode::View;
        if mode == MouseMode::View {
            // Local revocation never waits for remote settings.
            state.mouse.disable();
            if was_controlling {
                self.request_turn_locked(&mut state, false, false, false);
            }
        } else {
            ensure_ready(&state)?;
            if state.annotation.enabled || state.annotation.toggling() {
                bail!("{}", t().features.stream_no_control_while_annotating);
            }
            // One participant controls at a time; the others wait for it
            // to give control back.
            if let Some(holder) = self.participants.controlled_by_other() {
                bail!(super::control_turn::held_by(&holder.nickname));
            }
            let (relative, _) = mouse_policy(&state, mode);
            state.mouse.enable(mode, relative)?;
            if !was_controlling {
                self.request_turn_locked(&mut state, true, false, ask);
            }
        }
        state.restore_input_pending = false;
        if user_choice {
            let previous = state.device_preferences;
            state.device_preferences.control_enabled = mode != MouseMode::View;
            if mode != MouseMode::View {
                state.device_preferences.mouse_mode = mode;
            }
            if state.device_preferences != previous {
                self.publish_device_preferences(&mut state);
            }
        }
        // Explicit choices may retry uncertain cursor capture; newer intent is
        // independent of an earlier cursor request still awaiting its response.
        state.cursor_sync_needed = true;
        self.refresh_mouse_policy(&mut state);
        tracing::debug!(?mode, user_choice, "remote input control mode changed");
        drop(state);
        self.mouse.repaint();
        Ok(())
    }

    /// Called only by a focused viewer after its first frame and outside local
    /// menus/transitions. One attempt per restored connection, not a retry loop.
    pub(crate) fn restore_input_control(&self) -> Result<()> {
        let mut state = lock(&self.shared);
        let recovering = state.mouse.transport_recovering();
        if !recovering
            && (!state.restore_input_pending || !state.device_preferences.control_enabled)
        {
            return Ok(());
        }
        if state.annotation.enabled || state.annotation.toggling() {
            state.restore_input_pending = false;
            if recovering {
                state.mouse.disable();
            }
            return Ok(());
        }
        if !state.viewing_enabled
            || !state.mouse_transport_connected
            || ensure_ready(&state).is_err()
        {
            return Ok(());
        }
        // Several participants take control by hand (the host refuses an
        // automatic take as well); only a holder keeps it across recovery.
        if self.participants.others_present() && !self.participants.holds_control() {
            state.restore_input_pending = false;
            if recovering {
                state.mouse.disable();
                self.refresh_mouse_policy(&mut state);
            }
            return Ok(());
        }
        let mode = if recovering {
            let Some(mode) = state.mouse.recovered_mode() else {
                return Ok(());
            };
            mode
        } else {
            state.device_preferences.mouse_mode
        };
        state.restore_input_pending = false;
        let (relative, _) = mouse_policy(&state, mode);
        state.mouse.enable(mode, relative)?;
        // A rebuilt session starts without a token holder; the host decides
        // whether this participant may take it back without asking.
        self.request_turn_locked(&mut state, true, true, false);
        if recovering {
            tracing::info!("remote input control restored after transport recovery");
        }
        state.cursor_sync_needed = true;
        self.refresh_mouse_policy(&mut state);
        drop(state);
        self.mouse.repaint();
        Ok(())
    }

    pub(super) fn request_cursor_locked(
        &self,
        state: &mut StreamControlState,
        visible: bool,
    ) -> Result<i64> {
        ensure_ready(state)?;
        // The current intent: later stream requests carry it too.
        state.cursor_capture = visible;
        state.cursor_error = None;
        let target = if visible {
            t().features.stream_show_remote_cursor
        } else {
            t().features.stream_hide_remote_cursor
        };
        let sequence = self
            .request_stream_locked(state, Some(target.into()))?
            .unwrap_or(0);
        tracing::info!(sequence, visible, "remote cursor visibility requested");
        Ok(sequence)
    }

    pub(super) fn refresh_mouse_policy(&self, state: &mut StreamControlState) {
        let mode = state.mouse.policy_mode();
        // The microphone does not depend on control: everyone may talk.
        if !state.viewing_enabled || self.microphone.needs_cleanup() {
            self.disable_microphone_locked(state);
        }
        #[cfg(windows)]
        {
            // The exchange runs whenever the session is up; automatic sync needs control.
            let clipboard_ready = state.viewing_enabled
                && state.hello_answered
                && state.text_channel_open
                && state.peer.has(crate::protocol::peer::feature::CLIPBOARD);
            self.clipboard.policy(
                clipboard_ready,
                clipboard_ready
                    && state.control_channel_open
                    && state.mouse_transport_connected
                    && mode != MouseMode::View,
                state.peer.has(crate::protocol::peer::feature::CLIPBOARD_FILES)
                    && state.clipboard_files_allowed,
            );
            self.drag_drop.enable(
                state.viewing_enabled
                    && state.hello_answered
                    && state.text_channel_open
                    && state.mouse_transport_connected
                    && self.files.allowed(),
            );
        }
        if !state.viewing_enabled {
            return;
        }
        let (relative, wanted) = mouse_policy(state, mode);
        if mode == MouseMode::Smart && state.mouse.relative_mode() != relative {
            state.mouse.set_relative_mode(relative);
        }
        if state.cursor_desired_capture != wanted {
            state.cursor_desired_capture = wanted;
            state.cursor_sync_needed = true;
        }
        if !state.cursor_sync_needed || ensure_ready(state).is_err() {
            return;
        }
        state.cursor_sync_needed = false;
        if wanted == state.cursor_capture && state.cursor_error.is_none() {
            return;
        }
        // Failure does not revoke input or retry forever. A new policy change
        // or explicit choice is required before submitting again.
        if let Err(error) = self.request_cursor_locked(state, wanted) {
            state.cursor_error = Some(error.to_string());
        }
    }
}

pub(super) fn smart_mouse_requested(state: &StreamControlState) -> bool {
    state.mouse.policy_mode() == MouseMode::Smart
}

pub(super) fn mouse_policy(state: &StreamControlState, mode: MouseMode) -> (bool, bool) {
    match mode {
        MouseMode::View => (false, true),
        MouseMode::Local => (false, false),
        MouseMode::Remote => (cfg!(windows), true),
        // Relative while the host hides its cursor (a game took it).
        MouseMode::Smart => (state.remote_cursor.hidden(), false),
    }
}

pub(super) fn fail_cursor_request(state: &mut StreamControlState, sequence: i64, error: String) {
    if state.cursor_pending == Some(sequence) {
        state.cursor_pending = None;
        state.cursor_error = Some(error);
    }
}

