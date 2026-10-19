//! Per-account, per-publisher viewing choices; never remote physical modes.
use anyhow::{Context, Result, bail};
use keyring::{Entry, Error};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

use crate::features::stream_control::{
    LoadedStreamControl, MAX_CUSTOM_BITRATE_MBPS, SavedStreamControl, StreamControlHandle,
    StreamControlSettings, ViewingPreferenceUpdate,
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
}

/// Weak-network strategy as the viewer chooses it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum WeakNetwork {
    #[default]
    Bitrate,
    FrameRate,
}
impl WeakNetwork {
    pub(crate) fn wire(self) -> i32 {
        match self {
            Self::Bitrate => 0,
            Self::FrameRate => 1,
        }
    }
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Bitrate => "优先降低码率",
            Self::FrameRate => "优先降低帧率",
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
        }
    }
}
impl DevicePreferences {
    fn validate(self) -> Result<Self> {
        anyhow::ensure!(
            !self.codec.is_some_and(|codec| codec.restricted()),
            "设备首选不能包含全局格式限制"
        );
        anyhow::ensure!(
            self.mouse_mode != crate::features::remote_input::MouseMode::View,
            "已保存的鼠标模式无效"
        );
        Ok(self)
    }
}

#[derive(Serialize, Deserialize)]
struct DeviceRecord {
    schema: u8,
    preferences: DevicePreferences,
}

#[derive(Clone)]
pub(crate) struct ViewingSettingsStore {
    viewing: Arc<Entry>,
    audio: Arc<Entry>,
    microphone: Arc<Entry>,
    device: Arc<Entry>,
}

#[derive(Serialize, Deserialize)]
struct Record {
    schema: u8,
    settings: Option<StreamControlSettings>,
    auto_frame_quality: i32,
}

impl Record {
    fn decode(bytes: &[u8]) -> Result<Self> {
        let record: Self = serde_json::from_slice(bytes).map_err(|error| {
            anyhow::anyhow!("已保存的画面设置格式无效，请重新应用画质设置：{error}")
        })?;
        record.validate()?;
        Ok(record)
    }

    fn validate(&self) -> Result<()> {
        anyhow::ensure!(self.schema == 1, "画面设置版本不受支持，请重新应用画质设置");
        anyhow::ensure!(
            self.settings.is_none_or(|settings| {
                (1..=MAX_CUSTOM_BITRATE_MBPS).contains(&settings.custom_bitrate_mbps)
            }) && (1..=6).contains(&self.auto_frame_quality),
            "已保存的画面设置数值无效，请重新应用画质设置"
        );
        Ok(())
    }
}

#[derive(Serialize, Deserialize)]
struct AudioRecord {
    schema: u8,
    settings: crate::media::audio::AudioSettings,
}

impl ViewingSettingsStore {
    pub(crate) fn new(user_id: &str, publisher_id: &str) -> Result<Self> {
        crate::account::api::validate_device_id(publisher_id)?;
        if user_id.is_empty() {
            bail!("无法确定画面设置所属账号");
        }
        let service = crate::product::keyring_service(&format!(
            "viewing.{:x}",
            Sha256::digest(user_id.as_bytes())
        ));
        let audio_service = crate::product::keyring_service(&format!(
            "audio.{:x}",
            Sha256::digest(user_id.as_bytes())
        ));
        Ok(Self {
            device: Arc::new(
                Entry::new(
                    &crate::product::keyring_service(&format!(
                        "device-preferences.{:x}",
                        Sha256::digest(user_id.as_bytes())
                    )),
                    publisher_id,
                )
                .map_err(|_| anyhow::anyhow!("设备偏好存储不可用"))?,
            ),
            viewing: Arc::new(
                Entry::new(&service, publisher_id)
                    .map_err(|_| anyhow::anyhow!("画面设置存储不可用"))?,
            ),
            audio: Arc::new(
                Entry::new(&audio_service, publisher_id)
                    .map_err(|_| anyhow::anyhow!("音量设置存储不可用"))?,
            ),
            microphone: Arc::new(
                Entry::new(
                    &crate::product::keyring_service(&format!(
                        "microphone.{:x}",
                        Sha256::digest(user_id.as_bytes())
                    )),
                    publisher_id,
                )
                .map_err(|_| anyhow::anyhow!("麦克风设置存储不可用"))?,
            ),
        })
    }

    pub(crate) async fn load(&self) -> Result<Option<LoadedStreamControl>> {
        let entry = Arc::clone(&self.viewing);
        tokio::task::spawn_blocking(move || {
            let bytes = match entry.get_secret() {
                Ok(bytes) => bytes,
                Err(Error::NoEntry) => return Ok(None),
                Err(_) => bail!("无法读取此设备的画面设置"),
            };
            let record = Record::decode(&bytes)?;
            Ok(Some(LoadedStreamControl {
                settings: record.settings,
                auto_frame_quality: record.auto_frame_quality,
            }))
        })
        .await
        .context("画面设置读取任务中断")?
    }

    async fn save(&self, saved: SavedStreamControl) -> Result<()> {
        let entry = Arc::clone(&self.viewing);
        tokio::task::spawn_blocking(move || {
            let record = Record {
                schema: 1,
                settings: Some(saved.settings),
                auto_frame_quality: saved.auto_frame_quality,
            };
            record.validate()?;
            let bytes = serde_json::to_vec(&record)?;
            entry
                .set_secret(&bytes)
                .map_err(|_| anyhow::anyhow!("无法保存此设备的画面设置"))
        })
        .await
        .context("画面设置保存任务中断")?
    }

    pub(crate) async fn load_audio(&self) -> Result<Option<crate::media::audio::AudioSettings>> {
        let entry = Arc::clone(&self.audio);
        tokio::task::spawn_blocking(move || {
            let bytes = match entry.get_secret() {
                Ok(bytes) => bytes,
                Err(Error::NoEntry) => return Ok(None),
                Err(_) => bail!("无法读取此设备的音量设置"),
            };
            let record: AudioRecord = serde_json::from_slice(&bytes)
                .map_err(|_| anyhow::anyhow!("已保存的音量设置格式无效"))?;
            if record.schema != 1 || record.settings.volume > crate::media::audio::MAX_VOLUME {
                bail!("已保存的音量设置无效");
            }
            Ok(Some(record.settings))
        })
        .await
        .context("音量设置读取任务中断")?
    }

    async fn save_audio(&self, settings: crate::media::audio::AudioSettings) -> Result<()> {
        let entry = Arc::clone(&self.audio);
        tokio::task::spawn_blocking(move || {
            let bytes = serde_json::to_vec(&AudioRecord {
                schema: 1,
                settings,
            })?;
            entry
                .set_secret(&bytes)
                .map_err(|_| anyhow::anyhow!("无法保存此设备的音量设置"))
        })
        .await
        .context("音量设置保存任务中断")?
    }

    pub(crate) async fn load_microphone(&self) -> Result<crate::media::audio::encoder::Quality> {
        let entry = self.microphone.clone();
        tokio::task::spawn_blocking(move || {
            let bytes = match entry.get_secret() {
                Ok(bytes) => bytes,
                Err(Error::NoEntry) => return Ok(Default::default()),
                Err(_) => bail!("无法读取麦克风音质设置"),
            };
            let quality: crate::media::audio::encoder::Quality = serde_json::from_slice(&bytes)?;
            quality.restore()
        })
        .await
        .context("麦克风设置读取任务中断")?
    }
    async fn save_microphone(&self, quality: crate::media::audio::encoder::Quality) -> Result<()> {
        quality.validate()?;
        let entry = self.microphone.clone();
        tokio::task::spawn_blocking(move || {
            entry
                .set_secret(&serde_json::to_vec(&quality)?)
                .map_err(|_| anyhow::anyhow!("无法保存麦克风音质设置"))
        })
        .await
        .context("麦克风设置保存任务中断")?
    }
    pub(crate) async fn load_device(&self) -> Result<DevicePreferences> {
        let entry = self.device.clone();
        tokio::task::spawn_blocking(move || -> Result<DevicePreferences> {
            let bytes = match entry.get_secret() {
                Ok(bytes) => bytes,
                Err(Error::NoEntry) => return Ok(DevicePreferences::default()),
                Err(_) => bail!("无法读取此设备的控制偏好"),
            };
            let record: DeviceRecord =
                serde_json::from_slice(&bytes).context("设备偏好格式无效")?;
            anyhow::ensure!(record.schema == 1, "设备偏好版本无效");
            record.preferences.validate()
        })
        .await
        .context("设备偏好读取任务中断")
        .and_then(|r| r)
    }

    pub(crate) async fn restore_device(&self, handle: &StreamControlHandle) {
        // A shared session or an in-memory room replacement already owns newer
        // intent; a stale credential read must not overwrite it.
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
        let entry = self.device.clone();
        tokio::task::spawn_blocking(move || -> Result<()> {
            let preferences = preferences.validate()?;
            let bytes = serde_json::to_vec(&DeviceRecord {
                schema: 1,
                preferences,
            })?;
            entry
                .set_secret(&bytes)
                .map_err(|_| anyhow::anyhow!("无法保存此设备的控制偏好"))
        })
        .await
        .context("设备偏好写入任务中断")?
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
                        let result = match settings {
                            ViewingPreferenceUpdate::Settings(saved) => self.save(saved).await,
                            ViewingPreferenceUpdate::AutoQuality(quality) => {
                                self.save_auto_quality(quality).await
                            }
                        };
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

impl ViewingSettingsStore {
    async fn save_auto_quality(&self, quality: i32) -> Result<()> {
        let entry = Arc::clone(&self.viewing);
        tokio::task::spawn_blocking(move || {
            let mut record = match entry.get_secret() {
                Ok(bytes) => Record::decode(&bytes)?,
                Err(Error::NoEntry) => Record {
                    schema: 1,
                    settings: None,
                    auto_frame_quality: quality,
                },
                Err(_) => bail!("无法读取此设备的画面设置"),
            };
            anyhow::ensure!((1..=6).contains(&quality), "自动画质状态无效");
            record.auto_frame_quality = quality;
            entry
                .set_secret(&serde_json::to_vec(&record)?)
                .map_err(|_| anyhow::anyhow!("无法保存自动画质"))
        })
        .await
        .context("自动画质写入任务中断")?
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
