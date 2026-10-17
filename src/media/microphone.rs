//! Per-device microphone sender. No capture occurs until user permission,
//! the policy reply and the host's MicOpened event have all arrived.
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};
use std::time::{Duration, Instant};

pub(crate) use crate::media::audio::encoder::Config as Encoding;
use crate::media::audio::sender::{Frame, Source, Transmitter};
use anyhow::{Context, Result, anyhow, ensure};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use crossbeam_queue::ArrayQueue;
use tokio::sync::Notify;
use webrtc::rtp_transceiver::rtp_sender::RTCRtpSender;
use webrtc::track::track_local::track_local_static_rtp::TrackLocalStaticRTP;

const RATE: u32 = 48_000;
const BLOCK: usize = 480;

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

#[derive(Clone, Default)]
pub(crate) struct Snapshot {
    pub enabled: bool,
    pub pending: bool,
    pub remote_active: bool,
    pub capturing: bool,
    pub device: String,
    pub error: Option<String>,
    pub level: f32,
}

#[derive(Clone)]
pub(crate) struct InputDevice {
    pub id: cpal::DeviceId,
    pub name: String,
}

#[derive(Clone, Default)]
pub(crate) struct InputDevices {
    pub devices: Vec<InputDevice>,
    pub error: Option<String>,
    pub updated: Option<Instant>,
}

#[derive(Default)]
struct State {
    quality: crate::media::audio::encoder::Quality,
    selected_input: Option<cpal::DeviceId>,
    faulted: bool,
    cleanup_on_connect: bool,
    enabled: bool,
    confirmed: bool,
    remote_active: bool,
    pending: Option<(i64, bool)>,
    encoding: Option<Encoding>,
    device: String,
    error: Option<String>,
}

struct Shared {
    quality_updates: tokio::sync::watch::Sender<Option<crate::media::audio::encoder::Quality>>,
    input_devices: Mutex<InputDevices>,
    refresh_devices: AtomicBool,
    state: Mutex<State>,
    generation: AtomicU64,
    capture: AtomicBool,
    capturing: AtomicBool,
    stopped: AtomicBool,
    peak: AtomicU32,
    frames: ArrayQueue<Frame>,
    wake: Notify,
    owner: OnceLock<std::thread::Thread>,
    thread: Mutex<Option<std::thread::JoinHandle<()>>>,
    transmitter: Transmitter,
    report_wake: Notify,
}

impl Shared {
    fn refresh(&self, state: &State) {
        let wanted = state.enabled
            && !state.faulted
            && state.confirmed
            && state.remote_active
            && state.encoding.is_some()
            && !self.stopped.load(Ordering::Acquire);
        if self.capture.swap(wanted, Ordering::AcqRel) != wanted {
            self.generation.fetch_add(1, Ordering::AcqRel);
            self.peak.store(0, Ordering::Relaxed);
        }
        self.notify();
    }
    fn notify(&self) {
        if let Some(t) = self.owner.get() {
            t.unpark();
        }
        self.wake.notify_one();
        self.report_wake.notify_one();
    }
    fn current(&self, generation: u64) -> bool {
        self.capture.load(Ordering::Acquire)
            && self.generation.load(Ordering::Acquire) == generation
            && !self.stopped.load(Ordering::Acquire)
    }
}

#[derive(Clone)]
pub(crate) struct Microphone(Arc<Shared>);

impl Microphone {
    pub(crate) fn quality(&self) -> crate::media::audio::encoder::Quality {
        lock(&self.0.state).quality
    }
    pub(crate) fn set_quality(&self, quality: crate::media::audio::encoder::Quality) -> Result<()> {
        quality.validate()?;
        let mut state = lock(&self.0.state);
        if state.quality != quality {
            state.quality = quality;
            self.0.quality_updates.send_replace(Some(quality));
            self.0.wake.notify_one();
        }
        Ok(())
    }
    pub(crate) fn quality_updates(
        &self,
    ) -> tokio::sync::watch::Receiver<Option<crate::media::audio::encoder::Quality>> {
        self.0.quality_updates.subscribe()
    }
    pub(crate) fn target_bitrate(&self) -> Option<u32> {
        let state = lock(&self.0.state);
        state
            .encoding
            .map(|config| config.quality(state.quality).bitrate)
    }
    pub(crate) fn new() -> Self {
        Self(Arc::new(Shared {
            quality_updates: tokio::sync::watch::channel(None).0,
            input_devices: Mutex::new(InputDevices::default()),
            refresh_devices: AtomicBool::new(false),
            state: Mutex::new(State::default()),
            generation: AtomicU64::new(0),
            capture: AtomicBool::new(false),
            capturing: AtomicBool::new(false),
            stopped: AtomicBool::new(false),
            peak: AtomicU32::new(0),
            frames: ArrayQueue::new(10),
            wake: Notify::new(),
            owner: OnceLock::new(),
            thread: Mutex::new(None),
            transmitter: Transmitter::default(),
            report_wake: Notify::new(),
        }))
    }
    pub(crate) fn snapshot(&self) -> Snapshot {
        let s = lock(&self.0.state);
        Snapshot {
            enabled: s.enabled,
            pending: s.pending.is_some(),
            remote_active: s.remote_active,
            capturing: self.0.capture.load(Ordering::Acquire)
                && self.0.capturing.load(Ordering::Acquire),
            device: s.device.clone(),
            error: s.error.clone(),
            level: f32::from_bits(self.0.peak.load(Ordering::Relaxed)),
        }
    }
    pub(crate) fn selected_input(&self) -> Option<cpal::DeviceId> {
        lock(&self.0.state).selected_input.clone()
    }
    pub(crate) fn input_devices(&self) -> InputDevices {
        let devices = lock(&self.0.input_devices).clone();
        if devices
            .updated
            .is_some_and(|at| at.elapsed() >= Duration::from_secs(2))
        {
            self.0.refresh_devices.store(true, Ordering::Release);
            self.0.notify();
        }
        devices
    }
    pub(crate) fn refresh_input_devices(&self) -> Result<()> {
        self.start()?;
        self.0.refresh_devices.store(true, Ordering::Release);
        self.0.notify();
        Ok(())
    }
    pub(crate) fn set_input_device(&self, device: Option<cpal::DeviceId>) {
        let mut state = lock(&self.0.state);
        if state.selected_input == device {
            return;
        }
        state.selected_input = device;
        state.device.clear();
        state.error = None;
        state.faulted = false;
        // Invalidate callbacks and queued PCM before the old device is closed.
        self.0.generation.fetch_add(1, Ordering::AcqRel);
        self.0.capturing.store(false, Ordering::Release);
        self.0.peak.store(0, Ordering::Relaxed);
        self.0.refresh(&state);
    }
    pub(crate) fn configured(&self) -> bool {
        lock(&self.0.state).encoding.is_some()
    }
    pub(crate) fn configure(&self, encoding: Option<Encoding>) {
        let mut s = lock(&self.0.state);
        if s.encoding != encoding {
            tracing::info!(?encoding, "microphone audio sender negotiated");
            self.0.generation.fetch_add(1, Ordering::AcqRel);
        }
        s.encoding = encoding;
        self.0.refresh(&s);
    }
    pub(crate) fn request(&self, seq: i64, enabled: bool) {
        let mut s = lock(&self.0.state);
        s.cleanup_on_connect = false;
        s.enabled = enabled;
        s.faulted = false;
        if enabled {
            self.0.transmitter.reset_loss();
        }
        s.confirmed = false;
        // A host can send MicOpened before the policy reply; retain that event
        // until reply, but never across a user close or a new enable request.
        s.remote_active = false;
        s.pending = Some((seq, enabled));
        s.error = None;
        self.0.refresh(&s);
    }
    pub(crate) fn response(&self, seq: i64, code: i32) {
        let mut s = lock(&self.0.state);
        let Some((pending, enabled)) = s.pending else {
            return;
        };
        if pending != seq {
            return;
        }
        s.pending = None;
        s.confirmed = code == 0 && enabled;
        if code != 0 {
            s.enabled = false;
            s.error = Some(crate::protocol::microphone::failure_message(code));
        }
        self.0.refresh(&s);
        tracing::info!(seq, code, enabled, "microphone policy reply");
    }
    pub(crate) fn send_failed(&self, seq: i64, error: &str) -> bool {
        let mut s = lock(&self.0.state);
        if !s.pending.is_some_and(|(id, _)| id == seq) {
            return false;
        }
        s.pending = None;
        s.enabled = false;
        s.confirmed = false;
        s.error = Some(format!("麦克风请求发送失败，远端状态未确认：{error}"));
        self.0.refresh(&s);
        true
    }
    pub(crate) fn event(&self, action: i32) {
        let mut s = lock(&self.0.state);
        if !s.enabled {
            return;
        }
        match action {
            23 => s.remote_active = true,
            24 => s.remote_active = false,
            27 => {
                self.0.generation.fetch_add(1, Ordering::AcqRel);
            }
            _ => return,
        }
        self.0.refresh(&s);
        tracing::info!(
            action,
            confirmed = s.confirmed,
            "remote microphone activity"
        );
    }
    pub(crate) fn disconnect(&self) {
        let mut s = lock(&self.0.state);
        s.cleanup_on_connect |= s.enabled || s.confirmed || s.pending.is_some();
        s.enabled = false;
        s.confirmed = false;
        s.remote_active = false;
        s.pending = None;
        self.0.refresh(&s);
    }
    pub(crate) fn needs_cleanup(&self) -> bool {
        lock(&self.0.state).cleanup_on_connect
    }
    pub(crate) fn retry(&self) {
        let mut s = lock(&self.0.state);
        s.error = None;
        s.faulted = false;
        self.0.generation.fetch_add(1, Ordering::AcqRel);
        self.0.refresh(&s);
    }

    fn fault(&self, error: String) {
        let mut s = lock(&self.0.state);
        s.error = Some(error);
        s.faulted = true;
        self.0.refresh(&s);
    }
    pub(crate) fn stop(&self) {
        self.0.stopped.store(true, Ordering::Release);
        self.disconnect();
    }
    pub(crate) async fn close(&self) {
        self.stop();
        let thread = lock(&self.0.thread).take();
        if let Some(t) = thread {
            let _ = tokio::task::spawn_blocking(move || t.join()).await;
        }
        tracing::info!(
            packets = self.0.transmitter.sent.load(Ordering::Relaxed),
            reports = self.0.transmitter.reports.load(Ordering::Relaxed),
            "microphone sender closed"
        );
    }
    pub(crate) fn start(&self) -> Result<()> {
        let mut owner = lock(&self.0.thread);
        if owner.is_none() {
            let shared = Arc::clone(&self.0);
            *owner = Some(
                std::thread::Builder::new()
                    .name("uu-microphone".into())
                    .spawn(move || capture_owner(shared))?,
            );
        }
        Ok(())
    }
    pub(crate) async fn feedback(&self, sender: Arc<RTCRtpSender>) {
        self.0.transmitter.feedback(sender).await;
    }
    pub(crate) async fn send_reports(
        &self,
        connection: std::sync::Weak<webrtc::peer_connection::RTCPeerConnection>,
        sender: Arc<RTCRtpSender>,
    ) {
        self.0
            .transmitter
            .send_reports(self, connection, sender)
            .await;
    }
    pub(crate) async fn send(&self, track: Arc<TrackLocalStaticRTP>) {
        self.0.transmitter.send(self, track).await;
    }
}

impl Source for Microphone {
    fn current(&self, generation: u64) -> bool {
        self.0.current(generation)
    }
    fn stopped(&self) -> &AtomicBool {
        &self.0.stopped
    }
    fn encoding(&self) -> Option<Encoding> {
        let state = lock(&self.0.state);
        state.encoding.map(|config| config.quality(state.quality))
    }
    fn frames(&self) -> &ArrayQueue<Frame> {
        &self.0.frames
    }
    fn wake(&self) -> &Notify {
        &self.0.wake
    }
    fn report_wake(&self) -> &Notify {
        &self.0.report_wake
    }
    fn fault(&self, error: String) {
        Microphone::fault(self, error);
    }
    fn send_error(&self, error: String) {
        lock(&self.0.state).error = Some(error);
    }
}

#[derive(Default)]
struct CaptureAttempt {
    failed: AtomicBool,
    progressed: AtomicBool,
}
impl CaptureAttempt {
    fn update_failures(&self, failures: &mut usize, failed: bool) {
        // Opening the API alone is not recovery. At least one complete valid
        // PCM frame must have been delivered by this attempt.
        if self.progressed.swap(false, Ordering::AcqRel) {
            *failures = 0;
        }
        if failed {
            *failures += 1;
        }
    }
}

fn capture_owner(shared: Arc<Shared>) {
    let _ = shared.owner.set(std::thread::current());
    let mut host = cpal::default_host();
    // As for playback: reconnect the PulseAudio host after a failure.
    let mut reconnect = false;
    let origin = Instant::now();
    let mut attempt = Arc::new(CaptureAttempt::default());
    let mut stream = None;
    let mut device_id = None;
    let mut generation = u64::MAX;
    let mut failures = 0;
    while !shared.stopped.load(Ordering::Acquire) {
        if std::mem::take(&mut reconnect) {
            host = cpal::default_host();
        }
        if shared.refresh_devices.swap(false, Ordering::AcqRel) {
            let available = host.input_devices().map(|devices| {
                devices
                    .filter_map(|device| {
                        Some(InputDevice {
                            id: device.id().ok()?,
                            name: device.description().ok()?.name().to_owned(),
                        })
                    })
                    .collect()
            });
            *lock(&shared.input_devices) = match available {
                Ok(devices) => InputDevices {
                    devices,
                    error: None,
                    updated: Some(Instant::now()),
                },
                Err(error) => {
                    reconnect = true;
                    InputDevices {
                        devices: Vec::new(),
                        error: Some(format!("读取麦克风设备失败：{error}")),
                        updated: Some(Instant::now()),
                    }
                }
            };
        }
        let (mut next, selection) = {
            let state = lock(&shared.state);
            (
                shared.generation.load(Ordering::Acquire),
                state.selected_input.clone(),
            )
        };
        if !shared.capture.load(Ordering::Acquire) {
            stream = None;
            shared.capturing.store(false, Ordering::Release);
            shared.peak.store(0, Ordering::Relaxed);
            std::thread::park_timeout(Duration::from_secs(1));
            continue;
        }
        let device = match &selection {
            Some(id) => host.device_by_id(id).filter(DeviceTrait::supports_input),
            None => host.default_input_device(),
        };
        let id = device.as_ref().and_then(|d| d.id().ok());
        if next != generation || id != device_id {
            // A default-device/hotplug change must invalidate the old capture,
            // without adopting a concurrent UI selection under the wrong epoch.
            if next == generation {
                match shared.generation.compare_exchange(
                    next,
                    next.wrapping_add(1),
                    Ordering::AcqRel,
                    Ordering::Acquire,
                ) {
                    Ok(_) => next = next.wrapping_add(1),
                    Err(_) => continue,
                }
            }
            stream = None;
            attempt = Arc::new(CaptureAttempt::default());
            failures = 0;
            device_id = id;
            generation = next;
            shared.capturing.store(false, Ordering::Release);
            shared.peak.store(0, Ordering::Relaxed);
        }
        let stream_failed = attempt.failed.swap(false, Ordering::AcqRel);
        if stream_failed {
            stream = None;
            reconnect = true;
        }
        // Join the failed stream before taking its final progress marker.
        attempt.update_failures(&mut failures, stream_failed);
        if stream_failed {
            match shared.generation.compare_exchange(
                generation,
                generation.wrapping_add(1),
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => generation = generation.wrapping_add(1),
                Err(_) => continue,
            }
            shared.capturing.store(false, Ordering::Release);
            shared.peak.store(0, Ordering::Relaxed);
        }
        if stream.is_none() && failures < 3 {
            shared.capturing.store(false, Ordering::Release);
            attempt = Arc::new(CaptureAttempt::default());
            let result = device
                .ok_or_else(|| {
                    if selection.is_some() {
                        anyhow!("所选麦克风已断开，请重新连接或选择其他设备")
                    } else {
                        anyhow!("没有可用的默认麦克风")
                    }
                })
                .and_then(|device| {
                    open_input(
                        &device,
                        Arc::clone(&shared),
                        Arc::clone(&attempt),
                        generation,
                        origin,
                    )
                });
            if !shared.current(generation) {
                drop(result);
                continue;
            }
            match result {
                Ok(s) => {
                    stream = Some(s);
                    let _state = lock(&shared.state);
                    if shared.current(generation) {
                        shared.capturing.store(true, Ordering::Release);
                    }
                }
                Err(e) => {
                    let mut state = lock(&shared.state);
                    if shared.current(generation) {
                        state.error = Some(format!("麦克风不可用：{e:#}"));
                    }
                    failures += 1;
                }
            }
        }
        std::thread::park_timeout(if stream.is_none() && failures < 3 {
            Duration::from_millis(100)
        } else {
            Duration::from_secs(1)
        });
    }
    drop(stream);
    shared.capturing.store(false, Ordering::Release);
    shared.peak.store(0, Ordering::Relaxed);
    while shared.frames.pop().is_some() {}
}

fn open_input(
    device: &cpal::Device,
    shared: Arc<Shared>,
    attempt: Arc<CaptureAttempt>,
    generation: u64,
    origin: Instant,
) -> Result<cpal::Stream> {
    let supported = device
        .default_input_config()
        .context("读取麦克风格式失败")?;
    let config = supported.config();
    let stream = match supported.sample_format() {
        cpal::SampleFormat::F32 => {
            build_input::<f32>(device, config, shared.clone(), attempt, generation, origin)
        }
        cpal::SampleFormat::I16 => {
            build_input::<i16>(device, config, shared.clone(), attempt, generation, origin)
        }
        cpal::SampleFormat::I24 => {
            build_input::<cpal::I24>(device, config, shared.clone(), attempt, generation, origin)
        }
        cpal::SampleFormat::I32 => {
            build_input::<i32>(device, config, shared.clone(), attempt, generation, origin)
        }
        cpal::SampleFormat::F64 => {
            build_input::<f64>(device, config, shared.clone(), attempt, generation, origin)
        }
        format => Err(anyhow!("麦克风格式不受支持：{format}")),
    }?;
    stream
        .play()
        .context("启动麦克风失败（请检查系统麦克风权限）")?;
    let mut s = lock(&shared.state);
    if shared.current(generation) {
        s.device = device
            .description()
            .map(|d| d.name().to_owned())
            .unwrap_or_else(|_| "系统默认麦克风".into());
        s.error = None;
    }
    tracing::info!(
        rate = config.sample_rate,
        channels = config.channels,
        "microphone capture started"
    );
    Ok(stream)
}

fn input_error(
    shared: &Shared,
    failed: &AtomicBool,
    generation: u64,
    discontinuities: &mut u64,
    error: cpal::Error,
) {
    let mut state = lock(&shared.state);
    if !shared.current(generation) {
        return;
    }
    if error.kind() == cpal::ErrorKind::Xrun {
        // WASAPI reports DATA_DISCONTINUITY and then delivers the current
        // buffer. Rebuilding here discards that data and can repeat the same
        // startup discontinuity until the device retry budget is exhausted.
        // No missing-sample count is supplied: keep the valid callback data
        // and the existing RTP clock rather than inventing samples or a gap.
        *discontinuities = discontinuities.saturating_add(1);
        let count = *discontinuities;
        drop(state);
        if count.is_power_of_two() {
            tracing::warn!(
                generation,
                count,
                "microphone capture discontinuity; stream retained"
            );
        }
        return;
    }
    state.error = Some(format!("麦克风设备中断：{error}"));
    failed.store(true, Ordering::Release);
    drop(state);
    shared.notify();
}

fn build_input<T: cpal::SizedSample + cpal::Sample>(
    device: &cpal::Device,
    config: cpal::StreamConfig,
    shared: Arc<Shared>,
    attempt: Arc<CaptureAttempt>,
    generation: u64,
    origin: Instant,
) -> Result<cpal::Stream>
where
    f32: cpal::FromSample<T>,
{
    ensure!(config.channels > 0, "麦克风没有声道");
    let channels = config.channels as usize;
    let mut resampler = crate::media::audio::dsp::Resampler::with_rates(config.sample_rate, RATE)?;
    let mut input = [0f32; 320];
    let mut normalized = [0f32; 1920];
    let mut frame = [0f32; BLOCK * 2];
    let mut filled = 0;
    let mut timestamp = (origin.elapsed().as_secs_f64() * f64::from(RATE)) as u64 as u32;
    let errors = shared.clone();
    let callback_attempt = attempt.clone();
    let mut discontinuities = 0;
    device
        .build_input_stream(
            config,
            move |data: &[T], _| {
                if !shared.current(generation) {
                    return;
                }
                for chunk in data.chunks(channels * 160) {
                    let frames = chunk.len() / channels;
                    for (i, src) in chunk.chunks_exact(channels).enumerate() {
                        let l = <f32 as cpal::FromSample<T>>::from_sample_(src[0]);
                        let r = if channels > 1 {
                            <f32 as cpal::FromSample<T>>::from_sample_(src[1])
                        } else {
                            l
                        };
                        input[i * 2] = if l.is_finite() {
                            l.clamp(-1.0, 1.0)
                        } else {
                            0.0
                        };
                        input[i * 2 + 1] = if r.is_finite() {
                            r.clamp(-1.0, 1.0)
                        } else {
                            0.0
                        };
                    }
                    let mut consumed = 0;
                    while consumed < frames {
                        let result = if config.sample_rate == RATE {
                            normalized[..frames * 2].copy_from_slice(&input[..frames * 2]);
                            Ok((frames, frames))
                        } else {
                            resampler
                                .process(&input[consumed * 2..frames * 2], &mut normalized)
                                .map(|(used, produced)| (used / 2, produced / 2))
                        };
                        let Ok((used, produced)) = result else {
                            return;
                        };
                        if used == 0 && produced == 0 {
                            return;
                        }
                        consumed += used;
                        for &sample in &normalized[..produced * 2] {
                            frame[filled] = sample;
                            filled += 1;
                            if filled == frame.len() {
                                let peak = frame.iter().fold(0f32, |p, s| p.max(s.abs()));
                                shared.peak.store(peak.to_bits(), Ordering::Relaxed);
                                shared.frames.force_push(Frame {
                                    samples: frame,
                                    timestamp,
                                    generation,
                                    created: Instant::now(),
                                });
                                callback_attempt.progressed.store(true, Ordering::Release);
                                shared.wake.notify_one();
                                timestamp = timestamp.wrapping_add(BLOCK as u32);
                                filled = 0;
                            }
                        }
                    }
                }
            },
            move |e| {
                input_error(
                    &errors,
                    &attempt.failed,
                    generation,
                    &mut discontinuities,
                    e,
                );
            },
            None,
        )
        .context("打开麦克风失败")
}
