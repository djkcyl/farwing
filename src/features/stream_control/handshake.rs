//! Channel and protocol handshake state transitions.
use super::input::{expire_cursor_request, fail_cursor_request, smart_mouse_requested};
use super::settings::{
    apply_capture_setting_response, prepare_initial_capture_sync, refresh_active_screen,
    reported_auto_quality, restore_confirmed_capture, update_screen_baseline,
    viewing_quality_label,
};
use super::wire::{SessionBody, encode_pb_echo_response};
use super::{
    PbHandshakeStatus, PbMessageSource, StreamControlHandle, StreamControlProtocol,
    StreamControlState, VIDEO_QUALITY_AUTO, ViewingPreferenceUpdate, lock, protocol,
};
use crate::i18n::t;
use crate::protocol::peer::{self, FrameType};
use crate::protocol::peer::feature;
use crate::media::VideoCodec;
use crate::protocol::capability::DualCapability;
use anyhow::{Result, anyhow};
use std::sync::Arc;
use std::time::Instant;
use tokio::sync::Notify;

impl StreamControlHandle {
    pub(crate) fn poll_timeouts(&self) {
        let mut state = lock(&self.shared);
        self.drive_display_changes(&mut state);
        expire_cursor_request(&mut state);
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
                    state.initial_capture_sync_sent = false;
                }
                state.control_channel_open = open;
            }
            peer::channel::SESSION => {
                state.text_channel_open = open;
                if !open {
                    state.initial_capture_sync_sent = false;
                }
            }
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
            state.topology.disconnect();
            state
                .display_changes
                .cancel_all(t().features.stream_display_disconnected_unconfirmed);
            state.peer_mouse_relative = None;
            state.cursor_sync_needed = false;
            state.cursor_desired_capture = true;
            state.mouse_restore_point = None;
            state.baseline.cursor_capture = true;
            state.mouse.set_ready(false);
            if let Some((sequence, _, _)) = state.cursor_pending {
                fail_cursor_request(
                    &mut state,
                    sequence,
                    t().features.stream_cursor_disconnected_unconfirmed.into(),
                );
            }
            state.registered_video_tracks.clear();
            state.track_registration = None;
            state.track_registration_error = None;
            if let Some(sequence) = state.pending_sequences.back().copied() {
                state
                    .performance
                    .fail_stream_switch(sequence, (t().features.stream_channel_closed)(label));
            }
            state.pending_sequences.clear();
            state.pending_capture_preferences.clear();
        }
        self.maybe_send_initial_capture_sync(&mut state);
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
        state.active_video_track_index = video_track_index;
        // The initial CaptureSetting may already have selected another codec
        // before the first RTP packet. Do not overwrite that intent with an
        // old-format packet still in flight during the change.
        if !state.initial_capture_sync_sent {
            state.baseline.codec_type = match codec {
                VideoCodec::H264 => 1,
                VideoCodec::H265 => 2,
                VideoCodec::Av1 => 5,
            };
        }
        refresh_active_screen(&mut state);
        self.maybe_send_initial_capture_sync(&mut state);
    }

    pub(crate) fn set_capability(&self, capability: DualCapability) {
        tracing::info!(capability = %serde_json::to_string(&capability).expect("integer capability fields serialize"),
            "dual capability model updated");
        let mut state = lock(&self.shared);
        state.capability = Some(capability);
        // Complete a pending initial configuration once its actual inputs
        // exist. After it is submitted, late/duplicate capabilities do not
        // trigger stream changes or decoder restarts.
        self.maybe_send_initial_capture_sync(&mut state);
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
        if self.turn_send_failed(&mut state, sequence) {
            return;
        }
        if self.topology_send_failed(&mut state, sequence, error) {
            return;
        }
        if state.display_changes.send_failed(sequence, error) {
            return;
        }
        state
            .pending_capture_preferences
            .retain(|pending| pending.sequence != sequence);
        fail_cursor_request(&mut state, sequence, error.to_owned());
        if state
            .track_registration
            .as_ref()
            .is_some_and(|(seq, _)| *seq == sequence)
        {
            state.track_registration = None;
            state.track_registration_error =
                Some((t().features.stream_track_registration_send_failed)(error));
            return;
        }
        state
            .pending_sequences
            .retain(|pending| *pending != sequence);
        if state.latest_requested_sequence != Some(sequence) {
            return;
        }
        state.last_error = Some(error.to_owned());
        restore_confirmed_capture(&mut state);
        state.performance.fail_stream_switch(sequence, error);
    }

    pub(crate) fn protocol_notifications(&self) -> Arc<Notify> {
        Arc::clone(&self.protocol_changed)
    }

    pub(crate) fn set_viewing_enabled(&self, enabled: bool) {
        let mut state = lock(&self.shared);
        if enabled && !state.viewing_enabled {
            state.initial_capture_sync_sent = false;
            state.restore_input_pending = state.device_preferences.control_enabled;
        }
        state.viewing_enabled = enabled;
        if !enabled {
            state.restore_input_pending = false;
            self.disable_microphone_locked(&mut state);
            self.clipboard.suspend();
        }
        if enabled {
            self.maybe_send_initial_capture_sync(&mut state);
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
    pub(crate) fn handle_protocol_message(
        &self,
        payload: &[u8],
        source: PbMessageSource,
    ) -> Result<()> {
        let (header, body) = peer::decode(payload)?;
        match header.frame_type() {
            Some(FrameType::Annotation) => return self.handle_native_annotation(payload).map(drop),
            Some(FrameType::Audio) => return self.handle_audio_quality(payload).map(drop),
            Some(FrameType::PointerPolicy) => return self.handle_mouse_command(body, source),
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
            // A failed reply settles the request exactly like a lost send.
            tracing::warn!(code = %error.code, message = %error.message, "host rejected a session request");
            // Its message is in the host's language: the log keeps it.
            self.mark_send_failed(received.sequence(), t().features.stream_host_refused_request);
            return Ok(());
        }
        let sequence = received.sequence();
        let response = received.response;
        let Some(body) = received.body else {
            return Ok(());
        };
        let mut state = lock(&self.shared);
        match body {
            SessionBody::Microphone(event) => self.microphone.event(event.state),
            SessionBody::ClipboardFiles(permission) => {
                state.clipboard_files_allowed = permission.enabled;
            }
            SessionBody::Quality(qos) => {
                if state.baseline.frame_quality == VIDEO_QUALITY_AUTO
                    && let Some(auto_quality) = reported_auto_quality(qos.video_quality)
                {
                    state.baseline.auto_frame_quality = auto_quality;
                    if state.confirmed_preferences.settings == state.settings
                        && state.pending_capture_preferences.is_empty()
                        && state.confirmed_preferences.auto_frame_quality != auto_quality
                        && super::dev_auto_quality().is_none()
                    {
                        state.confirmed_preferences.auto_frame_quality = auto_quality;
                        let update = if state.user_settings_requested {
                            ViewingPreferenceUpdate::Settings(state.confirmed_preferences.saved())
                        } else {
                            ViewingPreferenceUpdate::AutoQuality(auto_quality)
                        };
                        state.preference_updates.send_replace(Some(update));
                    }
                    state.performance.set_quality(viewing_quality_label(&state));
                }
                tracing::debug!(encoder_type = %qos.encoder_type, capture_type = %qos.capture_type, probe_bps = qos.probe_bps, video_quality = qos.video_quality,
                    "QoS quality status received");
            }
            SessionBody::Screens(screens) => update_screen_baseline(&mut state, screens),
            body if response => {
                if !self.handle_topology_response(&mut state, sequence, &body) {
                    match body {
                        SessionBody::MicrophoneResult(response) => {
                            self.microphone.response(sequence, response.error_code);
                        }
                        SessionBody::ControlTurnResult(response) => {
                            self.turn_response_locked(&mut state, sequence, response);
                        }
                        SessionBody::CaptureSettingResult(capture) => {
                            apply_capture_setting_response(&mut state, sequence, capture)
                        }
                        SessionBody::DisplayResult(result)
                            if state
                                .track_registration
                                .as_ref()
                                .is_some_and(|(seq, _)| *seq == sequence) =>
                        {
                            let (_, tracks) = state
                                .track_registration
                                .take()
                                .expect("matching registration");
                            if result.error_code == 0 {
                                tracing::info!(
                                    ?tracks,
                                    "remote video track pool registration confirmed"
                                );
                                state.registered_video_tracks = tracks;
                            } else {
                                state.track_registration_error = Some((t()
                                    .features
                                    .stream_track_registration_refused)(
                                    result.error_code,
                                ));
                            }
                        }
                        other => {
                            tracing::debug!(message = other.name(), "unmatched session response")
                        }
                    }
                }
            }
            other => tracing::debug!(message = other.name(), "ignored session message"),
        }
        self.maybe_send_initial_capture_sync(&mut state);
        self.drive_display_changes(&mut state);
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
        tracing::info!(kind = event.kind, downtime_ms = event.expected_downtime_ms, "host event received");
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
                state.remote_notice =
                    Some((Instant::now(), t().features.stream_host_session_switched.into()));
            }
            kind::RESTARTING | kind::SLEEPING | kind::SHUTTING_DOWN => {
                state.remote_notice =
                    Some((Instant::now(), t().features.stream_host_going_offline.into()));
            }
            kind::DISPLAY_CHANGING => {
                let actor: String = event.actor.trim().chars().take(40).collect();
                state.remote_notice = Some((
                    Instant::now(),
                    if actor.is_empty() {
                        t().features.stream_host_display_changing.into()
                    } else {
                        (t().features.host_display_changed_by)(&actor)
                    },
                ));
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
            state.last_error = (protocol(&state) == StreamControlProtocol::Unsupported)
                .then(|| t().features.stream_protocol_unsupported.into());
            tracing::info!(
                features = hello.features.len(),
                protocol = protocol(&state).label(),
                peer_version = %hello.client_version,
                "peer feature negotiation completed"
            );
        }
        self.maybe_send_initial_capture_sync(&mut state);
        self.drive_display_changes(&mut state);
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

    pub(super) fn maybe_send_initial_capture_sync(&self, state: &mut StreamControlState) {
        if !state.viewing_enabled {
            return;
        }
        self.maybe_register_video_tracks(state);
        let outgoing = match prepare_initial_capture_sync(state) {
            Ok(Some(outgoing)) => outgoing,
            Ok(None) => return,
            Err(error) => {
                state.last_error = Some((t().features.stream_initial_settings_send_failed)(
                    &error.to_string(),
                ));
                tracing::warn!(%error, "failed to prepare the initial capture-setting sync");
                return;
            }
        };
        let _ = self.send_locked(state, outgoing, None);
    }
}
