//! Channel and protocol handshake state transitions.
use super::input::{expire_cursor_request, fail_cursor_request, smart_mouse_requested};
use super::settings::{
    apply_capture_setting_response, prepare_initial_capture_sync, refresh_active_screen,
    reported_auto_quality, restore_confirmed_capture, update_screen_baseline,
    viewing_quality_label,
};
use super::wire::{
    ClipboardPermissionState, PbControlMessage, PbPayload, PbRpcResponsePayload,
    PbSimpleActionParams, encode_pb_echo_response,
};
use super::{
    ACTION_TYPE_ECHO_REQUEST, ACTION_TYPE_ECHO_RESPONSE, PbHandshakeStatus, PbMessageSource,
    StreamControlHandle, StreamControlProtocol, StreamControlState, VIDEO_QUALITY_AUTO,
    ViewingPreferenceUpdate, lock, microphone, protocol,
};
use crate::media::VideoCodec;
use crate::protocol::capability::DualCapability;
use anyhow::{Result, anyhow};
use prost::Message as _;
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
        if label == "TEXT_DATA_CHANNEL" && !open {
            self.disconnect_audio_quality();
        }
        let mut state = lock(&self.shared);
        let membership_before = (
            state.protocol_generation,
            state.pb_connected && state.text_channel_open,
        );
        match label {
            "CONTROL_DATA_CHANNEL" => {
                if state.control_channel_open != open {
                    state.protocol_generation = state.protocol_generation.wrapping_add(1);
                    state.pb_connected = false;
                    state.initial_capture_sync_sent = false;
                }
                state.control_channel_open = open;
            }
            "TEXT_DATA_CHANNEL" => {
                state.text_channel_open = open;
                if !open {
                    state.initial_capture_sync_sent = false;
                }
            }
            _ => return,
        }
        if !open {
            state.entitlement_sync = None;
            state.restore_input_pending = state.device_preferences.control_enabled
                && state.viewing_enabled
                && !state.annotation.enabled
                && !state.annotation.toggling();
            self.microphone.disconnect();
            self.clipboard.suspend();
            state.peer_clipboard = 0;
            state.annotation.disconnect();
            state.annotation_extension = None;
            state.topology.disconnect();
            state
                .display_changes
                .cancel_all("连接已断开，显示设置未确认");
            state.peer_mouse_relative = None;
            state.cursor_sync_needed = false;
            state.cursor_desired_capture = true;
            state.mouse_restore_point = None;
            state.baseline.cursor_capture = true;
            state.mouse.set_ready(false);
            if let Some((sequence, _, _)) = state.cursor_pending {
                fail_cursor_request(&mut state, sequence, "连接已断开，光标设置未确认".into());
            }
            state.registered_video_tracks.clear();
            state.track_registration = None;
            state.track_registration_error = None;
            if let Some(sequence) = state.pending_sequences.back().copied() {
                state
                    .performance
                    .fail_stream_switch(sequence, format!("{label} 通道已关闭"));
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
        let membership_changed = membership_before
            != (
                state.protocol_generation,
                state.pb_connected && state.text_channel_open,
            );
        if membership_changed {
            self.entitlement_connection
                .send_modify(|n| *n = n.wrapping_add(1));
        }
        drop(state);
        if !open {
            self.cursor.clear();
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
            "official dual capability model updated");
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
            state.track_registration_error = Some(format!("视频轨道注册发送失败：{error}"));
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
            // D4C410 only stops retrying. No ECHO response means no negotiated
            // feature version; no protocol is selected without a response.
            state.last_error = Some("PB 特性协商超时；画面继续播放，串流设置尚未就绪".to_owned());
        }
    }

    pub(crate) fn handle_protocol_message(
        &self,
        payload: &[u8],
        source: PbMessageSource,
    ) -> Result<()> {
        if source == PbMessageSource::Text && self.handle_native_annotation(payload)? {
            return Ok(());
        }
        if payload.iter().find(|byte| !byte.is_ascii_whitespace()) == Some(&b'{') {
            if source == PbMessageSource::Text && self.handle_audio_quality(payload)? {
                return Ok(());
            }
            return self.handle_mouse_command(payload, source);
        }
        let message = PbControlMessage::decode(payload)
            .map_err(|error| anyhow!("decode UU protobuf domain message: {error}"))?;
        if let Some(PbPayload::SystemMetrics(bytes)) = &message.payload {
            self.files.metrics(bytes)?;
        }
        if let Some(PbPayload::SystemStateChange(bytes)) = &message.payload {
            if let Some(files) = ClipboardPermissionState::decode(bytes.as_slice())?.files {
                lock(&self.shared).clipboard_files_allowed = files.enabled;
            }
            let was_hidden = self.cursor.hidden();
            let result = self.cursor.receive(bytes);
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
            return result;
        }
        let mut echo_response = None;
        let mut handshake_changed = false;
        let mut state = lock(&self.shared);
        let membership_ready_before = state.pb_connected && state.text_channel_open;
        match message.payload {
            Some(PbPayload::SimpleAction(action)) if matches!(action.action, 23..=27) => {
                self.microphone.event(action.action);
            }
            Some(PbPayload::SimpleAction(action)) if source == PbMessageSource::Control => {
                // F91D10 dispatches CONTROL SimpleAction to the ECHO handler;
                // TEXT and signal_app_data only reach the business observers.
                match action.action {
                    ACTION_TYPE_ECHO_REQUEST | ACTION_TYPE_ECHO_RESPONSE => {
                        if let Some(PbSimpleActionParams::FeatureFlag(flags)) = action.params {
                            self.files
                                .capabilities(flags.file_transfer_ftp, flags.file_transfer_ftp2);
                            state.peer_clipboard = flags.clipboard;
                            state.peer_capture_setting = flags.capture_setting.max(0) as u32;
                        }
                        if action.action == ACTION_TYPE_ECHO_REQUEST {
                            echo_response =
                                Some(encode_pb_echo_response(message.seq, message.timestamp));
                            tracing::debug!(
                                request_sequence = message.seq,
                                capture_setting_feature_level = state.peer_capture_setting,
                                "official protobuf ECHO_REQUEST received"
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
                            state.last_error = (protocol(&state)
                                == StreamControlProtocol::Unsupported)
                                .then(|| "对端不支持当前串流协议（需要CaptureSetting RPC）".into());
                            tracing::info!(
                                capture_setting_feature_level = state.peer_capture_setting,
                                protocol = protocol(&state).label(),
                                "official protobuf feature negotiation completed"
                            );
                        }
                    }
                    _ => {}
                }
            }
            Some(PbPayload::ReportError(report)) => {
                if let Some(upgrade) = &state.remote_upgrade {
                    upgrade.receive(report.error_code);
                    if report.error_code == -6 {
                        state.mouse.disable();
                        state.annotation.disconnect();
                    }
                }
                // Upgrade notifications are attached only to a normal owned
                // computer viewing session; they never execute an inbound updater.
                if report.error_code == -9 {
                    state.remote_notice = Some((
                        Instant::now(),
                        "被控端系统会话发生变化，画面会有短暂卡顿，请稍候",
                    ));
                }
                tracing::debug!(
                    action = report.action,
                    code = report.error_code,
                    type_value = report.type_value,
                    "remote capture status received"
                );
            }
            Some(PbPayload::ReportQosStats(qos)) => {
                if state.baseline.frame_quality == VIDEO_QUALITY_AUTO
                    && let Some(auto_quality) = reported_auto_quality(qos.video_quality)
                {
                    state.baseline.auto_frame_quality = auto_quality;
                    if state.confirmed_preferences.settings == state.settings
                        && state.pending_capture_preferences.is_empty()
                        && state.confirmed_preferences.auto_frame_quality != auto_quality
                        && super::entitlements::can_save_auto_quality(&state)
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
                    fast_bitrate = qos.fast_bitrate, general_bitrate = qos.general_bitrate, hd_bitrate = qos.hd_bitrate, bluray_bitrate = qos.bluray_bitrate,
                    "official QoS quality status received");
            }
            Some(PbPayload::Screens(screens)) => update_screen_baseline(&mut state, screens),
            Some(PbPayload::CaptureSettingSync(bytes)) => {
                // 4.40 F6B150/F62360 -> 1410F2780 (ordinary video) has no
                // tag-25 state consumer. EC6650 is SecondScreenSettingsModel.
                // Keep the oneof tag, but do not import that module's state.
                tracing::debug!(
                    ?source,
                    seq = message.seq,
                    bytes = bytes.len(),
                    "ignored non-viewer CaptureSettingSync; viewing settings unchanged"
                );
            }
            Some(PbPayload::RpcResponse(response)) => {
                if let Some(header) = response.response_header {
                    if !self.handle_topology_response(
                        &mut state,
                        header.request_id,
                        response.payload.as_ref(),
                    ) {
                        match response.payload {
                            Some(PbRpcResponsePayload::UserEntitlementSyncRsp(reply)) => {
                                if state
                                    .entitlement_sync
                                    .as_ref()
                                    .is_some_and(|(id, _)| *id == header.request_id)
                                {
                                    let (_, sender) = state.entitlement_sync.take().unwrap();
                                    let _ = sender.send(reply.error_code);
                                }
                            }
                            Some(PbRpcResponsePayload::VirtualAudioDriverPolicyRsp(bytes)) => {
                                let response =
                                    microphone::PolicyResponse::decode(bytes.as_slice())?;
                                self.microphone
                                    .response(header.request_id, response.error_code);
                            }
                            Some(PbRpcResponsePayload::DrawResp(draw)) => {
                                state.annotation.response(header.request_id, draw)
                            }
                            Some(PbRpcResponsePayload::CaptureSetting(capture)) => {
                                apply_capture_setting_response(
                                    &mut state,
                                    header.request_id,
                                    capture,
                                )
                            }
                            Some(PbRpcResponsePayload::SendVideoTrackRsp(result))
                                if state
                                    .track_registration
                                    .as_ref()
                                    .is_some_and(|(seq, _)| *seq == header.request_id) =>
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
                                    state.track_registration_error = Some(format!(
                                        "视频轨道注册被拒绝（{}）",
                                        result.error_code
                                    ));
                                }
                            }
                            _ => {}
                        }
                    }
                }
            }
            _ => {}
        }
        self.maybe_send_initial_capture_sync(&mut state);
        self.drive_display_changes(&mut state);
        self.refresh_mouse_policy(&mut state);
        let membership_changed =
            membership_ready_before != (state.pb_connected && state.text_channel_open);
        if membership_changed {
            self.entitlement_connection
                .send_modify(|n| *n = n.wrapping_add(1));
        }
        drop(state);
        if handshake_changed {
            self.protocol_changed.notify_one();
        }
        if let Some(response) = echo_response {
            self.echo_responses
                .send(response)
                .map_err(|_| anyhow!("protobuf ECHO_RESPONSE sender has stopped"))?;
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
                state.last_error = Some(format!("初始串流设置未发送：{error}"));
                tracing::warn!(%error, "failed to prepare official initial capture-setting sync");
                return;
            }
        };
        let _ = self.send_locked(state, outgoing, None, false);
    }
}
