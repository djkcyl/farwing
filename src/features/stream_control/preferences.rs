//! Device-scoped user intent, separate from temporary session/input state.
use super::{StreamControlHandle, StreamControlState, lock};
use crate::features::viewing_settings::{DevicePreferences, PerformancePanelMode};
use crate::i18n::t;
use anyhow::Result;

impl StreamControlHandle {
    pub(crate) fn set_display_aspect(&self, aspect: crate::media::geometry::DisplayAspect) {
        let mut state = lock(&self.shared);
        if state.device_preferences.display_aspect != aspect {
            state.device_preferences.display_aspect = aspect;
            self.publish_device_preferences(&mut state);
        }
    }
    pub(crate) fn display_aspect(&self, track: i32) -> Option<(u32, u32)> {
        let state = lock(&self.shared);
        let preferred = state
            .screens
            .iter()
            .find(|s| s.video_track_index == track)
            .and_then(|s| s.display.preferred);
        state.device_preferences.display_aspect.ratio(preferred)
    }
    pub(crate) fn presentation_size(&self, track: i32, size: (u32, u32)) -> (u32, u32) {
        self.display_aspect(track).unwrap_or(size)
    }

    /// Remember this device's codec preference and switch the running
    /// stream to it when both sides support it (`None` follows the global
    /// setting). The shared stream of several viewers keeps its codec.
    pub(crate) fn set_codec_preference(&self, codec: Option<crate::media::CodecPreference>) -> Result<()> {
        let mut state = lock(&self.shared);
        if state.device_preferences.codec == codec {
            return Ok(());
        }
        state.device_preferences.codec = codec;
        self.publish_device_preferences(&mut state);
        let effective = codec.unwrap_or_else(|| {
            crate::media::preferences::load()
                .map(|options| options.codec)
                .unwrap_or_default()
        });
        state.codec_preference = effective.preferred();
        if self.participants.shared_view() {
            return Ok(());
        }
        self.request_stream_locked(&mut state, None).map(drop)
    }

    pub(crate) fn device_preferences(&self) -> DevicePreferences {
        lock(&self.shared).device_preferences
    }
    pub(crate) fn device_preferences_loaded(&self) -> bool {
        lock(&self.shared).device_preferences_loaded
    }
    pub(crate) fn device_preference_updates(
        &self,
    ) -> tokio::sync::watch::Receiver<Option<DevicePreferences>> {
        lock(&self.shared).device_preference_updates.subscribe()
    }
    pub(crate) fn set_device_persistence_error(&self, error: Option<String>) {
        lock(&self.shared).device_persistence_error = error;
    }
    pub(super) fn publish_device_preferences(&self, state: &mut StreamControlState) {
        state.device_preferences_loaded = true;
        state
            .device_preference_updates
            .send_replace(Some(state.device_preferences));
    }
    pub(crate) fn restore_device_preferences(&self, mut preferences: DevicePreferences) {
        let mut state = lock(&self.shared);
        if state.device_preferences_loaded {
            return;
        }
        if let Some(cinema) = super::dev_cinema() {
            preferences.cinema = cinema;
        }
        state.device_preferences = preferences;
        state.device_preferences_loaded = true;
        state.restore_input_pending = preferences.control_enabled;
        if let Err(error) = state
            .mouse
            .set_throttle(preferences.mouse_throttle, preferences.mouse_throttle_rate)
        {
            state.device_persistence_error =
                Some((t().features.stream_mouse_throttle_failed)(&format!("{error:#}")));
        }
        self.apply_playout_locked(&state);
        self.clipboard.set_files(preferences.clipboard_files);
        if let Err(error) = self.clipboard.set_enabled(preferences.clipboard_sync) {
            self.clipboard
                .protocol_error((t().features.stream_clipboard_restore_failed)(
                    &format!("{error:#}"),
                ));
        }
        self.refresh_mouse_policy(&mut state);
        // Loading is not a user edit and must not overwrite the saved record.
    }
    /// Weak-network strategy and cinema mode for this device; the host
    /// applies them with the next capture setting.
    pub(crate) fn set_playback(
        &self,
        weak_network: crate::features::viewing_settings::WeakNetwork,
        cinema: bool,
    ) -> Result<()> {
        let mut state = lock(&self.shared);
        if state.device_preferences.weak_network == weak_network
            && state.device_preferences.cinema == cinema
        {
            return Ok(());
        }
        state.device_preferences.weak_network = weak_network;
        state.device_preferences.cinema = cinema;
        self.apply_playout_locked(&state);
        self.publish_device_preferences(&mut state);
        self.request_stream_locked(&mut state, None).map(drop)
    }
    pub(crate) fn set_clipboard_enabled(&self, enabled: bool) -> Result<()> {
        self.clipboard.set_enabled(enabled)?;
        let mut state = lock(&self.shared);
        if state.device_preferences.clipboard_sync != enabled {
            state.device_preferences.clipboard_sync = enabled;
            self.publish_device_preferences(&mut state);
        }
        self.refresh_mouse_policy(&mut state);
        Ok(())
    }
    pub(crate) fn set_clipboard_files(&self, enabled: bool) {
        self.clipboard.set_files(enabled);
        let mut state = lock(&self.shared);
        if state.device_preferences.clipboard_files != enabled {
            state.device_preferences.clipboard_files = enabled;
            self.publish_device_preferences(&mut state);
        }
        self.refresh_mouse_policy(&mut state);
    }
    /// Show this participant's pointer to the host and the others.
    pub(crate) fn set_share_pointer(&self, enabled: bool) {
        let mut state = lock(&self.shared);
        if state.device_preferences.share_pointer != enabled {
            state.device_preferences.share_pointer = enabled;
            self.publish_device_preferences(&mut state);
        }
    }
    pub(crate) fn share_pointer(&self) -> bool {
        lock(&self.shared).device_preferences.share_pointer
    }
    pub(crate) fn set_intercept_shortcuts(&self, enabled: bool) {
        let mut state = lock(&self.shared);
        if state.device_preferences.intercept_shortcuts != enabled {
            state.device_preferences.intercept_shortcuts = enabled;
            self.publish_device_preferences(&mut state);
        }
    }
    pub(crate) fn set_mouse_throttle(
        &self,
        enabled: bool,
        rate: crate::features::remote_input::MouseThrottleRate,
    ) -> Result<()> {
        let mut state = lock(&self.shared);
        state.mouse.set_throttle(enabled, rate)?;
        if state.device_preferences.mouse_throttle != enabled
            || state.device_preferences.mouse_throttle_rate != rate
        {
            state.device_preferences.mouse_throttle = enabled;
            state.device_preferences.mouse_throttle_rate = rate;
            self.publish_device_preferences(&mut state);
        }
        Ok(())
    }
    pub(crate) fn set_performance_mode(&self, mode: PerformancePanelMode) {
        let mut state = lock(&self.shared);
        if state.device_preferences.performance_mode != mode {
            state.device_preferences.performance_mode = mode;
            self.publish_device_preferences(&mut state);
        }
    }
    pub(crate) fn set_performance_always_visible(&self, always: bool) {
        let mut state = lock(&self.shared);
        if state.device_preferences.performance_always_visible != always {
            state.device_preferences.performance_always_visible = always;
            self.publish_device_preferences(&mut state);
        }
    }
}
