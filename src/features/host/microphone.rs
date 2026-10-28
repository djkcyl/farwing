//! Authorized remote microphones -> shared NetEq implementation -> virtual input.
//!
//! The virtual microphone has one owner per host: a mixer thread holds the
//! bridge and the default-device routing and mixes every participant whose
//! microphone is on (`docs/protocol/v2-design.md` §4.2). Each participant's
//! microphone switch behaves as with a single connection: turning it off
//! mutes that participant, and the endpoint stays while any connection
//! retains it.
mod routing;
use super::{Lease, lock};
use crate::protocol::microphone as errors;
use crate::i18n::t;
use crate::{
    media::audio::neteq,
    platform::windows::virtual_audio::{Bridge, State},
};
use anyhow::{Context, Result};
use bytes::Bytes;
use crossbeam_queue::ArrayQueue;
use std::{
    collections::VecDeque,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc,
    },
    time::{Duration, Instant},
};
use tokio_util::sync::CancellationToken;

#[derive(Clone, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub(crate) struct Status {
    // Local publication revision, never a new network or IPC protocol field.
    #[serde(skip)]
    pub policy_generation: u64,
    pub enabled: bool,
    pub active: bool,
    #[serde(default)]
    pub speaker_active: bool,
    pub error: Option<String>,
    #[serde(default)]
    pub routing_error: Option<String>,
    pub underrun_frames: u64,
}
struct Packet {
    generation: u64,
    data: Bytes,
    sequence: u16,
    timestamp: u32,
    arrived: Instant,
}
#[derive(Debug)]
struct PolicyFailure {
    code: (&'static str, &'static str),
    detail: String,
}
impl std::fmt::Display for PolicyFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.detail.fmt(f)
    }
}
impl std::error::Error for PolicyFailure {}
impl PolicyFailure {
    fn not_allowed() -> Self {
        Self {
            code: errors::NOT_ALLOWED,
            detail: t().features.microphone_not_allowed.into(),
        }
    }
    fn bridge(error: anyhow::Error) -> anyhow::Error {
        use windows::Win32::Foundation::{
            ERROR_DEV_NOT_EXIST, ERROR_DEVICE_NOT_CONNECTED, ERROR_FILE_NOT_FOUND,
            ERROR_PATH_NOT_FOUND,
        };
        let unavailable = error
            .downcast_ref::<windows::core::Error>()
            .is_some_and(|e| {
                [
                    ERROR_FILE_NOT_FOUND,
                    ERROR_PATH_NOT_FOUND,
                    ERROR_DEV_NOT_EXIST,
                    ERROR_DEVICE_NOT_CONNECTED,
                ]
                .iter()
                .any(|code| e.code() == code.to_hresult())
            });
        if unavailable {
            Self {
                code: errors::COMPONENT_UNAVAILABLE,
                detail: format!("{error:#}"),
            }
            .into()
        } else {
            error
        }
    }
}
/// The `PeerError` code and reason of a refused microphone request.
pub(crate) fn refusal(error: &anyhow::Error) -> (&'static str, &'static str) {
    error
        .downcast_ref::<PolicyFailure>()
        .map_or(errors::INITIALIZATION_FAILED, |error| error.code)
}
struct Command {
    enabled: bool,
    intent: u64,
    done: tokio::sync::oneshot::Sender<Result<()>>,
}
enum Request {
    Join(Arc<Shared>),
    Command(Arc<Shared>, Command),
}
/// One participant's microphone.
struct Shared {
    lease: Lease,
    routing_error: Mutex<Option<String>>,
    cancel: CancellationToken,
    connected: Arc<AtomicBool>,
    enabled: AtomicBool,
    retained: AtomicBool,
    active: AtomicBool,
    generation: AtomicU64,
    intent: AtomicU64,
    accepted_policy: AtomicU64,
    gate: Mutex<()>,
    packets: ArrayQueue<Packet>,
    status: tokio::sync::watch::Sender<Status>,
}
#[derive(Clone)]
pub(crate) struct Receiver {
    shared: Arc<Shared>,
    requests: mpsc::SyncSender<Request>,
}
pub(crate) struct Session {
    receiver: Receiver,
}

/// The running mixer, if any; it exits with its last participant.
static MIXER: std::sync::LazyLock<Mutex<Option<mpsc::SyncSender<Request>>>> =
    std::sync::LazyLock::new(Default::default);

impl Session {
    pub fn new(
        lease: Lease,
        cancel: CancellationToken,
        connected: Arc<AtomicBool>,
    ) -> Result<Self> {
        let shared = Arc::new(Shared {
            routing_error: Mutex::new(None),
            lease,
            cancel,
            connected,
            enabled: AtomicBool::new(false),
            retained: AtomicBool::new(false),
            active: AtomicBool::new(false),
            generation: AtomicU64::new(0),
            intent: AtomicU64::new(0),
            accepted_policy: AtomicU64::new(0),
            gate: Mutex::new(()),
            packets: ArrayQueue::new(32),
            status: tokio::sync::watch::channel(Status::default()).0,
        });
        let mut mixer = lock(&MIXER);
        let requests = match mixer.as_ref() {
            Some(requests) if requests.try_send(Request::Join(shared.clone())).is_ok() => {
                requests.clone()
            }
            _ => {
                let (requests, rx) = mpsc::sync_channel(64);
                requests
                    .try_send(Request::Join(shared.clone()))
                    .expect("new mixer queue");
                std::thread::Builder::new()
                    .name("host-microphone".into())
                    .spawn(move || run(rx))
                    .context(t().features.microphone_thread_failed)?;
                *mixer = Some(requests.clone());
                requests
            }
        };
        Ok(Self {
            receiver: Receiver { shared, requests },
        })
    }
    pub fn receiver(&self) -> Receiver {
        self.receiver.clone()
    }
    pub fn close(&mut self) {
        self.receiver.shared.cancel.cancel();
        self.receiver.shared.flush();
    }
}
impl Drop for Session {
    fn drop(&mut self) {
        self.close();
    }
}
impl Receiver {
    pub async fn policy(&self, enabled: bool) -> Result<()> {
        let intent = {
            let _gate = lock(&self.shared.gate);
            let intent = self.shared.intent.fetch_add(1, Ordering::AcqRel) + 1;
            if !enabled {
                self.shared.enabled.store(false, Ordering::Release);
                self.shared.flush();
            }
            intent
        };
        if enabled && !self.shared.permitted() {
            return Err(PolicyFailure::not_allowed().into());
        }
        let (done, result) = tokio::sync::oneshot::channel();
        self.requests
            .try_send(Request::Command(
                self.shared.clone(),
                Command {
                    enabled,
                    intent,
                    done,
                },
            ))
            .map_err(|_| anyhow::anyhow!(t().features.microphone_busy))?;
        tokio::time::timeout(Duration::from_secs(3), result)
            .await
            .context(t().features.microphone_timeout)?
            .context(t().features.microphone_worker_stopped)?
    }
    pub fn status(&self) -> tokio::sync::watch::Receiver<Status> {
        self.shared.status.subscribe()
    }
    pub fn transport_lost(&self) {
        let _gate = lock(&self.shared.gate);
        self.shared.intent.fetch_add(1, Ordering::AcqRel);
        self.shared.enabled.store(false, Ordering::Release);
        self.shared.retained.store(false, Ordering::Release);
        self.shared.flush();
    }
    pub fn select_source(&self) -> u64 {
        self.shared.generation.fetch_add(1, Ordering::AcqRel) + 1
    }
    pub fn receive(&self, generation: u64, data: Bytes, sequence: u16, timestamp: u32) {
        if generation != self.shared.generation.load(Ordering::Acquire)
            || !self.shared.active.load(Ordering::Acquire)
            || !self.shared.enabled.load(Ordering::Acquire)
            || !self.shared.permitted()
        {
            return;
        }
        let Ok(frames) = opusic_c::utils::get_nb_samples(&data, opusic_c::SampleRate::Hz48000)
        else {
            return;
        };
        if data.len() > 65535 || !(480..=5760).contains(&frames) {
            return;
        }
        self.shared.packets.force_push(Packet {
            generation,
            data,
            sequence,
            timestamp,
            arrived: Instant::now(),
        });
    }
}
impl Shared {
    fn permitted(&self) -> bool {
        !self.cancel.is_cancelled()
            && self.lease.requested()
            && self.lease.granted(farwing_protocol::caps::MICROPHONE)
            && self.connected.load(Ordering::Acquire)
    }
    fn flush(&self) {
        self.active.store(false, Ordering::Release);
        while self.packets.pop().is_some() {}
    }
    fn publish(
        &self,
        enabled: bool,
        active: bool,
        speaker_active: bool,
        error: Option<String>,
        underrun_frames: u64,
    ) {
        self.active.store(active, Ordering::Release);
        let status = Status {
            policy_generation: self.accepted_policy.load(Ordering::Acquire),
            enabled,
            active,
            speaker_active,
            error,
            routing_error: lock(&self.routing_error).clone(),
            underrun_frames,
        };
        self.status.send_if_modified(|current| {
            if *current == status {
                false
            } else {
                *current = status.clone();
                true
            }
        });
    }
    pub(super) fn republish(&self) {
        let status = self.status.borrow().clone();
        self.publish(
            status.enabled,
            status.active,
            status.speaker_active,
            status.error,
            status.underrun_frames,
        );
    }
}

/// One participant's decoder in the mix.
struct Voice {
    shared: Arc<Shared>,
    decoder: Option<Decoder>,
}
struct Decoder {
    neteq: neteq::Receiver,
    generation: u64,
    seen: VecDeque<(u16, u32)>,
    started: bool,
    stats: neteq::Statistics,
}
impl Decoder {
    fn new(generation: u64) -> Result<Self> {
        Ok(Self {
            neteq: neteq::Receiver::new()?,
            generation,
            seen: VecDeque::with_capacity(128),
            started: false,
            stats: Default::default(),
        })
    }
}
impl Voice {
    fn live(&self) -> bool {
        self.shared.enabled.load(Ordering::Acquire) && self.shared.permitted()
    }
    /// Move queued packets into this participant's NetEq.
    fn insert(&mut self) -> Result<()> {
        let generation = self.shared.generation.load(Ordering::Acquire);
        if self.decoder.as_ref().is_none_or(|d| d.generation != generation) {
            self.decoder = Some(Decoder::new(generation)?);
        }
        let decoder = self.decoder.as_mut().expect("created above");
        for _ in 0..32 {
            let Some(packet) = self.shared.packets.pop() else {
                break;
            };
            if packet.generation != decoder.generation
                || decoder.seen.contains(&(packet.sequence, packet.timestamp))
            {
                continue;
            }
            if decoder.seen.len() == 128 {
                decoder.seen.pop_front();
            }
            decoder.seen.push_back((packet.sequence, packet.timestamp));
            decoder.started |= decoder
                .neteq
                .insert(&packet.data, packet.timestamp, packet.sequence, packet.arrived);
        }
        Ok(())
    }
    /// Add this participant's next 10 ms to `mix`.
    fn mix_into(&mut self, mix: &mut [f32; 960], scratch: &mut [f32; 960]) -> Result<()> {
        let Some(decoder) = self.decoder.as_mut().filter(|d| d.started) else {
            return Ok(());
        };
        scratch.fill(0.0);
        decoder.neteq.block(scratch, &mut decoder.stats)?;
        for (out, sample) in mix.iter_mut().zip(scratch.iter()) {
            *out += *sample;
        }
        Ok(())
    }
}

/// The bridge with the mixed 10-ms block being written to it.
struct Device {
    bridge: Bridge,
    state: State,
    pcm: [f32; 960],
    consumed: usize,
    /// The capture side is enabled (some participant's microphone is on).
    enabled: bool,
}
impl Device {
    fn open() -> Result<Self> {
        let mut bridge = Bridge::open().map_err(PolicyFailure::bridge)?;
        let state = bridge.enable(false, true)?;
        Ok(Self {
            bridge,
            state,
            pcm: [0.0; 960],
            consumed: 960,
            enabled: true,
        })
    }
    /// Turning every microphone off is a mute, not a device teardown: keep
    /// the endpoint so applications can keep recording.
    fn set_enabled(&mut self, enabled: bool) -> Result<()> {
        if self.enabled != enabled {
            self.state = self.bridge.enable(false, enabled)?;
            self.enabled = enabled;
            self.consumed = self.pcm.len();
        }
        Ok(())
    }
}

/// Keep the sum of several voices within range without hard clipping
/// quiet ones.
fn limit(sample: f32) -> f32 {
    const KNEE: f32 = 0.8;
    let magnitude = sample.abs();
    if magnitude <= KNEE {
        sample
    } else {
        let over = magnitude - KNEE;
        sample.signum() * (KNEE + (1.0 - KNEE) * (over / (1.0 - KNEE + over)))
    }
}

struct Mixer {
    voices: Vec<Voice>,
    device: Option<Device>,
    routing: Option<routing::Worker>,
    /// Participants the routing worker looks at.
    routed: Arc<Mutex<Vec<Arc<Shared>>>>,
    retry_at: Instant,
    scratch: [f32; 960],
    /// The aggregate last shown in the host GUI.
    host: Option<Status>,
}

fn run(requests: mpsc::Receiver<Request>) {
    let _priority = crate::platform::windows::virtual_audio::AudioPriority::enter()
        .map_err(|error| {
            tracing::warn!(%error,"virtual microphone multimedia scheduling unavailable");
        })
        .ok();
    let mut mixer = Mixer {
        voices: Vec::new(),
        device: None,
        routing: None,
        routed: Default::default(),
        retry_at: Instant::now(),
        scratch: [0.0; 960],
        host: None,
    };
    loop {
        let first = if mixer.device.is_none() {
            requests.recv_timeout(Duration::from_millis(100)).ok()
        } else {
            None
        };
        for request in first.into_iter().chain(requests.try_iter()) {
            match request {
                Request::Join(shared) => mixer.join(shared),
                Request::Command(shared, command) => mixer.command(&shared, command),
            }
        }
        mixer.retire();
        if mixer.voices.is_empty() {
            // Exit under the registry lock so that a new participant starts
            // a fresh mixer; answer anything that raced in.
            let mut registry = lock(&MIXER);
            if let Ok(request) = requests.try_recv() {
                drop(registry);
                match request {
                    Request::Join(shared) => mixer.join(shared),
                    Request::Command(shared, command) => mixer.command(&shared, command),
                }
                continue;
            }
            *registry = None;
            break;
        }
        mixer.maintain();
        mixer.tick();
    }
    drop(mixer);
    while let Ok(request) = requests.try_recv() {
        if let Request::Command(_, command) = request {
            let _ = command.done.send(Err(PolicyFailure::not_allowed().into()));
        }
    }
}

impl Mixer {
    fn join(&mut self, shared: Arc<Shared>) {
        lock(&self.routed).push(shared.clone());
        if self.routing.is_none() {
            match routing::Worker::new(self.routed.clone()) {
                Ok(worker) => self.routing = Some(worker),
                Err(error) => {
                    *lock(&shared.routing_error) = Some(error.to_string());
                    shared.republish();
                }
            }
        }
        self.voices.push(Voice {
            shared,
            decoder: None,
        });
    }

    /// Drop participants whose session ended.
    fn retire(&mut self) {
        let before = self.voices.len();
        self.voices.retain(|voice| {
            let alive = !voice.shared.cancel.is_cancelled();
            if !alive {
                voice.shared.flush();
                voice.shared.publish(false, false, false, None, 0);
            }
            alive
        });
        if self.voices.len() != before {
            lock(&self.routed).retain(|shared| !shared.cancel.is_cancelled());
            self.publish_host();
        }
        if self.voices.is_empty() {
            self.device = None;
            self.routing = None;
        }
    }

    /// Whether any participant retains the endpoint.
    fn retained(&self) -> bool {
        self.voices.iter().any(|voice| {
            voice.shared.retained.load(Ordering::Acquire) && voice.shared.permitted()
        })
    }

    fn command(&mut self, shared: &Arc<Shared>, command: Command) {
        if command.done.is_closed() || command.intent != shared.intent.load(Ordering::Acquire) {
            return;
        }
        let already_retained = shared.retained.load(Ordering::Acquire);
        let result = (|| -> Result<()> {
            if !command.enabled {
                shared.enabled.store(false, Ordering::Release);
                shared.flush();
                if let Some(voice) = self.voice(shared) {
                    voice.decoder = None;
                }
                self.sync_enabled()?;
                shared
                    .accepted_policy
                    .store(command.intent, Ordering::Release);
                shared.publish(false, false, false, None, 0);
                return Ok(());
            }
            if !shared.permitted() {
                return Err(PolicyFailure::not_allowed().into());
            }
            if self.device.is_none() {
                self.device = Some(Device::open()?);
            } else {
                self.device.as_mut().unwrap().set_enabled(true)?;
            }
            if let Some(voice) = self.voice(shared) {
                voice.decoder = None;
            }
            let _gate = lock(&shared.gate);
            if !shared.permitted() {
                return Err(PolicyFailure::not_allowed().into());
            }
            if command.done.is_closed() || command.intent != shared.intent.load(Ordering::Acquire)
            {
                anyhow::bail!("{}", t().features.microphone_request_withdrawn);
            }
            shared.enabled.store(true, Ordering::Release);
            shared.retained.store(true, Ordering::Release);
            shared
                .accepted_policy
                .store(command.intent, Ordering::Release);
            let state = self.device.as_ref().unwrap().state;
            shared.publish(
                true,
                state.microphone_running != 0,
                state.speaker_running != 0,
                None,
                state.microphone_underrun,
            );
            Ok(())
        })();
        if let Err(error) = &result {
            shared.enabled.store(false, Ordering::Release);
            let keep = already_retained
                && shared.retained.load(Ordering::Acquire)
                && shared.permitted();
            if !keep {
                shared.retained.store(false, Ordering::Release);
            }
            self.after_failure();
            shared.flush();
            shared.publish(false, false, false, Some(format!("{error:#}")), 0);
        }
        if command.done.send(result).is_err() && command.enabled {
            let _gate = lock(&shared.gate);
            if command.intent == shared.intent.load(Ordering::Acquire) {
                shared.enabled.store(false, Ordering::Release);
                shared.retained.store(
                    already_retained && shared.permitted(),
                    Ordering::Release,
                );
                self.after_failure();
                shared.flush();
                shared.publish(false, false, false, None, 0);
            }
        }
        self.publish_host();
    }

    fn voice(&mut self, shared: &Arc<Shared>) -> Option<&mut Voice> {
        self.voices
            .iter_mut()
            .find(|voice| Arc::ptr_eq(&voice.shared, shared))
    }

    /// Mute the capture side when no participant's microphone is on.
    fn sync_enabled(&mut self) -> Result<()> {
        let live = self.voices.iter().any(Voice::live);
        if let Some(device) = &mut self.device {
            device.set_enabled(live)?;
        }
        Ok(())
    }

    fn after_failure(&mut self) {
        if !self.retained() || self.sync_enabled().is_err() {
            self.device = None;
        }
    }

    /// Open, keep or release the bridge for the participants' state.
    fn maintain(&mut self) {
        if !self.retained() {
            if self.device.take().is_some() {
                for voice in &self.voices {
                    voice.shared.flush();
                    voice.shared.publish(false, false, false, None, 0);
                }
                self.publish_host();
            }
            return;
        }
        let live = self.voices.iter().any(Voice::live);
        if self.device.is_none() && live && Instant::now() >= self.retry_at {
            match Device::open() {
                Ok(device) => self.device = Some(device),
                Err(error) => {
                    self.retry_at = Instant::now() + Duration::from_secs(1);
                    for voice in self.voices.iter().filter(|voice| voice.live()) {
                        voice
                            .shared
                            .publish(true, false, false, Some(format!("{error:#}")), 0);
                    }
                }
            }
        }
    }

    /// Feed the driver and wait for its next quantum.
    fn tick(&mut self) {
        let Some(device) = self.device.as_mut() else {
            return;
        };
        let result = (|| -> Result<()> {
            let live = self.voices.iter().any(Voice::live);
            device.set_enabled(live)?;
            let next = device.bridge.state()?;
            if next.generation != device.state.generation {
                // The driver restarted its stream; start every voice afresh.
                for voice in &mut self.voices {
                    voice.shared.flush();
                    voice.decoder = None;
                }
                device.consumed = device.pcm.len();
            }
            device.state = next;
            let active = next.microphone_running != 0;
            for voice in &self.voices {
                if voice.live() {
                    voice.shared.publish(
                        true,
                        active,
                        next.speaker_running != 0,
                        None,
                        next.microphone_underrun,
                    );
                }
            }
            if live && active {
                for voice in self.voices.iter_mut().filter(|voice| voice.live()) {
                    voice.insert()?;
                }
                // NetEq pulls 10 ms. Refill at one 2.5-ms driver quantum or
                // less; never prebuffer a second network-sized queue in front
                // of the driver.
                if next.microphone_frames <= 120 {
                    if device.consumed == device.pcm.len() {
                        device.pcm.fill(0.0);
                        for voice in self.voices.iter_mut().filter(|voice| voice.live()) {
                            voice.mix_into(&mut device.pcm, &mut self.scratch)?;
                        }
                        for sample in &mut device.pcm {
                            *sample = limit(*sample);
                        }
                        device.consumed = 0;
                    }
                    // Submit only the next 2.5 ms, keeping the driver ring at
                    // at most 5 ms of PCM.
                    let end = device.consumed + 240;
                    device.bridge.write(&next, &device.pcm[device.consumed..end])?;
                    device.consumed = end;
                }
            }
            device.bridge.wait(&device.state, 100).map(|_| ())
        })();
        if let Err(error) = result {
            self.device = None;
            self.retry_at = Instant::now() + Duration::from_millis(250);
            for voice in &self.voices {
                voice.shared.flush();
                voice.shared.publish(
                    voice.shared.enabled.load(Ordering::Acquire),
                    false,
                    false,
                    Some(format!("{error:#}")),
                    0,
                );
            }
        }
        self.publish_host();
    }

    /// The host GUI shows whether any participant's microphone is on.
    fn publish_host(&mut self) {
        let mut statuses = self
            .voices
            .iter()
            .map(|voice| (voice.shared.lease.clone(), voice.shared.status.borrow().clone()));
        let Some((lease, mut status)) = statuses.next() else {
            return;
        };
        for (_, other) in statuses {
            status.enabled |= other.enabled;
            status.active |= other.active;
            status.speaker_active |= other.speaker_active;
            status.error = status.error.or(other.error);
            status.routing_error = status.routing_error.or(other.routing_error);
            status.underrun_frames = status.underrun_frames.max(other.underrun_frames);
        }
        if self.host.as_ref() != Some(&status) {
            self.host = Some(status.clone());
            lease.microphone_status(status);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::limit;

    #[test]
    fn limiter_keeps_quiet_samples_and_bounds_loud_sums() {
        assert_eq!(limit(0.5), 0.5);
        assert_eq!(limit(-0.8), -0.8);
        for sum in [1.0f32, 1.6, 3.0, 50.0] {
            let limited = limit(sum);
            assert!(limited > 0.8 && limited < 1.0, "{sum} -> {limited}");
            assert_eq!(limit(-sum), -limited);
        }
        assert!(limit(1.6) > limit(1.0));
    }
}
