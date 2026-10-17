//! Authenticated controlled-session input ownership.
mod backend;
// The installed Windows service's input agent, reached over a named pipe.
#[cfg(windows)]
pub(crate) mod broker;
pub(crate) mod config;
#[cfg(windows)]
mod engine;
#[cfg(target_os = "linux")]
#[path = "engine_linux.rs"]
mod engine;
mod geometry;
pub(crate) mod wire;

use super::{Lease, lock};
use anyhow::{Context, Result};
use backend::Backend;
use std::{
    collections::VecDeque,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    time::Duration,
};
use tokio_util::sync::CancellationToken;

pub(crate) use geometry::{Geometry, Rect, Screen};

// Drain only already queued movement. No batching timer, no summed relative
// deltas, and no reordering across a key/button/contact boundary.
const MAX_BATCH: usize = 16;

fn motion(event: &wire::Event) -> bool {
    matches!(
        event,
        wire::Event::Absolute { .. }
            | wire::Event::Relative { .. }
            | wire::Event::RelativeScaled { .. }
    )
}

struct Envelope {
    generation: u64,
    event: wire::Event,
    geometry: Geometry,
    queued_at: std::time::Instant,
}
impl Envelope {
    fn coalesce(&mut self, next: &Self) -> bool {
        if self.generation != next.generation || self.geometry != next.geometry {
            return false;
        }
        match (&mut self.event, &next.event) {
            (
                wire::Event::Absolute { screen, x, y },
                wire::Event::Absolute {
                    screen: target,
                    x: nx,
                    y: ny,
                },
            ) if screen == target => {
                *x = *nx;
                *y = *ny;
                true
            }
            (wire::Event::Heartbeat, wire::Event::Heartbeat) => true,
            // Preserve relative acceleration, drawing/contact paths and every
            // button/key boundary. Never merge across another event.
            _ => false,
        }
    }
}
#[derive(Default)]
struct Gate {
    stream: Option<u16>,
    binding: u64,
    generation: u64,
    faulted: bool,
    /// Why the last input was dropped, logged when it changes.
    refused: Option<&'static str>,
    pending: VecDeque<Envelope>,
    executing: Option<(std::time::Instant, usize)>,
    last_execution: Duration,
    drag_pointer: bool,
}
impl Gate {
    fn batch(&mut self, first: Envelope) -> (u64, Geometry, Vec<wire::Event>) {
        let mut events = Vec::with_capacity(MAX_BATCH);
        let moving = motion(&first.event);
        events.push(first.event);
        while moving
            && events.len() < MAX_BATCH
            && self.pending.front().is_some_and(|next| {
                next.generation == first.generation
                    && next.geometry == first.geometry
                    && motion(&next.event)
            })
        {
            events.push(self.pending.pop_front().unwrap().event);
        }
        (first.generation, first.geometry, events)
    }
    // Called only after the previous generation's holds have been released.
    fn resume(&mut self, generation: u64) {
        if self.generation == generation && self.stream.is_some() {
            self.faulted = false;
        }
    }
}
struct Shared {
    gate: Mutex<Gate>,
    lease: Lease,
    cancel: CancellationToken,
    connected: Arc<AtomicBool>,
    geometry: Box<dyn Fn() -> Geometry + Send + Sync>,

    policy: wire::Policy,
    configuration: tokio::sync::watch::Sender<Arc<config::Configuration>>,
    mouse_policy: tokio::sync::watch::Sender<config::MousePolicy>,
}
#[derive(Clone)]
pub(crate) struct Receiver {
    shared: Arc<Shared>,
    tx: mpsc::SyncSender<()>,
}
pub(crate) struct Session {
    receiver: Receiver,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Session {
    pub fn new(
        lease: Lease,
        cancel: CancellationToken,
        connected: Arc<AtomicBool>,
        policy: wire::Policy,
        geometry: impl Fn() -> Geometry + Send + Sync + 'static,
    ) -> Result<Self> {
        let (tx, rx) = mpsc::sync_channel(1);
        let shared = Arc::new(Shared {
            gate: Default::default(),
            lease,
            cancel,
            connected,
            geometry: Box::new(geometry),

            policy,
            configuration: tokio::sync::watch::channel(Arc::new(Default::default())).0,
            mouse_policy: tokio::sync::watch::channel(Default::default()).0,
        });
        let state = shared.clone();
        let thread = std::thread::Builder::new()
            .name("host-input".into())
            .spawn(move || {
                #[cfg(target_os = "linux")]
                run(state, rx, crate::platform::linux::input::realign_channel());
                #[cfg(not(target_os = "linux"))]
                run(state, rx);
            })
            .context("无法启动被控输入线程")?;
        Ok(Self {
            receiver: Receiver { shared, tx },
            thread: Some(thread),
        })
    }
    pub fn receiver(&self) -> Receiver {
        self.receiver.clone()
    }
    pub fn close(&mut self) {
        self.receiver.shared.cancel.cancel();
        if let Some(thread) = self.thread.take() {
            if thread.join().is_err() {
                tracing::error!("host input thread panicked");
            }
        }
    }
}
impl Drop for Session {
    fn drop(&mut self) {
        self.close();
    }
}
impl Receiver {
    pub fn drag_pointer(&self, owned: bool) {
        let mut gate = lock(&self.shared.gate);
        gate.drag_pointer = owned;
        if owned {
            gate.pending.retain(|p| !motion(&p.event));
        }
    }
    pub fn configure(&self, configuration: config::Configuration) {
        if self.shared.lease.requested() {
            self.shared
                .configuration
                .send_replace(Arc::new(configuration));
        }
    }
    pub fn mouse_policy(&self) -> tokio::sync::watch::Receiver<config::MousePolicy> {
        self.shared.mouse_policy.subscribe()
    }
    pub fn bind(&self, stream: u16) -> u64 {
        let mut gate = lock(&self.shared.gate);
        gate.binding = gate.binding.wrapping_add(1);
        gate.generation = gate.generation.wrapping_add(1);
        gate.stream = Some(stream);
        gate.faulted = false;
        gate.binding
    }
    pub fn generation(&self, stream: u16) -> Option<u64> {
        let gate = lock(&self.shared.gate);
        (gate.stream == Some(stream)).then_some(gate.binding)
    }
    pub fn close(&self, stream: u16, generation: u64) -> bool {
        let mut gate = lock(&self.shared.gate);
        if gate.stream == Some(stream) && gate.binding == generation {
            gate.stream = None;
            gate.generation = gate.generation.wrapping_add(1);
            true
        } else {
            false
        }
    }
    pub fn transport_lost(&self) {
        let mut gate = lock(&self.shared.gate);
        gate.generation = gate.generation.wrapping_add(1);
    }
    /// Return true for input envelopes, including rejected/unknown JSON actions.
    /// Never route malformed input into another business handler or log payloads.
    pub fn receive(&self, stream: u16, bytes: &[u8]) -> Result<bool> {
        let json = bytes.iter().find(|b| !b.is_ascii_whitespace()) == Some(&b'{');
        let parsed = if json {
            wire::json(bytes)
        } else {
            if bytes.len() > wire::MAX_MESSAGE {
                return Ok(false);
            }
            match crate::features::stream_control::publisher::input_event(bytes)? {
                Some(bytes) => wire::touch(&bytes),
                None => return Ok(false),
            }
        };
        let event = match parsed {
            Ok(Some(e)) => e,
            Ok(None) => return Ok(true),
            Err(error) => {
                self.shared
                    .lease
                    .input_status(None, Some(error.to_string()));
                return Ok(true);
            }
        };
        self.enqueue(Some(stream), event);
        Ok(true)
    }
    pub fn receive_action(&self, bytes: &[u8]) -> Result<bool> {
        if bytes.len() > wire::MAX_MESSAGE || bytes.first() == Some(&b'{') {
            return Ok(false);
        }
        let Some(event) = crate::features::stream_control::publisher::input_action(bytes)? else {
            return Ok(false);
        };
        self.enqueue(None, event);
        Ok(true)
    }
    fn enqueue(&self, stream: Option<u16>, event: wire::Event) {
        let mut gate = lock(&self.shared.gate);
        if gate.drag_pointer && motion(&event) {
            return;
        }
        let refused = if gate.stream.is_none() {
            Some("no input channel bound")
        } else if stream.is_some_and(|id| gate.stream != Some(id)) {
            Some("input on an unbound channel")
        } else if gate.faulted {
            Some("input generation faulted")
        } else if !self.shared.permitted() {
            Some("input not permitted")
        } else {
            None
        };
        if gate.refused != refused {
            gate.refused = refused;
            match refused {
                Some(reason) => {
                    tracing::warn!(reason, ?stream, bound = ?gate.stream, "host input dropped")
                }
                None => tracing::debug!(?stream, "host input accepted"),
            }
        }
        if refused.is_some() {
            return;
        }
        let item = Envelope {
            generation: gate.generation,
            event,
            geometry: (self.shared.geometry)(),
            queued_at: std::time::Instant::now(),
        };
        if gate
            .pending
            .back_mut()
            .is_some_and(|last| last.coalesce(&item))
        {
            return;
        }
        if gate.pending.len() >= 64 {
            // A lost up cannot be repaired by dropping one event. Invalidate this
            // generation, release all owned holds, then accept only fresh input.
            gate.faulted = true;
            gate.generation = gate.generation.wrapping_add(1);
            tracing::warn!(
                oldest_ms = gate
                    .pending
                    .front()
                    .map(|item| item.queued_at.elapsed().as_millis() as u64),
                executing_ms = gate
                    .executing
                    .map(|(when, _)| when.elapsed().as_millis() as u64),
                executing_events = gate.executing.map(|(_, count)| count),
                last_execution_us = gate.last_execution.as_micros() as u64,
                "host input queue saturated; discarding generation and releasing holds"
            );
            gate.pending.clear();
            self.shared
                .lease
                .input_status(None, Some("输入暂时阻塞，正在释放并恢复".into()));
        } else {
            gate.pending.push_back(item);
        }
        let _ = self.tx.try_send(());
    }
}
impl Shared {
    fn permitted(&self) -> bool {
        !self.cancel.is_cancelled()
            && self.lease.requested()
            && self.connected.load(Ordering::Acquire)
    }
}
fn run(
    shared: Arc<Shared>,
    rx: mpsc::Receiver<()>,
    #[cfg(target_os = "linux")] realign: std::sync::mpsc::Receiver<()>,
) {
    let mut engine: Option<Backend> = None;
    let mut generation = None;
    let mut require_service = false;
    let mut binding = None;
    let mut attempts = 0u8;
    let mut prepare_at = std::time::Instant::now();
    let mut configuration = shared.configuration.subscribe();
    let mut skipped = None;
    while !shared.cancel.is_cancelled() && shared.lease.requested() {
        let (current, current_binding) = {
            let gate = lock(&shared.gate);
            (
                (gate.stream.is_some() && shared.permitted()).then_some(gate.generation),
                gate.stream.map(|_| gate.binding),
            )
        };
        // Retry delay is bounded, not the number of recoveries during a viewing
        // session. The lease/channel still owns and cancels all recovery work.
        if binding != current_binding {
            binding = current_binding;
            attempts = 0;
            prepare_at = std::time::Instant::now();
        }
        if current != generation {
            shared.mouse_policy.send_replace(Default::default());
            let mut released = true;
            if let Some(engine) = &mut engine {
                if let Err(error) = engine.release() {
                    released = false;
                    shared
                        .lease
                        .input_status(Some(engine.backend()), Some(error.to_string()));
                }
            }
            if !released {
                // Do not reopen the gate until outstanding holds were discharged.
                if engine.as_ref().is_some_and(|engine| !engine.healthy()) {
                    engine.take();
                    prepare_at = std::time::Instant::now() + recovery_delay(attempts);
                }
                std::thread::sleep(Duration::from_millis(100));
                continue;
            }
            generation = current;
            let mut gate = lock(&shared.gate);
            gate.pending.retain(|item| Some(item.generation) == current);
            if let Some(current) = current.filter(|_| engine.as_ref().is_some_and(Backend::healthy))
            {
                gate.resume(current);
            }
        }
        if let Some(current) = current {
            if engine.is_none() && std::time::Instant::now() >= prepare_at {
                attempts = attempts.saturating_add(1).min(5);
                match Backend::new(shared.policy, require_service, || shared.permitted()) {
                    Ok(mut value) => {
                        require_service |= value.service();
                        let settings = (**configuration.borrow_and_update()).clone();
                        let configured = value.configure(settings);
                        if let Err(error) = configured {
                            shared
                                .lease
                                .input_status(Some(value.backend()), Some(error.to_string()));
                        }
                        engine = Some(value);
                        let mut gate = lock(&shared.gate);
                        if gate.generation == current {
                            gate.faulted = false;
                        }
                    }
                    Err(error) => {
                        prepare_at = std::time::Instant::now() + recovery_delay(attempts);
                        tracing::warn!(attempts, %error, "host input backend preparation failed");
                        shared.lease.input_status(None, Some(error.to_string()));
                        let mut gate = lock(&shared.gate);
                        if shared.permitted() {
                            gate.faulted = true;
                        }
                    }
                }
            }
        }
        // 有人（Wayland 上摆好接取面的那条路）请指针在原地动一下：合成器只在指针
        // 动过之后才重新判定指针底下是谁。见
        // `platform::linux::input::realign_pointer`。这也是一次注入，所以和别的
        // 输入一样要在许可之内（`generation` 只在许可之内才有值）；请求照样取走，
        // 许可收回时不会攒到之后再报。
        #[cfg(target_os = "linux")]
        if realign.try_recv().is_ok()
            && generation.is_some()
            && let Some(engine) = engine.as_mut()
        {
            match engine.realign_pointer() {
                Ok(true) => tracing::debug!("指针在原地重报了一次"),
                Ok(false) => {
                    tracing::debug!("指针不是经绝对指针摆的位置，接取面只能等合成器自己重判")
                }
                Err(error) => tracing::debug!(%error, "指针原地重报失败"),
            }
        }
        let mut item = lock(&shared.gate).pending.pop_front();
        if item.is_none() {
            if matches!(
                rx.recv_timeout(Duration::from_millis(5)),
                Err(mpsc::RecvTimeoutError::Disconnected)
            ) {
                break;
            }
            item = lock(&shared.gate).pending.pop_front();
        }
        if configuration.has_changed().unwrap_or(false)
            && let Some(engine) = &mut engine
        {
            let settings = (**configuration.borrow_and_update()).clone();
            if let Err(error) = engine.configure(settings) {
                shared
                    .lease
                    .input_status(Some(engine.backend()), Some(error.to_string()));
            }
        }
        if let Some(engine) = &mut engine
            && generation.is_some()
        {
            if let Err(error) = engine.synchronize((shared.geometry)()) {
                let _ = engine.release();
                shared
                    .lease
                    .input_status(Some(engine.backend()), Some(error.to_string()));
            }
        }
        if let Some(item) = item {
            let (item_generation, geometry, events) = lock(&shared.gate).batch(item);
            let stale = if generation != Some(item_generation) {
                Some("stale input generation")
            } else if !shared.permitted() {
                Some("input not permitted")
            } else if geometry != (shared.geometry)() {
                Some("screen layout changed")
            } else if engine.is_none() {
                Some("no input backend")
            } else {
                None
            };
            if stale != skipped {
                skipped = stale;
                if let Some(reason) = stale {
                    tracing::warn!(reason, "host input skipped");
                }
            }
            if stale.is_some() {
                continue;
            }
            let Some(engine) = engine.as_mut() else {
                continue;
            };
            // The gate is checked on every physical transition, including Unicode
            // batches; closing a channel must not finish an old queued sequence.
            let expected_geometry = geometry.clone();
            let permitted = || {
                let gate = lock(&shared.gate);
                shared.permitted()
                    && !gate.faulted
                    && Some(gate.generation) == generation
                    && gate.stream.is_some()
                    && expected_geometry == (shared.geometry)()
            };
            let started = std::time::Instant::now();
            lock(&shared.gate).executing = Some((started, events.len()));
            let result = engine.apply(events, geometry, permitted);
            {
                let mut gate = lock(&shared.gate);
                gate.executing = None;
                gate.last_execution = started.elapsed();
            }
            if let Err(error) = result {
                let release = engine.release();
                shared.lease.input_status(
                    Some(engine.backend()),
                    Some(match release {
                        Ok(()) => error.to_string(),
                        Err(release) => format!("{error}；释放输入失败：{release}"),
                    }),
                );
            } else {
                attempts = 0;
                shared.lease.input_status(Some(engine.backend()), None);
            }
        }
        if let Some(engine) = &mut engine {
            if let Err(error) = engine.tick() {
                shared
                    .lease
                    .input_status(Some(engine.backend()), Some(error.to_string()));
                let _ = engine.release();
            }
            if generation.is_some() {
                let policy = engine.mouse_policy();
                shared.mouse_policy.send_if_modified(|current| {
                    if *current == policy {
                        false
                    } else {
                        *current = policy;
                        true
                    }
                });
            }
        }
        if engine.as_ref().is_some_and(|engine| !engine.healthy()) {
            tracing::warn!(
                attempts,
                "host input service transport failed; releasing and rebuilding backend"
            );
            engine.take();
            prepare_at = std::time::Instant::now() + recovery_delay(attempts);
            let mut gate = lock(&shared.gate);
            gate.generation = gate.generation.wrapping_add(1);
            gate.faulted = true;
            generation = None;
        }
    }
    if let Some(mut engine) = engine {
        if let Err(error) = engine.release() {
            tracing::warn!(%error,"host input final release failed");
        }
    }
}

fn recovery_delay(attempts: u8) -> Duration {
    Duration::from_secs(1u64 << attempts.saturating_sub(1).min(4))
}
