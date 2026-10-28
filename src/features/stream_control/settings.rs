//! The stream this controller asks for: the settings it requests (one
//! `StreamRequest` with what changed), the state the host reports
//! (`StreamState`), and the saved preferences.
use super::input::fail_cursor_request;
use super::requests::Waiting;
use super::wire::{self, SessionBody, StreamRequest};
use super::{
    MAX_CUSTOM_BITRATE_MBPS, RemoteDisplayState, StreamControlHandle, StreamControlPreferences,
    StreamControlSettings, StreamControlState, StreamQuality, custom_bitrate_supported,
    ensure_ready, feature_supported, lock,
};
use crate::i18n::t;
use crate::media::{FrameRateChoice, LocalDisplayInfo};
use anyhow::{Result, bail};
use farwing_protocol::session::{self, StreamSettings};

/// How long the host may take to apply a stream change.
const STREAM_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15);

/// What this controller's choices ask of the host.
pub(crate) struct Wanted {
    pub settings: StreamControlSettings,
    pub display: LocalDisplayInfo,
    pub custom_bitrate_limit: u32,
    pub codec: Option<session::Codec>,
    pub cursor: bool,
    pub weak_network: crate::features::viewing_settings::WeakNetwork,
    pub cinema: bool,
}

impl Wanted {
    pub(crate) fn stream(&self) -> StreamSettings {
        let settings = self.settings;
        StreamSettings {
            quality: match settings.quality {
                StreamQuality::Auto => session::Quality::Auto,
                StreamQuality::Fast => session::Quality::Low,
                StreamQuality::Clear => session::Quality::Standard,
                StreamQuality::High => session::Quality::High,
                StreamQuality::Original => session::Quality::Ultra,
                StreamQuality::Custom => session::Quality::Custom,
            },
            bitrate_kbps: if settings.quality == StreamQuality::Custom {
                settings.custom_bitrate_mbps.min(self.custom_bitrate_limit) * 1_000
            } else {
                0
            },
            // No faster than this display shows.
            fps: settings
                .frame_rate
                .value(self.display)
                .min(self.display.refresh_hz.max(1))
                .max(1),
            chroma444: settings.true_color,
            hdr: settings.hdr,
            codec: self.codec,
            cursor: self.cursor,
            weak_network: match self.weak_network {
                crate::features::viewing_settings::WeakNetwork::Bitrate => {
                    session::WeakNetwork::Bitrate
                }
                crate::features::viewing_settings::WeakNetwork::FrameRate => {
                    session::WeakNetwork::FrameRate
                }
            },
            cinema: self.cinema,
        }
    }
}

fn wanted(state: &StreamControlState) -> StreamSettings {
    Wanted {
        settings: state.settings,
        display: state.local_display,
        custom_bitrate_limit: state.custom_bitrate_limit,
        codec: state.codec_preference,
        cursor: state.cursor_capture,
        weak_network: state.device_preferences.weak_network,
        cinema: state.device_preferences.cinema,
    }
    .stream()
}

impl StreamControlHandle {
    /// One `ReceiverStats` report for the host, once video is negotiated:
    /// it caps the frame rate while this decoder falls behind.
    pub(crate) fn receiver_stats_frame(&self) -> Option<Vec<u8>> {
        let mut state = lock(&self.shared);
        if !state.pb_connected || !state.viewing_enabled {
            tracing::debug!(
                connected = state.pb_connected,
                viewing = state.viewing_enabled,
                "receiver statistics not sent"
            );
            return None;
        }
        let snapshot = state.performance.snapshot();
        if snapshot.receive_fps <= 0.0 {
            tracing::debug!(
                decode_fps = snapshot.decode_fps,
                render_fps = snapshot.render_fps,
                "receiver statistics not sent: no received frame rate"
            );
            return None;
        }
        let dropped = snapshot
            .predecode_dropped_frames
            .saturating_add(snapshot.dropped_present_frames);
        let new_drops = dropped.saturating_sub(state.reported_drops);
        state.reported_drops = dropped;
        Some(wire::push(SessionBody::ReceiverStats(wire::ReceiverStats {
            receive_fps: snapshot.receive_fps as f32,
            decode_fps: snapshot.decode_fps as f32,
            render_fps: snapshot.render_fps as f32,
            decode_delay_ms: snapshot.decode_pipeline_delay_ms as f32,
            dropped_frames: u32::try_from(new_drops).unwrap_or(u32::MAX),
        })))
    }

    /// What this controller offered at `session.start`: the decoders, and
    /// the stream the host starts with.
    pub(crate) fn set_offer(&self, offer: &session::ConnectOptions) {
        let mut state = lock(&self.shared);
        state.decoders = offer.decoders.clone();
        state.held = offer.stream.map(wire::StreamSettings::from);
        if let Some(stream) = offer.stream {
            state.codec_preference = stream.codec;
            state.cursor_capture = stream.cursor;
        }
    }

    pub(crate) fn restore_preferences(&self, preferences: StreamControlPreferences) -> Result<()> {
        let mut state = lock(&self.shared);
        if preferences.settings.quality == StreamQuality::Custom
            && !(1..=MAX_CUSTOM_BITRATE_MBPS).contains(&preferences.settings.custom_bitrate_mbps)
        {
            bail!(
                "{}",
                (t().features.stream_custom_bitrate_range)(MAX_CUSTOM_BITRATE_MBPS)
            );
        }
        state.custom_bitrate_limit = preferences
            .custom_bitrate_limit
            .unwrap_or(MAX_CUSTOM_BITRATE_MBPS);
        state.settings = preferences.settings;
        state.confirmed_preferences = preferences;
        if let Some(audio) = preferences.audio {
            self.audio.set_settings(audio);
        }
        state.performance.set_quality(viewing_quality_label(&state));
        tracing::info!(?preferences, "restored viewing preferences for a new room generation");
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

    pub(crate) fn propose_format(
        &self,
        true_color: Option<bool>,
        hdr: Option<bool>,
    ) -> Result<StreamControlSettings> {
        format_proposal(&lock(&self.shared), true_color, hdr)
    }

    /// 4:4:4 and HDR apply to every display of the session.
    pub(crate) fn apply_format(
        &self,
        _screen_id: i32,
        settings: StreamControlSettings,
    ) -> Result<i64> {
        self.apply(settings)
    }

    pub(super) fn apply_settings_locked(
        &self,
        state: &mut StreamControlState,
        settings: StreamControlSettings,
    ) -> Result<i64> {
        ensure_ready(state)?;
        if settings.quality == StreamQuality::Custom {
            anyhow::ensure!(
                custom_bitrate_supported(state),
                "{}",
                t().features.stream_custom_bitrate_unsupported
            );
            anyhow::ensure!(
                (1..=MAX_CUSTOM_BITRATE_MBPS).contains(&settings.custom_bitrate_mbps),
                "{}",
                (t().features.stream_custom_bitrate_range)(MAX_CUSTOM_BITRATE_MBPS)
            );
            anyhow::ensure!(
                settings.custom_bitrate_mbps <= state.custom_bitrate_limit
                    || (state.settings.quality == StreamQuality::Custom
                        && settings.custom_bitrate_mbps == state.settings.custom_bitrate_mbps),
                "{}",
                (t().features.stream_custom_bitrate_limit)(state.custom_bitrate_limit)
            );
        }
        if settings.hdr && !state.settings.hdr {
            ensure_hdr(state)?;
        }
        if settings.true_color && !state.settings.true_color {
            anyhow::ensure!(
                color_supported(state),
                "{}",
                t().features.stream_color_unsupported
            );
        }
        let previous = state.settings;
        state.settings = settings;
        state.user_settings_requested = true;
        let target = format!(
            "{} · {}",
            settings.quality.label(),
            settings.frame_rate.label(state.local_display)
        );
        match self.request_stream_locked(state, Some(target)) {
            Ok(Some(sequence)) => Ok(sequence),
            Ok(None) => {
                // The host already holds it.
                confirm(state, settings, true);
                Ok(0)
            }
            Err(error) => {
                state.settings = previous;
                Err(error)
            }
        }
    }

    /// Ask the host for what this controller wants now, when it differs
    /// from what the host holds; `None` when nothing differs.
    pub(super) fn request_stream_locked(
        &self,
        state: &mut StreamControlState,
        switch_target: Option<String>,
    ) -> Result<Option<i64>> {
        let Some(held) = state.held else {
            return Ok(None);
        };
        let wanted: wire::StreamSettings = wanted(state).into();
        let request = StreamRequest::between(held, wanted);
        if request.is_empty() {
            return Ok(None);
        }
        let sequence = self.send_request(
            state,
            SessionBody::StreamRequest(request),
            Waiting::Stream {
                settings: state.settings,
                persist: state.user_settings_requested,
                before: held,
            },
            STREAM_TIMEOUT,
        )?;
        tracing::info!(sequence, ?request, "stream change requested");
        state.held = Some(wanted);
        state.latest_requested_sequence = Some(sequence);
        state.pending_stream = Some(sequence);
        if request.cursor.is_some() {
            state.cursor_pending = Some(sequence);
            state.cursor_error = None;
        }
        state.last_error = None;
        state.last_notice = None;
        if let Some(target) = switch_target {
            state.performance.begin_stream_switch(sequence, target);
        }
        Ok(Some(sequence))
    }

    /// Ask for what changed once the session can take it (the cursor
    /// policy, device preferences loaded after the start).
    pub(super) fn sync_stream_locked(&self, state: &mut StreamControlState) {
        if !state.viewing_enabled || ensure_ready(state).is_err() {
            return;
        }
        if let Err(error) = self.request_stream_locked(state, None) {
            tracing::warn!(%error, "stream change not sent");
            state.last_error = Some(error.to_string());
        }
    }

    /// The host answered stream request `sequence`.
    pub(super) fn stream_settled(
        &self,
        state: &mut StreamControlState,
        sequence: i64,
        settings: StreamControlSettings,
        persist: bool,
        before: wire::StreamSettings,
        result: std::result::Result<(), String>,
    ) {
        let latest = state.latest_requested_sequence == Some(sequence);
        if state.pending_stream == Some(sequence) {
            state.pending_stream = None;
        }
        if state.cursor_pending == Some(sequence) {
            match &result {
                Ok(()) => state.cursor_pending = None,
                Err(error) => fail_cursor_request(state, sequence, error.clone()),
            }
        }
        match result {
            Ok(()) => {
                confirm(state, settings, persist);
                if latest {
                    state.last_applied_sequence = Some(sequence);
                    state.last_error = None;
                    state.last_notice = notices(state);
                    state.performance.acknowledge_stream_switch(sequence);
                    state.performance.set_quality(viewing_quality_label(state));
                }
                tracing::info!(sequence, "stream change applied by the host");
            }
            Err(error) => {
                tracing::warn!(sequence, %error, "stream change refused");
                if latest {
                    state.held = Some(before);
                    state.settings = state.confirmed_preferences.settings;
                    state.last_error = Some(error.clone());
                    state.last_notice = None;
                    state.performance.fail_stream_switch(sequence, error);
                }
            }
        }
    }
}

/// The host holds `settings`: they are this device's preference now.
fn confirm(state: &mut StreamControlState, settings: StreamControlSettings, persist: bool) {
    let changed = state.confirmed_preferences.settings != settings;
    state.confirmed_preferences.settings = settings;
    // Reapplying a confirmed choice must repair a failed local save.
    if persist && (changed || state.persistence_error.is_some()) {
        state.preference_updates.send_replace(Some(settings));
    }
}

/// What the host could not give, as a notice.
fn notices(state: &StreamControlState) -> Option<String> {
    let stream = state.stream_state.as_ref()?;
    let settings = stream.settings?;
    let mut notices = Vec::new();
    if settings.chroma444
        && !stream.tracks.is_empty()
        && !stream.tracks.iter().any(|t| t.chroma444)
    {
        notices.push(t().features.stream_yuv444_fallback);
    }
    if stream
        .tracks
        .iter()
        .map(|t| t.fps)
        .max()
        .is_some_and(|fps| fps < settings.fps)
    {
        notices.push(t().features.stream_fps_reduced);
    }
    (!notices.is_empty()).then(|| notices.join("; "))
}

/// Take in the host's stream state (a push, or the answer to a request);
/// an older state than the one in effect is dropped.
pub(super) fn apply_stream_state(state: &mut StreamControlState, stream: wire::StreamState) {
    if state
        .stream_state
        .as_ref()
        .is_some_and(|current| stream.revision < current.revision)
    {
        return;
    }
    // While a request is out, what this controller asked for stands.
    if state.pending_stream.is_none() {
        state.held = stream.settings.or(state.held);
    }
    tracing::debug!(revision = stream.revision, tracks = stream.tracks.len(), "stream state received");
    state.performance.set_remote_senders(
        stream
            .tracks
            .iter()
            .map(|track| crate::diagnostics::performance::RemoteSenderInfo {
                video_track_index: u64::from(track.track),
                capture_impl: track.capture.clone(),
                encoder_impl: track.encoder.clone(),
            })
            .collect(),
    );
    state.stream_state = Some(stream);
    state.performance.set_quality(viewing_quality_label(state));
}

pub(super) fn color_supported(state: &StreamControlState) -> bool {
    feature_supported(state, crate::account::feature_ability::Feature::Host)
        && state
            .stream_state
            .as_ref()
            .and_then(|s| s.limits.as_ref())
            .is_some_and(|limits| limits.chroma444)
}

/// HDR takes a 10-bit format both sides support, and HDR on the viewed
/// display and on this one.
pub(super) fn ensure_hdr(state: &StreamControlState) -> Result<()> {
    let limits = state
        .stream_state
        .as_ref()
        .and_then(|s| s.limits.as_ref())
        .ok_or_else(|| anyhow::anyhow!("{}", t().features.stream_waiting_hdr_capability))?;
    anyhow::ensure!(limits.hdr, "{}", t().features.stream_hdr_unsupported);
    let viewed = state.remote_display.map(|d| d.screen_id);
    anyhow::ensure!(
        state
            .screens
            .iter()
            .filter(|s| viewed.is_none_or(|id| s.id == id))
            .any(|s| s.display.hdr),
        "{}",
        t().features.stream_host_hdr_off
    );
    anyhow::ensure!(
        crate::platform::display_hdr::any_hdr(),
        "{}",
        t().features.stream_local_hdr_off
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
        anyhow::ensure!(
            !enabled || color_supported(state),
            "{}",
            t().features.stream_color_unsupported
        );
        settings.true_color = enabled;
    }
    if let Some(enabled) = hdr {
        if enabled {
            ensure_hdr(state)?;
        }
        settings.hdr = enabled;
    }
    Ok(settings)
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
        "active-screen baseline synchronized"
    );
}

/// A refused stream request in the interface language, from its reason.
pub(super) fn refusal_text(reason: &str) -> String {
    match reason {
        wire::stream_reason::UPDATING => t().features.stream_host_updating_settings_locked,
        _ => t().features.stream_setting_not_applied,
    }
    .into()
}

pub(super) fn quality_name(quality: wire::Quality) -> &'static str {
    match quality {
        wire::Quality::Ultra => t().features.stream_quality_original,
        wire::Quality::High => t().features.stream_quality_ultra,
        wire::Quality::Low => t().features.stream_quality_low_bitrate,
        _ => t().features.stream_quality_high,
    }
}

/// The tier automatic quality runs at for the viewed track.
pub(super) fn automatic_tier(state: &StreamControlState) -> Option<wire::Quality> {
    let tracks = &state.stream_state.as_ref()?.tracks;
    tracks
        .iter()
        .find(|t| t.track as i32 == state.active_video_track_index)
        .or(tracks.first())
        .map(|t| t.quality())
        .filter(|q| *q != wire::Quality::Custom)
}

pub(super) fn viewing_quality_label(state: &StreamControlState) -> String {
    match state.settings.quality {
        StreamQuality::Auto => (t().features.stream_quality_auto_with)(quality_name(
            automatic_tier(state).unwrap_or(wire::Quality::Standard),
        )),
        StreamQuality::Custom => format!(
            "{} Mbps",
            state
                .settings
                .custom_bitrate_mbps
                .min(state.custom_bitrate_limit)
        ),
        quality => quality.label().to_owned(),
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
