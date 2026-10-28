//! Channel and protocol handshake state transitions.
use super::input::{fail_cursor_request, smart_mouse_requested};
use super::settings::{apply_stream_state, refresh_active_screen};
use super::wire::{SessionBody, encode_pb_echo_response};
use super::{PbHandshakeStatus, StreamControlHandle, StreamControlProtocol, lock, protocol};
use crate::i18n::t;
use crate::protocol::peer::{self, FrameType};
use crate::protocol::peer::feature;
use crate::media::VideoCodec;
use anyhow::{Result, anyhow};
use std::sync::Arc;
use tokio::sync::Notify;

impl StreamControlHandle {
    pub(crate) fn poll_timeouts(&self) {
        let mut state = lock(&self.shared);
        self.expire_requests(&mut state);
        self.refresh_mouse_policy(&mut state);
    }

    pub(crate) fn set_data_channel_open(&self, label: &str, open: bool) {
        if label == peer::channel::SESSION && !open {
            self.disconnect_audio_quality();
        }
        let mut state = lock(&self.shared);
        match label {
            peer::channel::CONTROL => {
                if state.control_channel_open != open {
                    state.protocol_generation = state.protocol_generation.wrapping_add(1);
                    state.pb_connected = false;
                }
                state.control_channel_open = open;
            }
            peer::channel::SESSION => state.text_channel_open = open,
            _ => return,
        }
        if !open {
            state.restore_input_pending = state.device_preferences.control_enabled
                && state.viewing_enabled
                && !state.annotation.enabled
                && !state.annotation.toggling();
            self.microphone.disconnect();
            self.clipboard.suspend();
            state.peer = Default::default();
            state.grants = None;
            state.annotation.disconnect();
            state.annotation_extension = None;
            for (id, waiting) in state.requests.drain() {
                self.settle(
                    &mut state,
                    id,
                    waiting,
                    Err(t().features.stream_display_disconnected_unconfirmed.into()),
                );
            }
            state.topology.disconnect();
            state
                .display_changes
                .cancel_all(t().features.stream_display_disconnected_unconfirmed);
            state.displays_revision = 0;
            state.cursor_sync_needed = false;
            state.cursor_desired_capture = true;
            state.mouse_restore_point = None;
            state.cursor_capture = true;
            state.mouse.set_ready(false);
            if let Some(sequence) = state.cursor_pending {
                fail_cursor_request(
                    &mut state,
                    sequence,
                    t().features.stream_cursor_disconnected_unconfirmed.into(),
                );
            }
        }
        self.sync_stream_locked(&mut state);
        if open
            && protocol(&state) == StreamControlProtocol::CaptureSetting
            && state.control_channel_open
            && state.text_channel_open
        {
            state.mouse.set_ready(state.mouse_transport_connected);
        }
        drop(state);
        if !open {
            self.cursor.clear();
            self.participants.clear();
        }
        self.protocol_changed.notify_one();
    }

    pub(crate) fn set_video_stream(&self, codec: VideoCodec, video_track_index: i32) {
        let mut state = lock(&self.shared);
        tracing::debug!(?codec, video_track_index, "video stream received");
        state.active_video_track_index = video_track_index;
        refresh_active_screen(&mut state);
        self.sync_stream_locked(&mut state);
    }

    pub(crate) fn select_viewed_video_track(&self, video_track_index: i32) {
        let mut state = lock(&self.shared);
        state.active_video_track_index = video_track_index;
        refresh_active_screen(&mut state);
    }

    pub(crate) fn mark_send_failed(&self, sequence: i64, error: &str) {
        if self.microphone.send_failed(sequence, error) {
            return;
        }
        let mut state = lock(&self.shared);
        let id = super::wire::request_id(sequence);
        if let Some(waiting) = state.requests.take(id) {
            self.settle(&mut state, id, waiting, Err((t().features.stream_send_failed)(error)));
            return;
        }
        if self.turn_send_failed(&mut state, sequence) {
            return;
        }
        fail_cursor_request(&mut state, sequence, error.to_owned());
    }

    pub(crate) fn protocol_notifications(&self) -> Arc<Notify> {
        Arc::clone(&self.protocol_changed)
    }

    pub(crate) fn set_viewing_enabled(&self, enabled: bool) {
        let mut state = lock(&self.shared);
        if enabled && !state.viewing_enabled {
            state.restore_input_pending = state.device_preferences.control_enabled;
        }
        state.viewing_enabled = enabled;
        if !enabled {
            state.restore_input_pending = false;
            self.disable_microphone_locked(&mut state);
            self.clipboard.suspend();
        }
        if enabled {
            self.sync_stream_locked(&mut state);
        }
    }

    pub(crate) fn handshake_status(&self) -> PbHandshakeStatus {
        let state = lock(&self.shared);
        PbHandshakeStatus {
            generation: state.protocol_generation,
            open: state.control_channel_open,
            connected: state.pb_connected,
        }
    }

    pub(crate) fn mark_pb_handshake_timeout(&self) {
        let mut state = lock(&self.shared);
        if !state.pb_connected {
            // Stop retrying only. Without a HELLO response no feature level is
            // negotiated, so no stream-control protocol is selected.
            state.last_error = Some(t().features.stream_handshake_timeout.to_owned());
        }
    }

    /// Handle a peer frame received on the control or session channel.
    pub(crate) fn handle_protocol_message(&self, payload: &[u8]) -> Result<()> {
        let (header, body) = peer::decode(payload)?;
        match header.frame_type() {
            Some(FrameType::Annotation) => return self.handle_native_annotation(payload).map(drop),
            Some(FrameType::Audio) => return self.handle_audio_quality(payload).map(drop),
            Some(FrameType::Hello) => return self.handle_hello(payload),
            Some(FrameType::Cursor) => return self.handle_cursor(body),
            Some(FrameType::HostEvent) => return self.handle_host_event(body),
            Some(FrameType::Permissions) => return self.handle_permissions(body),
            Some(FrameType::Participants) => {
                let list = crate::protocol::participants::decode_participants(body)?;
                // The host sends this only when the list or the turn changes.
                tracing::info!(
                    participants = list.participants.len(),
                    controlling = list.controlling,
                    "session participants received"
                );
                let held = self.participants.holds_control();
                self.participants.receive_list(list);
                let mut state = lock(&self.shared);
                if held && !self.participants.holds_control() {
                    self.turn_lost_locked(&mut state);
                }
                self.apply_playout_locked(&state);
                return Ok(());
            }
            Some(FrameType::Session) => {}
            other => {
                tracing::debug!(kind = header.kind, known = ?other.map(FrameType::name), "ignored peer frame on a control channel");
                return Ok(());
            }
        }
        let Some(received) = super::wire::decode(payload)? else {
            return Ok(());
        };
        if let Some(error) = &received.error {
            tracing::warn!(code = %error.code, reason = %error.reason, "host refused a session request");
            let mut state = lock(&self.shared);
            if let Some(waiting) = state.requests.take(received.request_id) {
                let refusal = waiting.refusal(&error.reason);
                self.settle(&mut state, received.request_id, waiting, Err(refusal));
                return Ok(());
            }
            drop(state);
            // A failed reply settles the request exactly like a lost send.
            self.mark_send_failed(received.sequence(), t().features.stream_host_refused_request);
            return Ok(());
        }
        let sequence = received.sequence();
        let response = received.response;
        let Some(body) = received.body else {
            return Ok(());
        };
        let mut state = lock(&self.shared);
        if response
            && let Some(waiting) = state.requests.take(received.request_id)
        {
            // The answer carries the display list or the stream state the
            // request left.
            match body {
                SessionBody::Displays(list) => {
                    super::display_settings::apply_displays(&mut state, list)
                }
                SessionBody::Stream(stream) => apply_stream_state(&mut state, stream),
                _ => {}
            }
            self.settle(&mut state, received.request_id, waiting, Ok(()));
            self.refresh_mouse_policy(&mut state);
            return Ok(());
        }
        match body {
            SessionBody::Microphone(event) => self.microphone.event(event.state),
            SessionBody::Forward(forward) => {
                tracing::info!(room = %forward.room, generation = forward.generation,
                    tracks = forward.tracks.len(), "host forwarding instruction");
                self.forward.send_replace(Some(forward));
            }
            SessionBody::ClipboardFiles(permission) => {
                state.clipboard_files_allowed = permission.enabled;
            }
            SessionBody::Stream(stream) => apply_stream_state(&mut state, stream),
            SessionBody::Displays(list) => super::display_settings::apply_displays(&mut state, list),
            body if response => match body {
                SessionBody::MicrophoneResult(response) => {
                    self.microphone.response(sequence, response.error_code);
                }
                SessionBody::ControlTurnResult(response) => {
                    self.turn_response_locked(&mut state, sequence, response);
                }
                other => {
                    tracing::debug!(message = other.name(), "unmatched session response")
                }
            },
            other => tracing::debug!(message = other.name(), "ignored session message"),
        }
        self.sync_stream_locked(&mut state);
        self.refresh_mouse_policy(&mut state);
        Ok(())
    }

    /// Host cursor shape (`CURSOR`).
    fn handle_cursor(&self, body: &[u8]) -> Result<()> {
        let was_hidden = self.cursor.hidden();
        let result = match self.cursor.receive(body) {
            Ok(Some(pointers)) => {
                self.participants.receive_pointers(pointers);
                return Ok(());
            }
            Ok(None) => Ok(()),
            Err(error) => Err(error),
        };
        let mut state = lock(&self.shared);
        if was_hidden
            && !self.cursor.hidden()
            && smart_mouse_requested(&state)
            && let Some(cursor) = self.cursor.snapshot()
            && let Some(point) = cursor.sampled_position
        {
            state.mouse_restore_point = Some((cursor.screen_id, point));
        }
        self.refresh_mouse_policy(&mut state);
        drop(state);
        self.mouse.repaint();
        result
    }

    /// Host lifecycle event (`HOST_EVENT`).
    fn handle_host_event(&self, body: &[u8]) -> Result<()> {
        use crate::protocol::host_event::kind;
        let event = crate::protocol::host_event::decode(body)?;
        tracing::info!(kind = event.kind, "host event received");
        let mut state = lock(&self.shared);
        match event.kind {
            kind::UPDATING => {
                // The host installs a locally confirmed update and comes back.
                if let Some(upgrade) = &state.remote_upgrade {
                    upgrade.host_updating();
                }
                state.mouse.disable();
                state.annotation.disconnect();
            }
            kind::SESSION_SWITCH => {
                state.notify_remote(t().features.stream_host_session_switched.into());
            }
            kind::RESTARTING | kind::SLEEPING | kind::SHUTTING_DOWN => {
                state.notify_remote(t().features.stream_host_going_offline.into());
            }
            kind::DISPLAY_CHANGING => {
                let actor: String = event.actor.trim().chars().take(40).collect();
                state.notify_remote(if actor.is_empty() {
                    t().features.stream_host_display_changing.into()
                } else {
                    (t().features.host_display_changed_by)(&actor)
                });
            }
            _ => {}
        }
        Ok(())
    }

    /// Capabilities in effect for this participant (`PERMISSIONS`).
    fn handle_permissions(&self, body: &[u8]) -> Result<()> {
        let grants = crate::protocol::host_event::decode_permissions(body)?;
        tracing::info!(?grants, "session capabilities received");
        self.files
            .permit(grants.iter().any(|cap| cap == farwing_protocol::caps::FILES));
        let mut state = lock(&self.shared);
        // A change during the session is told, except control alone while
        // controlling (granted on request, or ended with its own notice).
        if let Some(old) = state.grants.as_ref().filter(|old| **old != grants) {
            let added: Vec<&String> = grants.iter().filter(|c| !old.contains(c)).collect();
            let removed: Vec<&String> = old.iter().filter(|c| !grants.contains(c)).collect();
            let control_only = added.iter().chain(&removed).all(|c| *c == farwing_protocol::caps::CONTROL);
            if !(control_only && state.mouse.mode() != crate::features::remote_input::MouseMode::View) {
                let labels = |list: &[&String]| {
                    list.iter()
                        .map(|c| crate::application::app::friends::cap_label(c))
                        .collect::<Vec<_>>()
                        .join("、")
                };
                let notice = (t().features.permissions_changed)(&labels(&added), &labels(&removed));
                state.notify_remote(notice);
            }
        }
        state.grants = Some(grants);
        self.refresh_mouse_policy(&mut state);
        drop(state);
        self.protocol_changed.notify_one();
        Ok(())
    }

    /// Peer `HELLO`: record the host's feature levels; answer a request, or
    /// complete the handshake on the response to our own request.
    fn handle_hello(&self, payload: &[u8]) -> Result<()> {
        let Some(received) = peer::decode_hello(payload)? else {
            return Ok(());
        };
        let hello = &received.hello;
        let features = received.features();
        self.files.capabilities(features.has(feature::FILES));
        let mut state = lock(&self.shared);
        state.peer = features;
        let mut response = None;
        let mut handshake_changed = false;
        if !received.response {
            response = Some(encode_pb_echo_response(received.request_id));
            tracing::debug!(
                features = ?hello.features.keys().collect::<Vec<_>>(),
                peer_version = %hello.client_version,
                "peer HELLO received"
            );
        } else {
            state.pb_connected = true;
            state.mouse.set_ready(
                state.mouse_transport_connected
                    && state.control_channel_open
                    && state.text_channel_open
                    && protocol(&state) == StreamControlProtocol::CaptureSetting,
            );
            handshake_changed = true;
            state.last_error = None;
            tracing::info!(
                features = hello.features.len(),
                protocol = protocol(&state).label(),
                peer_version = %hello.client_version,
                "peer feature negotiation completed"
            );
        }
        self.sync_stream_locked(&mut state);
        self.refresh_mouse_policy(&mut state);
        drop(state);
        if handshake_changed {
            self.protocol_changed.notify_one();
        }
        if let Some(response) = response {
            self.echo_responses
                .send(response)
                .map_err(|_| anyhow!("peer HELLO response sender has stopped"))?;
        }
        Ok(())
    }

}
