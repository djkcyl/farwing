//! A negotiated drag owns an isolated file provider and one native operation.
//! Clipboard observation, input ownership and network binding remain separate.
pub(crate) mod controller;
mod returning;
use crate::{
    features::clipboard::{self, Clipboard, FileOffer, FileSummary},
    media::capture::Screen,
    platform::windows::drag_drop as native,
    protocol::drag_drop::{self as wire, Packet, Payload, Point},
};
use anyhow::{Result, ensure};
use crate::i18n::t;
use std::{
    collections::HashMap,
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Role {
    Controller,
    Host,
}
#[derive(Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) struct HostPolicy {
    pub token: u64,
    pub enabled: bool,
    pub screens: Vec<Screen>,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Stage {
    Preparing,
    Dragging,
    Submitted,
    Returning,
    Reading,
    Complete,
    Cancelled,
    Failed,
}
#[derive(Clone, Debug)]
pub(crate) struct Snapshot {
    pub stage: Stage,
    pub effect: u32,
    pub summary: Option<FileSummary>,
    pub bytes_read: u64,
    pub error: Option<String>,
}
pub(crate) struct Ticket {
    id: u64,
    token: u64,
    binding: std::sync::Weak<Shared>,
    alive: Arc<AtomicBool>,
    point: Mutex<Point>,
    resume_position: Mutex<Option<Arc<dyn Fn() -> Option<Point> + Send + Sync>>>,
    snapshot: Mutex<Snapshot>,
    input: mpsc::Sender<Command>,
}
impl Ticket {
    pub fn snapshot(&self) -> Snapshot {
        let mut value = lock(&self.snapshot).clone();
        let current = self.binding.upgrade().is_some_and(|s| {
            !s.stop.is_cancelled()
                && s.enabled.load(Ordering::Acquire)
                && s.peer_enabled.load(Ordering::Acquire)
                && s.token.load(Ordering::Acquire) == self.token
        });
        if (!current || !self.alive.load(Ordering::Acquire))
            && !matches!(
                value.stage,
                Stage::Complete | Stage::Cancelled | Stage::Failed
            )
        {
            value.stage = Stage::Cancelled;
            value.effect = 0;
        }
        value
    }
    pub fn position(&self, point: Point) {
        if point.valid() {
            *lock(&self.point) = point;
        }
    }
    pub fn commit(&self, point: Point) -> Result<()> {
        ensure!(
            point.valid() && self.alive.load(Ordering::Acquire),
            "{}",
            t().features.drag_ended
        );
        let state = self.snapshot();
        ensure!(
            state.stage == Stage::Dragging && state.effect == wire::COPY,
            "{}",
            t().features.drag_not_accepted
        );
        self.input
            .try_send(Command::Commit(self.id, point))
            .map_err(|_| anyhow::anyhow!("{}", t().features.drag_commit_queue_unavailable))
    }
    pub fn cancel(&self) {
        self.alive.store(false, Ordering::Release);
    }
}
pub(crate) struct Outgoing {
    pub token: u64,
    pub bulk: bool,
    pub data: Vec<u8>,
}
struct Shared {
    output_ready: Mutex<Option<Arc<dyn Fn() + Send + Sync>>>,
    enabled: AtomicBool,
    ready: AtomicBool,
    peer_enabled: AtomicBool,
    token: AtomicU64,
    screens: Mutex<Vec<Screen>>,
    stop: CancellationToken,
    pointer_owner: Arc<AtomicU64>,
    source_held: Arc<AtomicU64>,
}
fn target_current(shared: &Shared, target: &Screen) -> bool {
    lock(&shared.screens).iter().any(|s| {
        s.id == target.id
            && s.identity == target.identity
            && s.device_name == target.device_name
            && s.adapter == target.adapter
            && (s.left, s.top, s.width, s.height, s.dpi_scale)
                == (
                    target.left,
                    target.top,
                    target.width,
                    target.height,
                    target.dpi_scale,
                )
    })
}
struct Lifetime(CancellationToken);
impl Drop for Lifetime {
    fn drop(&mut self) {
        self.0.cancel();
    }
}
#[derive(Clone)]
pub(crate) struct Endpoint {
    shared: Arc<Shared>,
    input: mpsc::Sender<Command>,
    _life: Arc<Lifetime>,
}
struct ReleaseSource {
    release: Option<Arc<dyn Fn() + Send + Sync>>,
    resume: Option<Arc<dyn Fn(Point, bool) -> bool + Send + Sync>>,
    current: Option<Arc<dyn Fn() -> bool + Send + Sync>>,
    wake: Option<Arc<dyn Fn() + Send + Sync>>,
}
impl Drop for ReleaseSource {
    fn drop(&mut self) {
        if let Some(release) = self.release.take() {
            release();
        }
    }
}

enum Command {
    Policy,
    Probe(Arc<Ticket>, ReleaseSource),
    Portal(u64, native::portal::Event),
    Incoming(Packet),
    /// `TRANSFER`/`FLOW` of a drag's file exchange (kind DRAG).
    Transfer(Vec<u8>),
    Start(Vec<PathBuf>, Arc<Ticket>),
    Commit(u64, Point),
    Clipboard(u64, Vec<u8>),
    Prepared(u64, Result<FileSummary, String>),
    Offer(u64, FileOffer, Result<FileSummary, String>),
    Native(u64, native::Event),
    Resume(u64),
    Image(u64, wire::DragImage),
}
impl Command {
    fn drag(&self) -> Option<u64> {
        match self {
            Self::Policy | Self::Transfer(_) => None,
            Self::Incoming(p) => (p.drag != 0).then_some(p.drag),
            Self::Start(_, ticket) | Self::Probe(ticket, _) => Some(ticket.id),
            Self::Portal(id, _) => Some(*id),
            Self::Commit(id, _)
            | Self::Clipboard(id, _)
            | Self::Prepared(id, _)
            | Self::Offer(id, _, _)
            | Self::Native(id, _)
            | Self::Resume(id)
            | Self::Image(id, _) => Some(*id),
        }
    }
}
impl Endpoint {
    pub fn new(role: Role) -> (Self, mpsc::Receiver<Outgoing>) {
        Self::with_input_flags(
            role,
            Arc::new(AtomicU64::new(0)),
            Arc::new(AtomicU64::new(0)),
        )
    }
    pub(super) fn with_input_flags(
        role: Role,
        pointer_owner: Arc<AtomicU64>,
        source_held: Arc<AtomicU64>,
    ) -> (Self, mpsc::Receiver<Outgoing>) {
        let stop = CancellationToken::new();
        let shared = Arc::new(Shared {
            output_ready: Mutex::new(None),
            enabled: AtomicBool::new(false),
            ready: AtomicBool::new(false),
            peer_enabled: AtomicBool::new(role == Role::Host),
            token: AtomicU64::new(0),
            screens: Mutex::default(),
            stop: stop.clone(),
            pointer_owner,
            source_held,
        });
        // A stream's sender stays within the credit it was granted, so a
        // full window of chunks fits.
        let (input, receiver) = mpsc::channel(2048);
        let (output, outgoing) = mpsc::channel(16);
        let actor = Actor {
            role,
            shared: shared.clone(),
            input: input.clone(),
            output,
            entries: HashMap::new(),
            token: 0,
            reported: None,
        };
        tokio::spawn(actor.run(receiver));
        (
            Self {
                shared,
                input,
                _life: Arc::new(Lifetime(stop)),
            },
            outgoing,
        )
    }
    pub(crate) fn on_output(&self, notify: Arc<dyn Fn() + Send + Sync>) {
        *lock(&self.shared.output_ready) = Some(notify.clone());
        notify();
    }
    pub fn host_policy(&self, token: u64, enabled: bool, screens: Vec<Screen>) {
        self.shared.token.store(token, Ordering::Release);
        self.shared.enabled.store(enabled, Ordering::Release);
        *lock(&self.shared.screens) = screens;
        let _ = self.input.try_send(Command::Policy);
    }
    pub fn enable(&self, enabled: bool) {
        if self.shared.enabled.swap(enabled, Ordering::AcqRel) != enabled {
            let _ = self.input.try_send(Command::Policy);
        }
    }
    pub fn available(&self) -> bool {
        self.shared.enabled.load(Ordering::Acquire)
            && self.shared.peer_enabled.load(Ordering::Acquire)
            && self.shared.ready.load(Ordering::Acquire)
            && !self.shared.stop.is_cancelled()
    }
    /// A `DRAG` frame, or `TRANSFER`/`FLOW` of a drag's file exchange.
    pub fn receive(&self, bytes: &[u8]) -> Result<bool> {
        let command = if crate::transport::transfer::kind_of(bytes)
            == Some(crate::transport::transfer::kind::DRAG)
        {
            Command::Transfer(bytes.to_vec())
        } else {
            let Some(packet) = wire::decode(bytes)? else {
                return Ok(false);
            };
            Command::Incoming(packet)
        };
        self.input
            .try_send(command)
            .map_err(|_| anyhow::anyhow!("{}", t().features.drag_receive_queue_full))?;
        Ok(true)
    }
    pub fn begin(&self, paths: Vec<PathBuf>, point: Point) -> Result<Arc<Ticket>> {
        ensure!(self.available() && point.valid(), "{}", t().features.drag_not_allowed);
        let id = (uuid::Uuid::new_v4().as_u128() as u64).max(1);
        let ticket = Arc::new(Ticket {
            id,
            token: self.shared.token.load(Ordering::Acquire),
            binding: Arc::downgrade(&self.shared),
            alive: Arc::new(AtomicBool::new(true)),
            point: Mutex::new(point),
            resume_position: Mutex::new(None),
            snapshot: Mutex::new(Snapshot {
                stage: Stage::Preparing,
                effect: 0,
                summary: None,
                bytes_read: 0,
                error: None,
            }),
            input: self.input.clone(),
        });
        ensure!(
            self.shared
                .pointer_owner
                .compare_exchange(0, id, Ordering::AcqRel, Ordering::Acquire)
                .is_ok(),
            "{}",
            t().features.drag_handoff_pending
        );
        if self
            .input
            .try_send(Command::Start(paths, ticket.clone()))
            .is_err()
        {
            let _ = self.shared.pointer_owner.compare_exchange(
                id,
                0,
                Ordering::AcqRel,
                Ordering::Acquire,
            );
            anyhow::bail!("{}", t().features.drag_start_queue_full);
        }
        Ok(ticket)
    }
    fn probe(&self, point: Point, release: ReleaseSource) -> Result<Arc<Ticket>> {
        ensure!(
            self.available() && point.valid(),
            "{}",
            t().features.drag_takeover_unavailable
        );
        let id = (uuid::Uuid::new_v4().as_u128() as u64).max(1);
        ensure!(
            self.shared
                .pointer_owner
                .compare_exchange(0, id, Ordering::AcqRel, Ordering::Acquire)
                .is_ok(),
            "{}",
            t().features.drag_in_progress
        );
        self.shared.source_held.store(id, Ordering::Release);
        let ticket = Arc::new(Ticket {
            id,
            token: self.shared.token.load(Ordering::Acquire),
            binding: Arc::downgrade(&self.shared),
            alive: Arc::new(AtomicBool::new(true)),
            point: Mutex::new(point),
            resume_position: Mutex::new(None),
            snapshot: Mutex::new(Snapshot {
                stage: Stage::Preparing,
                effect: 0,
                summary: None,
                bytes_read: 0,
                error: None,
            }),
            input: self.input.clone(),
        });
        if self
            .input
            .try_send(Command::Probe(ticket.clone(), release))
            .is_err()
        {
            let _ = self.shared.source_held.compare_exchange(
                id,
                0,
                Ordering::AcqRel,
                Ordering::Acquire,
            );
            let _ = self.shared.pointer_owner.compare_exchange(
                id,
                0,
                Ordering::AcqRel,
                Ordering::Acquire,
            );
            anyhow::bail!("{}", t().features.drag_out_queue_unavailable);
        }
        Ok(ticket)
    }
    pub fn close(&self) {
        self.shared.enabled.store(false, Ordering::Release);
        self.shared.stop.cancel();
    }
    pub fn stop_token(&self) -> CancellationToken {
        self.shared.stop.clone()
    }
    pub fn pointer_owned(&self) -> bool {
        self.shared.pointer_owner.load(Ordering::Acquire) != 0 && !self.shared.stop.is_cancelled()
    }
}
struct Entry {
    target: Option<Screen>,
    sending: bool,
    reverse: bool,
    point: Point,
    sequence: u64,
    last_position: u64,
    committed: bool,
    ticket: Arc<Ticket>,
    clipboard: Clipboard,
    sender: tokio::task::JoinHandle<()>,
    prepare: Option<tokio::task::JoinHandle<()>>,
    offer: Option<FileOffer>,
    native: Option<native::Session>,
    portal: Option<native::portal::Portal>,
    release_source: Option<ReleaseSource>,
    source_released: bool,
    image: Option<wire::DragImage>,
    preserve_source: bool,
    resuming: bool,
    returned: Option<bool>,
    native_finished: bool,
    resume_at: Option<Instant>,
    cancel_requested: bool,
    original: Option<native::original::Original>,
    handed_back: bool,
    pending_offer: Option<(FileOffer, FileSummary)>,
    shared: Arc<Shared>,
    accepted: bool,
    progress_at: Instant,
    created_at: Instant,
}
impl Drop for Entry {
    fn drop(&mut self) {
        if !self.handed_back {
            if let Some(original) = self.original {
                let _ = original.cancel();
            }
        }
        let _ = self.shared.pointer_owner.compare_exchange(
            self.ticket.id,
            0,
            Ordering::AcqRel,
            Ordering::Acquire,
        );
        self.ticket.alive.store(false, Ordering::Release);
        let _ = self.shared.source_held.compare_exchange(
            self.ticket.id,
            0,
            Ordering::AcqRel,
            Ordering::Acquire,
        );
        drop(self.release_source.take());
        self.clipboard.suspend();
        self.sender.abort();
        if let Some(task) = &self.prepare {
            task.abort();
        }
        if let Some(offer) = &self.offer {
            offer.cancel();
        }
        if let Some(native) = &self.native {
            native.cancel();
        }
    }
}
struct Actor {
    role: Role,
    shared: Arc<Shared>,
    input: mpsc::Sender<Command>,
    output: mpsc::Sender<Outgoing>,
    entries: HashMap<u64, Entry>,
    token: u64,
    reported: Option<bool>,
}
impl Actor {
    async fn run(mut self, mut receiver: mpsc::Receiver<Command>) {
        let mut tick = tokio::time::interval(Duration::from_millis(10));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            let command = tokio::select! { biased; _=self.shared.stop.cancelled()=>break,
            command=receiver.recv()=>match command { Some(c)=>Some(c), None=>break }, _=tick.tick(), if !self.entries.is_empty()=>None };
            let drag = command.as_ref().and_then(Command::drag);
            let result = match command {
                Some(command) => self.command(command).await,
                None => self.tick().await,
            };
            if let Err(error) = result {
                tracing::warn!(%error, ?drag, "drag session command failed");
                if let Some(id) = drag {
                    let (reason, detail) = wire::failure(&error);
                    let _ = self.finish(id, 4, reason, detail).await;
                }
            }
        }
        for (_, entry) in self.entries.drain() {
            let mut state = lock(&entry.ticket.snapshot);
            state.stage = Stage::Cancelled;
            state.effect = 0;
        }
        self.shared.ready.store(false, Ordering::Release);
    }
    async fn emit(&mut self, id: u64, payload: Payload, bulk: bool) -> Result<()> {
        if self.token == 0 {
            return Ok(());
        }
        let sequence = if let Some(entry) = self.entries.get_mut(&id) {
            entry.sequence = entry
                .sequence
                .checked_add(1)
                .ok_or_else(|| anyhow::anyhow!("{}", t().features.drag_sequence_exhausted))?;
            entry.sequence
        } else {
            u64::from(id != 0)
        };
        let data = wire::encode(Packet {
            token: self.token,
            drag: id,
            sequence,
            payload: Some(payload),
        })?;
        tokio::select! { _=self.shared.stop.cancelled()=>{}, sent=self.output.send(Outgoing { token: self.token, bulk, data })=>{sent.map_err(|_| anyhow::anyhow!("{}", t().features.drag_send_channel_closed))?;} }
        if let Some(notify) = lock(&self.shared.output_ready).clone() {
            notify();
        }
        Ok(())
    }
    /// Send a frame of a drag's file exchange unwrapped on the bulk channel.
    async fn raw(&mut self, data: Vec<u8>) -> Result<()> {
        if self.token == 0 {
            return Ok(());
        }
        tokio::select! { _=self.shared.stop.cancelled()=>{}, sent=self.output.send(Outgoing { token: self.token, bulk: true, data })=>{sent.map_err(|_| anyhow::anyhow!("{}", t().features.drag_send_channel_closed))?;} }
        if let Some(notify) = lock(&self.shared.output_ready).clone() {
            notify();
        }
        Ok(())
    }
    fn entry(
        &self,
        id: u64,
        point: Point,
        sending: bool,
        ticket: Option<Arc<Ticket>>,
    ) -> Result<Entry> {
        ensure!(
            self.entries.len() < 4 && !self.entries.contains_key(&id),
            "{}",
            t().features.drag_session_exists
        );
        let ticket = ticket.unwrap_or_else(|| {
            Arc::new(Ticket {
                id,
                token: self.token,
                binding: Arc::downgrade(&self.shared),
                alive: Arc::new(AtomicBool::new(true)),
                point: Mutex::new(point),
                resume_position: Mutex::new(None),
                snapshot: Mutex::new(Snapshot {
                    stage: Stage::Preparing,
                    effect: 0,
                    summary: None,
                    bytes_read: 0,
                    error: None,
                }),
                input: self.input.clone(),
            })
        });
        let state = self.shared.clone();
        let alive = ticket.alive.clone();
        let token = self.token;
        let guard = Arc::new(move || {
            alive.load(Ordering::Acquire)
                && state.enabled.load(Ordering::Acquire)
                && state.peer_enabled.load(Ordering::Acquire)
                && state.token.load(Ordering::Acquire) == token
                && !state.stop.is_cancelled()
                && crate::features::host::clipboard::agent::desktop_available()
        });
        let (clipboard, mut offers) = Clipboard::isolated_files(guard);
        clipboard.set_enabled(true)?;
        clipboard.policy(true, true, true);
        let outgoing = self.input.clone();
        let sender = tokio::spawn(clipboard.sender(move |_, data| {
            let outgoing = outgoing.clone();
            async move {
                outgoing
                    .send(Command::Clipboard(id, data))
                    .await
                    .map_err(Into::into)
            }
        }));
        let preparing = if !sending {
            let commands = self.input.clone();
            Some(tokio::spawn(async move {
                if let Some(offer) = offers.recv().await {
                    let prepared = offer.clone();
                    let result = tokio::task::spawn_blocking(move || prepared.prepare())
                        .await
                        .map_err(|e| e.to_string())
                        .and_then(|v| v.map_err(|e| e.to_string()));
                    let _ = commands.send(Command::Offer(id, offer, result)).await;
                }
            }))
        } else {
            None
        };
        Ok(Entry {
            target: if self.role == Role::Host {
                Some(self.screen(point)?)
            } else {
                None
            },
            sending,
            reverse: false,
            point,
            sequence: 0,
            last_position: 0,
            committed: false,
            ticket,
            clipboard,
            sender,
            prepare: preparing,
            offer: None,
            native: None,
            portal: None,
            release_source: None,
            source_released: false,
            image: None,
            preserve_source: false,
            resuming: false,
            returned: None,
            native_finished: false,
            resume_at: None,
            cancel_requested: false,
            original: None,
            handed_back: false,
            pending_offer: None,
            shared: self.shared.clone(),
            accepted: false,
            progress_at: Instant::now(),
            created_at: Instant::now(),
        })
    }
    fn screen(&self, point: Point) -> Result<Screen> {
        lock(&self.shared.screens)
            .iter()
            .find(|s| s.id == point.screen && s.width > 0 && s.height > 0)
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("{}", t().features.drag_display_unavailable))
    }
    fn position(&self, point: Point) -> Result<native::Position> {
        Self::screen_position(point, &self.screen(point)?)
    }
    fn screen_position(point: Point, s: &Screen) -> Result<native::Position> {
        let x = (point.x * f64::from(s.width))
            .round()
            .clamp(0., f64::from(s.width - 1)) as i64
            + i64::from(s.left);
        let y = (point.y * f64::from(s.height))
            .round()
            .clamp(0., f64::from(s.height - 1)) as i64
            + i64::from(s.top);
        Ok(native::Position {
            x: i32::try_from(x)?,
            y: i32::try_from(y)?,
        })
    }
    /// Ends drag `id` at `stage` (see [`wire::Finished`]); a failure carries
    /// its `reason` and, for [`wire::reason::FAILED`], this side's `detail`.
    async fn finish(
        &mut self,
        id: u64,
        stage: u32,
        reason: &'static str,
        detail: String,
    ) -> Result<()> {
        // After the portal is gone, cancel the identified original OLE loop
        // before acknowledging failure (the peer then releases its held press).
        if let Some(original) = self
            .entries
            .get(&id)
            .and_then(|e| e.original)
            .filter(|v| v.live())
        {
            original.cancel()?;
            let until = Instant::now() + Duration::from_millis(500);
            while original.live() && Instant::now() < until {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        }
        tracing::info!(drag = id, stage, "native file drag finished");
        let _ =
            self.shared
                .source_held
                .compare_exchange(id, 0, Ordering::AcqRel, Ordering::Acquire);
        let _ =
            self.shared
                .pointer_owner
                .compare_exchange(id, 0, Ordering::AcqRel, Ordering::Acquire);
        let bytes_read = self
            .entries
            .get(&id)
            .and_then(|e| e.offer.as_ref())
            .map_or(0, |o| o.status().bytes_read);
        let detail: String = detail.chars().take(240).collect();
        if !reason.is_empty() {
            tracing::info!(drag = id, reason, error = %detail, "native file drag failed");
        }
        if let Some(entry) = self.entries.remove(&id) {
            let mut state = lock(&entry.ticket.snapshot);
            state.stage = match stage {
                2 => Stage::Complete,
                3 => Stage::Cancelled,
                _ => Stage::Failed,
            };
            state.effect = 0;
            state.error = if detail.is_empty() {
                wire::reason_text(reason)
            } else {
                Some(detail)
            };
        }
        self.emit(
            id,
            Payload::Finished(wire::Finished {
                stage,
                reason: reason.into(),
                bytes_read,
            }),
            false,
        )
        .await?;
        Ok(())
    }
    async fn command(&mut self, command: Command) -> Result<()> {
        match command {
            Command::Policy => self.policy().await?,
            Command::Resume(id) => self.begin_return(id).await?,
            Command::Image(id, image) => {
                if let Some(entry) = self.entries.get_mut(&id) {
                    entry.image = Some(image.clone());
                    if self.shared.ready.load(Ordering::Acquire) {
                        self.emit(id, Payload::Image(image), false).await?;
                    }
                }
            }
            Command::Incoming(packet) => self.receive(packet).await?,
            Command::Probe(ticket, release) => {
                tracing::info!(drag = ticket.id, "native file drag handoff requested");
                if ticket.token != self.token || !self.shared.enabled.load(Ordering::Acquire) {
                    ticket.cancel();
                    drop(release);
                    let _ = self.shared.source_held.compare_exchange(
                        ticket.id,
                        0,
                        Ordering::AcqRel,
                        Ordering::Acquire,
                    );
                    let _ = self.shared.pointer_owner.compare_exchange(
                        ticket.id,
                        0,
                        Ordering::AcqRel,
                        Ordering::Acquire,
                    );
                    return Ok(());
                }
                let id = ticket.id;
                let point = *lock(&ticket.point);
                let mut entry = self.entry(id, point, false, Some(ticket))?;
                entry.reverse = true;
                entry.preserve_source = self.shared.ready.load(Ordering::Acquire)
                    && release.current.as_ref().is_some_and(|f| f());
                entry.release_source = Some(release);
                self.entries.insert(id, entry);
                self.emit(id, Payload::Probe(point), false).await?;
            }
            Command::Portal(id, event) => {
                if !self.entries.contains_key(&id) {
                    return Ok(());
                }
                match event {
                    native::portal::Event::Captured(paths, preserve) => {
                        tracing::info!(
                            drag = id,
                            selections = paths.len(),
                            preserve,
                            "native file drag selection captured"
                        );
                        let entry = self.entries.get_mut(&id).unwrap();
                        entry.preserve_source = preserve;
                        let source = entry.clipboard.clone();
                        let commands = self.input.clone();
                        entry.prepare = Some(tokio::spawn(async move {
                            let image = selection_image(&paths).await;
                            if commands.send(Command::Image(id, image)).await.is_err() {
                                return;
                            }
                            let result =
                                source.publish_files(paths).await.map_err(|e| e.to_string());
                            let _ = commands.send(Command::Prepared(id, result)).await;
                        }));
                        let point = entry.point;
                        self.emit(
                            id,
                            Payload::Begin(wire::Begin {
                                point: Some(point),
                                reverse: true,
                                preserve,
                            }),
                            false,
                        )
                        .await?;
                    }
                    native::portal::Event::Resumed(original) => {
                        tracing::info!(drag = id, "native original portal handed back");
                        let entry = self.entries.get_mut(&id).unwrap();
                        entry.original = Some(original);
                        let _ = self.shared.pointer_owner.compare_exchange(
                            id,
                            0,
                            Ordering::AcqRel,
                            Ordering::Acquire,
                        );
                        self.emit(id, Payload::Resumed(true), false).await?;
                    }
                    native::portal::Event::Released => {
                        let entry = self.entries.get_mut(&id).unwrap();
                        entry.source_released = true;
                        let _ = self.shared.pointer_owner.compare_exchange(
                            id,
                            0,
                            Ordering::AcqRel,
                            Ordering::Acquire,
                        );
                        self.emit(id, Payload::SourceReleased(true), false).await?;
                    }
                    native::portal::Event::Failed(error) => {
                        self.finish(id, 4, wire::reason::FAILED, error).await?
                    }
                    native::portal::Event::Unavailable => {
                        self.finish(id, 3, "", String::new()).await?
                    }
                }
            }
            Command::Start(paths, ticket) => {
                tracing::info!(
                    drag = ticket.id,
                    selections = paths.len(),
                    "native file drag started"
                );
                if ticket.token != self.token
                    || !self.shared.ready.load(Ordering::Acquire)
                    || !self.shared.enabled.load(Ordering::Acquire)
                {
                    ticket.cancel();
                    let _ = self.shared.pointer_owner.compare_exchange(
                        ticket.id,
                        0,
                        Ordering::AcqRel,
                        Ordering::Acquire,
                    );
                    return Ok(());
                }
                let id = ticket.id;
                let point = *lock(&ticket.point);
                let old: Vec<_> = self
                    .entries
                    .iter()
                    .filter(|(_, e)| e.sending && !e.committed)
                    .map(|(id, _)| *id)
                    .collect();
                for previous in old {
                    self.emit(previous, Payload::Cancel(true), false).await?;
                    self.finish(previous, 3, "", String::new()).await?;
                }
                let mut entry = match self.entry(id, point, true, Some(ticket.clone())) {
                    Ok(entry) => entry,
                    Err(error) => {
                        let mut state = lock(&ticket.snapshot);
                        state.stage = Stage::Failed;
                        state.error = Some(error.to_string());
                        ticket.cancel();
                        return Err(error);
                    }
                };
                let source = entry.clipboard.clone();
                let commands = self.input.clone();
                entry.prepare = Some(tokio::spawn(async move {
                    let image = selection_image(&paths).await;
                    if commands.send(Command::Image(id, image)).await.is_err() {
                        return;
                    }
                    let result = source.publish_files(paths).await.map_err(|e| e.to_string());
                    let _ = commands.send(Command::Prepared(id, result)).await;
                }));
                self.entries.insert(id, entry);
                self.emit(
                    id,
                    Payload::Begin(wire::Begin {
                        point: Some(point),
                        reverse: false,
                        preserve: false,
                    }),
                    false,
                )
                .await?;
            }
            Command::Commit(id, point) => {
                if let Some(entry) = self.entries.get_mut(&id) {
                    if entry.sending && !entry.reverse && !entry.committed {
                        entry.committed = true;
                        entry.point = point;
                        lock(&entry.ticket.snapshot).stage = Stage::Submitted;
                        self.emit(id, Payload::Commit(point), false).await?;
                    }
                }
            }
            Command::Clipboard(id, data) => {
                if self.entries.contains_key(&id) {
                    if clipboard::is_bulk(&data) {
                        // Stream data and credit go straight to the bulk channel.
                        self.raw(data).await?;
                    } else {
                        self.emit(id, Payload::Files(data), false).await?;
                    }
                }
            }
            Command::Transfer(data) => {
                // Stream IDs are unique per process: exactly one exchange claims it.
                for entry in self.entries.values() {
                    if entry.clipboard.receive(&data)? {
                        break;
                    }
                }
            }
            Command::Prepared(id, result) => {
                if let Some(entry) = self.entries.get(&id) {
                    match result {
                        Ok(summary) => lock(&entry.ticket.snapshot).summary = Some(summary),
                        Err(error) => self.finish(id, 4, wire::reason::FAILED, error).await?,
                    }
                }
            }
            Command::Offer(id, offer, result) => {
                if !self.entries.contains_key(&id) {
                    offer.cancel();
                    return Ok(());
                }
                match result {
                    Err(error) => self.finish(id, 4, wire::reason::FAILED, error).await?,
                    Ok(summary) => {
                        self.entries.get_mut(&id).unwrap().pending_offer = Some((offer, summary));
                        self.try_start_native(id)?;
                    }
                }
            }
            Command::Native(id, event) => {
                if !self.entries.contains_key(&id) {
                    return Ok(());
                }
                match event {
                    native::Event::Feedback(effect) => {
                        if self.entries[&id].resuming {
                            return Ok(());
                        }
                        if !self.entries[&id].committed {
                            let mut snapshot = lock(&self.entries[&id].ticket.snapshot);
                            snapshot.stage = Stage::Dragging;
                            snapshot.effect = effect & wire::COPY;
                        }
                        self.emit(
                            id,
                            Payload::Feedback(wire::Feedback {
                                ready: true,
                                effect: effect & wire::COPY,
                            }),
                            false,
                        )
                        .await?;
                    }
                    native::Event::Released => {
                        if self.entries[&id].resuming {
                            return Ok(());
                        }
                        tracing::info!(drag = id, "native file drag released to target");
                        if self.entries[&id].reverse {
                            let entry = self.entries.get_mut(&id).unwrap();
                            entry.committed = true;
                            drop(entry.release_source.take());
                            let _ = self.shared.source_held.compare_exchange(
                                id,
                                0,
                                Ordering::AcqRel,
                                Ordering::Acquire,
                            );
                            lock(&entry.ticket.snapshot).stage = Stage::Submitted;
                            if let Some(offer) = &entry.offer {
                                offer.dragging(false);
                            }
                            let point = entry.point;
                            self.emit(id, Payload::Commit(point), false).await?;
                        }
                        let _ = self.shared.pointer_owner.compare_exchange(
                            id,
                            0,
                            Ordering::AcqRel,
                            Ordering::Acquire,
                        );
                        self.emit(
                            id,
                            Payload::Finished(wire::Finished {
                                stage: 1,
                                reason: String::new(),
                                bytes_read: 0,
                            }),
                            false,
                        )
                        .await?;
                    }
                    native::Event::Finished {
                        accepted,
                        effect,
                        error,
                    } => {
                        if self.entries[&id].resuming {
                            self.entries.get_mut(&id).unwrap().native_finished = true;
                            self.complete_return(id).await?;
                            return Ok(());
                        }
                        if accepted && effect == wire::COPY {
                            let entry = self.entries.get_mut(&id).unwrap();
                            entry.accepted = true;
                            if let Some(offer) = &entry.offer {
                                offer.dragging(false);
                            }
                            lock(&entry.ticket.snapshot).stage = Stage::Reading;
                        } else {
                            let (stage, reason) = match error {
                                Some(_) => (4, wire::reason::FAILED),
                                None => (3, ""),
                            };
                            self.finish(id, stage, reason, error.unwrap_or_default())
                                .await?;
                        }
                    }
                }
            }
        }
        Ok(())
    }
    fn start_native(&mut self, id: u64, offer: FileOffer, summary: FileSummary) -> Result<()> {
        let point = self.entries[&id].point;
        let reverse = self.entries[&id].reverse;
        let target = self.entries[&id].target.clone();
        let position = if reverse {
            native::Position { x: 0, y: 0 }
        } else {
            Self::screen_position(
                point,
                target
                    .as_ref()
                    .ok_or_else(|| anyhow::anyhow!("{}", t().features.drag_target_missing))?,
            )?
        };
        offer.dragging(true);
        ensure!(
            self.shared.pointer_owner.load(Ordering::Acquire) == id
                || self
                    .shared
                    .pointer_owner
                    .compare_exchange(0, id, Ordering::AcqRel, Ordering::Acquire)
                    .is_ok(),
            "{}",
            t().features.drag_mouse_busy
        );
        let file = offer.clone();
        let commands = self.input.clone();
        let alive = self.entries[&id].ticket.alive.clone();
        let state = self.shared.clone();
        let token = self.token;
        let guard = Arc::new(move || {
            alive.load(Ordering::Acquire)
                && state.enabled.load(Ordering::Acquire)
                && state.peer_enabled.load(Ordering::Acquire)
                && state.token.load(Ordering::Acquire) == token
                && target
                    .as_ref()
                    .is_none_or(|target| target_current(&state, target))
                && !state.stop.is_cancelled()
                && crate::features::host::clipboard::agent::desktop_available()
        });
        let alive = self.entries[&id].ticket.alive.clone();
        let notify: Arc<dyn Fn(native::Event) + Send + Sync> = Arc::new(move |event| {
            if commands.try_send(Command::Native(id, event)).is_err() {
                alive.store(false, Ordering::Release);
            }
        });
        let image = self.entries[&id].image.clone();
        let identity = (reverse && self.entries[&id].preserve_source).then_some(
            native::appearance::Identity {
                token: self.token,
                drag: id,
            },
        );
        let object = move || {
            let object = file.object()?;
            if let Some(identity) = identity {
                native::appearance::identify(&object, identity)?;
            }
            if let Some(image) = image {
                if let Err(error) = native::appearance::decorate(&object, &image) {
                    tracing::debug!(%error,"native drag image unavailable");
                }
            }
            Ok(object)
        };
        let native = if reverse {
            native::Session::physical(guard, object, notify)?
        } else {
            native::Session::start(position, guard, object, notify)?
        };
        let entry = self.entries.get_mut(&id).unwrap();
        lock(&entry.ticket.snapshot).summary = Some(summary);
        if reverse {
            // Local OLE now owns the gesture; its target may activate another
            // application before the first GiveFeedback callback reaches us.
            lock(&entry.ticket.snapshot).stage = Stage::Dragging;
        }
        entry.offer = Some(offer);
        entry.native = Some(native);
        if entry.committed {
            entry.offer.as_ref().unwrap().dragging(false);
            entry.native.as_ref().unwrap().commit(position)?;
        }
        Ok(())
    }
    async fn policy(&mut self) -> Result<()> {
        let token = self.shared.token.load(Ordering::Acquire);
        if self.role == Role::Host && token != self.token {
            self.entries.clear();
            self.token = token;
            self.reported = None;
            self.shared.ready.store(false, Ordering::Release);
            if token != 0 {
                self.emit(0, Payload::Hello(1), false).await?;
            }
        }
        let enabled = self.shared.enabled.load(Ordering::Acquire);
        if self.role == Role::Host && self.token != 0 && self.reported != Some(enabled) {
            self.emit(0, Payload::Available(enabled), false).await?;
            self.reported = Some(enabled);
        }
        if !enabled || !self.shared.peer_enabled.load(Ordering::Acquire) {
            let ids: Vec<_> = self.entries.keys().copied().collect();
            for id in ids {
                self.finish(id, 3, "", String::new()).await?;
            }
        }
        Ok(())
    }
    async fn receive(&mut self, packet: Packet) -> Result<()> {
        if self.role == Role::Controller && matches!(packet.payload, Some(Payload::Hello(1))) {
            if packet.token != self.token {
                self.entries.clear();
                self.shared.peer_enabled.store(false, Ordering::Release);
                self.token = packet.token;
                self.shared.token.store(self.token, Ordering::Release);
            }
            self.shared.ready.store(true, Ordering::Release);
            self.emit(0, Payload::HelloAck(1), false).await?;
            return Ok(());
        }
        if packet.token != self.token || self.token == 0 {
            return Ok(());
        }
        let id = packet.drag;
        let commit = matches!(&packet.payload, Some(Payload::Commit(_)));
        match packet.payload.unwrap() {
            Payload::HelloAck(1) if self.role == Role::Host => {
                self.shared.ready.store(true, Ordering::Release)
            }
            Payload::Resume(point) => self.receive_return(id, point).await?,
            Payload::Resumed(ok) => {
                if let Some(entry) = self
                    .entries
                    .get_mut(&id)
                    .filter(|e| e.resuming && !e.sending)
                {
                    entry.returned = Some(ok);
                    self.complete_return(id).await?;
                }
            }
            Payload::HandoffDone(true) => {
                if let Some(entry) = self
                    .entries
                    .get_mut(&id)
                    .filter(|e| e.resuming && e.sending && e.original.is_some())
                {
                    entry.handed_back = true;
                    self.entries.remove(&id);
                }
            }
            Payload::Image(image) => {
                ensure!(
                    self.shared.ready.load(Ordering::Acquire),
                    "{}",
                    t().features.drag_image_not_negotiated
                );
                if let Some(entry) = self
                    .entries
                    .get_mut(&id)
                    .filter(|e| !e.sending && !e.committed)
                {
                    entry.image = Some(image);
                    self.try_start_native(id)?;
                }
            }
            Payload::Available(enabled) if self.role == Role::Controller => {
                self.shared.peer_enabled.store(enabled, Ordering::Release)
            }
            Payload::Begin(begin) => {
                if begin.reverse && self.role == Role::Controller {
                    let entry = self
                        .entries
                        .get_mut(&id)
                        .filter(|e| e.reverse && !e.sending)
                        .ok_or_else(|| anyhow::anyhow!("{}", t().features.drag_reverse_not_requested))?;
                    ensure!(entry.point == begin.point.unwrap(), "{}", t().features.drag_source_screen_mismatch);
                    entry.preserve_source &= begin.preserve;
                    if !entry.preserve_source {
                        drop(entry.release_source.take());
                        let _ = self.shared.source_held.compare_exchange(
                            id,
                            0,
                            Ordering::AcqRel,
                            Ordering::Acquire,
                        );
                    }
                    return Ok(());
                }
                ensure!(
                    self.role == Role::Host
                        && !begin.reverse
                        && self.shared.ready.load(Ordering::Acquire),
                    wire::Refused(wire::reason::DIRECTION_DENIED)
                );
                if !self.shared.enabled.load(Ordering::Acquire) {
                    self.finish(id, 4, wire::reason::REMOTE_DISABLED, String::new())
                        .await?;
                    return Ok(());
                }
                let point = begin.point.unwrap();
                self.position(point)?;
                let mut entry = self.entry(id, point, false, None)?;
                entry.last_position = packet.sequence;
                self.entries.insert(id, entry);
                self.emit(
                    id,
                    Payload::Feedback(wire::Feedback {
                        ready: false,
                        effect: 0,
                    }),
                    false,
                )
                .await?;
            }
            Payload::Position(point) | Payload::Commit(point) => {
                if commit
                    && self
                        .entries
                        .get(&id)
                        .is_some_and(|e| e.reverse && e.sending)
                {
                    self.entries.get_mut(&id).unwrap().committed = true;
                    return Ok(());
                }
                let position = self.position(point)?;
                if let Some(entry) = self.entries.get_mut(&id) {
                    if entry.sending || packet.sequence <= entry.last_position || entry.committed {
                        return Ok(());
                    }
                    entry.last_position = packet.sequence;
                    entry.point = point;
                    if commit {
                        ensure!(
                            lock(&entry.ticket.snapshot).stage == Stage::Dragging
                                && entry.native.is_some(),
                            "{}",
                            t().features.drag_not_ready
                        );
                        entry.committed = true;
                        if let Some(offer) = &entry.offer {
                            offer.dragging(false);
                        }
                        if let Some(native) = &entry.native {
                            native.commit(position)?;
                        }
                    } else if let Some(native) = &entry.native {
                        native.position(position);
                    }
                }
            }
            Payload::Files(data) => {
                if let Some(entry) = self.entries.get(&id) {
                    entry.clipboard.receive(&data)?;
                }
            }
            Payload::Feedback(feedback) => {
                if let Some(entry) = self.entries.get(&id).filter(|e| e.sending && !e.committed) {
                    let mut state = lock(&entry.ticket.snapshot);
                    state.effect = feedback.effect;
                    state.stage = if feedback.ready {
                        Stage::Dragging
                    } else {
                        Stage::Preparing
                    };
                }
            }
            Payload::Finished(result) => {
                if !self
                    .entries
                    .get(&id)
                    .is_some_and(|e| e.sending || result.stage >= 3 || (e.reverse && e.resuming))
                {
                    return Ok(());
                }
                if let Some(entry) = self.entries.get(&id) {
                    let _ = self.shared.pointer_owner.compare_exchange(
                        id,
                        0,
                        Ordering::AcqRel,
                        Ordering::Acquire,
                    );
                    let mut state = lock(&entry.ticket.snapshot);
                    state.bytes_read = result.bytes_read;
                    state.stage = match result.stage {
                        1 => Stage::Reading,
                        2 => Stage::Complete,
                        3 => Stage::Cancelled,
                        _ => Stage::Failed,
                    };
                    state.error = wire::reason_text(&result.reason);
                    state.effect = 0;
                }
                if result.stage != 1 {
                    self.entries.remove(&id);
                }
            }
            Payload::Cancel(_) => {
                self.finish(id, 3, "", String::new()).await?;
            }
            Payload::Probe(point) => {
                ensure!(
                    self.role == Role::Host
                        && self.shared.enabled.load(Ordering::Acquire)
                        && self.shared.ready.load(Ordering::Acquire),
                    wire::Refused(wire::reason::OUT_NOT_ALLOWED)
                );
                ensure!(
                    self.shared
                        .pointer_owner
                        .compare_exchange(0, id, Ordering::AcqRel, Ordering::Acquire)
                        .is_ok(),
                    "{}",
                    t().features.drag_in_progress
                );
                let mut entry = self.entry(id, point, true, None)?;
                entry.reverse = true;
                let target = entry
                    .target
                    .clone()
                    .ok_or_else(|| anyhow::anyhow!("{}", t().features.drag_out_source_missing))?;
                let position = Self::screen_position(point, &target)?;
                let alive = entry.ticket.alive.clone();
                let binding = self.shared.clone();
                let token = self.token;
                let guard = Arc::new(move || {
                    alive.load(Ordering::Acquire)
                        && binding.enabled.load(Ordering::Acquire)
                        && binding.token.load(Ordering::Acquire) == token
                        && target_current(&binding, &target)
                        && !binding.stop.is_cancelled()
                        && crate::features::host::clipboard::agent::desktop_available()
                });
                let commands = self.input.clone();
                let alive = entry.ticket.alive.clone();
                entry.portal = Some(native::portal::Portal::start(
                    position,
                    self.shared.ready.load(Ordering::Acquire),
                    guard,
                    Arc::new(move |event| {
                        if commands.try_send(Command::Portal(id, event)).is_err() {
                            alive.store(false, Ordering::Release);
                        }
                    }),
                )?);
                self.entries.insert(id, entry);
            }
            Payload::SourceReleased(true) => {
                if let Some(entry) = self
                    .entries
                    .get_mut(&id)
                    .filter(|e| e.reverse && !e.sending)
                {
                    entry.source_released = true;
                    if entry.preserve_source && !entry.committed && !entry.resuming {
                        entry.ticket.cancel();
                    }
                    self.try_start_native(id)?;
                }
            }
            Payload::Progress(bytes) => {
                if let Some(entry) = self.entries.get(&id).filter(|e| e.sending && e.committed) {
                    let mut state = lock(&entry.ticket.snapshot);
                    state.bytes_read = bytes;
                }
            }
            _ => {}
        }
        Ok(())
    }
    async fn tick(&mut self) -> Result<()> {
        self.policy().await?;
        let ids: Vec<_> = self.entries.keys().copied().collect();
        for id in ids {
            if self.entries[&id].resuming {
                self.return_tick(id).await?;
                continue;
            }
            let entry = &self.entries[&id];
            if entry.reverse
                && !entry.sending
                && entry.release_source.is_some()
                && lock(&entry.ticket.snapshot).stage == Stage::Preparing
                && entry.created_at.elapsed() > Duration::from_secs(3)
            {
                self.emit(id, Payload::Cancel(true), false).await?;
                self.finish(id, 3, "", String::new()).await?;
                continue;
            }
            if !entry.committed
                && !(entry.reverse && entry.sending && entry.source_released)
                && entry
                    .target
                    .as_ref()
                    .is_some_and(|target| !target_current(&self.shared, target))
            {
                self.finish(id, 4, wire::reason::LAYOUT_CHANGED, String::new())
                    .await?;
                continue;
            }
            if !entry.committed
                && ((!entry.sending && !entry.reverse && self.position(entry.point).is_err())
                    || (lock(&entry.ticket.snapshot).stage == Stage::Preparing
                        && entry.created_at.elapsed() > Duration::from_secs(30)))
            {
                self.finish(id, 4, wire::reason::PREPARE_TIMEOUT, String::new())
                    .await?;
                continue;
            }
            if !entry.ticket.alive.load(Ordering::Acquire) {
                self.emit(id, Payload::Cancel(true), false).await?;
                self.finish(id, 3, "", String::new()).await?;
                continue;
            }
            if entry.sending && !entry.reverse && !entry.committed {
                let point = *lock(&entry.ticket.point);
                if point != entry.point {
                    self.entries.get_mut(&id).unwrap().point = point;
                    self.emit(id, Payload::Position(point), false).await?;
                }
            } else if !entry.sending && entry.committed {
                if let Some(status) = entry.offer.as_ref().map(FileOffer::status) {
                    lock(&entry.ticket.snapshot).bytes_read = status.bytes_read;
                    if entry.accepted
                        && let Some(result) = status.completed
                    {
                        let (stage, reason) = match result {
                            Ok(()) => (2, ""),
                            Err(_) => (4, wire::reason::FAILED),
                        };
                        self.finish(id, stage, reason, result.err().unwrap_or_default())
                            .await?;
                    } else if entry.progress_at.elapsed() >= Duration::from_millis(250) {
                        self.entries.get_mut(&id).unwrap().progress_at = Instant::now();
                        self.emit(id, Payload::Progress(status.bytes_read), false)
                            .await?;
                    }
                }
            }
        }
        Ok(())
    }
}

async fn selection_image(paths: &[PathBuf]) -> wire::DragImage {
    let paths = paths.to_vec();
    tokio::task::spawn_blocking(move || native::appearance::selection(&paths))
        .await
        .ok()
        .and_then(Result::ok)
        .unwrap_or_default()
}
