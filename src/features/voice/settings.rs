//! Voice settings of this Windows user (`docs/protocol/chat-design.md`
//! §7.4): devices, processing, how speaking starts, each person's volume.

use super::capture::Noise;
use super::sounds::SoundGroup;
use anyhow::{Context, Result};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Mode {
    /// Sends while the level is over the threshold.
    #[default]
    Activity,
    /// Sends while the key is held.
    PushToTalk,
}

/// A push-to-talk key: a virtual-key code (mouse buttons included:
/// `VK_XBUTTON1` = 5, `VK_XBUTTON2` = 6, `VK_MBUTTON` = 4).
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) struct Key {
    pub vk: u16,
}

#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub(crate) struct VoiceSettings {
    /// Endpoint IDs; `None`: the system's default.
    pub input: Option<String>,
    pub output: Option<String>,
    /// The camera for voice channels (its ID); `None`: the first one.
    pub camera: Option<String>,
    pub echo: bool,
    pub noise: Noise,
    /// Automatic gain.
    pub gain: bool,
    /// Microphone volume (percent, 0–200), before anything else.
    pub input_volume: u8,
    pub mode: Mode,
    /// Activity threshold (dBFS); `None`: follows the background noise.
    pub threshold_db: Option<f32>,
    pub key: Option<Key>,
    /// Keys that toggle mute and deafen, wherever the focus is.
    pub mute_key: Option<Key>,
    pub deafen_key: Option<Key>,
    /// Output volume (percent, 0–200).
    pub volume: u8,
    /// Each person's volume (percent), by account.
    pub volumes: BTreeMap<String, u8>,
    /// People this device does not play, by account.
    pub muted: BTreeSet<String>,
    /// Sounds' volume (percent of the output volume).
    pub sound_volume: u8,
    /// Kinds of sounds turned off.
    pub sounds_off: BTreeSet<SoundGroup>,
}

impl Default for VoiceSettings {
    fn default() -> Self {
        Self {
            input: None,
            output: None,
            camera: None,
            echo: true,
            noise: Noise::Standard,
            gain: true,
            input_volume: 100,
            mode: Mode::Activity,
            threshold_db: None,
            key: None,
            mute_key: None,
            deafen_key: None,
            volume: 100,
            volumes: BTreeMap::new(),
            muted: BTreeSet::new(),
            sound_volume: 60,
            sounds_off: BTreeSet::new(),
        }
    }
}

fn path() -> Result<std::path::PathBuf> {
    Ok(crate::product::local_data_dir()
        .context("LOCALAPPDATA unavailable")?
        .join("voice-settings.json"))
}

pub(crate) fn load() -> VoiceSettings {
    path()
        .ok()
        .and_then(|p| crate::storage::read(&p).ok().flatten())
        .unwrap_or_default()
}

impl VoiceSettings {
    /// What `user` is played at (0–2).
    pub(crate) fn gain_of(&self, user: &str) -> f32 {
        if self.muted.contains(user) {
            0.0
        } else {
            f32::from(*self.volumes.get(user).unwrap_or(&100)) / 100.0
        }
    }

    /// `sound`'s volume (0–2), `None` when it is off.
    pub(crate) fn sound_level(&self, sound: super::sounds::Sound) -> Option<f32> {
        let level = f32::from(self.sound_volume) / 100.0 * f32::from(self.volume) / 100.0;
        (!self.sounds_off.contains(&sound.group()) && level > 0.0).then_some(level)
    }
}

pub(crate) fn save(settings: &VoiceSettings) -> Result<()> {
    crate::storage::write(&path()?, settings)
}
