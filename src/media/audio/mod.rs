//! One receive-only audio output per UU connection, shared by all its windows.
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
    /// A persistent outage prompts once; transient recovery stays in diagnostics.
    pub notice_error: Option<String>,
    pub receiving: bool,
}

#[derive(Default)]
struct OutputStatus {
    device: String,
    error: Option<String>,
    notice_error: Option<String>,
    source_error: Option<String>,
    notice_suppressed: bool,
}

impl OutputStatus {
    fn failed(&mut self, message: String, exhausted: bool) {
        self.error = Some(message.clone());
        if exhausted && !self.notice_suppressed && self.notice_error.is_none() {
            // One message for the outage, even if device discovery changes the
            // underlying error. Only sustained playback re-arms it.
            self.notice_error = Some(message);
        }
    }

    fn opened(&mut self, device: String) {
        self.device = device;
        self.error = None;
        self.notice_error = None;
        self.notice_suppressed = false;
    }

    fn retrying(&mut self) {
        // Retrying acknowledges this outage; another failed attempt belongs to
        // the same outage and remains visible in audio settings and diagnostics.
        self.notice_error = None;
        self.notice_suppressed = true;
    }
}

/// Each stream owns its first failure. Retired callbacks cannot fail a replacement.
#[derive(Default)]
struct OutputFailure {
    error: OnceLock<String>,
    played: OnceLock<Instant>,
}

impl OutputFailure {
    fn report(&self, shared: &Shared, message: String) {
        if self.error.set(message).is_ok() {
            shared.notify_output();
        }
    }
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
    output_thread: Mutex<Option<std::thread::Thread>>,
    status: Mutex<OutputStatus>,
    output_samples: AtomicU64,
    concealed: AtomicU64,
    callbacks: AtomicU64,
    peak: AtomicU32,
    waveform: Arc<waveform::Waveform>,
    input_level: StereoLevel,
    output_level: StereoLevel,
    preference_updates: tokio::sync::watch::Sender<Option<AudioSettings>>,
}

impl Shared {
    fn notify_output(&self) {
        if let Some(thread) = lock(&self.output_thread).as_ref() {
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
                output_thread: Mutex::new(None),
                status: Mutex::new(OutputStatus::default()),
                output_samples: AtomicU64::new(0),
                concealed: AtomicU64::new(0),
                callbacks: AtomicU64::new(0),
                peak: AtomicU32::new(0),
                waveform: waveform::Waveform::new(),
                input_level: StereoLevel::new(),
                output_level: StereoLevel::new(),
                preference_updates: tokio::sync::watch::channel(None).0,
            }),
            worker: Mutex::new(None),
        }))
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
            device: status.device.clone(),
            error: status
                .source_error
                .as_ref()
                .or(status.error.as_ref())
                .cloned(),
            notice_error: (!status.notice_suppressed)
                .then(|| {
                    status
                        .source_error
                        .as_ref()
                        .or(status.notice_error.as_ref())
                        .cloned()
                })
                .flatten(),
            receiving: shared.receiving.load(Ordering::Relaxed),
        }
    }

    pub fn retry(&self) {
        lock(&self.0.shared.status).retrying();
        self.0.shared.retry.store(true, Ordering::Release);
        let _ = self.start();
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
        lock(&self.0.shared.status).retrying();
        self.0.shared.retry.store(true, Ordering::Release);
        let _ = self.ensure_worker();
        self.0.shared.notify_output();
    }

    pub fn select_source(&self, codec: &str, rate: u32, channels: u16) -> Option<u64> {
        let shared = &self.0.shared;
        let generation = shared.generation.fetch_add(1, Ordering::AcqRel) + 1;
        shared.receiving.store(false, Ordering::Relaxed);
        shared.clear_levels();
        if !codec.eq_ignore_ascii_case("audio/opus") || rate != RATE || channels != 2 {
            let message = format!("暂不支持音频格式：{codec} / {rate} Hz / {channels} 声道");
            tracing::warn!(codec, rate, channels, generation, error = %message, "audio source format rejected");
            lock(&shared.status).source_error = Some(message);
            shared.notify_output();
            return None;
        }
        lock(&shared.status).source_error = None;
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
        if worker.as_ref().is_some_and(JoinHandle::is_finished) {
            if worker.take().unwrap().join().is_err() {
                tracing::warn!("audio output worker panicked; starting a fresh owner");
            }
        }
        if worker.is_some() || self.0.shared.stopped.load(Ordering::Acquire) {
            return Ok(());
        }
        let shared = Arc::clone(&self.0.shared);
        shared.started.store(true, Ordering::Release);
        let thread = std::thread::Builder::new()
            .name("audio-output".into())
            .spawn(move || output_worker(shared))
            .context("创建音频输出线程失败");
        match thread {
            Ok(thread) => *worker = Some(thread),
            Err(error) => {
                self.0.shared.started.store(false, Ordering::Release);
                let message = format!("{error:#}");
                tracing::warn!(error = %message, "audio output worker could not start");
                lock(&self.0.shared.status).failed(message, true);
                return Err(error);
            }
        }
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
                .insert(&packet.data, packet.timestamp, packet.sequence)
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
            "不支持输出采样率：{rate}"
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
                ensure!(used != 0 || written != 0, "音频重采样未前进");
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
    failed: Arc<OutputFailure>,
) -> Result<cpal::Stream> {
    let channels = config.channels as usize;
    ensure!(channels > 0, "输出设备没有声道");
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
                    let pair = if decode_failed.error.get().is_some() {
                        [0.0; 2]
                    } else {
                        renderer.next().unwrap_or_else(|error| {
                            decode_failed.report(&shared, format!("音频处理失败：{error:#}"));
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
                if !output.is_empty()
                    && decode_failed.error.get().is_none()
                    && decode_failed.played.get().is_none()
                    && decode_failed.played.set(Instant::now()).is_ok()
                {
                    shared.notify_output();
                }
            },
            move |error| {
                // Only an actual endpoint/stream error triggers rebuild; video is unaffected.
                failed.report(&errors, format!("音频输出中断：{error}"));
            },
            None,
        )
        .context("打开音频输出失败")?;
    stream.play().context("启动音频输出失败")?;
    Ok(stream)
}

fn open_output(
    device: &cpal::Device,
    shared: Arc<Shared>,
    failed: Arc<OutputFailure>,
) -> Result<cpal::Stream> {
    let config = device
        .default_output_config()
        .context("读取音频输出格式失败")?;
    let format = config.sample_format();
    let config = config.config();
    tracing::info!(?format, rate = config.sample_rate, channels = config.channels,
        buffer = ?config.buffer_size, "audio output format selected");
    match format {
        cpal::SampleFormat::F32 => build_stream::<f32>(device, &config, shared, failed),
        cpal::SampleFormat::F64 => build_stream::<f64>(device, &config, shared, failed),
        cpal::SampleFormat::I16 => build_stream::<i16>(device, &config, shared, failed),
        cpal::SampleFormat::I24 => build_stream::<cpal::I24>(device, &config, shared, failed),
        cpal::SampleFormat::I32 => build_stream::<i32>(device, &config, shared, failed),
        cpal::SampleFormat::U16 => build_stream::<u16>(device, &config, shared, failed),
        _ => Err(anyhow!("不支持输出格式：{format}")),
    }
    .with_context(|| {
        format!(
            "输出配置：{format} / {} Hz / {} 声道",
            config.sample_rate, config.channels
        )
    })
}

fn resolve_output_device(
    host: &cpal::Host,
    selection: Option<&cpal::DeviceId>,
) -> Option<cpal::Device> {
    let id = match selection {
        Some(id) => id.clone(),
        None => host.default_output_device()?.id().ok()?,
    };
    // This owner already follows the default endpoint and rebuilds on changes.
    // Open that concrete endpoint (IMMDevice::Activate), not CPAL's virtual
    // default/ActivateAudioInterfaceAsync stream with a second routing lifetime.
    host.device_by_id(&id).filter(DeviceTrait::supports_output)
}

fn output_worker(shared: Arc<Shared>) {
    // Endpoint invalidation wakes this owner; the periodic check also follows
    // default-device changes while the previous endpoint is still available.
    *lock(&shared.output_thread) = Some(std::thread::current());
    let _exit = crate::lifecycle::OnDrop::new(|| {
        *lock(&shared.output_thread) = None;
        shared.started.store(false, Ordering::Release);
    });
    if let Err(error) = crate::platform::audio_thread::initialize() {
        let message = format!("初始化音频输出线程（COM MTA）失败：{error}");
        tracing::warn!(error = %message,
            com_apartment = %crate::platform::audio_thread::description(),
            "audio output COM initialization failed");
        lock(&shared.status).failed(message, true);
        return;
    }
    tracing::info!(com_apartment = %crate::platform::audio_thread::description(),
        "audio output COM apartment initialized");
    let mut host = cpal::default_host();
    // The Linux PulseAudio host is one server connection for its lifetime;
    // after a stream or enumeration failure the server may have restarted, so
    // the next check connects again. Creating a WASAPI host is free.
    let mut reconnect = false;
    let mut failed = Arc::new(OutputFailure::default());
    let mut stream = None;
    let mut current_id = None;
    let mut current_selection = None;
    let mut current_generation = 0;
    let mut failures = 0_u32;
    let mut confirmed = false;
    let mut next_check = Instant::now();
    let mut next_retry = Instant::now();
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
        // A successful Initialize/Start alone is not recovery: some devices
        // invalidate after a few callbacks. Require sustained playback before
        // treating a replacement as recovery and re-arming outage notifications.
        if stream.is_some()
            && !confirmed
            && failed
                .played
                .get()
                .is_some_and(|at| at.elapsed() >= Duration::from_secs(2))
            && failed.error.get().is_none()
        {
            confirmed = true;
            failures = 0;
            let mut status = lock(&shared.status);
            let name = status.device.clone();
            status.opened(name.clone());
            tracing::info!(device = %name, "native Opus audio output producing");
        }
        if next_check <= Instant::now()
            || retry
            || refresh
            || failed.error.get().is_some()
            || generation != current_generation
            || (stream.is_none() && failures == 0 && shared.receiving.load(Ordering::Acquire))
        {
            if reconnect || failed.error.get().is_some() {
                host = cpal::default_host();
                reconnect = false;
            }
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
            let outputs = match available {
                Ok(devices) => OutputDevices {
                    devices,
                    error: None,
                    loaded: true,
                },
                Err(error) => {
                    reconnect = true;
                    OutputDevices {
                        error: Some(format!("读取输出设备失败：{error}")),
                        loaded: true,
                        ..Default::default()
                    }
                }
            };
            {
                let mut previous = lock(&shared.output_devices);
                if outputs.error != previous.error {
                    if let Some(error) = &outputs.error {
                        tracing::warn!(%error, "audio output device enumeration failed");
                    }
                }
                *previous = outputs;
            }
            let selection = lock(&shared.selected_output).clone();
            let device = resolve_output_device(&host, selection.as_ref());
            let id = device.as_ref().and_then(|device| device.id().ok());
            let name = device
                .as_ref()
                .and_then(|device| device.description().ok())
                .map(|description| description.name().to_owned())
                .unwrap_or_default();
            // Consume the current stream's failure before retiring it on an
            // endpoint change, so a switching failure is still present in logs.
            let stream_error = failed.error.get().cloned();
            if let Some(error) = &stream_error {
                tracing::warn!(device = %lock(&shared.status).device, %error,
                    "audio output interrupted; recovering");
            }
            let changed = id != current_id
                || selection != current_selection
                || retry
                || generation != current_generation;
            if changed {
                tracing::info!(previous = ?current_id, next = ?id, device = %name,
                    explicit = selection.is_some(), retry, generation,
                    "audio output device or source changed");
                failures = 0;
                next_retry = Instant::now();
                stream = None;
                confirmed = false;
                failed = Arc::new(OutputFailure::default());
                shared.clear_levels();
                current_id = id;
                current_selection = selection.clone();
                current_generation = generation;
                // Keep an outage latched across automatic device changes.
                // Clearing it here re-armed a dismissed popup on every switch.
                let mut status = lock(&shared.status);
                status.device = name.clone();
            }
            if let Some(error) = stream_error {
                stream = None;
                confirmed = false;
                failed = Arc::new(OutputFailure::default());
                shared.clear_levels();
                if !changed {
                    failures = failures.saturating_add(1);
                }
                lock(&shared.status).failed(error, failures >= 3);
                if failures >= 3 {
                    next_retry = Instant::now() + Duration::from_secs(5);
                }
            }
            if stream.is_none()
                && Instant::now() >= next_retry
                && shared.receiving.load(Ordering::Acquire)
            {
                // Old audio collected while no output was available must not be replayed.
                while shared.packets.pop().is_some() {}
                failed = Arc::new(OutputFailure::default());
                tracing::info!(device = %name, endpoint = ?current_id, attempt = failures.saturating_add(1),
                    "opening audio output");
                match device.as_ref().map_or_else(
                    || {
                        Err(anyhow!(if selection.is_some() {
                            "所选音频输出设备不可用"
                        } else {
                            "未找到音频输出设备"
                        }))
                    },
                    |device| open_output(device, Arc::clone(&shared), Arc::clone(&failed)),
                ) {
                    Ok(output) => {
                        stream = Some(output);
                        confirmed = false;
                        lock(&shared.status).device = name.clone();
                        tracing::info!(device = %name, host = ?host.id(), "native Opus audio output started");
                    }
                    Err(error) => {
                        failures = failures.saturating_add(1);
                        let message = format!("{error:#}");
                        tracing::warn!(device = %name, endpoint = ?current_id,
                            attempt = failures, exhausted = failures >= 3, activation = "endpoint", error = %message,
                            com_apartment = %crate::platform::audio_thread::description(),
                            callback_error = ?failed.error.get(),
                            "audio output initialization failed");
                        // Failed initialization may have delivered a callback;
                        // its stream is already gone and must not fail the next one.
                        failed = Arc::new(OutputFailure::default());
                        reconnect = true;
                        lock(&shared.status).failed(message, failures >= 3);
                        next_retry =
                            Instant::now() + Duration::from_secs(if failures >= 3 { 5 } else { 1 });
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
