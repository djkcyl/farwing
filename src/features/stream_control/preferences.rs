//! Device-scoped user intent, separate from temporary session/input state.
use super::{StreamControlHandle, StreamControlState, lock};
use crate::features::viewing_settings::{DevicePreferences, PerformancePanelMode};
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
    #[cfg_attr(
        not(windows),
        allow(
            dead_code,
            reason = "The Linux viewer keeps the aspect with its frame size and fits the video itself."
        )
    )]
    pub(crate) fn presentation_size(&self, track: i32, size: (u32, u32)) -> (u32, u32) {
        self.display_aspect(track).unwrap_or(size)
    }

    pub(crate) fn set_codec_preference(&self, codec: Option<crate::media::CodecPreference>) {
        let mut state = lock(&self.shared);
        if state.device_preferences.codec != codec {
            state.device_preferences.codec = codec;
            self.publish_device_preferences(&mut state);
        }
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
    pub(crate) fn restore_device_preferences(&self, preferences: DevicePreferences) {
        let mut state = lock(&self.shared);
        if state.device_preferences_loaded {
            return;
        }
        state.device_preferences = preferences;
        state.device_preferences_loaded = true;
        state.restore_input_pending = preferences.control_enabled;
        if let Err(error) = state
            .mouse
            .set_throttle(preferences.mouse_throttle, preferences.mouse_throttle_rate)
        {
            state.device_persistence_error = Some(format!("鼠标节流未启用：{error:#}"));
        }
        self.clipboard.set_files(preferences.clipboard_files);
        if let Err(error) = self.clipboard.set_enabled(preferences.clipboard_sync) {
            self.clipboard
                .protocol_error(format!("恢复剪贴板偏好失败：{error:#}"));
        }
        self.refresh_mouse_policy(&mut state);
        // Loading is not a user edit and must not overwrite the saved record.
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
