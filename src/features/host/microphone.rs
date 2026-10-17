//! Authorized remote microphone -> shared NetEq implementation -> virtual input.
mod routing;
use super::{Lease, lock};
use crate::protocol::microphone as errors;
use crate::{
    media::audio::neteq,
    platform::virtual_audio::{Bridge, State},
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
}
#[derive(Debug)]
struct PolicyFailure {
    code: i32,
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
            detail: "本次会话不允许麦克风输入".into(),
        }
    }
    fn bridge(error: anyhow::Error) -> anyhow::Error {
        if crate::platform::virtual_audio::unavailable(&error) {
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
pub(crate) fn error_code(error: &anyhow::Error) -> i32 {
    error
        .downcast_ref::<PolicyFailure>()
        .map_or(errors::INITIALIZATION_FAILED, |error| error.code)
}
struct Command {
    enabled: bool,
    intent: u64,
    done: tokio::sync::oneshot::Sender<Result<()>>,
}
struct Shared {
    lease: Lease,
    routing_error: Mutex<Option<String>>,
    routing_changed: AtomicBool,
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
    commands: mpsc::SyncSender<Command>,
}
pub(crate) struct Session {
    receiver: Receiver,
    worker: Option<std::thread::JoinHandle<()>>,
}
impl Session {
    pub fn new(
        lease: Lease,
        cancel: CancellationToken,
        connected: Arc<AtomicBool>,
    ) -> Result<Self> {
        let shared = Arc::new(Shared {
            routing_error: Mutex::new(None),
            routing_changed: AtomicBool::new(false),
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
        let (commands, rx) = mpsc::sync_channel(8);
        let worker_shared = shared.clone();
        let worker = std::thread::Builder::new()
            .name("host-microphone".into())
            .spawn(move || run(worker_shared, rx))
            .context("无法创建虚拟麦克风线程")?;
        Ok(Self {
            receiver: Receiver { shared, commands },
            worker: Some(worker),
        })
    }
    pub fn receiver(&self) -> Receiver {
        self.receiver.clone()
    }
    pub fn close(&mut self) {
        self.receiver.shared.cancel.cancel();
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
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
        self.commands
            .try_send(Command {
                enabled,
                intent,
                done,
            })
            .map_err(|_| anyhow::anyhow!("麦克风设置繁忙"))?;
        tokio::time::timeout(Duration::from_secs(3), result)
            .await
            .context("麦克风设置超时")?
            .context("麦克风执行线程已结束")?
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
        });
    }
}
impl Shared {
    fn permitted(&self) -> bool {
        !self.cancel.is_cancelled()
            && self.lease.requested()
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
        let changed = self.status.send_if_modified(|current| {
            if *current == status {
                false
            } else {
                *current = status.clone();
                true
            }
        });
        if changed {
            self.lease.microphone_status(status);
        }
    }
}
struct Playout {
    decoder: neteq::Receiver,
    state: State,
    generation: u64,
    seen: VecDeque<(u16, u32)>,
    started: bool,
    stats: neteq::Statistics,
    pcm: [f32; 960],
    consumed: usize,
}
impl Playout {
    fn new(state: State, generation: u64) -> Result<Self> {
        Ok(Self {
            decoder: neteq::Receiver::new()?,
            state,
            generation,
            seen: VecDeque::with_capacity(128),
            started: false,
            stats: Default::default(),
            pcm: [0.0; 960],
            consumed: 960,
        })
    }
    fn tick(&mut self, shared: &Shared, bridge: &mut Bridge) -> Result<()> {
        let generation = shared.generation.load(Ordering::Acquire);
        let mut next = bridge.state()?;
        let source_changed = generation != self.generation;
        if source_changed {
            bridge.enable(false, false)?;
            next = bridge.enable(false, true)?;
        }
        if next.generation != self.state.generation || generation != self.generation {
            if !source_changed {
                shared.flush();
            }
            *self = Self::new(next, generation)?;
        } else {
            self.state = next;
        }
        let active = next.microphone_running != 0;
        shared.publish(
            true,
            active,
            next.speaker_running != 0,
            None,
            next.microphone_underrun,
        );
        if !active {
            return Ok(());
        }
        for _ in 0..32 {
            let Some(packet) = shared.packets.pop() else {
                break;
            };
            if packet.generation != self.generation
                || self.seen.contains(&(packet.sequence, packet.timestamp))
            {
                continue;
            }
            if self.seen.len() == 128 {
                self.seen.pop_front();
            }
            self.seen.push_back((packet.sequence, packet.timestamp));
            self.started |= self
                .decoder
                .insert(&packet.data, packet.timestamp, packet.sequence);
        }
        // NetEq pulls 10 ms. Refill at one 2.5-ms driver quantum or less;
        // never prebuffer a second network-sized queue in front of the driver.
        if next.microphone_frames <= 120
            && shared.enabled.load(Ordering::Acquire)
            && shared.permitted()
            && self.generation == shared.generation.load(Ordering::Acquire)
        {
            if self.consumed == self.pcm.len() {
                self.pcm.fill(0.0);
                if self.started {
                    self.decoder.block(&mut self.pcm, &mut self.stats)?;
                }
                self.consumed = 0;
            }
            // NetEq's 10-ms block stays in this one owner. Submit only the
            // next 2.5 ms, keeping the driver ring at at most 5 ms of PCM.
            let end = self.consumed + 240;
            bridge.write(&next, &self.pcm[self.consumed..end])?;
            self.consumed = end;
        }
        Ok(())
    }
}
fn run(shared: Arc<Shared>, commands: mpsc::Receiver<Command>) {
    let _priority = crate::platform::virtual_audio::AudioPriority::enter()
        .map_err(|error| {
            tracing::warn!(%error,"virtual microphone multimedia scheduling unavailable");
        })
        .ok();
    let mut device = None::<(Bridge, Playout)>;
    let routing = routing::Worker::new(shared.clone())
        .map_err(|error| {
            *lock(&shared.routing_error) = Some(error.to_string());
            shared.routing_changed.store(true, Ordering::Release);
        })
        .ok();
    let mut retry_at = Instant::now();
    while !shared.cancel.is_cancelled() {
        if shared.routing_changed.swap(false, Ordering::AcqRel) {
            let status = shared.status.borrow().clone();
            shared.publish(
                status.enabled,
                status.active,
                status.speaker_active,
                status.error,
                status.underrun_frames,
            );
        }
        let first = if device.is_none() {
            commands.recv_timeout(Duration::from_millis(100)).ok()
        } else {
            None
        };
        for command in first.into_iter().chain(commands.try_iter()) {
            if command.done.is_closed() || command.intent != shared.intent.load(Ordering::Acquire) {
                continue;
            }
            let already_retained = shared.retained.load(Ordering::Acquire);
            let result = (|| -> Result<()> {
                if !command.enabled {
                    // A microphone toggle is a mute, not a device teardown.
                    // Keep the endpoint, default-device lease and bridge for
                    // this connection so applications can keep recording.
                    if let Some((bridge, playout)) = &mut device {
                        playout.state = bridge.enable(false, false)?;
                    }
                    shared.enabled.store(false, Ordering::Release);
                    shared.flush();
                    shared
                        .accepted_policy
                        .store(command.intent, Ordering::Release);
                    shared.publish(false, false, false, None, 0);
                    return Ok(());
                }
                if !shared.permitted() {
                    return Err(PolicyFailure::not_allowed().into());
                }
                if device.is_none() {
                    let mut bridge = Bridge::open().map_err(PolicyFailure::bridge)?;
                    let state = bridge.enable(false, true)?;
                    let playout = Playout::new(state, shared.generation.load(Ordering::Acquire))?;
                    device = Some((bridge, playout));
                } else if !shared.enabled.load(Ordering::Acquire) {
                    let (bridge, playout) = device.as_mut().unwrap();
                    let state = bridge.enable(false, true)?;
                    *playout = Playout::new(state, shared.generation.load(Ordering::Acquire))?;
                }
                let _gate = lock(&shared.gate);
                if !shared.permitted() {
                    return Err(PolicyFailure::not_allowed().into());
                }
                if command.done.is_closed()
                    || command.intent != shared.intent.load(Ordering::Acquire)
                {
                    anyhow::bail!("麦克风请求已撤销");
                }
                shared.enabled.store(true, Ordering::Release);
                shared.retained.store(true, Ordering::Release);
                shared
                    .accepted_policy
                    .store(command.intent, Ordering::Release);
                let state = device.as_ref().unwrap().1.state;
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
                    && shared.permitted()
                    && mute(&mut device).is_ok();
                if !keep {
                    shared.retained.store(false, Ordering::Release);
                    device = None;
                }
                shared.flush();
                shared.publish(false, false, false, Some(format!("{error:#}")), 0);
            }
            if command.done.send(result).is_err() && command.enabled {
                let _gate = lock(&shared.gate);
                if command.intent == shared.intent.load(Ordering::Acquire) {
                    shared.enabled.store(false, Ordering::Release);
                    let keep = already_retained && shared.permitted() && mute(&mut device).is_ok();
                    shared.retained.store(keep, Ordering::Release);
                    if !keep {
                        device = None;
                    }
                    shared.flush();
                    shared.publish(false, false, false, None, 0);
                }
            }
        }
        if !shared.permitted() || !shared.retained.load(Ordering::Acquire) {
            if device.take().is_some() {
                shared.flush();
                shared.publish(false, false, false, None, 0);
            }
        }
        if device.is_none()
            && shared.enabled.load(Ordering::Acquire)
            && shared.permitted()
            && Instant::now() >= retry_at
        {
            let reopened = (|| -> Result<_> {
                let mut bridge = Bridge::open()?;
                let state = bridge.enable(false, true)?;
                let playout = Playout::new(state, shared.generation.load(Ordering::Acquire))?;
                Ok((bridge, playout))
            })();
            match reopened {
                Ok(value) => device = Some(value),
                Err(error) => {
                    retry_at = Instant::now() + Duration::from_secs(1);
                    shared.publish(true, false, false, Some(format!("{error:#}")), 0);
                }
            }
        }
        if let Some((bridge, playout)) = &mut device {
            let result = (if shared.enabled.load(Ordering::Acquire) {
                playout.tick(&shared, bridge)
            } else {
                // Capture remains clocked by the driver and produces silence.
                // No packets are accepted or decoded while muted.
                bridge.state().map(|state| playout.state = state)
            })
            .and_then(|()| bridge.wait(&playout.state, 100).map(|_| ()));
            if let Err(error) = result {
                device = None;
                retry_at = Instant::now() + Duration::from_millis(250);
                shared.flush();
                shared.publish(
                    shared.enabled.load(Ordering::Acquire),
                    false,
                    false,
                    Some(format!("{error:#}")),
                    0,
                );
            }
        }
    }
    drop(device);
    drop(routing);
    shared.flush();
    shared.publish(false, false, false, None, 0);
    while let Ok(command) = commands.try_recv() {
        let _ = command.done.send(Err(PolicyFailure::not_allowed().into()));
    }
}
fn mute(device: &mut Option<(Bridge, Playout)>) -> Result<()> {
    if let Some((bridge, playout)) = device {
        playout.state = bridge.enable(false, false)?;
    }
    Ok(())
}
