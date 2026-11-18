//! Capture proposals, validation, acknowledgement and saved preference policy.
use super::display_settings::RemoteDisplayInfo;
use super::input::{expire_cursor_request, fail_cursor_request};
use super::wire::{
    PbCaptureSettingRequest, PbCaptureSettingResponse, PbError, PbPayload, PbRequestHeader,
    PbRpcRequest, PbRpcRequestPayload, PbScreenSources, encode_envelope,
};
use super::{
    CAPTURE_RESULT_FPS_ADJUSTED, CHROMA_420, CHROMA_444, CaptureSettingBaseline,
    EXISTING_SESSION_TRACKS, FPS_30, FPS_60, FPS_90, FPS_144, MAX_CUSTOM_BITRATE_MBPS,
    OutgoingControlMessage, RESOLUTION_DEFAULT, RemoteDisplayState, ScreenBaseline,
    StreamControlHandle, StreamControlPreferences, StreamControlProtocol, StreamControlSettings,
    StreamControlState, StreamQuality, UNCHANGED_PHYSICAL_DIMENSION, VIDEO_QUALITY_AUTO,
    VIDEO_QUALITY_BLURAY, VIDEO_QUALITY_CUSTOM, VIDEO_QUALITY_FAST, VIDEO_QUALITY_GENERAL,
    VIDEO_QUALITY_HD, ViewingPreferenceUpdate, custom_bitrate_supported, ensure_ready,
    feature_supported, lock, protocol,
};
use crate::media::FrameRateChoice;
use crate::protocol::capability::{FrameQualityCapability, QUALITY_DIMENSIONS};
use anyhow::{Result, anyhow, bail};
use prost::Message as _;

impl StreamControlHandle {
    pub(crate) fn restore_preferences(&self, preferences: StreamControlPreferences) -> Result<()> {
        let mut state = lock(&self.shared);
        if state.initial_capture_sync_sent || state.baseline.codec_type != 0 {
            bail!("串流偏好必须在新房间选择视频轨道之前恢复");
        }
        state.custom_bitrate_limit = preferences
            .custom_bitrate_limit
            .unwrap_or(MAX_CUSTOM_BITRATE_MBPS);
        set_requested_settings(&mut state, preferences.settings)?;
        state.baseline.auto_frame_quality = preferences.auto_frame_quality;
        state.confirmed_preferences = preferences;
        if let Some(audio) = preferences.audio {
            self.audio.set_settings(audio);
        }
        state.performance.set_quality(viewing_quality_label(&state));
        tracing::info!(
            ?preferences,
            "restored viewing preferences for a new room generation"
        );
        drop(state);
        if let Some(device) = preferences.device {
            self.restore_device_preferences(device);
        }
        Ok(())
    }

    pub fn apply(&self, settings: StreamControlSettings) -> Result<i64> {
        self.apply_settings_locked(&mut lock(&self.shared), settings)
    }

    pub(crate) fn set_custom_bitrate_limit(&self, limit: Option<u32>) {
        lock(&self.shared).custom_bitrate_limit = limit
            .filter(|v| (1..=MAX_CUSTOM_BITRATE_MBPS).contains(v))
            .unwrap_or(MAX_CUSTOM_BITRATE_MBPS);
    }

    pub fn apply_color(
        &self,
        screen_id: i32,
        enabled: bool,
        quality: Option<StreamQuality>,
    ) -> Result<i64> {
        let mut state = lock(&self.shared);
        ensure_ready(&state)?;
        anyhow::ensure!(color_supported(&state), "当前会话不支持色彩切换");
        anyhow::ensure!(
            state.screens.iter().any(|screen| screen.id == screen_id),
            "显示器已断开"
        );
        let mut settings = state.settings;
        settings.true_color = enabled;
        if let Some(quality) = quality {
            settings.quality = quality;
        }
        self.apply_settings_target(&mut state, settings, Some(screen_id))
    }

    pub(crate) fn propose_format(
        &self,
        true_color: Option<bool>,
        hdr: Option<bool>,
    ) -> Result<StreamControlSettings> {
        format_proposal(&lock(&self.shared), true_color, hdr)
    }

    pub(crate) fn apply_format(
        &self,
        screen_id: i32,
        settings: StreamControlSettings,
    ) -> Result<i64> {
        self.apply_settings_target(&mut lock(&self.shared), settings, Some(screen_id))
    }

    pub(super) fn apply_settings_locked(
        &self,
        state: &mut StreamControlState,
        settings: StreamControlSettings,
    ) -> Result<i64> {
        self.apply_settings_target(state, settings, None)
    }

    pub(super) fn apply_settings_target(
        &self,
        mut state: &mut StreamControlState,
        settings: StreamControlSettings,
        screen: Option<i32>,
    ) -> Result<i64> {
        {
            expire_cursor_request(&mut state);
            ensure_ready(&state)?;
            let active_protocol = protocol(&state);
            if matches!(settings.quality, StreamQuality::Custom)
                && !custom_bitrate_supported(&state)
            {
                bail!("被控端不支持运行时自定义码率");
            }
            anyhow::ensure!(
                settings.quality != StreamQuality::Custom
                    || settings.custom_bitrate_mbps <= state.custom_bitrate_limit
                    || (state.settings.quality == StreamQuality::Custom
                        && settings.custom_bitrate_mbps == state.settings.custom_bitrate_mbps),
                "当前连接自定义码率最高为 {} Mbps",
                state.custom_bitrate_limit
            );
            let quality_changed = settings.quality != state.settings.quality;
            let color_changed = settings.true_color != state.settings.true_color;
            let hdr_changed = settings.hdr != state.settings.hdr;
            if hdr_changed {
                anyhow::ensure!(state.peer_capture_setting >= 6, "当前会话不支持 HDR 切换");
                if settings.hdr {
                    ensure_hdr_displays(&state)?;
                }
            }
            if color_changed {
                anyhow::ensure!(color_supported(&state), "当前会话不支持色彩切换");
            }
            let selected = if color_changed
                || hdr_changed
                || (quality_changed && !matches!(settings.quality, StreamQuality::Custom))
            {
                validate_format(&state, settings.quality, settings.true_color, settings.hdr)?
            } else {
                None
            };
            let previous_quality = state.settings.quality;
            set_requested_settings(&mut state, settings)?;
            if settings.quality == StreamQuality::Auto
                && matches!(
                    previous_quality,
                    StreamQuality::Clear | StreamQuality::High | StreamQuality::Original
                )
            {
                state.baseline.auto_frame_quality = previous_quality.protobuf();
            }
            if let Some(selected) = selected {
                apply_codec_limits(&mut state, selected);
            }
            constrain_auto_quality(&mut state);
            tracing::debug!(
                screen_id = EXISTING_SESSION_TRACKS,
                enable_hdr = state.baseline.enable_hdr,
                requested_fps = state.baseline.requested_fps,
                fps_count = state.baseline.fps_count,
                frame_quality = state.baseline.frame_quality,
                auto_frame_quality = state.baseline.auto_frame_quality,
                max_scale_width = state.baseline.max_scale_width,
                max_scale_height = state.baseline.max_scale_height,
                chroma_format = state.baseline.chroma_format,
                codec_type = state.baseline.codec_type,
                max_custom_bitrate = state.baseline.max_custom_bitrate,
                "runtime capture-setting snapshot prepared"
            );

            let sequence = state.next_sequence;
            let payload = match active_protocol {
                StreamControlProtocol::CaptureSetting => {
                    let mut request = capture_setting_request(state.baseline)?;
                    if let Some(id) = screen {
                        let _screen = state
                            .screens
                            .iter()
                            .find(|s| s.id == id)
                            .ok_or_else(|| anyhow!("显示器已断开"))?;
                        request.screen_id = id;
                        // Format/quality changes do not own the physical mode.
                        // A cached ScreenSources size may predate a game's mode switch.
                    }
                    encode_capture_request(sequence, request)
                }

                StreamControlProtocol::Negotiating => bail!("PB 特性协商尚未完成"),
                StreamControlProtocol::Unsupported => {
                    bail!("对端不支持当前串流协议（需要CaptureSetting RPC）")
                }
            };
            state.next_sequence = state.next_sequence.wrapping_add(1);
            state.pending_sequences.push_back(sequence);
            // An explicit full snapshot also satisfies initial synchronization,
            // including a supported choice after a rejected restored preference.
            state.initial_capture_sync_sent = true;
            state.last_error = None;
            state.last_notice = None;
            let outgoing = OutgoingControlMessage {
                annotation_generation: None,
                sequence,
                payload,
                protocol: active_protocol,
                completion: None,
            };
            let switch_target = format!(
                "{} · {}",
                settings.quality.label(),
                settings.frame_rate.label(state.local_display)
            );
            state.user_settings_requested = true;
            let sent = self.send_locked(&mut state, outgoing, Some(switch_target))?;
            Ok(sent)
        }
    }
}

pub(super) fn set_requested_settings(
    state: &mut StreamControlState,
    mut settings: StreamControlSettings,
) -> Result<()> {
    normalize_low_quality(&mut settings);
    if settings.quality == StreamQuality::Custom
        && !(1..=MAX_CUSTOM_BITRATE_MBPS).contains(&settings.custom_bitrate_mbps)
    {
        bail!("自定义码率必须在 1..={MAX_CUSTOM_BITRATE_MBPS} Mbps 之间");
    }
    let requested_fps = settings.frame_rate.value(state.local_display);
    state.baseline.requested_fps = requested_fps;
    state.baseline.fps_count = state
        .local_display
        .refresh_hz
        .clamp(1, requested_fps.max(1));
    state.baseline.frame_quality = settings.quality.protobuf();
    state.baseline.enable_hdr = settings.hdr;
    state.baseline.chroma_format = if settings.true_color {
        CHROMA_444
    } else {
        CHROMA_420
    };
    state.baseline.max_custom_bitrate = match settings.quality {
        StreamQuality::Custom => {
            settings.custom_bitrate_mbps.min(state.custom_bitrate_limit) * 1_000_000
        }
        _ => 0,
    };
    state.settings = settings;
    Ok(())
}

pub(super) fn normalize_low_quality(settings: &mut StreamControlSettings) {
    // Current 4.40.1 buildCaptureConfig also normalizes an actual low-tier
    // capability fallback to Custom 1M; this is not a separate menu entry.
    if settings.quality == StreamQuality::Fast {
        settings.quality = StreamQuality::Custom;
        settings.custom_bitrate_mbps = 1;
    }
}

pub(super) fn prepare_initial_capture_sync(
    state: &mut StreamControlState,
) -> Result<Option<OutgoingControlMessage>> {
    if state.initial_capture_sync_sent
        || !state.control_channel_open
        || !state.text_channel_open
        || !state.pb_connected
        || state.remote_display.is_none()
    {
        return Ok(None);
    }
    let active_protocol = protocol(state);
    if matches!(state.settings.quality, StreamQuality::Custom) && !custom_bitrate_supported(&state)
    {
        bail!("新连接的被控端不支持自定义码率，不能恢复该选择");
    }
    if (state.settings.hdr || state.settings.true_color) && state.capability.is_none() {
        return Ok(None);
    }
    if let Some(cap) = &state.capability {
        let original = state.settings;
        let hdr_allowed = ensure_hdr_displays(state).is_ok();
        let color_allowed = color_supported(state);
        let candidates = [
            (original.true_color, original.hdr),
            (false, original.hdr),
            (original.true_color, false),
            (false, false),
        ];
        if let Some((color, hdr)) = candidates.into_iter().find(|(color, hdr)| {
            (!*hdr || hdr_allowed)
                && (!*color || color_allowed)
                && cap.select(if *color { 3 } else { 1 }, *hdr, 0).result == 0
        }) {
            if color != original.true_color || hdr != original.hdr {
                let mut settings = original;
                settings.true_color = color;
                settings.hdr = hdr;
                set_requested_settings(state, settings)?;
                state.last_notice =
                    Some("当前设备组合无法恢复原色彩/HDR选择，本次连接已使用可用模式".into());
            }
        }
    }
    if let Some(capability) = &state.capability {
        let requested = state.settings.quality.capability_quality();
        let selected = capability.select(
            state.baseline.chroma_format as u8,
            state.settings.hdr,
            requested,
        );
        if selected.result != 0 {
            bail!(
                "双端没有可用的所选色彩视频格式（协商结果{}）",
                selected.result
            );
        }
        if !matches!(requested, 0 | 5) && selected.max_frame_quality < requested {
            let mut settings = state.settings;
            settings.quality =
                quality_from_capability(selected.max_frame_quality).ok_or_else(|| {
                    anyhow!("invalid negotiated quality {}", selected.max_frame_quality)
                })?;
            set_requested_settings(state, settings)?;
            state.last_notice = Some(format!(
                "初始画质按双端能力调整为{}",
                settings.quality.label()
            ));
        }
        apply_codec_limits(state, selected);
    }
    // Submit from screen state and negotiated capabilities, independently
    // of first-frame delivery. The evidence is in the controller-route doc.
    // If neither negotiation nor RTP supplied a codec, keep waiting rather
    // than inventing a default codec or an incomplete RPC.
    if state.baseline.codec_type == 0 {
        return Ok(None);
    }
    constrain_auto_quality(state);
    let baseline = state.baseline;
    let sequence = state.next_sequence;
    let payload = match active_protocol {
        StreamControlProtocol::CaptureSetting => encode_capture_setting(sequence, baseline)?,

        StreamControlProtocol::Negotiating | StreamControlProtocol::Unsupported => return Ok(None),
    };
    state.next_sequence = state.next_sequence.wrapping_add(1);
    state.pending_sequences.push_back(sequence);
    state.initial_capture_sync_sent = true;
    tracing::info!(
        sequence,
        protocol = active_protocol.label(),
        requested_fps = baseline.requested_fps,
        fps_count = baseline.fps_count,
        frame_quality = baseline.frame_quality,
        auto_frame_quality = baseline.auto_frame_quality,
        max_custom_bitrate = baseline.max_custom_bitrate,
        screen_id = EXISTING_SESSION_TRACKS,
        physical_width = UNCHANGED_PHYSICAL_DIMENSION,
        physical_height = UNCHANGED_PHYSICAL_DIMENSION,
        max_scale_width = baseline.max_scale_width,
        max_scale_height = baseline.max_scale_height,
        codec_type = baseline.codec_type,
        "official initial capture-setting snapshot prepared"
    );
    Ok(Some(OutgoingControlMessage {
        annotation_generation: None,
        sequence,
        payload,
        protocol: active_protocol,
        completion: None,
    }))
}

pub(super) fn quality_from_capability(quality: i32) -> Option<StreamQuality> {
    match quality {
        1 => Some(StreamQuality::Fast),
        2 => Some(StreamQuality::Clear),
        3 => Some(StreamQuality::High),
        4 => Some(StreamQuality::Original),
        _ => None,
    }
}

pub(super) fn color_supported(state: &StreamControlState) -> bool {
    state.peer_capture_setting >= 3
        && feature_supported(
            state,
            crate::account::feature_ability::Feature::ScreenChroma,
        )
        && state.capability.is_some()
}

pub(super) fn validate_color(state: &StreamControlState, enabled: bool) -> Result<()> {
    anyhow::ensure!(color_supported(state), "当前会话未开放色彩切换");
    validate_format(state, state.settings.quality, enabled, state.settings.hdr)?;
    Ok(())
}

pub(super) fn validate_format(
    state: &StreamControlState,
    quality: StreamQuality,
    true_color: bool,
    hdr: bool,
) -> Result<Option<FrameQualityCapability>> {
    let Some(capability) = &state.capability else {
        return Ok(None);
    };
    let requested = quality.capability_quality();
    let chroma = if true_color { 3 } else { 1 };
    let selected = capability.select(chroma, hdr, requested);
    let color = match (true_color, hdr) {
        (true, true) => "4:4:4 HDR",
        (true, false) => "4:4:4",
        (false, true) => "4:2:0 HDR",
        (false, false) => "4:2:0",
    };
    if selected.result != 0 {
        bail!(
            "{color} 下双端没有共同可用的视频编码，无法使用{}",
            quality.label()
        );
    }
    if !matches!(requested, 0 | 5) && selected.max_frame_quality < requested {
        let (width, height) = QUALITY_DIMENSIONS[(requested - 1) as usize];
        let best = quality_from_capability(selected.max_frame_quality)
            .map_or("更低档位", StreamQuality::label);
        let reason = if !capability.hardware(true, chroma, hdr) {
            "：本机没有可用的硬件解码，只能软件解码"
        } else if !capability.hardware(false, chroma, hdr) {
            "：被控端没有可用的硬件编码，只能软件编码"
        } else {
            ""
        };
        bail!(
            "{}需要 {width}×{height}，{color} 下双端最高只到 {}×{}（{best}）{reason}",
            quality.label(),
            selected.max_width,
            selected.max_height,
        );
    }
    Ok(Some(selected))
}

pub(super) fn ensure_hdr_displays(state: &StreamControlState) -> Result<()> {
    anyhow::ensure!(state.peer_capture_setting >= 6, "当前会话不支持 HDR");
    let cap = state
        .capability
        .as_ref()
        .ok_or_else(|| anyhow!("正在等待 HDR 能力"))?;
    anyhow::ensure!(
        cap.remote_display_info
            .iter()
            .any(|display| display.hdr == 0),
        "请先在被控端支持 HDR 的屏幕上开启 Windows HDR"
    );
    anyhow::ensure!(
        cap.local_display_info
            .iter()
            .any(|display| display.hdr == 0),
        "请先在本机支持 HDR 的屏幕上开启 Windows HDR"
    );
    Ok(())
}

pub(super) fn format_proposal(
    state: &StreamControlState,
    color: Option<bool>,
    hdr: Option<bool>,
) -> Result<StreamControlSettings> {
    let mut settings = state.settings;
    if let Some(enabled) = color {
        anyhow::ensure!(color_supported(state), "当前会话不支持色彩切换");
        settings.true_color = enabled;
    }
    if let Some(enabled) = hdr {
        anyhow::ensure!(state.peer_capture_setting >= 6, "当前会话不支持 HDR");
        settings.hdr = enabled;
    }
    if hdr == Some(true) {
        ensure_hdr_displays(state)?;
    }
    let cap = state
        .capability
        .as_ref()
        .ok_or_else(|| anyhow!("正在等待串流能力"))?;
    let mut selected = cap.select(
        if settings.true_color { 3 } else { 1 },
        settings.hdr,
        settings.quality.capability_quality(),
    );
    if selected.result != 0 && settings.hdr && settings.true_color {
        if hdr == Some(true) {
            selected = cap.select(1, true, settings.quality.capability_quality());
            if selected.result == 0 {
                settings.true_color = false;
            }
        } else if color == Some(true) {
            selected = cap.select(3, false, settings.quality.capability_quality());
            if selected.result == 0 {
                settings.hdr = false;
            }
        }
    }
    anyhow::ensure!(selected.result == 0, "双方设备不支持所选色彩与 HDR 组合");
    let requested = settings.quality.capability_quality();
    if !matches!(requested, 0 | 5) && selected.max_frame_quality < requested {
        settings.quality = quality_from_capability(selected.max_frame_quality)
            .ok_or_else(|| anyhow!("无可用画质档位"))?;
    }
    Ok(settings)
}

pub(super) fn apply_codec_limits(state: &mut StreamControlState, selected: FrameQualityCapability) {
    state.baseline.codec_type = selected.video_codec;
    state.baseline.max_scale_width = selected.max_width as u32;
    state.baseline.max_scale_height = selected.max_height as u32;
}

pub(super) fn constrain_auto_quality(state: &mut StreamControlState) {
    if state.baseline.frame_quality != VIDEO_QUALITY_AUTO {
        return;
    }
    if let Some(row) = state
        .capability
        .as_ref()
        .and_then(|cap| {
            cap.exact(
                state.baseline.codec_type,
                state.baseline.chroma_format as u8,
                state.settings.hdr,
            )
        })
        .filter(|row| row.result == 0)
        && let Some(maximum) = quality_from_capability(row.max_frame_quality)
        && state.baseline.auto_frame_quality > maximum.protobuf()
    {
        state.baseline.auto_frame_quality = maximum.protobuf();
    }
}

pub(super) fn update_screen_baseline(state: &mut StreamControlState, screens: PbScreenSources) {
    let previous_screens = state.screens.clone();
    state.screens_generation = state.screens_generation.wrapping_add(1);
    state.current_screen_id = screens.current_screen_id;
    state.screens = screens
        .screens
        .into_iter()
        .filter_map(|screen| {
            let display = RemoteDisplayInfo::from_screen(&screen)?;
            let resolution = screen.current_resolution?;
            let width = u32::try_from(resolution.width).ok()?;
            let height = u32::try_from(resolution.height).ok()?;
            if width == 0 || height == 0 {
                return None;
            }
            Some(ScreenBaseline {
                id: screen.id,
                name: screen.display_name,
                primary: screen.is_primary_screen,
                video_track_index: screen.video_track_index,
                fps: u32::try_from(screen.fps).unwrap_or_default(),
                width,
                height,
                pixel_width: u32::try_from(resolution.pixel_width).unwrap_or_default(),
                pixel_height: u32::try_from(resolution.pixel_height).unwrap_or_default(),
                dpi_scale: screen
                    .dpi_scale
                    .map(|dpi| u32::try_from(dpi.current_dpi).unwrap_or_default())
                    .unwrap_or_default(),
                resolution_type: screen.resolution_type,
                display,
            })
        })
        .collect();
    if previous_screens.iter().any(|old| {
        !state.screens.iter().any(|new| {
            old.id == new.id
                && old.video_track_index == new.video_track_index
                && old.display.screen_type == new.display.screen_type
        })
    }) {
        state.mouse.pause_layout();
    }
    state
        .topology
        .observe(state.screens_generation, &state.screens);
    for screen in &state.screens {
        tracing::debug!(screen_id = screen.id, name = %screen.name,
            screen_type = screen.display.screen_type,
            primary = screen.primary, track = screen.video_track_index,
            width = screen.width, height = screen.height, "remote screen mapping");
    }
    refresh_active_screen(state);
}

pub(super) fn refresh_active_screen(state: &mut StreamControlState) {
    let selected = state
        .screens
        .iter()
        .find(|screen| screen.video_track_index == state.active_video_track_index)
        .or_else(|| {
            state
                .screens
                .iter()
                .find(|screen| screen.id == state.current_screen_id)
        })
        .or_else(|| state.screens.first())
        .cloned();
    let Some(screen) = selected else {
        return;
    };
    state.remote_display = Some(RemoteDisplayState {
        screen_id: screen.id,
        video_track_index: screen.video_track_index,
        width: screen.width,
        height: screen.height,
        refresh_hz: screen.fps,
    });
    tracing::info!(
        screen_id = screen.id,
        video_track_index = screen.video_track_index,
        width = screen.width,
        height = screen.height,
        pixel_width = screen.pixel_width,
        pixel_height = screen.pixel_height,
        dpi_scale = screen.dpi_scale,
        resolution_type = screen.resolution_type,
        "official active-screen baseline synchronized"
    );
}

pub(super) fn apply_capture_setting_response(
    state: &mut StreamControlState,
    request_id: i64,
    response: PbCaptureSettingResponse,
) {
    if state.display_changes.ack(request_id, &response) {
        return;
    }
    let current = state.latest_requested_sequence == Some(request_id)
        && state.pending_sequences.contains(&request_id);
    let mut reported_color = None;
    let mut failures = Vec::new();
    let mut notices = Vec::new();
    for error in response.errors {
        match error.error_code {
            0 => {}
            -6 => {
                match serde_json::from_str::<serde_json::Value>(&error.error_detail)
                    .ok()
                    .and_then(|v| v.get("error_code").and_then(|n| n.as_i64()))
                {
                    Some(code @ (0 | 1 | 2 | 3 | 4 | 5)) => {
                        reported_color = Some(matches!(code, 0 | 3 | 4));
                        if matches!(code, 1 | 2) {
                            notices.push("远端未能启用 YUV 4:4:4，已使用 YUV 4:2:0".into());
                        }
                        if matches!(code, 3 | 4) {
                            notices.push("远端已保留 YUV 4:4:4，其他显示操作未完全生效".into());
                        }
                    }
                    _ => failures.push(format_pb_error(error)),
                }
            }
            CAPTURE_RESULT_FPS_ADJUSTED => notices.push(format!(
                "远端屏幕刷新率低于请求档位，串流已按屏幕能力降档（{}）",
                format_pb_error(error)
            )),
            _ => failures.push(format_pb_error(error)),
        }
    }
    if let Some(enabled) = reported_color
        && let Some(pending) = state
            .pending_capture_preferences
            .iter_mut()
            .find(|p| p.sequence == request_id)
    {
        pending.preferences.settings.true_color = enabled;
    }
    finish_request(state, request_id, failures, notices);
    if current && let Some(enabled) = reported_color {
        apply_reported_color(state, enabled, state.user_settings_requested);
    }
}

pub(super) fn apply_reported_color(state: &mut StreamControlState, enabled: bool, persist: bool) {
    state.settings.true_color = enabled;
    state.baseline.chroma_format = if enabled { CHROMA_444 } else { CHROMA_420 };
    let changed = state.confirmed_preferences.settings.true_color != enabled;
    state.confirmed_preferences.settings.true_color = enabled;
    if let Ok(Some(selected)) =
        validate_format(state, state.settings.quality, enabled, state.settings.hdr)
    {
        apply_codec_limits(state, selected);
    }
    if changed && persist {
        state
            .preference_updates
            .send_replace(Some(ViewingPreferenceUpdate::Settings(
                state.confirmed_preferences.saved(),
            )));
    }
}

pub(super) fn format_pb_error(error: PbError) -> String {
    if error.error_message.is_empty() && error.error_detail.is_empty() {
        error.error_code.to_string()
    } else if error.error_detail.is_empty() {
        format!("{}: {}", error.error_code, error.error_message)
    } else {
        format!(
            "{}: {} ({})",
            error.error_code, error.error_message, error.error_detail
        )
    }
}

pub(super) fn reported_auto_quality(quality: i32) -> Option<i32> {
    // F58210 -> 1042310 rejects the uninitialized PB value 0 before
    // B9A320 translates it to the GUI enum. F09580/BA3F10 then preserve
    // the translated auto sub-tier. Do not let startup QoS reset it to Clear.
    match quality {
        0 => None,
        VIDEO_QUALITY_FAST..=VIDEO_QUALITY_CUSTOM => Some(quality),
        // B9A320's unknown nonzero value maps to GUI0, which B9A2B0
        // writes back as PB General. This is not the zero/uninitialized case.
        _ => Some(VIDEO_QUALITY_GENERAL),
    }
}

pub(super) fn quality_name(quality: i32) -> &'static str {
    match quality {
        VIDEO_QUALITY_BLURAY => "原画",
        VIDEO_QUALITY_HD => "超清",
        VIDEO_QUALITY_GENERAL => "高清",
        VIDEO_QUALITY_FAST => "低码率",
        _ => "高清",
    }
}

pub(super) fn official_quality_label(baseline: &CaptureSettingBaseline) -> String {
    match baseline.frame_quality {
        VIDEO_QUALITY_FAST..=VIDEO_QUALITY_BLURAY => {
            quality_name(baseline.frame_quality).to_owned()
        }
        VIDEO_QUALITY_AUTO => format!("自动（{}）", quality_name(baseline.auto_frame_quality)),
        VIDEO_QUALITY_CUSTOM if baseline.max_custom_bitrate >= 1_000_000 => {
            format!("{} Mbps", baseline.max_custom_bitrate / 1_000_000)
        }
        VIDEO_QUALITY_CUSTOM => "自定义".to_owned(),
        _ => "auto".to_owned(),
    }
}

pub(super) fn viewing_quality_label(state: &StreamControlState) -> String {
    official_quality_label(&state.baseline)
}

pub(super) fn finish_request(
    state: &mut StreamControlState,
    request_id: i64,
    failures: Vec<String>,
    notices: Vec<String>,
) {
    if !state.pending_sequences.contains(&request_id) {
        return;
    }
    let Some(index) = state
        .pending_capture_preferences
        .iter()
        .position(|pending| pending.sequence == request_id)
    else {
        return;
    };
    let completed = state
        .pending_capture_preferences
        .remove(index)
        .expect("matched capture request");
    state
        .pending_sequences
        .retain(|pending| *pending != request_id);
    if failures.is_empty() {
        // A successful complete snapshot supersedes older snapshots. A
        // refusal does not: older requests still retain their own ACKs.
        for earlier in state.pending_capture_preferences.drain(..index) {
            state
                .pending_sequences
                .retain(|seq| *seq != earlier.sequence);
        }
        if completed.cursor_capture == state.baseline.cursor_capture {
            state.cursor_error = None;
        }
        if state
            .cursor_pending
            .is_some_and(|(seq, _, _)| !state.pending_sequences.contains(&seq))
        {
            state.cursor_pending = None;
        }
        let settings_changed = state.confirmed_preferences.saved() != completed.preferences.saved();
        state.confirmed_preferences = completed.preferences;
        // Reapplying a confirmed choice must repair a failed local save even
        // when it equals the in-memory default restored after a read failure.
        if completed.persist && (settings_changed || state.persistence_error.is_some()) {
            state
                .preference_updates
                .send_replace(Some(ViewingPreferenceUpdate::Settings(
                    completed.preferences.saved(),
                )));
        }
    } else {
        fail_cursor_request(state, request_id, failures.join("; "));
    }
    if state.latest_requested_sequence != Some(request_id) {
        return;
    }
    if failures.is_empty() {
        state.last_applied_sequence = Some(request_id);
        state.last_error = None;
        state.last_notice = (!notices.is_empty()).then(|| notices.join("; "));
        state.performance.acknowledge_stream_switch(request_id);
        state.performance.set_quality(viewing_quality_label(state));
        tracing::info!(
            sequence = request_id,
            "runtime stream settings applied by remote host"
        );
        if let Some(notice) = state.last_notice.as_deref() {
            tracing::info!(sequence = request_id, %notice, "remote host adjusted runtime stream settings");
        }
    } else {
        let error = failures.join("; ");
        restore_confirmed_capture(state);
        state.last_error = Some(error.clone());
        state.last_notice = None;
        state
            .performance
            .fail_stream_switch(request_id, error.clone());
        tracing::warn!(sequence = request_id, %error, "remote host rejected runtime stream settings");
    }
}

pub(super) fn restore_confirmed_capture(state: &mut StreamControlState) {
    let confirmed = state.confirmed_preferences;
    let _ = set_requested_settings(state, confirmed.settings);
    state.baseline.auto_frame_quality = confirmed.auto_frame_quality;
    if let Ok(Some(selected)) = validate_format(
        state,
        confirmed.settings.quality,
        confirmed.settings.true_color,
        confirmed.settings.hdr,
    ) {
        apply_codec_limits(state, selected);
    }
}

pub(super) fn encode_capture_setting(
    sequence: i64,
    baseline: CaptureSettingBaseline,
) -> Result<Vec<u8>> {
    let request = capture_setting_request(baseline)?;
    Ok(encode_capture_request(sequence, request))
}

pub(super) fn capture_setting_request(
    baseline: CaptureSettingBaseline,
) -> Result<PbCaptureSettingRequest> {
    Ok(PbCaptureSettingRequest {
        fps: fps_to_protobuf(baseline.requested_fps),
        frame_quality: baseline.frame_quality,
        cursor_capture: baseline.cursor_capture,
        screen_id: EXISTING_SESSION_TRACKS,
        resolution_width: UNCHANGED_PHYSICAL_DIMENSION,
        resolution_height: UNCHANGED_PHYSICAL_DIMENSION,
        chroma_format: baseline.chroma_format,
        max_custom_bitrate: i32::try_from(baseline.max_custom_bitrate)?,
        dpi_scale: 0,
        resolution_type: RESOLUTION_DEFAULT,
        enable_hdr: baseline.enable_hdr,
        auto_frame_quality: baseline.auto_frame_quality,
        codec_type: baseline.codec_type,
        max_scale_width: i32::try_from(baseline.max_scale_width)?,
        max_scale_height: i32::try_from(baseline.max_scale_height)?,
        resolution_pixel_width: 0,
        resolution_pixel_height: 0,
        fps_count: i32::try_from(baseline.fps_count)?,
    })
}

pub(super) fn encode_capture_request(sequence: i64, request: PbCaptureSettingRequest) -> Vec<u8> {
    encode_envelope(
        sequence,
        PbPayload::RpcRequest(
            PbRpcRequest {
                request_header: Some(PbRequestHeader {
                    request_id: sequence,
                }),
                payload: Some(PbRpcRequestPayload::CaptureSetting(request)),
            }
            .encode_to_vec(),
        ),
    )
}

pub(super) fn fps_to_protobuf(fps: u32) -> i32 {
    match fps {
        30 => FPS_30,
        60 => FPS_60,
        90 => FPS_90,
        _ => FPS_144,
    }
}

pub(super) fn frame_rate_choice(fps: u32) -> FrameRateChoice {
    match fps {
        30 => FrameRateChoice::Fps30,
        60 => FrameRateChoice::Fps60,
        90 => FrameRateChoice::Fps90,
        _ => FrameRateChoice::Fps144,
    }
}
