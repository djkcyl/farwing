//! Voice sounds (`docs/protocol/chat-design.md` §7.5): a ringing call,
//! joining and leaving, people coming and going, mute, new messages, a
//! share starting and ending, being moved or disconnected. Short
//! synthesized tones on the call output, each on a thread of its own; the
//! same output plays the microphone test back.

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Sound {
    /// A call for this account.
    Ring,
    /// This device calls.
    Ringback,
    Joined,
    Left,
    /// Someone came into this device's channel.
    Came,
    Went,
    Muted,
    Unmuted,
    Message,
    /// Someone in this device's channel started or ended a share.
    LiveStarted,
    LiveEnded,
    /// Moved to another channel or disconnected by a moderator.
    Removed,
}

/// The switches of the voice settings, one per kind of sound.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum SoundGroup {
    Calls,
    /// This device joining and leaving.
    Presence,
    /// Others joining and leaving.
    People,
    Mute,
    Messages,
    Live,
    Removed,
}

impl SoundGroup {
    pub(crate) const ALL: [SoundGroup; 7] = [
        Self::Calls,
        Self::Presence,
        Self::People,
        Self::Mute,
        Self::Messages,
        Self::Live,
        Self::Removed,
    ];
}

impl Sound {
    pub(crate) fn group(self) -> SoundGroup {
        match self {
            Self::Ring | Self::Ringback => SoundGroup::Calls,
            Self::Joined | Self::Left => SoundGroup::Presence,
            Self::Came | Self::Went => SoundGroup::People,
            Self::Muted | Self::Unmuted => SoundGroup::Mute,
            Self::Message => SoundGroup::Messages,
            Self::LiveStarted | Self::LiveEnded => SoundGroup::Live,
            Self::Removed => SoundGroup::Removed,
        }
    }
}

/// (frequency Hz, milliseconds); 0 Hz is a pause.
fn notes(sound: Sound) -> &'static [(f32, u32)] {
    match sound {
        Sound::Ring => &[(784.0, 180), (0.0, 60), (659.0, 180), (0.0, 60), (784.0, 180), (0.0, 60), (659.0, 180)],
        Sound::Ringback => &[(440.0, 900)],
        Sound::Joined => &[(523.0, 90), (784.0, 140)],
        Sound::Left => &[(784.0, 90), (523.0, 140)],
        Sound::Came => &[(880.0, 90)],
        Sound::Went => &[(587.0, 90)],
        Sound::Muted => &[(392.0, 60)],
        Sound::Unmuted => &[(659.0, 60)],
        Sound::Message => &[(988.0, 70), (0.0, 30), (1319.0, 90)],
        Sound::LiveStarted => &[(523.0, 70), (659.0, 70), (784.0, 120)],
        Sound::LiveEnded => &[(784.0, 70), (659.0, 70), (523.0, 120)],
        Sound::Removed => &[(659.0, 120), (0.0, 40), (440.0, 120), (0.0, 40), (330.0, 200)],
    }
}

fn render(sound: Sound, rate: u32, volume: f32) -> Vec<f32> {
    let mut out = Vec::new();
    for (hz, ms) in notes(sound) {
        let n = (rate as u64 * u64::from(*ms) / 1000) as usize;
        let fade = (rate as usize / 200).min(n / 2).max(1);
        for i in 0..n {
            let env = (i.min(n - 1 - i) as f32 / fade as f32).min(1.0);
            let s = if *hz > 0.0 {
                (i as f32 * hz * std::f32::consts::TAU / rate as f32).sin() * 0.2 * volume * env
            } else {
                0.0
            };
            out.push(s);
        }
    }
    out
}

/// Plays `sound` on output `device` (`None`: the default) at `volume` (0–2).
pub(crate) fn play(sound: Sound, device: Option<String>, volume: f32) {
    play_with(device, move |rate| render(sound, rate, volume));
}

/// Plays mono 48 kHz `samples` (the microphone test) on `device`.
pub(crate) fn play_recording(samples: Vec<f32>, device: Option<String>, volume: f32) {
    play_with(device, move |rate| {
        // Linear resampling to the output's rate.
        let step = 48_000.0 / rate as f64;
        let n = (samples.len() as f64 / step) as usize;
        (0..n)
            .map(|i| {
                let at = i as f64 * step;
                let j = at as usize;
                let frac = (at - j as f64) as f32;
                let a = samples.get(j).copied().unwrap_or(0.0);
                let b = samples.get(j + 1).copied().unwrap_or(a);
                (a + (b - a) * frac) * volume
            })
            .collect()
    });
}

/// Plays what `render` makes for the output's rate.
fn play_with(device: Option<String>, render: impl FnOnce(u32) -> Vec<f32> + Send + 'static) {
    std::thread::Builder::new()
        .name("voice-sound".into())
        .spawn(move || {
            let host = cpal::default_host();
            let output = device
                .and_then(|id| host.output_devices().ok()?.find(|d| d.id().ok().is_some_and(|i| i.id() == id)))
                .or_else(|| host.default_output_device());
            let Some(output) = output else { return };
            let Ok(supported) = output.default_output_config() else { return };
            if supported.sample_format() != cpal::SampleFormat::F32 {
                return;
            }
            let config = supported.config();
            let channels = config.channels as usize;
            let samples = Arc::new(render(config.sample_rate));
            let length = samples.len();
            let position = Arc::new(AtomicUsize::new(0));
            let stream = {
                let (samples, position) = (Arc::clone(&samples), Arc::clone(&position));
                output.build_output_stream(
                    config,
                    move |data: &mut [f32], _| {
                        for frame in data.chunks_mut(channels) {
                            let i = position.fetch_add(1, Ordering::Relaxed);
                            let s = samples.get(i).copied().unwrap_or(0.0);
                            frame.fill(s);
                        }
                    },
                    |_| {},
                    None,
                )
            };
            let Ok(stream) = stream else { return };
            if stream.play().is_err() {
                return;
            }
            let ms = (length as u64 * 1000 / u64::from(supported.config().sample_rate)) + 120;
            std::thread::sleep(Duration::from_millis(ms));
        })
        .ok();
}
