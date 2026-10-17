//! Remote input and cursor policy transitions.
use super::settings::encode_capture_setting;
use super::{
    OutgoingControlMessage, PbMessageSource, StreamControlHandle, StreamControlProtocol,
    StreamControlState, ensure_ready, lock, protocol,
};
use crate::features::remote_cursor::RemoteCursor;
use crate::features::remote_input::MouseMode;
use anyhow::{Result, anyhow, bail};
use std::time::Instant;

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
            self.clipboard.suspend();
            state.annotation.disconnect();
            state.peer_mouse_relative = None;
            state.cursor_sync_needed = false;
            state.cursor_desired_capture = true;
            state.mouse_restore_point = None;
            // Revoke actual input now. A focused, ready viewer may later restore
            // the separately saved intent; held keys/clicks are never replayed.
            state.baseline.cursor_capture = true;
            state.initial_capture_sync_sent = false;
            if let Some((sequence, _, _)) = state.cursor_pending {
                fail_cursor_request(&mut state, sequence, "鼠标连接已中断".into());
                state
                    .pending_sequences
                    .retain(|pending| *pending != sequence);
                state
                    .pending_capture_preferences
                    .retain(|pending| pending.sequence != sequence);
            }
            state.mouse.set_ready(false);
        } else {
            state.mouse.set_ready(
                protocol(&state) == StreamControlProtocol::CaptureSetting
                    && state.control_channel_open
                    && state.text_channel_open,
            );
            self.maybe_send_initial_capture_sync(&mut state);
        }
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

    /// Whether the host is drawing its own pointer into the captured frames.
    /// While it is not, nothing but this client can put a pointer on screen.
    /// The Linux player draws the remote pointer itself and asks; the Windows
    /// player shows it as the system cursor, which needs no such check.
    #[cfg_attr(windows, allow(dead_code))]
    pub(crate) fn remote_cursor_captured(&self) -> bool {
        lock(&self.shared).baseline.cursor_capture
    }

    pub fn set_mouse_mode(&self, mode: MouseMode) -> Result<()> {
        self.change_mouse_mode(mode, true)
    }

    pub(crate) fn suspend_mouse_control(&self) -> Result<()> {
        self.change_mouse_mode(MouseMode::View, false)
    }

    pub(crate) fn reconcile_mouse_failure(&self) {
        // RemoteInput already released the failed activation. Update cursor and
        // clipboard policy without canceling restoration armed by a concurrent
        // transport loss. Transient send recovery is owned by RemoteInput.
        let mut state = lock(&self.shared);
        state.cursor_sync_needed = true;
        self.refresh_mouse_policy(&mut state);
    }

    fn change_mouse_mode(&self, mode: MouseMode, user_choice: bool) -> Result<()> {
        let mut state = lock(&self.shared);
        expire_cursor_request(&mut state);
        state.mouse_restore_point = None;
        if mode == MouseMode::View {
            // Local revocation never waits for remote settings.
            state.mouse.disable();
        } else {
            ensure_ready(&state)?;
            if state.annotation.enabled || state.annotation.toggling() {
                bail!("请先关闭批注，再开启键鼠控制");
            }
            let (relative, _) = mouse_policy(&state, mode);
            state.mouse.enable(mode, relative)?;
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
        expire_cursor_request(state);
        ensure_ready(state)?;
        let sequence = state.next_sequence;
        let mut baseline = state.baseline;
        baseline.cursor_capture = visible;
        // BECA40/FD9FF0 update desired capture immediately, independently of
        // ACKs. Later complete snapshots must carry this same current intent.
        state.baseline.cursor_capture = visible;
        let active_protocol = protocol(state);
        let payload = match active_protocol {
            StreamControlProtocol::CaptureSetting => encode_capture_setting(sequence, baseline)?,

            StreamControlProtocol::Negotiating => bail!("PB 特性协商尚未完成"),
            StreamControlProtocol::Unsupported => {
                bail!("对端不支持当前串流协议（需要CaptureSetting RPC）")
            }
        };
        state.next_sequence = state.next_sequence.wrapping_add(1);
        state.pending_sequences.push_back(sequence);
        state.cursor_pending = Some((sequence, visible, Instant::now()));
        state.initial_capture_sync_sent = true;
        state.cursor_error = None;
        state.last_error = None;
        state.last_notice = None;
        let result = self.send_locked(
            state,
            OutgoingControlMessage {
                annotation_generation: None,
                sequence,
                payload,
                protocol: active_protocol,
                completion: None,
            },
            Some(
                if visible {
                    "显示远端光标"
                } else {
                    "隐藏远端光标"
                }
                .into(),
            ),
        );
        if let Err(error) = &result {
            fail_cursor_request(state, sequence, error.to_string());
        }
        tracing::info!(sequence, visible, "remote cursor visibility requested");
        result
    }

    pub(super) fn handle_mouse_command(
        &self,
        payload: &[u8],
        source: PbMessageSource,
    ) -> Result<()> {
        if source != PbMessageSource::Control {
            return Ok(());
        }
        anyhow::ensure!(payload.len() <= 16 * 1024, "mouse command too large");
        let value: serde_json::Value = serde_json::from_slice(payload)?;
        if value.get("action").and_then(|v| v.as_str()) != Some("special_game_mouse") {
            return Ok(());
        }
        let mode = value
            .get("mouse_mode")
            .and_then(|v| v.as_i64())
            .ok_or_else(|| anyhow!("invalid mouse mode"))?;
        let force = value
            .get("force_mode")
            .and_then(|v| v.as_bool())
            .ok_or_else(|| anyhow!("invalid force mode"))?;
        let x = value.get("coordinate_x_scale");
        let y = value.get("coordinate_y_scale");
        let (relative, restore) = match mode {
            0 | 1 => {
                anyhow::ensure!(
                    force && x.is_none() && y.is_none(),
                    "invalid forced mouse command"
                );
                (Some(mode == 1), None)
            }
            2 => {
                anyhow::ensure!(
                    !force || (x.is_none() && y.is_none()),
                    "invalid mouse restore command"
                );
                let restore = match (x, y) {
                    (None, None) => None,
                    (Some(x), Some(y)) => {
                        let x = x
                            .as_f64()
                            .ok_or_else(|| anyhow!("invalid mouse restore x"))?;
                        let y = y
                            .as_f64()
                            .ok_or_else(|| anyhow!("invalid mouse restore y"))?;
                        anyhow::ensure!(
                            x.is_finite()
                                && y.is_finite()
                                && (0.0..=1.0).contains(&x)
                                && (0.0..=1.0).contains(&y),
                            "mouse restore outside screen"
                        );
                        Some([x, y])
                    }
                    _ => bail!("mouse restore coordinates must appear together"),
                };
                (None, restore)
            }
            _ => bail!("unknown mouse mode"),
        };
        let mut state = lock(&self.shared);
        state.peer_mouse_relative = relative;
        state.mouse_restore_point = if smart_mouse_requested(&state) {
            restore.and_then(|point| state.remote_display.map(|screen| (screen.screen_id, point)))
        } else {
            None
        };
        self.refresh_mouse_policy(&mut state);
        drop(state);
        self.mouse.repaint();
        Ok(())
    }

    pub(super) fn refresh_mouse_policy(&self, state: &mut StreamControlState) {
        let mode = state.mouse.policy_mode();
        if !state.viewing_enabled || mode == MouseMode::View || self.microphone.needs_cleanup() {
            self.disable_microphone_locked(state);
        }
        let clipboard_ready = state.viewing_enabled
            && state.pb_connected
            && state.control_channel_open
            && state.text_channel_open
            && state.mouse_transport_connected
            && state.peer_clipboard >= 1
            && mode != MouseMode::View;
        // Clipboard sync is gated on seven separate conditions, and a session
        // that never syncs looks identical whichever one is missing.
        if clipboard_ready != state.clipboard_ready_reported {
            state.clipboard_ready_reported = clipboard_ready;
            tracing::debug!(
                ready = clipboard_ready,
                viewing = state.viewing_enabled,
                pb = state.pb_connected,
                control = state.control_channel_open,
                text = state.text_channel_open,
                mouse_transport = state.mouse_transport_connected,
                peer = state.peer_clipboard,
                mode = ?mode,
                files_allowed = state.clipboard_files_allowed,
                "剪贴板同步条件"
            );
        }
        self.clipboard.policy(
            clipboard_ready,
            state.peer_clipboard >= 2 && state.clipboard_files_allowed,
        );
        self.drag_drop.enable(
            state.viewing_enabled
                && state.pb_connected
                && state.text_channel_open
                && state.mouse_transport_connected
                && self.files.allowed(),
        );
        if !state.viewing_enabled {
            return;
        }
        let (relative, wanted) = mouse_policy(state, mode);
        // Relative motion is only correct while this client holds the pointer.
        // A window that could not take it says so, and absolute positioning is
        // then the only honest way to aim, whatever the policy would prefer.
        let relative = relative && state.mouse.relative_available();
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
        if wanted == state.baseline.cursor_capture && state.cursor_error.is_none() {
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
        MouseMode::Remote => (true, true),
        MouseMode::Smart => match state.peer_mouse_relative {
            Some(true) => (true, true),
            Some(false) => (false, false),
            // The host says a game took the mouse with `special_game_mouse`,
            // and that report is what pairs relative input with the host
            // drawing its own pointer into the picture. Until it arrives,
            // absolute positioning is the only coherent choice: inferring
            // relative input from a hidden cursor fires on an ordinary desktop
            // -- Windows hides the pointer for anyone typing, for as long as
            // they type -- and leaves the client sending deltas while nothing
            // draws a pointer to aim with.
            None => (false, false),
        },
    }
}

pub(super) fn fail_cursor_request(state: &mut StreamControlState, sequence: i64, error: String) {
    if state
        .cursor_pending
        .is_some_and(|(pending, _, _)| pending == sequence)
    {
        state.cursor_pending = None;
        state.cursor_error = Some(error);
    }
}

pub(super) fn expire_cursor_request(state: &mut StreamControlState) {
    if let Some((sequence, _, started)) = state.cursor_pending
        && started.elapsed() >= std::time::Duration::from_secs(10)
    {
        let error = "光标设置确认超时，远端状态未知；请重试".to_owned();
        fail_cursor_request(state, sequence, error.clone());
        state
            .pending_capture_preferences
            .retain(|pending| pending.sequence != sequence);
        state
            .pending_sequences
            .retain(|pending| *pending != sequence);
        state
            .performance
            .fail_stream_switch(sequence, error.clone());
        state.last_error = Some(error);
    }
}
