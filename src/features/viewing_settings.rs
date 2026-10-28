//! Per-account, per-publisher viewing choices; never remote physical modes.
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

use crate::i18n::t;

use crate::features::stream_control::{
    MAX_CUSTOM_BITRATE_MBPS, StreamControlHandle, StreamControlSettings,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum PerformancePanelMode {
    Compact,
    Detailed,
}
impl PerformancePanelMode {
    pub(crate) fn next(self) -> Self {
        match self {
            Self::Compact => Self::Detailed,
            Self::Detailed => Self::Compact,
        }
    }
}

/// Explicit user choices. Runtime readiness, held input and clipboard contents
/// must never be serialized here or written back by session cleanup.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub(crate) struct DevicePreferences {
    pub display_aspect: crate::media::geometry::DisplayAspect,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub codec: Option<crate::media::CodecPreference>,
    pub clipboard_sync: bool,
    pub clipboard_files: bool,
    pub control_enabled: bool,
    pub mouse_mode: crate::features::remote_input::MouseMode,
    pub intercept_shortcuts: bool,
    pub mouse_throttle: bool,
    pub mouse_throttle_rate: crate::features::remote_input::MouseThrottleRate,
    pub performance_mode: PerformancePanelMode,
    #[serde(skip_serializing_if = "is_true")]
    pub performance_always_visible: bool,
    /// What gives way on a weak network (the output size never changes).
    pub weak_network: WeakNetwork,
    /// Cinema mode: smooth, synchronized playback and quality-first encoding.
    pub cinema: bool,
    /// Show this participant's pointer to the host and the others while
    /// not controlling.
    #[serde(skip_serializing_if = "is_true")]
    pub share_pointer: bool,
}

/// Weak-network strategy as the viewer chooses it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum WeakNetwork {
    #[default]
    Bitrate,
    FrameRate,
}
impl WeakNetwork {
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Bitrate => t().features.viewing_weak_network_bitrate,
            Self::FrameRate => t().features.viewing_weak_network_frame_rate,
        }
    }
}
fn is_true(value: &bool) -> bool {
    *value
}
impl Default for DevicePreferences {
    fn default() -> Self {
        Self {
            display_aspect: Default::default(),
            codec: None,
            // Clipboard text and files sync both ways unless turned off.
            clipboard_sync: true,
            clipboard_files: true,
            control_enabled: false,
            mouse_mode: crate::features::remote_input::MouseMode::Smart,
            intercept_shortcuts: true,
            mouse_throttle: false,
            mouse_throttle_rate: Default::default(),
            performance_mode: PerformancePanelMode::Compact,
            performance_always_visible: true,
            weak_network: WeakNetwork::Bitrate,
            cinema: false,
            share_pointer: true,
        }
    }
}
impl DevicePreferences {
    fn validate(self) -> Result<Self> {
        anyhow::ensure!(
            !self.codec.is_some_and(|codec| codec.restricted()),
            "{}",
            t().features.viewing_device_codec_restricted
        );
        anyhow::ensure!(
            self.mouse_mode != crate::features::remote_input::MouseMode::View,
            "{}",
            t().features.viewing_saved_mouse_mode_invalid
        );
        Ok(self)
    }
}

/// One file per account and publisher (`viewing/<account>/<publisher>.json`):
/// stream choices, volume, microphone quality and device preferences.
#[derive(Clone)]
pub(crate) struct ViewingSettingsStore {
    path: Arc<std::path::PathBuf>,
}

#[derive(Default, Serialize, Deserialize)]
#[serde(default)]
struct Saved {
    stream: Option<StreamControlSettings>,
    audio: Option<crate::media::audio::AudioSettings>,
    microphone: Option<crate::media::audio::encoder::Quality>,
    device: Option<DevicePreferences>,
}

/// The parts are saved separately: each save reads, changes and writes the
/// one file.
static WRITES: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn valid_stream(settings: StreamControlSettings) -> Result<StreamControlSettings> {
    anyhow::ensure!(
        (1..=MAX_CUSTOM_BITRATE_MBPS).contains(&settings.custom_bitrate_mbps),
        "{}",
        t().features.viewing_saved_settings_invalid
    );
    Ok(settings)
}

impl ViewingSettingsStore {
    pub(crate) fn new(user_id: &str, publisher_id: &str) -> Result<Self> {
        crate::account::api::validate_device_id(publisher_id)?;
        if user_id.is_empty() {
            bail!("{}", t().features.viewing_settings_account_unknown);
        }
        let path = crate::storage::path(format!(
            "viewing/{:x}/{publisher_id}.json",
            Sha256::digest(user_id.as_bytes())
        ))?;
        Ok(Self { path: Arc::new(path) })
    }

    /// What is saved; `failed` words a damaged or unreadable file.
    async fn read(&self, failed: &'static str) -> Result<Saved> {
        let path = Arc::clone(&self.path);
        tokio::task::spawn_blocking(move || {
            crate::storage::read::<Saved>(&path)
                .map(Option::unwrap_or_default)
                .map_err(|error| anyhow::anyhow!("{failed}: {error:#}"))
        })
        .await
        .context(t().features.viewing_settings_read_interrupted)?
    }

    async fn update(&self, change: impl FnOnce(&mut Saved) + Send + 'static) -> Result<()> {
        let path = Arc::clone(&self.path);
        tokio::task::spawn_blocking(move || {
            let _writing = WRITES.lock().unwrap_or_else(|e| e.into_inner());
            let mut saved = crate::storage::read::<Saved>(&path)
                .ok()
                .flatten()
                .unwrap_or_default();
            change(&mut saved);
            crate::storage::write(&path, &saved)
        })
        .await
        .context(t().features.viewing_settings_save_interrupted)?
    }

    pub(crate) async fn load(&self) -> Result<Option<StreamControlSettings>> {
        self.read(t().features.viewing_settings_read_failed)
            .await?
            .stream
            .map(valid_stream)
            .transpose()
    }

    async fn save(&self, settings: StreamControlSettings) -> Result<()> {
        let settings = valid_stream(settings)?;
        self.update(move |saved| saved.stream = Some(settings))
            .await
            .context(t().features.viewing_settings_save_failed)
    }

    pub(crate) async fn load_audio(&self) -> Result<Option<crate::media::audio::AudioSettings>> {
        let audio = self.read(t().features.viewing_volume_read_failed).await?.audio;
        if audio.is_some_and(|settings| settings.volume > crate::media::audio::MAX_VOLUME) {
            bail!("{}", t().features.viewing_volume_invalid);
        }
        Ok(audio)
    }

    async fn save_audio(&self, settings: crate::media::audio::AudioSettings) -> Result<()> {
        self.update(move |saved| saved.audio = Some(settings))
            .await
            .context(t().features.viewing_volume_save_failed)
    }

    pub(crate) async fn load_microphone(&self) -> Result<crate::media::audio::encoder::Quality> {
        self.read(t().features.viewing_microphone_read_failed)
            .await?
            .microphone
            .unwrap_or_default()
            .restore()
    }
    async fn save_microphone(&self, quality: crate::media::audio::encoder::Quality) -> Result<()> {
        quality.validate()?;
        self.update(move |saved| saved.microphone = Some(quality))
            .await
            .context(t().features.viewing_microphone_save_failed)
    }
    pub(crate) async fn load_device(&self) -> Result<DevicePreferences> {
        self.read(t().features.viewing_device_read_failed)
            .await?
            .device
            .unwrap_or_default()
            .validate()
    }

    pub(crate) async fn restore_device(&self, handle: &StreamControlHandle) {
        // A shared session or an in-memory room replacement already owns newer
        // intent; a stale read must not overwrite it.
        if handle.device_preferences_loaded() {
            return;
        }
        let result = self.load_device().await;
        match result {
            Ok(preferences) => handle.restore_device_preferences(preferences),
            Err(error) => handle.set_device_persistence_error(Some(error.to_string())),
        }
    }

    async fn save_device(&self, preferences: DevicePreferences) -> Result<()> {
        let preferences = preferences.validate()?;
        self.update(move |saved| saved.device = Some(preferences))
            .await
            .context(t().features.viewing_device_save_failed)
    }

    pub(crate) fn bind_local_preferences(self, handle: StreamControlHandle) -> PreferenceWriter {
        // Subscribe after restoring startup settings, so --mute and defaults
        // do not silently overwrite saved user choices.
        let mut updates = handle.audio().preference_updates();
        let mut microphone = handle.microphone().quality_updates();
        let mut device = handle.device_preference_updates();
        let stop = CancellationToken::new();
        let cancelled = stop.clone();
        let task = tokio::spawn(async move {
            loop {
                let mut changed = tokio::select! {
                    _ = cancelled.cancelled() => 0u8,
                    changed = updates.changed() => if changed.is_ok(){1}else{0},
                    changed = microphone.changed() => if changed.is_ok(){2}else{0},
                    changed = device.changed() => if changed.is_ok(){4}else{0},
                };
                if changed != 0 {
                    tokio::select! { _=cancelled.cancelled()=>{}, _=tokio::time::sleep(std::time::Duration::from_millis(250))=>{} }
                }
                if updates.has_changed().unwrap_or(false) {
                    changed |= 1;
                }
                if microphone.has_changed().unwrap_or(false) {
                    changed |= 2;
                }
                if device.has_changed().unwrap_or(false) {
                    changed |= 4;
                }
                let mut error = None;
                if changed & 1 != 0 {
                    let settings = *updates.borrow_and_update();
                    if let Some(settings) = settings {
                        error = self.save_audio(settings).await.err().map(|e| e.to_string());
                        if error.is_none() {
                            tracing::debug!(?settings, "saved audio settings for this device");
                        }
                    }
                }
                if changed & 2 != 0 {
                    let quality = *microphone.borrow_and_update();
                    if let Some(quality) = quality {
                        error = self
                            .save_microphone(quality)
                            .await
                            .err()
                            .map(|e| e.to_string())
                            .or(error);
                    }
                }
                if changed & 3 != 0 {
                    handle.set_audio_persistence_error(error);
                }
                if changed & 4 != 0 {
                    let preferences = *device.borrow_and_update();
                    if let Some(preferences) = preferences {
                        let error = self
                            .save_device(preferences)
                            .await
                            .err()
                            .map(|e| e.to_string());
                        handle.set_device_persistence_error(error);
                    }
                }
                if (cancelled.is_cancelled() || changed == 0)
                    && !updates.has_changed().unwrap_or(false)
                    && !microphone.has_changed().unwrap_or(false)
                    && !device.has_changed().unwrap_or(false)
                {
                    break;
                }
            }
        });
        PreferenceWriter {
            stop,
            task: Some(task),
        }
    }

    pub(crate) fn bind(self, handle: StreamControlHandle) -> PreferenceWriter {
        let mut updates = handle.preference_updates();
        let stop = CancellationToken::new();
        let cancelled = stop.clone();
        let task = tokio::spawn(async move {
            loop {
                let changed = tokio::select! {
                    _ = cancelled.cancelled() => false,
                    changed = updates.changed() => changed.is_ok(),
                };
                if changed || updates.has_changed().unwrap_or(false) {
                    let settings = *updates.borrow_and_update();
                    if let Some(settings) = settings {
                        let result = self.save(settings).await;
                        let error = result.err().map(|e| e.to_string());
                        if error.is_none() {
                            tracing::info!(?settings, "saved viewing settings for this device");
                        }
                        handle.set_persistence_error(error);
                    }
                }
                if cancelled.is_cancelled() || !changed {
                    break;
                }
            }
        });
        PreferenceWriter {
            stop,
            task: Some(task),
        }
    }
}

pub(crate) struct PreferenceWriter {
    stop: CancellationToken,
    task: Option<tokio::task::JoinHandle<()>>,
}
impl PreferenceWriter {
    pub(crate) async fn finish(&mut self) {
        self.stop.cancel();
        if let Some(task) = self.task.take() {
            let _ = task.await;
        }
    }
}
impl Drop for PreferenceWriter {
    fn drop(&mut self) {
        self.stop.cancel();
    }
}
