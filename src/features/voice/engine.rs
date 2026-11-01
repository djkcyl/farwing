//! The voice media (`docs/protocol/chat-design.md` §7.4): the microphone to
//! an RTP track — gated by activity or push-to-talk, Opus, frame-encrypted,
//! with its level in the header — and every received track to the call
//! output: decrypted, decoded and mixed, each sender with its level.

use super::capture::{BLOCK, Capture, CaptureConfig};
use super::settings::{Mode, VoiceSettings};
use crate::media::audio::{AudioPlayback, AudioSettings, mix};
use crate::media::frame_crypto::{Codec, Decryptor, Encryptor, FrameKey};
use bytes::Bytes;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio_util::sync::CancellationToken;
use webrtc::rtp::extension::HeaderExtension;
use webrtc::rtp::extension::audio_level_extension::AudioLevelExtension;
use webrtc::rtp::packet::Packet;
use webrtc::track::track_local::track_local_static_rtp::TrackLocalStaticRTP;
use webrtc::track::track_remote::TrackRemote;

/// 20 ms of mono at 48 kHz: one packet.
const FRAME: usize = BLOCK * 2;
/// Activity keeps sending this long after the level falls.
const ACTIVITY_HOLD: Duration = Duration::from_millis(400);
/// Push-to-talk keeps sending this long after the key is let go.
const PTT_TAIL: Duration = Duration::from_millis(200);
/// A sender counts as speaking while its level is over this (dBov) and fresh.
const SPEAKING_DBOV: u8 = 50;
const SPEAKING_FRESH: Duration = Duration::from_millis(300);

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|p| p.into_inner())
}

/// The audio-level extension's ID (the shared tables).
fn audio_level_id() -> u8 {
    farwing_rtc_wire::header_extension_ids(&[])
        .into_iter()
        .find(|(_, uri)| *uri == farwing_rtc_wire::AUDIO_LEVEL_URI)
        .map_or(3, |(id, _)| id)
}

/// What one sender's track brought (`dev voice`).
#[derive(Clone, Debug, serde::Serialize)]
pub(crate) struct ReceiverStats {
    pub participant: u32,
    pub packets: u64,
    /// Decrypted (all of them in compatible mode).
    pub opened: u64,
    /// The last level in the header (dBov).
    pub level: u8,
    /// The last decoded 10 ms's peak (0–1).
    pub decoded_peak: f32,
}

struct Receiver {
    voice: Arc<mix::Voice>,
    counts: Arc<(std::sync::atomic::AtomicU64, std::sync::atomic::AtomicU64)>,
    decryptor: Arc<Mutex<Decryptor>>,
    user: String,
    /// (level in dBov, when), from the packets' header.
    level: Arc<Mutex<(u8, Instant)>>,
    task: tokio::task::JoinHandle<()>,
}

struct Inner {
    settings: Mutex<VoiceSettings>,
    capture: Mutex<Option<Capture>>,
    track: Mutex<Option<Arc<TrackLocalStaticRTP>>>,
    e2ee: AtomicBool,
    encryptor: Mutex<Option<Encryptor>>,
    bitrate: AtomicU32,
    self_muted: AtomicBool,
    self_deafened: AtomicBool,
    server_muted: AtomicBool,
    server_deafened: AtomicBool,
    /// Mute (bit 0) and deafen (bit 1) keys pressed since last asked.
    toggles: std::sync::atomic::AtomicU8,
    speaking: AtomicBool,
    playback: AudioPlayback,
    receivers: Mutex<HashMap<u32, Receiver>>,
    /// Keys that came before their sender's track.
    early_keys: Mutex<HashMap<u32, Vec<FrameKey>>>,
    /// What the senders who left brought.
    gone: Mutex<Vec<ReceiverStats>>,
    stop: CancellationToken,
}

#[derive(Clone)]
pub(crate) struct Engine(Arc<Inner>);

impl Engine {
    pub(crate) fn start(settings: VoiceSettings, runtime: &tokio::runtime::Handle) -> Self {
        let playback = AudioPlayback::new();
        let inner = Arc::new(Inner {
            settings: Mutex::new(settings.clone()),
            capture: Mutex::new(None),
            track: Mutex::new(None),
            e2ee: AtomicBool::new(false),
            encryptor: Mutex::new(None),
            bitrate: AtomicU32::new(64_000),
            self_muted: AtomicBool::new(false),
            self_deafened: AtomicBool::new(false),
            server_muted: AtomicBool::new(false),
            server_deafened: AtomicBool::new(false),
            toggles: std::sync::atomic::AtomicU8::new(0),
            speaking: AtomicBool::new(false),
            playback,
            receivers: Mutex::new(HashMap::new()),
            early_keys: Mutex::new(HashMap::new()),
            gone: Mutex::new(Vec::new()),
            stop: CancellationToken::new(),
        });
        let engine = Self(inner);
        engine.apply_settings(&settings, true);
        let _ = engine.0.playback.start();
        runtime.spawn(send(Arc::clone(&engine.0)));
        engine
    }

    fn capture_config(settings: &VoiceSettings) -> CaptureConfig {
        CaptureConfig {
            input: settings.input.clone(),
            output: settings.output.clone(),
            echo: settings.echo,
            noise: settings.noise,
            gain: settings.gain,
            tone: std::env::var("FARWING_DEV_VOICE_TONE").ok().and_then(|v| v.parse().ok()),
        }
    }

    fn apply_settings(&self, settings: &VoiceSettings, first: bool) {
        let old = lock(&self.0.settings).clone();
        let restart = first
            || old.input != settings.input
            || old.output != settings.output
            || old.echo != settings.echo
            || old.noise != settings.noise
            || old.gain != settings.gain;
        *lock(&self.0.settings) = settings.clone();
        if restart {
            // The old capture stops (and joins its thread) before the new one.
            lock(&self.0.capture).take();
            *lock(&self.0.capture) = Some(Capture::start(Self::capture_config(settings)));
        }
        if first || old.output != settings.output {
            self.0.playback.set_output_device(
                settings
                    .output
                    .as_ref()
                    .map(|id| cpal::DeviceId::new(cpal::HostId::Wasapi, id)),
            );
        }
        self.apply_output();
        for r in lock(&self.0.receivers).values() {
            r.voice.set_gain(f32::from(*settings.volumes.get(&r.user).unwrap_or(&100)) / 100.0);
        }
    }

    pub(crate) fn set_settings(&self, settings: &VoiceSettings) {
        self.apply_settings(settings, false);
    }

    fn apply_output(&self) {
        let deaf = self.0.self_deafened.load(Ordering::Relaxed) || self.0.server_deafened.load(Ordering::Relaxed);
        let volume = lock(&self.0.settings).volume;
        self.0.playback.set_settings(AudioSettings { volume, muted: deaf });
    }

    /// The uplink's track (`None`: not sending).
    pub(crate) fn set_track(&self, track: Option<Arc<TrackLocalStaticRTP>>) {
        *lock(&self.0.track) = track;
    }

    pub(crate) fn set_bitrate(&self, kbps: u32) {
        self.0.bitrate.store(kbps.clamp(16, 128) * 1000, Ordering::Relaxed);
    }

    /// End-to-end: frames are sealed with this device's key and opened
    /// with each sender's.
    pub(crate) fn set_e2ee(&self, on: bool) {
        self.0.e2ee.store(on, Ordering::Relaxed);
    }

    /// This device's frame key (a new one replaces the old: sent with
    /// the new ID from now on).
    pub(crate) fn set_key(&self, key: FrameKey) {
        let mut enc = lock(&self.0.encryptor);
        match enc.as_mut() {
            Some(e) => e.set_key(key),
            None => *enc = Some(Encryptor::new(key)),
        }
    }

    /// A sender's frame key.
    pub(crate) fn add_key(&self, participant: u32, key: FrameKey) {
        if let Some(r) = lock(&self.0.receivers).get(&participant) {
            lock(&r.decryptor).add_key(key);
            return;
        }
        lock(&self.0.early_keys).entry(participant).or_default().push(key);
    }

    pub(crate) fn set_self(&self, muted: bool, deafened: bool) {
        self.0.self_muted.store(muted, Ordering::Relaxed);
        self.0.self_deafened.store(deafened, Ordering::Relaxed);
        self.apply_output();
    }

    pub(crate) fn set_server(&self, muted: bool, deafened: bool) {
        self.0.server_muted.store(muted, Ordering::Relaxed);
        self.0.server_deafened.store(deafened, Ordering::Relaxed);
        self.apply_output();
    }

    /// The mute (bit 0) and deafen (bit 1) keys pressed since last asked.
    pub(crate) fn take_toggles(&self) -> u8 {
        self.0.toggles.swap(0, Ordering::Relaxed)
    }

    /// A received track of `participant` (account `user`).
    pub(crate) fn receive(&self, participant: u32, user: &str, track: Arc<TrackRemote>) {
        let Ok(voice) = self.0.playback.add_voice() else { return };
        let gain = f32::from(*lock(&self.0.settings).volumes.get(user).unwrap_or(&100)) / 100.0;
        voice.set_gain(gain);
        let decryptor = Arc::new(Mutex::new(Decryptor::default()));
        for key in lock(&self.0.early_keys).remove(&participant).unwrap_or_default() {
            lock(&decryptor).add_key(key);
        }
        let level = Arc::new(Mutex::new((127u8, Instant::now())));
        let counts = Arc::new((std::sync::atomic::AtomicU64::new(0), std::sync::atomic::AtomicU64::new(0)));
        let task = {
            let (inner, voice, decryptor, level, counts) = (
                Arc::clone(&self.0),
                Arc::clone(&voice),
                Arc::clone(&decryptor),
                Arc::clone(&level),
                Arc::clone(&counts),
            );
            tokio::spawn(async move {
                let level_id = audio_level_id();
                let mut warned = false;
                while let Ok((packet, _)) = track.read_rtp().await {
                    counts.0.fetch_add(1, Ordering::Relaxed);
                    if let Some(ext) = packet.header.get_extension(level_id)
                        && let Some(byte) = ext.first()
                    {
                        *lock(&level) = (byte & 0x7F, Instant::now());
                    }
                    let payload = if inner.e2ee.load(Ordering::Relaxed) {
                        match lock(&decryptor).decrypt(Codec::Opus, &packet.payload) {
                            Ok(p) => Bytes::from(p),
                            Err(_) => {
                                if !warned {
                                    tracing::debug!(participant, "voice frame not opened yet");
                                    warned = true;
                                }
                                continue;
                            }
                        }
                    } else {
                        packet.payload.clone()
                    };
                    counts.1.fetch_add(1, Ordering::Relaxed);
                    voice.receive(payload, packet.header.timestamp, packet.header.sequence_number);
                }
                voice.end();
            })
        };
        let old = lock(&self.0.receivers).insert(
            participant,
            Receiver {
                voice,
                counts,
                decryptor,
                user: user.to_owned(),
                level,
                task,
            },
        );
        if let Some(old) = old {
            old.task.abort();
            old.voice.end();
        }
    }

    /// `participant` is gone.
    pub(crate) fn forget(&self, participant: u32) {
        if let Some(r) = lock(&self.0.receivers).remove(&participant) {
            r.task.abort();
            r.voice.end();
            lock(&self.0.gone).push(ReceiverStats {
                participant,
                packets: r.counts.0.load(Ordering::Relaxed),
                opened: r.counts.1.load(Ordering::Relaxed),
                level: lock(&r.level).0,
                decoded_peak: r.voice.peak(),
            });
        }
        lock(&self.0.early_keys).remove(&participant);
    }

    /// Who speaks now: the participants, and whether this device does.
    pub(crate) fn speaking(&self) -> (Vec<u32>, bool) {
        let mut out: Vec<u32> = lock(&self.0.receivers)
            .iter()
            .filter(|(_, r)| {
                let (level, at) = *lock(&r.level);
                level < SPEAKING_DBOV && at.elapsed() < SPEAKING_FRESH
            })
            .map(|(p, _)| *p)
            .collect();
        out.sort_unstable();
        (out, self.0.speaking.load(Ordering::Relaxed))
    }

    pub(crate) fn stats(&self) -> Vec<ReceiverStats> {
        let mut out: Vec<ReceiverStats> = lock(&self.0.gone).clone();
        out.extend(lock(&self.0.receivers)
            .iter()
            .map(|(p, r)| ReceiverStats {
                participant: *p,
                packets: r.counts.0.load(Ordering::Relaxed),
                opened: r.counts.1.load(Ordering::Relaxed),
                level: lock(&r.level).0,
                decoded_peak: r.voice.peak(),
            }));
        out.sort_by_key(|s| s.participant);
        out
    }

    /// The microphone's level now (0–1), for a meter.
    pub(crate) fn input_level(&self) -> f32 {
        lock(&self.0.capture).as_ref().map_or(0.0, Capture::peak)
    }

    pub(crate) fn capture_error(&self) -> Option<String> {
        lock(&self.0.capture).as_ref().and_then(Capture::error)
    }

    pub(crate) async fn close(&self) {
        self.0.stop.cancel();
        lock(&self.0.capture).take();
        let receivers: Vec<Receiver> = lock(&self.0.receivers).drain().map(|(_, r)| r).collect();
        for r in receivers {
            r.task.abort();
            r.voice.end();
        }
        self.0.playback.close().await;
    }
}

fn dbov(frame: &[f32]) -> (u8, f32) {
    let rms = (frame.iter().map(|x| f64::from(*x).powi(2)).sum::<f64>() / frame.len().max(1) as f64).sqrt();
    if rms > 0.0 {
        ((-20.0 * rms.log10()).round().clamp(0.0, 127.0) as u8, (20.0 * rms.log10()) as f32)
    } else {
        (127, -127.0)
    }
}

/// The send loop: blocks from the microphone, 20-ms packets out.
async fn send(inner: Arc<Inner>) {
    let mut frame: Vec<f32> = Vec::with_capacity(FRAME);
    let mut encoder: Option<(u32, opusic_c::Encoder)> = None;
    let mut out = [0u8; 1500];
    let mut sequence: u16 = rand::random();
    let mut timestamp: u32 = rand::random();
    let mut open_until: Option<Instant> = None;
    let mut was_open = false;
    let mut silent_frames: u64 = 0;
    let mut keys_down = (false, false);
    // The background noise, followed slowly upwards and quickly downwards.
    let mut floor_db = -60f32;
    let mut tick = tokio::time::interval(Duration::from_millis(10));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        tokio::select! {
            _ = inner.stop.cancelled() => return,
            _ = tick.tick() => {}
        }
        // The toggle keys, read as they are (no hook): a press counts once.
        {
            let settings = lock(&inner.settings);
            let down = |k: Option<super::settings::Key>| {
                // SAFETY: a plain query of the key's state.
                k.is_some_and(|k| unsafe { windows::Win32::UI::Input::KeyboardAndMouse::GetAsyncKeyState(i32::from(k.vk)) < 0 })
            };
            let now = (down(settings.mute_key), down(settings.deafen_key));
            if now.0 && !keys_down.0 {
                inner.toggles.fetch_or(1, Ordering::Relaxed);
            }
            if now.1 && !keys_down.1 {
                inner.toggles.fetch_or(2, Ordering::Relaxed);
            }
            keys_down = now;
        }
        loop {
            let block = {
                let capture = lock(&inner.capture);
                match capture.as_ref().and_then(|c| c.blocks.pop()) {
                    Some(b) => b,
                    None => break,
                }
            };
            frame.extend_from_slice(&block);
            if frame.len() < FRAME {
                continue;
            }
            let pcm: Vec<f32> = frame.drain(..FRAME).collect();
            let this_ts = timestamp;
            timestamp = timestamp.wrapping_add(FRAME as u32);
            let (level, db) = dbov(&pcm);
            floor_db = if db < floor_db { floor_db + (db - floor_db) * 0.2 } else { floor_db + (db - floor_db) * 0.005 };
            let settings = lock(&inner.settings).clone();
            let silenced = inner.self_muted.load(Ordering::Relaxed)
                || inner.self_deafened.load(Ordering::Relaxed)
                || inner.server_muted.load(Ordering::Relaxed);
            let now = Instant::now();
            let wants = match settings.mode {
                // The key is read as it is, in the foreground or not: no
                // hook, no raw input taken from the windows.
                Mode::PushToTalk => {
                    let held = settings.key.is_some_and(|k| {
                        // SAFETY: a plain query of the key's state.
                        unsafe { windows::Win32::UI::Input::KeyboardAndMouse::GetAsyncKeyState(i32::from(k.vk)) < 0 }
                    });
                    held.then_some(PTT_TAIL)
                }
                Mode::Activity => {
                    let threshold = settings.threshold_db.unwrap_or((floor_db + 12.0).clamp(-55.0, -30.0));
                    (db > threshold).then_some(ACTIVITY_HOLD)
                }
            };
            if let Some(hold) = wants {
                open_until = Some(now + hold);
            }
            let open = !silenced && open_until.is_some_and(|t| now < t);
            inner.speaking.store(open, Ordering::Relaxed);
            // Quiet: a silent packet every half second keeps the path and
            // the forwarder's view of this device alive.
            silent_frames = if open { 0 } else { silent_frames + 1 };
            let keepalive = !open && silent_frames % 25 == 0;
            if !open && !keepalive {
                was_open = false;
                continue;
            }
            let pcm = if open { pcm } else { vec![0f32; FRAME] };
            let level = if open { level } else { 127 };
            let Some(track) = lock(&inner.track).clone() else { continue };
            let bitrate = inner.bitrate.load(Ordering::Relaxed);
            if encoder.as_ref().is_none_or(|(b, _)| *b != bitrate) {
                let config = crate::media::audio::encoder::Config {
                    stereo: false,
                    fec: true,
                    dtx: true,
                    cbr: false,
                    bitrate,
                    bitrate_limit: bitrate,
                    packet_ms: 20,
                    max_playback_rate: 48_000,
                };
                match crate::media::audio::encoder::create_encoder(config) {
                    Ok(e) => encoder = Some((bitrate, e)),
                    Err(e) => {
                        tracing::warn!(error = ?e, "voice encoder failed");
                        continue;
                    }
                }
            }
            let Some((_, enc)) = encoder.as_mut() else { continue };
            let length = match enc.encode_float_to_slice(&pcm, &mut out) {
                Ok(n) => n,
                Err(e) => {
                    tracing::debug!(error = ?e, "voice frame not encoded");
                    continue;
                }
            };
            let payload = if inner.e2ee.load(Ordering::Relaxed) {
                // Nothing goes out unsealed in an end-to-end channel.
                let mut enc = lock(&inner.encryptor);
                let Some(e) = enc.as_mut() else { continue };
                match e.encrypt(Codec::Opus, &out[..length]) {
                    Ok(p) => Bytes::from(p),
                    Err(_) => continue,
                }
            } else {
                Bytes::copy_from_slice(&out[..length])
            };
            let packet = Packet {
                header: webrtc::rtp::header::Header {
                    version: 2,
                    marker: !was_open,
                    sequence_number: sequence,
                    timestamp: this_ts,
                    ..Default::default()
                },
                payload,
            };
            sequence = sequence.wrapping_add(1);
            was_open = open;
            let extensions = [HeaderExtension::AudioLevel(AudioLevelExtension { level, voice: open })];
            if let Err(e) = track.write_rtp_with_extensions(&packet, &extensions).await {
                tracing::trace!(error = %e, "voice packet not written");
            }
        }
    }
}
