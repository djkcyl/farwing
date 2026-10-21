//! One receive-only audio output per connection, shared by all its windows.
pub(crate) mod dsp;
pub(crate) mod encoder;
pub(crate) mod neteq;
pub(crate) mod sender;
pub(crate) mod waveform;

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, ensure};
use bytes::Bytes;
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use crossbeam_queue::ArrayQueue;
use opusic_c::SampleRate;

const RATE: u32 = 48_000;
const BLOCK: usize = 480;
const MAX_SAMPLES: usize = 5_760;
const MAX_PACKET: usize = 65_535;
pub(crate) const MAX_VOLUME: u8 = 200;
#[derive(Clone, Copy, Debug, serde::Serialize, serde::Deserialize)]
pub(crate) struct AudioSettings {
    pub volume: u8,
    pub muted: bool,
}

#[derive(Clone, Default)]
pub(crate) struct AudioSnapshot {
    pub device: String,
    pub error: Option<String>,
    pub receiving: bool,
}

#[derive(Clone)]
pub(crate) struct OutputDevice {
    pub id: cpal::DeviceId,
    pub name: String,
}

#[derive(Clone, Default)]
pub(crate) struct OutputDevices {
    pub devices: Vec<OutputDevice>,
    pub error: Option<String>,
    pub loaded: bool,
}

struct Packet {
    data: Bytes,
    timestamp: u32,
    sequence: u16,
    generation: u64,
    arrived: std::time::Instant,
}

enum Audit {
    Rejected(u16),
    Statistics(neteq::Statistics),
}

struct Shared {
    audit: ArrayQueue<(u64, Audit)>,
    packets: ArrayQueue<Packet>,
    settings: AtomicU32,
    generation: AtomicU64,
    started: AtomicBool,
    audible: AtomicBool,
    stopped: AtomicBool,
    retry: AtomicBool,
    refresh_devices: AtomicBool,
    selected_output: Mutex<Option<cpal::DeviceId>>,
    output_devices: Mutex<OutputDevices>,
    receiving: AtomicBool,
    output_thread: OnceLock<std::thread::Thread>,
    status: Mutex<(String, Option<String>)>,
    output_samples: AtomicU64,
    concealed: AtomicU64,
    callbacks: AtomicU64,
    peak: AtomicU32,
    waveform: Arc<waveform::Waveform>,
    input_level: StereoLevel,
    output_level: StereoLevel,
    preference_updates: tokio::sync::watch::Sender<Option<AudioSettings>>,
    /// Cinema mode: audio and video keep a playout buffer and stay in step.
    cinema: AtomicBool,
}

/// Cinema-mode playout buffers. Video waits this long after its expected
/// arrival; audio keeps a similar buffer minus the video decode and render
/// time, so both reach the viewer together.
pub(crate) const CINEMA_VIDEO_DELAY: std::time::Duration = std::time::Duration::from_millis(120);
pub(crate) const CINEMA_VIDEO_MAX_DELAY: std::time::Duration = std::time::Duration::from_millis(500);
const CINEMA_AUDIO_DELAY_MS: u32 = 100;

impl Shared {
    fn notify_output(&self) {
        if let Some(thread) = self.output_thread.get() {
            thread.unpark();
        }
    }

    fn clear_levels(&self) {
        self.waveform.clear();
        self.input_level.clear();
        self.output_level.clear();
    }
}

// One published sample for all windows. Reading the meter never consumes a
// peak or touches the audio queue. Release is visual only; PCM is unchanged.
struct StereoLevel {
    origin: Instant,
    sample: AtomicU64,
}

impl StereoLevel {
    fn new() -> Self {
        Self {
            origin: Instant::now(),
            sample: AtomicU64::new(0),
        }
    }

    fn levels_at(&self, now_ms: u32) -> [f32; 2] {
        let sample = self.sample.load(Ordering::Relaxed);
        // A callback can publish between the reader's clock sample and load.
        let age_ms = (now_ms.wrapping_sub((sample >> 32) as u32) as i32).max(0);
        if age_ms > 250 {
            return [0.0; 2];
        }
        let decay = 10.0_f32.powf(-3.0 * age_ms as f32 / 1_000.0);
        [((sample >> 16) as u16), sample as u16].map(|level| f32::from(level) / 65_535.0 * decay)
    }

    fn record(&self, peaks: [f32; 2]) {
        let now_ms = self.origin.elapsed().as_millis() as u32;
        let previous = self.levels_at(now_ms);
        let levels: [u16; 2] = std::array::from_fn(|channel| {
            (peaks[channel].clamp(0.0, 1.0).max(previous[channel]) * 65_535.0).round() as u16
        });
        // Two display-only 16-bit amplitudes plus one timestamp in a single
        // atomic publication; the PCM passed to the device stays unchanged.
        self.sample.store(
            (u64::from(now_ms) << 32) | (u64::from(levels[0]) << 16) | u64::from(levels[1]),
            Ordering::Relaxed,
        );
    }

    fn clear(&self) {
        self.sample.store(0, Ordering::Relaxed);
    }
}

struct Owner {
    shared: Arc<Shared>,
    worker: Mutex<Option<JoinHandle<()>>>,
}

#[derive(Clone)]
pub(crate) struct AudioPlayback(Arc<Owner>);

fn lock<T>(value: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    value
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

impl AudioPlayback {
    pub fn new() -> Self {
        Self(Arc::new(Owner {
            shared: Arc::new(Shared {
                audit: ArrayQueue::new(32),
                packets: ArrayQueue::new(200),
                settings: AtomicU32::new(100),
                generation: AtomicU64::new(0),
                started: AtomicBool::new(false),
                audible: AtomicBool::new(false),
                stopped: AtomicBool::new(false),
                retry: AtomicBool::new(false),
                refresh_devices: AtomicBool::new(false),
                selected_output: Mutex::new(None),
                output_devices: Mutex::new(OutputDevices::default()),
                receiving: AtomicBool::new(false),
                output_thread: OnceLock::new(),
                status: Mutex::new((String::new(), None)),
                output_samples: AtomicU64::new(0),
                concealed: AtomicU64::new(0),
                callbacks: AtomicU64::new(0),
                peak: AtomicU32::new(0),
                waveform: waveform::Waveform::new(),
                input_level: StereoLevel::new(),
                output_level: StereoLevel::new(),
                preference_updates: tokio::sync::watch::channel(None).0,
                cinema: AtomicBool::new(false),
            }),
            worker: Mutex::new(None),
        }))
    }

    /// Cinema mode for this session's audio and video playout.
    pub(crate) fn set_cinema(&self, cinema: bool) {
        self.0.shared.cinema.store(cinema, Ordering::Relaxed);
    }

    pub(crate) fn cinema(&self) -> bool {
        self.0.shared.cinema.load(Ordering::Relaxed)
    }

    pub fn settings(&self) -> AudioSettings {
        let bits = self.0.shared.settings.load(Ordering::Relaxed);
        AudioSettings {
            volume: (bits & 255) as u8,
            muted: bits & 256 != 0,
        }
    }

    pub fn set_settings(&self, settings: AudioSettings) {
        let settings = AudioSettings {
            volume: settings.volume.min(MAX_VOLUME),
            ..settings
        };
        let bits = u32::from(settings.volume) | (u32::from(settings.muted) << 8);
        let previous = self.0.shared.settings.swap(bits, Ordering::Relaxed);
        if settings.muted || settings.volume == 0 {
            self.0.shared.output_level.clear();
        }
        if previous != bits {
            self.0
                .shared
                .preference_updates
                .send_replace(Some(settings));
        }
    }

    pub(crate) fn preference_updates(&self) -> tokio::sync::watch::Receiver<Option<AudioSettings>> {
        self.0.shared.preference_updates.subscribe()
    }

    /// Unity-gain reference at the device-channel mix, before volume/mute.
    /// This is a current display peak, not the lifetime diagnostic `peak`.
    pub(crate) fn waveform(&self) -> waveform::Reader {
        self.0.shared.waveform.subscribe()
    }

    pub fn input_levels(&self) -> [f32; 2] {
        if self.0.shared.stopped.load(Ordering::Acquire)
            || !self.0.shared.audible.load(Ordering::Acquire)
        {
            return [0.0; 2];
        }
        let meter = &self.0.shared.input_level;
        meter.levels_at(meter.origin.elapsed().as_millis() as u32)
    }

    pub fn output_levels(&self) -> [f32; 2] {
        let settings = self.settings();
        if settings.muted
            || settings.volume == 0
            || self.0.shared.stopped.load(Ordering::Acquire)
            || !self.0.shared.audible.load(Ordering::Acquire)
        {
            return [0.0; 2];
        }
        let meter = &self.0.shared.output_level;
        meter.levels_at(meter.origin.elapsed().as_millis() as u32)
    }

    pub fn snapshot(&self) -> AudioSnapshot {
        let shared = &self.0.shared;
        let status = lock(&shared.status);
        AudioSnapshot {
            device: status.0.clone(),
            error: status.1.clone(),
            receiving: shared.receiving.load(Ordering::Relaxed),
        }
    }

    pub fn retry(&self) {
        let _ = self.start();
        self.0.shared.retry.store(true, Ordering::Release);
        self.0.shared.notify_output();
    }

    pub fn output_devices(&self) -> OutputDevices {
        lock(&self.0.shared.output_devices).clone()
    }

    pub fn selected_output(&self) -> Option<cpal::DeviceId> {
        lock(&self.0.shared.selected_output).clone()
    }

    pub fn refresh_output_devices(&self) -> Result<()> {
        self.ensure_worker()?;
        self.0.shared.refresh_devices.store(true, Ordering::Release);
        self.0.shared.notify_output();
        Ok(())
    }

    pub fn set_output_device(&self, device: Option<cpal::DeviceId>) {
        let mut selected = lock(&self.0.shared.selected_output);
        if *selected == device {
            return;
        }
        *selected = device;
        drop(selected);
        self.0.shared.retry.store(true, Ordering::Release);
        self.0.shared.notify_output();
    }

    pub fn select_source(&self, codec: &str, rate: u32, channels: u16) -> Option<u64> {
        let shared = &self.0.shared;
        let generation = shared.generation.fetch_add(1, Ordering::AcqRel) + 1;
        shared.receiving.store(false, Ordering::Relaxed);
        shared.clear_levels();
        if !codec.eq_ignore_ascii_case("audio/opus") || rate != RATE || channels != 2 {
            lock(&shared.status).1 = Some((crate::i18n::t().session.audio_format_unsupported)(codec, rate, channels));
            shared.notify_output();
            return None;
        }
        shared.notify_output();
        Some(generation)
    }

    pub fn receive(&self, generation: u64, data: Bytes, timestamp: u32, sequence: u16) {
        let shared = &self.0.shared;
        if shared.stopped.load(Ordering::Acquire)
            || generation != shared.generation.load(Ordering::Acquire)
        {
            return;
        }
        let Ok(samples) = opusic_c::utils::get_nb_samples(&data, SampleRate::Hz48000) else {
            return;
        };
        // Our negotiated Opus receive fmtp is minptime=10. Keep the jitter
        // processing quantum at 10 ms even when RTP packets contain 20/60 ms.
        if data.len() > MAX_PACKET || !(BLOCK..=MAX_SAMPLES).contains(&samples) {
            return;
        }
        let first_packet = !shared.receiving.swap(true, Ordering::AcqRel);
        // No pre-player backlog, and no unbounded queue while an output device is absent.
        if shared.started.load(Ordering::Acquire) {
            shared.packets.force_push(Packet {
                data,
                timestamp,
                sequence,
                generation,
                arrived: std::time::Instant::now(),
            });
        }
        if first_packet {
            shared.notify_output();
        }
    }

    pub fn start(&self) -> Result<()> {
        self.0.shared.audible.store(true, Ordering::Release);
        self.ensure_worker()
    }

    fn ensure_worker(&self) -> Result<()> {
        let mut worker = lock(&self.0.worker);
        if worker.is_some() || self.0.shared.stopped.load(Ordering::Acquire) {
            return Ok(());
        }
        let shared = Arc::clone(&self.0.shared);
        let thread = std::thread::Builder::new()
            .name("audio-output".into())
            .spawn(move || output_worker(shared))
            .context(crate::i18n::t().session.audio_output_thread_failed);
        match thread {
            Ok(thread) => *worker = Some(thread),
            Err(error) => {
                lock(&self.0.shared.status).1 = Some(error.to_string());
                return Err(error);
            }
        }
        self.0.shared.started.store(true, Ordering::Release);
        Ok(())
    }

    pub async fn close(&self) {
        self.0.shared.stopped.store(true, Ordering::Release);
        self.0.shared.clear_levels();
        let worker = lock(&self.0.worker).take();
        if let Some(worker) = worker {
            worker.thread().unpark();
            let _ = tokio::task::spawn_blocking(move || worker.join()).await;
        }
    }
    pub(crate) fn suspend(&self) {
        self.0.shared.audible.store(false, Ordering::Release);
        self.0.shared.clear_levels();
    }
}

impl Drop for Owner {
    fn drop(&mut self) {
        self.shared.stopped.store(true, Ordering::Release);
        if let Some(worker) = lock(&self.worker).take() {
            worker.thread().unpark();
            let _ = worker.join();
        }
    }
}

struct Engine {
    shared: Arc<Shared>,
    receiver: neteq::Receiver,
    /// The minimum delay last applied to `receiver`.
    minimum_delay: u32,
    generation: u64,
    seen: VecDeque<(u16, u32)>,
    stats: neteq::Statistics,
    blocks: u64,
    started: bool,
}

impl Engine {
    fn new(shared: Arc<Shared>) -> Result<Self> {
        Ok(Self {
            shared,
            receiver: neteq::Receiver::new()?,
            minimum_delay: 0,
            generation: 0,
            seen: VecDeque::with_capacity(256),
            stats: neteq::Statistics::default(),
            blocks: 0,
            started: false,
        })
    }

    fn reset(&mut self, generation: u64) -> Result<()> {
        // FlushBuffers alone retains codec state. A new source gets a new
        // packet timeline, decoder, PLC history and time-stretching state.
        let receiver = neteq::Receiver::new()?;
        self.receiver = receiver;
        self.minimum_delay = 0;
        self.generation = generation;
        self.seen.clear();
        self.stats = neteq::Statistics::default();
        self.blocks = 0;
        self.started = false;
        Ok(())
    }

    fn block(&mut self, output: &mut [f32; BLOCK * 2]) -> Result<()> {
        let generation = self.shared.generation.load(Ordering::Acquire);
        let waveform_epoch = self.shared.waveform.epoch();
        if generation != self.generation {
            self.reset(generation)?;
        }
        let minimum_delay = if self.shared.cinema.load(Ordering::Relaxed) {
            CINEMA_AUDIO_DELAY_MS
        } else {
            0
        };
        if minimum_delay != self.minimum_delay && self.receiver.set_minimum_delay(minimum_delay) {
            self.minimum_delay = minimum_delay;
        }
        for _ in 0..200 {
            let Some(packet) = self.shared.packets.pop() else {
                break;
            };
            if packet.generation != generation
                || self.seen.contains(&(packet.sequence, packet.timestamp))
            {
                continue;
            }
            if self.seen.len() == 256 {
                self.seen.pop_front();
            }
            self.seen.push_back((packet.sequence, packet.timestamp));
            if self
                .receiver
                .insert(&packet.data, packet.timestamp, packet.sequence, packet.arrived)
            {
                self.started = true;
            } else {
                let _ = self
                    .shared
                    .audit
                    .push((generation, Audit::Rejected(packet.sequence)));
            }
        }
        if !self.started {
            output.fill(0.0);
            return Ok(());
        }
        // NetEq owns RTP packet duration, redundant frames, late-packet
        // admission, PLC, Merge and sample-domain time stretching. The device
        // still consumes exactly 10 ms; no application-level PCM truncation.
        let previous = self.stats;
        self.receiver.block(output, &mut self.stats)?;
        self.shared
            .waveform
            .record(output, self.blocks, waveform_epoch);
        let produced = self
            .stats
            .output_samples
            .saturating_sub(previous.output_samples);
        let concealed = self
            .stats
            .concealed_samples
            .saturating_sub(previous.concealed_samples);
        self.shared
            .output_samples
            .fetch_add(produced, Ordering::Relaxed);
        self.shared
            .concealed
            .fetch_add(concealed, Ordering::Relaxed);
        let peak = output
            .iter()
            .fold(0.0_f32, |peak, sample| peak.max(sample.abs()));
        self.shared
            .peak
            .fetch_max(peak.to_bits(), Ordering::Relaxed);
        self.blocks += 1;
        if self.blocks.is_multiple_of(500) {
            let _ = self
                .shared
                .audit
                .push((generation, Audit::Statistics(self.stats)));
        }

        Ok(())
    }
}

impl Drop for Engine {
    fn drop(&mut self) {
        if self.started {
            tracing::info!(?self.stats, "native NetEq playout ended");
        }
    }
}
struct Renderer {
    engine: Engine,
    resampler: Option<dsp::Resampler>,
    input: [f32; BLOCK * 2],
    input_offset: usize,
    output: Vec<f32>,
    output_offset: usize,
    output_len: usize,
}

impl Renderer {
    fn new(shared: Arc<Shared>, rate: u32) -> Result<Self> {
        ensure!(
            (8_000..=384_000).contains(&rate),
            "{}",
            (crate::i18n::t().session.audio_output_rate_unsupported)(rate)
        );
        Ok(Self {
            engine: Engine::new(shared)?,
            resampler: if rate == RATE {
                None
            } else {
                Some(dsp::Resampler::new(rate)?)
            },
            input: [0.0; BLOCK * 2],
            input_offset: BLOCK * 2,
            output: vec![0.0; (rate as usize / 100 + 64) * 2],
            output_offset: 0,
            output_len: 0,
        })
    }

    fn next(&mut self) -> Result<[f32; 2]> {
        while self.output_offset == self.output_len {
            if self.input_offset == self.input.len() {
                self.engine.block(&mut self.input)?;
                self.input_offset = 0;
            }
            if let Some(resampler) = self.resampler.as_mut() {
                let (used, written) =
                    resampler.process(&self.input[self.input_offset..], &mut self.output)?;
                ensure!(used != 0 || written != 0, "{}", crate::i18n::t().session.audio_resample_stalled);
                self.input_offset += used;
                self.output_len = written;
            } else {
                self.output[..self.input.len()].copy_from_slice(&self.input);
                self.input_offset = self.input.len();
                self.output_len = self.input.len();
            }
            self.output_offset = 0;
        }
        let pair = [
            self.output[self.output_offset],
            self.output[self.output_offset + 1],
        ];
        self.output_offset += 2;
        Ok(pair)
    }
}

fn build_stream<T: cpal::SizedSample + cpal::FromSample<f32>>(
    device: &cpal::Device,
    config: &cpal::StreamConfig,
    shared: Arc<Shared>,
    failed: Arc<AtomicBool>,
) -> Result<cpal::Stream> {
    let channels = config.channels as usize;
    ensure!(channels > 0, "{}", crate::i18n::t().session.audio_output_no_channels);
    let mut renderer = Renderer::new(Arc::clone(&shared), config.sample_rate)?;
    let errors = Arc::clone(&shared);
    let decode_failed = Arc::clone(&failed);
    let stream = device
        .build_output_stream(
            *config,
            move |output: &mut [T], _info| {
                shared.callbacks.fetch_add(1, Ordering::Relaxed);
                let bits = shared.settings.load(Ordering::Relaxed);
                let gain = if bits & 256 != 0
                    || shared.stopped.load(Ordering::Acquire)
                    || !shared.audible.load(Ordering::Acquire)
                {
                    0.0
                } else {
                    (bits & 255) as f32 / 100.0
                };
                let mut peaks = [0.0_f32; 2];
                let mut input_peaks = [0.0_f32; 2];
                for frame in output.chunks_mut(channels) {
                    let pair = if decode_failed.load(Ordering::Acquire) {
                        [0.0; 2]
                    } else {
                        renderer.next().unwrap_or_else(|_| {
                            decode_failed.store(true, Ordering::Release);
                            shared.notify_output();
                            [0.0; 2]
                        })
                    };
                    for (channel, sample) in frame.iter_mut().enumerate() {
                        let value = if channels == 1 {
                            (pair[0] + pair[1]) * 0.5
                        } else {
                            pair.get(channel).copied().unwrap_or(0.0)
                        };
                        let input_peak = value.abs();
                        let value = (value * gain).clamp(-1.0, 1.0);
                        if channels == 1 {
                            input_peaks[0] = input_peaks[0].max(input_peak);
                            input_peaks[1] = input_peaks[0];
                            peaks[0] = peaks[0].max(value.abs());
                            peaks[1] = peaks[0];
                        } else if channel < 2 {
                            input_peaks[channel] = input_peaks[channel].max(input_peak);
                            peaks[channel] = peaks[channel].max(value.abs());
                        }
                        *sample = T::from_sample(value);
                    }
                }
                shared.input_level.record(input_peaks);
                shared.output_level.record(peaks);
            },
            move |error| {
                // Only an actual endpoint/stream error triggers rebuild; video is unaffected.
                lock(&errors.status).1 = Some((crate::i18n::t().session.audio_output_interrupted)(&error.to_string()));
                errors.clear_levels();
                failed.store(true, Ordering::Release);
                errors.notify_output();
            },
            None,
        )
        .context(crate::i18n::t().session.audio_output_open_failed)?;
    stream.play().context(crate::i18n::t().session.audio_output_start_failed)?;
    Ok(stream)
}

fn open_output(
    device: &cpal::Device,
    shared: Arc<Shared>,
    failed: Arc<AtomicBool>,
) -> Result<cpal::Stream> {
    let config = device
        .default_output_config()
        .context(crate::i18n::t().session.audio_output_format_unreadable)?;
    let format = config.sample_format();
    let config = config.config();
    match format {
        cpal::SampleFormat::F32 => build_stream::<f32>(device, &config, shared, failed),
        cpal::SampleFormat::F64 => build_stream::<f64>(device, &config, shared, failed),
        cpal::SampleFormat::I16 => build_stream::<i16>(device, &config, shared, failed),
        cpal::SampleFormat::I24 => build_stream::<cpal::I24>(device, &config, shared, failed),
        cpal::SampleFormat::I32 => build_stream::<i32>(device, &config, shared, failed),
        cpal::SampleFormat::U16 => build_stream::<u16>(device, &config, shared, failed),
        _ => Err(anyhow!("{}", (crate::i18n::t().session.audio_output_format_unsupported)(&format.to_string()))),
    }
}

fn output_worker(shared: Arc<Shared>) {
    // CPAL's WASAPI default-device monitor delivers StreamInvalidated /
    // DeviceNotAvailable to the error callback. Wake this owner immediately;
    // the periodic check is only a fallback while no stream can report events.
    let _ = shared.output_thread.set(std::thread::current());
    let host = cpal::default_host();
    let failed = Arc::new(AtomicBool::new(false));
    let mut stream = None;
    let mut current_id = None;
    let mut current_selection = None;
    let mut current_generation = 0;
    let mut failures = 0;
    let mut next_check = Instant::now();
    while !shared.stopped.load(Ordering::Acquire) {
        while let Some((generation, event)) = shared.audit.pop() {
            if generation != shared.generation.load(Ordering::Acquire) {
                continue;
            }
            match event {
                Audit::Rejected(sequence) => {
                    tracing::debug!(sequence, "NetEq rejected an audio packet")
                }
                Audit::Statistics(stats) => tracing::debug!(
                    buffer_ms = stats.buffer_ms,
                    target_ms = stats.target_ms,
                    concealed_samples = stats.concealed_samples,
                    inserted_samples = stats.inserted_samples,
                    removed_samples = stats.removed_samples,
                    discarded_packets = stats.discarded_packets,
                    "native NetEq playout"
                ),
            }
        }

        let retry = shared.retry.swap(false, Ordering::AcqRel);
        let refresh = shared.refresh_devices.swap(false, Ordering::AcqRel);
        let generation = shared.generation.load(Ordering::Acquire);
        if next_check <= Instant::now()
            || retry
            || refresh
            || failed.load(Ordering::Acquire)
            || generation != current_generation
            || (stream.is_none() && failures == 0 && shared.receiving.load(Ordering::Acquire))
        {
            let available = host.output_devices().map(|devices| {
                devices
                    .filter_map(|device| {
                        Some(OutputDevice {
                            id: device.id().ok()?,
                            name: device.description().ok()?.name().to_owned(),
                        })
                    })
                    .collect()
            });
            *lock(&shared.output_devices) = match available {
                Ok(devices) => OutputDevices {
                    devices,
                    error: None,
                    loaded: true,
                },
                Err(error) => OutputDevices {
                    error: Some((crate::i18n::t().session.audio_output_devices_unreadable)(&error.to_string())),
                    loaded: true,
                    ..Default::default()
                },
            };
            let selection = lock(&shared.selected_output).clone();
            let device = match &selection {
                Some(id) => host.device_by_id(id).filter(DeviceTrait::supports_output),
                None => host.default_output_device(),
            };
            let id = device.as_ref().and_then(|device| device.id().ok());
            if id != current_id
                || selection != current_selection
                || retry
                || generation != current_generation
            {
                failures = 0;
                stream = None;
                shared.clear_levels();
                current_id = id;
                current_selection = selection.clone();
                current_generation = generation;
                *lock(&shared.status) = (String::new(), None);
            }
            if failed.swap(false, Ordering::AcqRel) {
                stream = None;
                shared.clear_levels();
                failures += 1;
                lock(&shared.status)
                    .1
                    .get_or_insert_with(|| crate::i18n::t().session.audio_processing_failed.into());
            }
            if stream.is_none() && failures < 3 && shared.receiving.load(Ordering::Acquire) {
                // Old audio collected while no output was available must not be replayed.
                while shared.packets.pop().is_some() {}
                match device.as_ref().map_or_else(
                    || {
                        Err(anyhow!(
                            "{}",
                            if selection.is_some() {
                                crate::i18n::t().session.audio_output_selected_unavailable
                            } else {
                                crate::i18n::t().session.audio_output_none
                            }
                        ))
                    },
                    |device| open_output(device, Arc::clone(&shared), Arc::clone(&failed)),
                ) {
                    Ok(output) => {
                        // Recovery succeeded. A later stream interruption is a
                        // new incident, not another failure of this attempt.
                        // Every restart event is handled; the three-attempt
                        // limit is local to device reselection.
                        failures = 0;
                        stream = Some(output);
                        let name = device
                            .as_ref()
                            .and_then(|device| device.description().ok())
                            .map(|description| description.name().to_owned())
                            .unwrap_or_default();
                        *lock(&shared.status) = (name.clone(), None);
                        tracing::info!(device = %name, "native Opus audio output started");
                    }
                    Err(error) => {
                        lock(&shared.status).1 = Some(error.to_string());
                        failures += 1;
                    }
                }
            }
            next_check = Instant::now() + Duration::from_secs(1);
        }
        std::thread::park_timeout(Duration::from_millis(100));
    }
    drop(stream);
    shared.clear_levels();
    tracing::info!(
        output_samples = shared.output_samples.load(Ordering::Relaxed),
        concealed_samples = shared.concealed.load(Ordering::Relaxed),
        output_callbacks = shared.callbacks.load(Ordering::Relaxed),
        peak = f32::from_bits(shared.peak.load(Ordering::Relaxed)),
        "native audio output closed"
    );
}
