//! Voice sounds (`docs/protocol/chat-design.md` §7.5): a ringing call,
//! joining and leaving, people coming and going, mute. Short synthesized
//! tones on the call output, each on a thread of its own.

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
            let samples = Arc::new(render(sound, config.sample_rate, volume));
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
