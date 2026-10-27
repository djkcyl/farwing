//! Authenticated controlled-session input ownership.
use crate::i18n::t;
mod backend;
pub(crate) mod broker;
mod engine;
mod locks;
mod pointers;
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

pub(crate) use engine::{Geometry, Screen};

// Drain only already queued movement. No batching timer, no summed relative
// deltas, and no reordering across a key/button/contact boundary.
const MAX_BATCH: usize = 16;

fn motion(event: &wire::Event) -> bool {
    matches!(
        event,
        wire::Event::Absolute { .. } | wire::Event::Relative { .. }
    )
}

/// Allowance for heartbeat spacing and network jitter on top of 3×RTT.
const HOLD_JITTER: Duration = Duration::from_millis(50);

/// Held keys are released after `max(150 ms, 3×RTT + jitter)` without any
/// INPUT frame from their owner (§7). Without an RTT sample the floor applies.
fn hold_timeout(round_trip: Option<Duration>) -> Duration {
    let round_trip = round_trip.unwrap_or_default().min(Duration::from_secs(3));
    (round_trip * 3 + HOLD_JITTER).max(engine::MIN_HOLD)
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
    pending: VecDeque<Envelope>,
    executing: Option<(std::time::Instant, usize)>,
    last_execution: Duration,
    drag_pointer: bool,
    /// Lock-key presses received on the bound stream.
    presses: locks::Presses,
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
    /// Set by the input thread once this session's holds are released.
    done: (Mutex<bool>, std::sync::Condvar),
    connected: Arc<AtomicBool>,
    geometry: Box<dyn Fn() -> Geometry + Send + Sync>,
    /// RTT to this participant, when measured.
    round_trip: Box<dyn Fn() -> Option<Duration> + Send + Sync>,
    /// The participant has used the keyboard or mouse (its record says so).
    controlled: AtomicBool,
}
#[derive(Clone)]
pub(crate) struct Receiver {
    shared: Arc<Shared>,
    tx: mpsc::SyncSender<()>,
}
pub(crate) struct Session {
    receiver: Receiver,
    closed: bool,
}

impl Session {
    pub fn new(
        lease: Lease,
        cancel: CancellationToken,
        connected: Arc<AtomicBool>,
        round_trip: impl Fn() -> Option<Duration> + Send + Sync + 'static,
        geometry: impl Fn() -> Geometry + Send + Sync + 'static,
    ) -> Result<Self> {
        let shared = Arc::new(Shared {
            gate: Default::default(),
            lease,
            cancel,
            done: (Mutex::new(false), std::sync::Condvar::new()),
            connected,
            geometry: Box::new(geometry),
            round_trip: Box::new(round_trip),
            controlled: AtomicBool::new(false),
        });
        let tx = register(shared.clone())?;
        Ok(Self {
            receiver: Receiver { shared, tx },
            closed: false,
        })
    }
    pub fn receiver(&self) -> Receiver {
        self.receiver.clone()
    }
    /// Stop this participant's input and wait until its holds are released.
    pub fn close(&mut self) {
        if std::mem::replace(&mut self.closed, true) {
            return;
        }
        let shared = &self.receiver.shared;
        shared.cancel.cancel();
        let _ = self.receiver.tx.try_send(());
        let (done, signal) = &shared.done;
        let guard = lock(done);
        let (guard, timeout) = signal
            .wait_timeout_while(guard, Duration::from_secs(3), |done| !*done)
            .unwrap_or_else(|e| e.into_inner());
        if timeout.timed_out() && !*guard {
            tracing::warn!("host input session did not finish in time");
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
    /// Return true for input frames, including rejected/unknown events.
    /// Never route malformed input into another business handler or log payloads.
    pub fn receive(&self, stream: u16, bytes: &[u8]) -> Result<bool> {
        if bytes.len() > wire::MAX_MESSAGE {
            return Ok(false);
        }
        let input = match crate::protocol::input::decode(bytes) {
            Ok(Some(message)) => Some(wire::input(message)),
            Ok(None) => None,
            Err(error) => Some(Err(error)),
        };
        let Some(parsed) = input else {
            // TOUCH / PEN: a frame that does not validate cancels every
            // contact rather than dropping an UP.
            return Ok(match crate::protocol::pointer::decode(bytes) {
                Ok(None) => false,
                Ok(Some(message)) => {
                    match wire::pointer(message) {
                        Ok(event) => self.enqueue(Some(stream), event),
                        Err(error) => {
                            self.shared
                                .lease
                                .input_status(None, Some(error.to_string()));
                            self.enqueue(Some(stream), wire::Event::PointerCancel);
                        }
                    }
                    true
                }
                Err(error) => {
                    self.shared
                        .lease
                        .input_status(None, Some(error.to_string()));
                    self.enqueue(Some(stream), wire::Event::PointerCancel);
                    true
                }
            });
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
    fn enqueue(&self, stream: Option<u16>, event: wire::Event) {
        let mut gate = lock(&self.shared.gate);
        if gate.drag_pointer && motion(&event) {
            return;
        }
        if gate.stream.is_none() || stream.is_some_and(|id| gate.stream != Some(id)) {
            return;
        }
        // Count every press of the stream, including input dropped below.
        match event {
            wire::Event::Key {
                usage,
                down,
                repeat,
            } => gate.presses.key(usage, down, repeat),
            wire::Event::LockState { seq, .. } if !gate.presses.aligned(seq) => {
                tracing::debug!("lock state crossed a lock-key press; not applied");
                return;
            }
            _ => {}
        }
        if gate.faulted || !self.shared.permitted() {
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
                .input_status(None, Some(t().features.input_blocked_recovering.into()));
        } else {
            gate.pending.push_back(item);
        }
        let _ = self.tx.try_send(());
    }
}
impl Shared {
    /// Input is applied only for the participant holding the control token
    /// (`docs/protocol/v2-design.md` §4.2); losing it closes the gate, which
    /// releases the keys and buttons it held.
    fn permitted(&self) -> bool {
        !self.cancel.is_cancelled()
            && self.lease.requested()
            && self.lease.granted(farwing_protocol::caps::CONTROL)
            && self.connected.load(Ordering::Acquire)
            && crate::features::host::participants::holds_control(self.lease.participant())
    }
}
/// One input thread and backend for the whole host. The engine switches the
/// thread's desktop, so it never leaves this thread. Only the holder of the
/// control token injects; the others' input is dropped at the gate.
#[derive(Default)]
struct Hub {
    engine: Option<Backend>,
    require_service: bool,
    /// The input session whose input was applied last.
    owner: Option<usize>,
}

/// How often the input thread re-checks that the token holder still may
/// control (blocked by the host owner, grant withdrawn).
const HOLDER_CHECK: Duration = Duration::from_millis(200);

#[derive(Default)]
struct Registry {
    pending: Vec<Arc<Shared>>,
    wake: Option<mpsc::SyncSender<()>>,
}

static REGISTRY: std::sync::LazyLock<Mutex<Registry>> = std::sync::LazyLock::new(Default::default);

/// Add a participant's input session to the host input thread, starting the
/// thread when it is not running.
fn register(shared: Arc<Shared>) -> Result<mpsc::SyncSender<()>> {
    let mut registry = lock(&REGISTRY);
    registry.pending.push(shared);
    if let Some(wake) = &registry.wake {
        let _ = wake.try_send(());
        return Ok(wake.clone());
    }
    let (tx, rx) = mpsc::sync_channel(1);
    std::thread::Builder::new()
        .name("host-input".into())
        .spawn(move || serve(rx))
        .context(t().features.input_thread_start_failed)?;
    registry.wake = Some(tx.clone());
    Ok(tx)
}

impl Hub {
    /// `id` (the token holder) injects now; a handover releases the previous
    /// session's held keys and buttons first.
    fn admit(&mut self, id: usize) -> Result<()> {
        if self.owner != Some(id) {
            if self.owner.is_some()
                && let Some(engine) = &mut self.engine
            {
                engine.release()?;
            }
            tracing::info!(participant = id, "host input control handed over");
            self.owner = Some(id);
        }
        Ok(())
    }
    fn release_engine(&mut self) {
        self.owner = None;
        if let Some(mut engine) = self.engine.take()
            && let Err(error) = engine.release()
        {
            tracing::warn!(%error,"host input final release failed");
        }
        self.require_service = false;
    }
}

fn serve(rx: mpsc::Receiver<()>) {
    let mut hub = Hub::default();
    let mut workers: Vec<Worker> = Vec::new();
    let mut holder_checked = std::time::Instant::now();
    loop {
        if holder_checked.elapsed() >= HOLDER_CHECK {
            holder_checked = std::time::Instant::now();
            crate::features::host::participants::validate_holder();
        }
        {
            let mut registry = lock(&REGISTRY);
            workers.extend(registry.pending.drain(..).map(Worker::new));
            if workers.is_empty() {
                // Exit under the registry lock so that a new registration
                // starts a fresh thread.
                registry.wake = None;
                drop(registry);
                hub.release_engine();
                return;
            }
        }
        let mut busy = false;
        workers.retain_mut(|worker| match worker.step(&mut hub) {
            Step::Done => {
                worker.finish(&mut hub);
                false
            }
            Step::Busy => {
                busy = true;
                true
            }
            Step::Idle => true,
        });
        if workers.is_empty() {
            hub.release_engine();
            continue;
        }
        if !busy {
            let _ = rx.recv_timeout(Duration::from_millis(5));
        }
    }
}

enum Step {
    Idle,
    Busy,
    Done,
}

/// One participant's input session on the host input thread.
struct Worker {
    shared: Arc<Shared>,
    id: usize,
    generation: Option<u64>,
    binding: Option<u64>,
    attempts: u8,
    prepare_at: std::time::Instant,
}

impl Worker {
    fn new(shared: Arc<Shared>) -> Self {
        Self {
            id: Arc::as_ptr(&shared) as usize,
            shared,
            generation: None,
            binding: None,
            attempts: 0,
            prepare_at: std::time::Instant::now(),
        }
    }

    fn finish(&mut self, hub: &mut Hub) {
        if hub.owner == Some(self.id) {
            hub.owner = None;
            if let Some(engine) = &mut hub.engine
                && let Err(error) = engine.release()
            {
                tracing::warn!(%error,"host input final release failed");
            }
        }
        let (done, signal) = &self.shared.done;
        *lock(done) = true;
        signal.notify_all();
    }

    fn step(&mut self, hub: &mut Hub) -> Step {
        let shared = self.shared.clone();
        if shared.cancel.is_cancelled() || !shared.lease.requested() {
            return Step::Done;
        }
        let (current, current_binding) = {
            let gate = lock(&shared.gate);
            (
                (gate.stream.is_some() && shared.permitted()).then_some(gate.generation),
                gate.stream.map(|_| gate.binding),
            )
        };
        // Retry delay is bounded, not the number of recoveries during a viewing
        // session. The lease/channel still owns and cancels all recovery work.
        if self.binding != current_binding {
            self.binding = current_binding;
            self.attempts = 0;
            self.prepare_at = std::time::Instant::now();
        }
        if current != self.generation {
            let mut released = true;
            // Only the participant in control holds keys or buttons.
            if hub.owner == Some(self.id) {
                hub.owner = None;
                if let Some(engine) = &mut hub.engine
                    && let Err(error) = engine.release()
                {
                    released = false;
                    shared
                        .lease
                        .input_status(Some(engine.backend()), Some(error.to_string()));
                }
            }
            if !released {
                // Do not reopen the gate until outstanding holds were discharged.
                if hub.engine.as_ref().is_some_and(|engine| !engine.healthy()) {
                    hub.engine.take();
                    self.prepare_at = std::time::Instant::now() + recovery_delay(self.attempts);
                }
                return Step::Idle;
            }
            self.generation = current;
            let healthy = hub.engine.as_ref().is_some_and(Backend::healthy);
            let mut gate = lock(&shared.gate);
            gate.pending.retain(|item| Some(item.generation) == current);
            if let Some(current) = current.filter(|_| healthy) {
                gate.resume(current);
            }
        }
        if let Some(current) = current {
            if hub.engine.is_none() && std::time::Instant::now() >= self.prepare_at {
                self.attempts = self.attempts.saturating_add(1).min(5);
                match Backend::new(hub.require_service, || shared.permitted()) {
                    Ok(value) => {
                        hub.require_service |= value.service();
                        hub.engine = Some(value);
                        let mut gate = lock(&shared.gate);
                        if gate.generation == current {
                            gate.faulted = false;
                        }
                    }
                    Err(error) => {
                        self.prepare_at = std::time::Instant::now() + recovery_delay(self.attempts);
                        tracing::warn!(attempts = self.attempts, %error, "host input backend preparation failed");
                        shared.lease.input_status(None, Some(error.to_string()));
                        let mut gate = lock(&shared.gate);
                        if shared.permitted() {
                            gate.faulted = true;
                        }
                    }
                }
            } else if hub.engine.is_some() {
                // Another participant's session may have built the backend.
                let mut gate = lock(&shared.gate);
                if gate.generation == current && gate.faulted {
                    gate.resume(current);
                }
            }
        }
        let item = lock(&shared.gate).pending.pop_front();
        let busy = item.is_some();
        if let Some(item) = item {
            self.apply(hub, item);
        }
        if let Some(engine) = &mut hub.engine {
            if let Err(error) = engine.tick() {
                shared
                    .lease
                    .input_status(Some(engine.backend()), Some(error.to_string()));
                let _ = engine.release();
            }
        }
        if hub.engine.as_ref().is_some_and(|engine| !engine.healthy()) {
            tracing::warn!(
                attempts = self.attempts,
                "host input service transport failed; releasing and rebuilding backend"
            );
            hub.engine.take();
            hub.owner = None;
            self.prepare_at = std::time::Instant::now() + recovery_delay(self.attempts);
            let mut gate = lock(&shared.gate);
            gate.generation = gate.generation.wrapping_add(1);
            gate.faulted = true;
            self.generation = None;
        }
        if busy { Step::Busy } else { Step::Idle }
    }

    fn apply(&mut self, hub: &mut Hub, item: Envelope) {
        let shared = self.shared.clone();
        let (item_generation, geometry, events) = lock(&shared.gate).batch(item);
        if self.generation != Some(item_generation)
            || !shared.permitted()
            || geometry != (shared.geometry)()
            || hub.engine.is_none()
        {
            return;
        }
        // Heartbeats only keep held keys of the session already in control.
        let heartbeat = events.iter().all(|e| matches!(e, wire::Event::Heartbeat));
        if heartbeat && hub.owner != Some(self.id) {
            return;
        }
        if let Err(error) = hub.admit(self.id) {
            shared.lease.input_status(
                hub.engine.as_ref().map(Backend::backend),
                Some((t().features.input_handover_release_failed)(
                    &error.to_string(),
                )),
            );
            return;
        }
        let Some(engine) = hub.engine.as_mut() else {
            return;
        };
        if !heartbeat && !shared.controlled.swap(true, std::sync::atomic::Ordering::Relaxed) {
            shared.lease.record_control();
        }
        if let Err(error) = engine.synchronize(geometry.clone()) {
            let _ = engine.release();
            shared
                .lease
                .input_status(Some(engine.backend()), Some(error.to_string()));
        }
        // The gate is checked on every physical transition, including Unicode
        // batches; closing a channel must not finish an old queued sequence.
        let expected_geometry = geometry.clone();
        let generation = self.generation;
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
        let hold = hold_timeout((shared.round_trip)());
        let result = engine.apply(events, geometry, hold, permitted);
        {
            let mut gate = lock(&shared.gate);
            gate.executing = None;
            gate.last_execution = started.elapsed();
        }
        if let Err(error) = result {
            tracing::debug!(%error, "host input batch failed; releasing");
            let release = engine.release();
            shared.lease.input_status(
                Some(engine.backend()),
                Some(match release {
                    Ok(()) => error.to_string(),
                    Err(release) => (t().features.input_release_also_failed)(
                        &error.to_string(),
                        &release.to_string(),
                    ),
                }),
            );
        } else {
            self.attempts = 0;
            shared.lease.input_status(Some(engine.backend()), None);
        }
    }
}

fn recovery_delay(attempts: u8) -> Duration {
    Duration::from_secs(1u64 << attempts.saturating_sub(1).min(4))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hold_timeout_follows_round_trip() {
        assert_eq!(hold_timeout(None), Duration::from_millis(150));
        assert_eq!(
            hold_timeout(Some(Duration::from_millis(10))),
            Duration::from_millis(150)
        );
        assert_eq!(
            hold_timeout(Some(Duration::from_millis(100))),
            Duration::from_millis(350)
        );
        assert_eq!(
            hold_timeout(Some(Duration::from_secs(60))),
            Duration::from_millis(9050)
        );
    }
}
