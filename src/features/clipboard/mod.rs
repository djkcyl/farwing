//! Device-scoped clipboard exchange (CLIPBOARD v2, `docs/protocol/v2-design.md`
//! §8.2) with a process-wide Windows/OLE adapter.
//!
//! Contents are rendered on demand: an `Offer` lists formats, and the peer
//! fetches a format or reads files only when a local application pastes.
//! Contents above [`wire::INLINE`] stream as `TRANSFER` chunks on the bulk
//! channel, paced by the requester's `FLOW` credits (§8).
mod formats;
pub(crate) mod memory;
mod native;
pub(crate) mod system;
pub(crate) mod wire;
use crate::protocol::peer::{self, FrameType};
use crate::i18n::t;
use crate::transport::transfer::{self, Chunk, Credit, Flow, Window};
use anyhow::{Result, anyhow, bail, ensure};
use bytes::Bytes;
pub(crate) use native::{FileOffer, FileSummary};
use prost::Message;
use std::{
    collections::{HashMap, VecDeque},
    sync::{
        Arc, Condvar, Mutex, Weak,
        atomic::{AtomicBool, AtomicI32, AtomicU32, AtomicU64, Ordering},
    },
    time::Duration,
};
use tokio::sync::{Notify, mpsc};
use webrtc::data_channel::RTCDataChannel;
use wire::{Body, FileItem, Format, Received};

/// Largest `ReadFile` request; one request per block keeps memory bounded.
const FILE_BLOCK: usize = 1024 * 1024;
const MAX_DATA: usize = 128 * 1024 * 1024;
pub(crate) const MAX_FILES: usize = 100_000;
const MAX_PENDING: usize = 64;
/// Chunks a sender emits before it looks at its queue again, so requests
/// and responses are not stuck behind a long stream.
const BURST: usize = 16;
pub(crate) fn shutdown() {
    native::shutdown();
}
fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

static DEV_EXCHANGE: Mutex<Option<Clipboard>> = Mutex::new(None);
/// `dev clipboard`: connections made afterwards use this (in-memory)
/// exchange instead of the viewer's Windows clipboard.
pub(crate) fn install_dev_exchange(clipboard: Clipboard) {
    *lock(&DEV_EXCHANGE) = Some(clipboard);
}
pub(crate) fn dev_exchange() -> Option<Clipboard> {
    lock(&DEV_EXCHANGE).clone()
}

/// Whether an outgoing frame belongs on the bulk channel (`TRANSFER`/`FLOW`)
/// rather than the session channel.
pub(crate) fn is_bulk(bytes: &[u8]) -> bool {
    peer::is_type(bytes, &[FrameType::Transfer, FrameType::Flow])
}

/// Answer a request that arrived while no clipboard is available (for
/// example before an interactive user logged on); `None` for other frames.
pub(crate) fn reject_request(bytes: &[u8]) -> Result<Option<Vec<u8>>> {
    let Ok((header, _)) = peer::decode(bytes) else {
        return Ok(None);
    };
    Ok((header.frame_type() == Some(FrameType::Clipboard)
        && header.request_id != 0
        && !header.is_response())
    .then(|| {
        wire::failure(
            header.request_id,
            farwing_protocol::peer::error_code::UNAVAILABLE,
            wire::reason::UNAVAILABLE,
        )
    }))
}

#[derive(Clone)]
pub(crate) struct Clipboard(Arc<Inner>);
pub(crate) struct Snapshot {
    pub enabled: bool,
    pub files: bool,
    /// The exchange runs (`Pull` works).
    pub active: bool,
    /// Copies are synchronized automatically.
    pub auto: bool,
    pub error: Option<String>,
}
enum Backend {
    Native,
    Memory(Arc<memory::Store>),
}
struct Inner {
    id: u64,
    /// `TRANSFER` kind of this exchange: CLIPBOARD, or DRAG for a drag's
    /// private file exchange.
    kind: i32,
    backend: Backend,
    permitted: Option<Arc<dyn Fn() -> bool + Send + Sync>>,
    /// User switches: automatic sync and files.
    enabled: AtomicBool,
    files: AtomicBool,
    /// The connection's last policy: exchange ready, automatic sync
    /// allowed, files allowed.
    ready: AtomicBool,
    auto_ready: AtomicBool,
    files_ready: AtomicBool,
    /// The exchange runs (requests are served; `Pull` works).
    active: AtomicBool,
    /// Local copies are offered and peer offers are applied.
    auto: AtomicBool,
    allowed_files: AtomicBool,
    /// This side is the host: it answers `Pull`.
    host_role: AtomicBool,
    epoch: AtomicU64,
    requests: AtomicU32,
    streams: AtomicU32,
    offers: AtomicU32,
    sender: Mutex<Option<mpsc::Sender<Queued>>>,
    /// Credit granted by the peer for the streams we send, by stream ID.
    credit: Mutex<HashMap<u32, Credit>>,
    credit_changed: Notify,
    pending: Mutex<HashMap<u32, Pending>>,
    reassembly: Mutex<peer::Reassembly>,
    error: Mutex<Option<String>>,
    file_offers: Option<mpsc::Sender<FileOffer>>,
}
static SEND_BYTES: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
struct SendBudget(usize);
impl Drop for SendBudget {
    fn drop(&mut self) {
        SEND_BYTES.fetch_sub(self.0, Ordering::AcqRel);
    }
}
struct Queued {
    item: Outbound,
    budget: SendBudget,
}
enum Outbound {
    /// Frames of this epoch, dropped when the exchange is suspended.
    Frames(u64, Vec<Vec<u8>>),
    /// Failure replies and stream/offer releases, sent even while suspended
    /// so the peer never waits for an answer that will not come.
    Cleanup(Vec<u8>),
    /// Contents of a stream the peer requested, paced by its credit.
    Stream(u64, u32, Bytes),
}
/// Epoch passed to the transport for [`Outbound::Cleanup`] frames.
const CLEANUP: u64 = 0;
enum Value {
    Data(Vec<u8>),
    Files(Vec<FileItem>),
    Offer(u32, Vec<Format>),
}
/// A remote clipboard request without any progress for this long is given
/// up, so a synchronous paste in a local application never hangs forever.
const REQUEST_STALL_TIMEOUT: Duration = Duration::from_secs(30);

struct Waiter {
    value: Mutex<Option<Result<Value, String>>>,
    changed: Condvar,
    progress: Mutex<std::time::Instant>,
}
impl Waiter {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            value: Mutex::new(None),
            changed: Condvar::new(),
            progress: Mutex::new(std::time::Instant::now()),
        })
    }
    fn touch(&self) {
        *lock(&self.progress) = std::time::Instant::now();
    }
    fn finish(&self, value: Result<Value, String>) {
        let mut slot = lock(&self.value);
        if slot.is_none() {
            *slot = Some(value);
        }
        self.changed.notify_all();
    }
}
enum Expect {
    /// `Data`, inline or on `stream`.
    Data {
        stream: u32,
        maximum: usize,
        size: Option<u64>,
        buffer: Vec<u8>,
        window: Window,
        last: bool,
    },
    Files,
    /// The answer to `Pull`.
    Offer,
}
struct Pending {
    waiter: Arc<Waiter>,
    expect: Expect,
}

impl Clipboard {
    pub fn new() -> Self {
        Self::guarded(None)
    }
    pub(crate) fn guarded(permitted: Option<Arc<dyn Fn() -> bool + Send + Sync>>) -> Self {
        Self::create(permitted, None, transfer::kind::CLIPBOARD, Backend::Native)
    }
    /// File-only OLE exchange of a native drag (§8.3) with its own lifetime
    /// and wire owner. Never reads or writes the system clipboard and never
    /// observes ordinary copy events. Its data streams are `TRANSFER{kind: DRAG}`.
    pub(crate) fn isolated_files(
        permitted: Arc<dyn Fn() -> bool + Send + Sync>,
    ) -> (Self, mpsc::Receiver<FileOffer>) {
        let (sender, receiver) = mpsc::channel(1);
        (
            Self::create(
                Some(permitted),
                Some(sender),
                transfer::kind::DRAG,
                Backend::Native,
            ),
            receiver,
        )
    }
    /// An exchange backed by in-memory contents instead of the Windows
    /// clipboard (protocol tests and `dev clipboard`).
    pub(crate) fn memory(kind: i32) -> Self {
        let clipboard = Self::create(
            None,
            None,
            kind,
            Backend::Memory(Arc::new(memory::Store::default())),
        );
        for flag in [
            &clipboard.0.enabled,
            &clipboard.0.ready,
            &clipboard.0.auto_ready,
            &clipboard.0.files_ready,
            &clipboard.0.active,
            &clipboard.0.auto,
        ] {
            flag.store(true, Ordering::Release);
        }
        clipboard
    }
    fn create(
        permitted: Option<Arc<dyn Fn() -> bool + Send + Sync>>,
        file_offers: Option<mpsc::Sender<FileOffer>>,
        kind: i32,
        backend: Backend,
    ) -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(1);
        Self(Arc::new(Inner {
            id: NEXT.fetch_add(1, Ordering::Relaxed),
            kind,
            backend,
            permitted,
            enabled: AtomicBool::new(false),
            files: AtomicBool::new(true),
            ready: AtomicBool::new(false),
            auto_ready: AtomicBool::new(false),
            files_ready: AtomicBool::new(false),
            active: AtomicBool::new(false),
            auto: AtomicBool::new(false),
            allowed_files: AtomicBool::new(true),
            host_role: AtomicBool::new(false),
            epoch: AtomicU64::new(1),
            requests: AtomicU32::new(0),
            streams: AtomicU32::new(0),
            offers: AtomicU32::new(0),
            sender: Mutex::new(None),
            credit: Mutex::new(HashMap::new()),
            credit_changed: Notify::new(),
            pending: Mutex::new(HashMap::new()),
            reassembly: Mutex::new(peer::Reassembly::new(wire::MAX_MESSAGE)),
            error: Mutex::new(None),
            file_offers,
        }))
    }
    pub(crate) async fn publish_files(
        &self,
        paths: Vec<std::path::PathBuf>,
    ) -> Result<FileSummary> {
        ensure!(self.0.file_offers.is_some(), "{}", t().features.clipboard_publish_needs_session);
        let epoch = self.epoch();
        ensure!(
            self.0.valid(epoch) && self.0.file_allowed(),
            "{}",
            t().features.clipboard_drag_session_unavailable
        );
        let source = self.0.clone();
        let prepared = tokio::task::spawn_blocking(move || {
            native::prepare_files(paths, || source.valid(epoch))
        })
        .await??;
        let summary = prepared.summary();
        let (sender, receiver) = tokio::sync::oneshot::channel();
        native::post(native::Command::PublishFiles(
            Arc::downgrade(&self.0),
            epoch,
            prepared,
            sender,
        ))?;
        receiver
            .await
            .map_err(|_| anyhow!("{}", t().features.clipboard_publish_ended))?
            .map_err(anyhow::Error::msg)?;
        Ok(summary)
    }
    pub(crate) fn epoch(&self) -> u64 {
        self.0.epoch.load(Ordering::Acquire)
    }
    /// Whether an outgoing frame handed to the transport for `epoch` is
    /// still current.
    pub(crate) fn delivery_allowed(&self, epoch: u64) -> bool {
        epoch == CLEANUP || (epoch == self.epoch() && self.0.valid(epoch))
    }
    pub fn snapshot(&self) -> Snapshot {
        Snapshot {
            enabled: self.0.enabled.load(Ordering::Acquire),
            files: self.0.files.load(Ordering::Acquire),
            active: self.0.active.load(Ordering::Acquire),
            auto: self.0.active.load(Ordering::Acquire) && self.0.auto.load(Ordering::Acquire),
            error: lock(&self.0.error).clone(),
        }
    }
    pub fn protocol_error(&self, error: String) {
        self.0.fail(error);
        self.0.cancel_pending();
    }
    /// The user switch for automatic sync. `Pull` stays available.
    pub fn set_enabled(&self, enabled: bool) -> Result<()> {
        self.0.enabled.store(enabled, Ordering::Release);
        self.apply();
        Ok(())
    }
    pub fn set_files(&self, enabled: bool) {
        self.0.files.store(enabled, Ordering::Release);
        self.apply();
    }
    /// This side is the host and answers `Pull`.
    pub(crate) fn host_role(&self) {
        self.0.host_role.store(true, Ordering::Release);
    }
    /// The connection's policy: `ready` runs the exchange (requests are
    /// served and `Pull` works); `auto` additionally offers local copies and
    /// applies the peer's offers; `files` allows file lists.
    pub fn policy(&self, ready: bool, auto: bool, files: bool) {
        self.0.ready.store(ready, Ordering::Release);
        self.0.auto_ready.store(auto, Ordering::Release);
        self.0.files_ready.store(files, Ordering::Release);
        self.apply();
    }
    fn apply(&self) {
        let inner = &self.0;
        let files = inner.files_ready.load(Ordering::Acquire) && inner.files.load(Ordering::Acquire);
        let auto = inner.auto_ready.load(Ordering::Acquire) && inner.enabled.load(Ordering::Acquire);
        let files_changed = inner.allowed_files.swap(files, Ordering::AcqRel) != files;
        let auto_changed = inner.auto.swap(auto, Ordering::AcqRel) != auto;
        if files_changed || auto_changed {
            self.suspend();
        }
        if !inner.ready.load(Ordering::Acquire) {
            self.suspend();
            return;
        }
        if inner.active.swap(true, Ordering::AcqRel) {
            return;
        }
        if matches!(inner.backend, Backend::Native) {
            let started = native::start()
                .and_then(|_| native::post(native::Command::Activate(Arc::downgrade(inner))));
            if let Err(e) = started {
                inner.active.store(false, Ordering::Release);
                inner.fail(e.to_string());
            }
        }
    }
    /// Copy the host's current clipboard into ours (`Pull`): blocks until the
    /// host answered, then installs its offer locally; contents are fetched
    /// when a local application pastes. Returns the number of formats.
    pub(crate) fn pull(&self) -> Result<usize> {
        let epoch = self.epoch();
        ensure!(self.0.valid(epoch), "{}", t().features.clipboard_channel_not_ready);
        ensure!(!self.0.host_role.load(Ordering::Acquire), "{}", t().features.clipboard_pull_controller_only);
        let (id, waiter) = self.0.begin(epoch, Expect::Offer, Body::Pull(wire::Pull {}))?;
        let Value::Offer(offer, formats) = self.0.wait(epoch, id, waiter)? else {
            bail!("{}", t().features.clipboard_wrong_response);
        };
        ensure!(!formats.is_empty(), "{}", t().features.clipboard_host_empty);
        let count = formats.len();
        self.0.apply_offer(epoch, offer, formats, true)?;
        Ok(count)
    }
    pub fn suspend(&self) {
        if self.0.active.swap(false, Ordering::AcqRel) {
            self.0.epoch.fetch_add(1, Ordering::AcqRel);
            self.0.cancel_pending();
            lock(&self.0.credit).clear();
            lock(&self.0.reassembly).clear();
            if matches!(self.0.backend, Backend::Native) {
                let _ = native::post(native::Command::Remove(self.0.id));
            }
        }
    }
    /// Controller transport: `CLIPBOARD` frames on the session channel,
    /// `TRANSFER`/`FLOW` on the bulk channel through its shared writer gate.
    pub(crate) async fn run_sender(
        &self,
        session: Arc<RTCDataChannel>,
        bulk: Arc<RTCDataChannel>,
    ) {
        let owner = self.clone();
        self.sender(move |epoch, data| {
            let session = session.clone();
            let bulk = bulk.clone();
            let owner = owner.clone();
            async move {
                if is_bulk(&data) {
                    crate::transport::outbox::send_while(&bulk, Bytes::from(data), || {
                        owner.delivery_allowed(epoch)
                    })
                    .await
                } else {
                    crate::transport::outbox::send(&session, Bytes::from(data)).await
                }
            }
        })
        .await;
    }
    /// The sending task: hands frames to `send(epoch, frame)` in order and
    /// paces streams by the peer's credit. The transport chooses the
    /// channel by frame type ([`is_bulk`]).
    pub(crate) fn sender<F, Fut>(
        &self,
        mut send: F,
    ) -> impl std::future::Future<Output = ()> + Send + 'static
    where
        F: FnMut(u64, Vec<u8>) -> Fut + Send + 'static,
        Fut: std::future::Future<Output = Result<()>> + Send,
    {
        let (tx, mut rx) = mpsc::channel(256);
        *lock(&self.0.sender) = Some(tx);
        let owner = self.clone();
        async move {
            let this = &owner.0;
            let mut streams = VecDeque::<SendStream>::new();
            let mut open = true;
            while open || !streams.is_empty() {
                let changed = this.credit_changed.notified();
                tokio::pin!(changed);
                changed.as_mut().enable();
                let mut result = Ok(());
                // Drain queued frames first; they are small and time-sensitive.
                loop {
                    let queued = match rx.try_recv() {
                        Ok(queued) => queued,
                        Err(mpsc::error::TryRecvError::Empty) => break,
                        Err(mpsc::error::TryRecvError::Disconnected) => {
                            open = false;
                            break;
                        }
                    };
                    result = this.dispatch(&mut send, queued, &mut streams).await;
                    if result.is_err() {
                        break;
                    }
                }
                if result.is_ok() {
                    result = this.pump(&mut send, &mut streams).await;
                }
                if let Err(error) = result {
                    if this.active.load(Ordering::Acquire) {
                        this.fail(error.to_string());
                        this.cancel_pending();
                    }
                    continue;
                }
                let blocked = streams.iter().all(|s| !this.can_send(s));
                if !open && streams.is_empty() {
                    break;
                }
                if !blocked {
                    // More credited data: let other tasks run, then continue.
                    tokio::task::yield_now().await;
                    continue;
                }
                tokio::select! {
                    queued = rx.recv(), if open => match queued {
                        Some(queued) => {
                            if let Err(error) = this.dispatch(&mut send, queued, &mut streams).await
                                && this.active.load(Ordering::Acquire)
                            {
                                this.fail(error.to_string());
                                this.cancel_pending();
                            }
                        }
                        None => open = false,
                    },
                    _ = &mut changed, if !streams.is_empty() => {},
                    // A safety net; FLOW and Abort wake the sender directly.
                    _ = tokio::time::sleep(Duration::from_millis(50)), if !streams.is_empty() => {},
                }
                if !open && streams.iter().all(|s| !this.can_send(s)) {
                    // Nothing more will arrive; streams waiting for credit end
                    // with the connection.
                    break;
                }
            }
        }
    }
    /// Feed one received frame: `CLIPBOARD`, or `TRANSFER`/`FLOW` of a stream
    /// of this exchange. Returns `false` for frames that belong elsewhere
    /// (another kind, or a stream this exchange does not know).
    pub fn receive(&self, bytes: &[u8]) -> Result<bool> {
        let (header, body) = peer::decode(bytes)?;
        match header.frame_type() {
            Some(FrameType::Clipboard) => {
                let whole = lock(&self.0.reassembly).push(&header, body)?;
                if let Some(whole) = whole {
                    self.0.message(wire::decode(&header, &whole)?)?;
                }
                Ok(true)
            }
            Some(FrameType::Transfer) => {
                let chunk = Chunk::decode(body)?;
                if chunk.kind != self.0.kind {
                    return Ok(false);
                }
                self.0.chunk(chunk)
            }
            Some(FrameType::Flow) => {
                let flow = Flow::decode(body)?;
                if flow.kind != self.0.kind || flow.channel != transfer::channel::BULK {
                    return Ok(false);
                }
                let mut credits = lock(&self.0.credit);
                let Some(credit) = credits.get_mut(&flow.stream) else {
                    return Ok(false);
                };
                if credit.grant(flow.credit) {
                    self.0.credit_changed.notify_waiters();
                }
                Ok(true)
            }
            _ => Ok(false),
        }
    }
}

struct SendStream {
    epoch: u64,
    stream: u32,
    data: Bytes,
    offset: usize,
    _budget: SendBudget,
}

impl Inner {
    fn next_request(&self) -> u32 {
        loop {
            let id = self.requests.fetch_add(1, Ordering::Relaxed).wrapping_add(1);
            if id != 0 {
                return id;
            }
        }
    }
    fn next_stream(&self) -> u32 {
        // Unique within the process, so a drag endpoint can route a stream to
        // the one exchange that registered it.
        static STREAMS: AtomicU32 = AtomicU32::new(0);
        let _ = &self.streams;
        loop {
            let id = STREAMS.fetch_add(1, Ordering::Relaxed).wrapping_add(1);
            if id != 0 {
                return id;
            }
        }
    }
    fn next_offer(&self) -> u32 {
        loop {
            let id = self.offers.fetch_add(1, Ordering::Relaxed).wrapping_add(1);
            if id != 0 {
                return id;
            }
        }
    }
    fn valid(&self, epoch: u64) -> bool {
        self.active.load(Ordering::Acquire)
            && self.epoch.load(Ordering::Acquire) == epoch
            && self.permitted.as_ref().is_none_or(|p| p())
    }
    fn file_allowed(&self) -> bool {
        self.allowed_files.load(Ordering::Acquire)
    }
    fn fail(&self, text: String) {
        *lock(&self.error) = Some(text);
    }
    fn cancel_pending(&self) {
        let pending = std::mem::take(&mut *lock(&self.pending));
        for (_, p) in pending {
            if let Expect::Data { stream, .. } = p.expect {
                self.abort(stream);
            }
            p.waiter.finish(Err(t().features.clipboard_cancelled.into()));
        }
    }
    /// Tell the peer to stop sending `stream`.
    fn abort(&self, stream: u32) {
        let _ = self.enqueue(Outbound::Cleanup(wire::push(Body::Abort(wire::Abort {
            stream,
        }))));
    }
    fn enqueue(&self, item: Outbound) -> Result<()> {
        let size = match &item {
            Outbound::Frames(_, frames) => frames.iter().map(Vec::len).sum(),
            Outbound::Cleanup(frame) => frame.len(),
            Outbound::Stream(_, _, data) => data.len(),
        };
        SEND_BYTES
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |n| {
                n.checked_add(size)
                    .filter(|n| *n <= MAX_DATA + 4 * 1024 * 1024)
            })
            .map_err(|_| anyhow!("{}", t().features.clipboard_send_over_budget))?;
        let queued = Queued {
            item,
            budget: SendBudget(size),
        };
        lock(&self.sender)
            .as_ref()
            .ok_or_else(|| anyhow!("{}", t().features.clipboard_channel_not_ready))?
            .try_send(queued)
            .map_err(|_| anyhow!("{}", t().features.clipboard_send_queue_full))
    }
    fn emit(&self, epoch: u64, frames: Vec<Vec<u8>>) -> Result<()> {
        ensure!(self.valid(epoch), "{}", t().features.clipboard_paused);
        self.enqueue(Outbound::Frames(epoch, frames))
    }
    /// Announce new local contents; returns the offer number.
    fn offer(&self, epoch: u64, formats: Vec<Format>) -> Result<u32> {
        let offer = self.next_offer();
        self.emit(
            epoch,
            vec![wire::push(Body::Offer(wire::Offer { offer, formats }))],
        )?;
        Ok(offer)
    }
    /// Answer `request` with `data`: inline, or as a stream.
    fn respond_data(&self, epoch: u64, request: u32, stream: u32, data: Vec<u8>) -> Result<()> {
        let size = data.len() as u64;
        if data.len() <= wire::INLINE {
            return self.emit(
                epoch,
                wire::response(request, Body::Data(wire::Data { size, inline: data })),
            );
        }
        ensure!(stream != 0, "{}", t().features.clipboard_no_stream);
        lock(&self.credit).insert(stream, Credit::new(transfer::INITIAL_WINDOW));
        self.emit(
            epoch,
            wire::response(
                request,
                Body::Data(wire::Data {
                    size,
                    inline: Vec::new(),
                }),
            ),
        )?;
        self.enqueue(Outbound::Stream(epoch, stream, Bytes::from(data)))
    }
    fn respond(&self, epoch: u64, request: u32, body: Body) -> Result<()> {
        self.emit(epoch, wire::response(request, body))
    }
    /// Refuse `request` for `reason` (one of [`wire::reason`]).
    fn refuse(&self, request: u32, code: &str, reason: &str) {
        let _ = self.enqueue(Outbound::Cleanup(wire::failure(request, code, reason)));
    }
    async fn dispatch<F, Fut>(
        &self,
        send: &mut F,
        queued: Queued,
        streams: &mut VecDeque<SendStream>,
    ) -> Result<()>
    where
        F: FnMut(u64, Vec<u8>) -> Fut,
        Fut: std::future::Future<Output = Result<()>>,
    {
        let Queued { item, budget } = queued;
        match item {
            Outbound::Frames(epoch, frames) => {
                for frame in frames {
                    if !self.valid(epoch) {
                        break;
                    }
                    send(epoch, frame).await?;
                }
            }
            Outbound::Cleanup(frame) => send(CLEANUP, frame).await?,
            Outbound::Stream(epoch, stream, data) => {
                if self.valid(epoch) {
                    streams.push_back(SendStream {
                        epoch,
                        stream,
                        data,
                        offset: 0,
                        _budget: budget,
                    });
                }
            }
        }
        Ok(())
    }
    fn can_send(&self, stream: &SendStream) -> bool {
        let len = (stream.data.len() - stream.offset).min(transfer::CHUNK);
        lock(&self.credit)
            .get(&stream.stream)
            .is_some_and(|credit| credit.available() >= len as u64)
    }
    /// Send up to [`BURST`] chunks of the first stream that has credit.
    async fn pump<F, Fut>(&self, send: &mut F, streams: &mut VecDeque<SendStream>) -> Result<()>
    where
        F: FnMut(u64, Vec<u8>) -> Fut,
        Fut: std::future::Future<Output = Result<()>>,
    {
        let mut sent = 0;
        let mut checked = 0;
        while sent < BURST && checked < streams.len() {
            let Some(mut stream) = streams.pop_front() else {
                break;
            };
            // An aborted stream lost its credit entry.
            let registered = lock(&self.credit).contains_key(&stream.stream);
            if !self.valid(stream.epoch) || !registered {
                lock(&self.credit).remove(&stream.stream);
                checked = 0;
                continue;
            }
            if !self.can_send(&stream) {
                streams.push_back(stream);
                checked += 1;
                continue;
            }
            checked = 0;
            while sent < BURST && self.can_send(&stream) {
                let end = (stream.offset + transfer::CHUNK).min(stream.data.len());
                let chunk = Chunk {
                    stream: stream.stream,
                    kind: self.kind,
                    offset: stream.offset as u64,
                    data: stream.data.slice(stream.offset..end),
                    last: end == stream.data.len(),
                    entry: 0,
                };
                if let Some(credit) = lock(&self.credit).get_mut(&stream.stream) {
                    credit.spend(end - stream.offset)?;
                }
                send(stream.epoch, peer::message(FrameType::Transfer, &chunk)).await?;
                stream.offset = end;
                sent += 1;
                if chunk.last {
                    break;
                }
            }
            if stream.offset == stream.data.len() {
                lock(&self.credit).remove(&stream.stream);
            } else {
                streams.push_front(stream);
            }
        }
        Ok(())
    }
    fn message(self: &Arc<Self>, message: Received) -> Result<()> {
        let epoch = self.epoch.load(Ordering::Acquire);
        match message {
            Received::Push(Body::Offer(offer)) => {
                ensure!(valid_formats(&offer.formats), "{}", t().features.clipboard_invalid_formats);
                // Without automatic sync (or while suspended) the peer's
                // copies are not applied.
                if self.valid(epoch) && self.auto.load(Ordering::Acquire) {
                    self.apply_offer(epoch, offer.offer, offer.formats, false)?;
                }
            }
            Received::Push(Body::Release(release)) => match &self.backend {
                Backend::Native => {
                    native::post(native::Command::Release(self.id, release.offer))?
                }
                Backend::Memory(_) => {}
            },
            Received::Push(Body::Abort(abort)) => {
                lock(&self.credit).remove(&abort.stream);
                self.credit_changed.notify_waiters();
            }
            Received::Push(_) => bail!("{}", t().features.clipboard_invalid_push),
            Received::Request(id, body) => {
                if !self.valid(epoch) {
                    self.refuse(id, farwing_protocol::peer::error_code::UNAVAILABLE, wire::reason::PAUSED);
                    return Ok(());
                }
                let files = matches!(body, Body::ListFiles(_) | Body::ReadFile(_));
                if matches!(body, Body::Pull(_)) && !self.host_role.load(Ordering::Acquire) {
                    self.refuse(id, farwing_protocol::peer::error_code::FORBIDDEN, wire::reason::PULL_NOT_SERVED);
                    return Ok(());
                }
                if !matches!(body, Body::Fetch(_) | Body::Pull(_)) && !files {
                    self.refuse(id, farwing_protocol::peer::error_code::BAD_REQUEST, wire::reason::INVALID_REQUEST);
                    return Ok(());
                }
                if (files && !self.file_allowed())
                    || (self.file_offers.is_some() && !files)
                {
                    self.refuse(id, farwing_protocol::peer::error_code::FORBIDDEN, wire::reason::FILES_OFF);
                    return Ok(());
                }
                match &self.backend {
                    Backend::Native => native::post(native::Command::Request(
                        Arc::downgrade(self),
                        epoch,
                        id,
                        body,
                    ))?,
                    Backend::Memory(store) => memory::serve(self, store, epoch, id, body),
                }
            }
            Received::Response(id, body) => self.response(id, body)?,
            Received::Failure(id, code, message) => {
                if let Some(p) = lock(&self.pending).remove(&id) {
                    if let Expect::Data { stream, .. } = p.expect {
                        lock(&self.credit).remove(&stream);
                    }
                    tracing::debug!(id, code, reason = %message, "remote clipboard request failed");
                    p.waiter.finish(Err(wire::reason_text(&message).into()));
                }
            }
        }
        Ok(())
    }
    fn response(&self, id: u32, body: Body) -> Result<()> {
        let mut pending = lock(&self.pending);
        let Some(p) = pending.get_mut(&id) else {
            return Ok(());
        };
        match (&mut p.expect, body) {
            (
                Expect::Data {
                    maximum,
                    size,
                    buffer,
                    last,
                    ..
                },
                Body::Data(data),
            ) => {
                if data.size > *maximum as u64
                    || (data.size as usize <= wire::INLINE && data.inline.len() as u64 != data.size)
                    || (data.size as usize > wire::INLINE && !data.inline.is_empty())
                {
                    let p = pending.remove(&id).unwrap();
                    p.waiter.finish(Err(t().features.clipboard_invalid_length.into()));
                    return Ok(());
                }
                if data.size as usize <= wire::INLINE {
                    let p = pending.remove(&id).unwrap();
                    p.waiter.finish(Ok(Value::Data(data.inline)));
                    return Ok(());
                }
                *size = Some(data.size);
                p.waiter.touch();
                if *last {
                    let complete = buffer.len() as u64 == data.size;
                    let p = pending.remove(&id).unwrap();
                    p.waiter.finish(match (complete, p.expect) {
                        (true, Expect::Data { buffer, .. }) => Ok(Value::Data(buffer)),
                        _ => Err(t().features.clipboard_length_mismatch.into()),
                    });
                }
            }
            (Expect::Offer, Body::Offer(offer)) => {
                let p = pending.remove(&id).unwrap();
                p.waiter.finish(if valid_formats(&offer.formats) {
                    Ok(Value::Offer(offer.offer, offer.formats))
                } else {
                    Err(t().features.clipboard_invalid_formats.into())
                });
            }
            (Expect::Files, Body::Files(files)) => {
                let p = pending.remove(&id).unwrap();
                p.waiter.finish(
                    if files.items.len() <= MAX_FILES
                        && files.items.iter().all(|f| native::safe_name(&f.path))
                    {
                        Ok(Value::Files(files.items))
                    } else {
                        Err(t().features.clipboard_invalid_file_list.into())
                    },
                );
            }
            _ => {
                let p = pending.remove(&id).unwrap();
                p.waiter.finish(Err(t().features.clipboard_wrong_response.into()));
            }
        }
        Ok(())
    }
    /// A chunk of one of our streams; `false` when the stream is unknown
    /// (another exchange's, or one we gave up and aborted).
    fn chunk(&self, chunk: Chunk) -> Result<bool> {
        enum Step {
            Failed(u32, u32, String),
            Complete(u32),
            Grant(u32, Option<u64>),
        }
        let mut pending = lock(&self.pending);
        let Some((&id, p)) = pending.iter_mut().find(
            |(_, p)| matches!(&p.expect, Expect::Data { stream, .. } if *stream == chunk.stream),
        ) else {
            return Ok(false);
        };
        p.waiter.touch();
        let Expect::Data {
            stream,
            maximum,
            size,
            buffer,
            window,
            last,
        } = &mut p.expect
        else {
            unreachable!()
        };
        let accepted = (|| -> Result<Option<u64>> {
            ensure!(!*last && chunk.entry == 0, "{}", t().features.clipboard_stream_ended);
            window.receive(chunk.offset, chunk.data.len())?;
            ensure!(
                buffer.len() + chunk.data.len() <= *maximum
                    && size.is_none_or(|size| (buffer.len() + chunk.data.len()) as u64 <= size),
                "{}",
                t().features.clipboard_data_too_long
            );
            buffer.extend_from_slice(&chunk.data);
            *last = chunk.last;
            Ok(window.deliver(chunk.data.len()))
        })();
        let step = match accepted {
            Err(error) => Step::Failed(id, *stream, error.to_string()),
            // Complete once the last chunk and the `Data` response arrived.
            Ok(_) if *last && size.is_some() => Step::Complete(id),
            Ok(grant) => Step::Grant(*stream, grant.filter(|_| !*last)),
        };
        match step {
            Step::Failed(id, stream, error) => {
                let p = pending.remove(&id).unwrap();
                drop(pending);
                self.abort(stream);
                p.waiter.finish(Err(error));
            }
            Step::Complete(id) => {
                let p = pending.remove(&id).unwrap();
                drop(pending);
                p.waiter.finish(match p.expect {
                    Expect::Data {
                        buffer,
                        size: Some(size),
                        ..
                    } if buffer.len() as u64 == size => Ok(Value::Data(buffer)),
                    _ => Err(t().features.clipboard_length_mismatch.into()),
                });
            }
            Step::Grant(stream, grant) => {
                drop(pending);
                if let Some(credit) = grant {
                    let flow = Flow {
                        channel: transfer::channel::BULK,
                        stream,
                        credit,
                        kind: self.kind,
                    };
                    let epoch = self.epoch.load(Ordering::Acquire);
                    let _ = self.emit(epoch, vec![peer::message(FrameType::Flow, &flow)]);
                }
            }
        }
        Ok(true)
    }
    fn begin(&self, epoch: u64, expect: Expect, body: Body) -> Result<(u32, Arc<Waiter>)> {
        let id = self.next_request();
        let waiter = Waiter::new();
        {
            let mut p = lock(&self.pending);
            ensure!(p.len() < MAX_PENDING, "{}", t().features.clipboard_too_many_requests);
            p.insert(
                id,
                Pending {
                    waiter: waiter.clone(),
                    expect,
                },
            );
        }
        if let Err(e) = self.emit(epoch, wire::request(id, body)) {
            lock(&self.pending).remove(&id);
            return Err(e);
        }
        Ok((id, waiter))
    }
    fn wait(&self, epoch: u64, id: u32, waiter: Arc<Waiter>) -> Result<Value> {
        let give_up = |message: &str| -> anyhow::Error {
            if let Some(Pending {
                expect: Expect::Data { stream, .. },
                ..
            }) = lock(&self.pending).remove(&id)
            {
                self.abort(stream);
            }
            anyhow!("{message}")
        };
        loop {
            let completed = { lock(&waiter.value).take() };
            if let Some(value) = completed {
                lock(&self.pending).remove(&id);
                return value.map_err(anyhow::Error::msg);
            }
            if !self.valid(epoch) {
                return Err(give_up(t().features.clipboard_cancelled));
            }
            if lock(&waiter.progress).elapsed() >= REQUEST_STALL_TIMEOUT {
                tracing::warn!(id, "remote clipboard request stalled; giving up");
                return Err(give_up(t().features.clipboard_remote_timeout));
            }
            if matches!(self.backend, Backend::Native) {
                native::pump();
            }
            let guard = lock(&waiter.value);
            if guard.is_none() {
                drop(waiter.changed.wait_timeout(guard, Duration::from_millis(5)));
            }
        }
    }
    fn data_request(&self, maximum: usize) -> (u32, Expect) {
        let stream = self.next_stream();
        (
            stream,
            Expect::Data {
                stream,
                maximum,
                size: None,
                buffer: Vec::new(),
                window: Window::new(),
                last: false,
            },
        )
    }
    /// Contents of `format` in the peer's offer `offer`.
    fn data(&self, epoch: u64, offer: u32, format: &Format) -> Result<Vec<u8>> {
        let (stream, expect) = self.data_request(MAX_DATA);
        let (id, w) = self.begin(
            epoch,
            expect,
            Body::Fetch(wire::Fetch {
                offer,
                format: Some(format.clone()),
                stream,
            }),
        )?;
        match self.wait(epoch, id, w)? {
            Value::Data(v) => Ok(v),
            _ => bail!("{}", t().features.clipboard_wrong_response),
        }
    }
    /// The file list of the peer's offer `offer`.
    fn descriptors(&self, epoch: u64, offer: u32) -> Result<Vec<FileItem>> {
        ensure!(self.file_allowed(), "{}", t().features.clipboard_files_off);
        let (id, w) = self.begin(
            epoch,
            Expect::Files,
            Body::ListFiles(wire::ListFiles { offer }),
        )?;
        match self.wait(epoch, id, w)? {
            Value::Files(v) => Ok(v),
            _ => bail!("{}", t().features.clipboard_wrong_file_list_response),
        }
    }
    fn read_file(
        &self,
        epoch: u64,
        offer: u32,
        index: u32,
        offset: u64,
        length: usize,
    ) -> Result<Vec<u8>> {
        ensure!(self.file_allowed(), "{}", t().features.clipboard_files_off);
        ensure!(length <= FILE_BLOCK, "{}", t().features.clipboard_read_too_large);
        let (stream, expect) = self.data_request(length);
        let (id, w) = self.begin(
            epoch,
            expect,
            Body::ReadFile(wire::ReadFile {
                offer,
                index,
                offset,
                length: length as u32,
                stream,
            }),
        )?;
        match self.wait(epoch, id, w)? {
            Value::Data(v) => Ok(v),
            _ => bail!("{}", t().features.clipboard_wrong_file_response),
        }
    }
    /// Install the peer's offer locally (rendered on demand).
    fn apply_offer(
        self: &Arc<Self>,
        epoch: u64,
        offer: u32,
        formats: Vec<Format>,
        pulled: bool,
    ) -> Result<()> {
        match &self.backend {
            Backend::Native => native::post(native::Command::Offer(
                Arc::downgrade(self),
                epoch,
                offer,
                formats,
                pulled,
            )),
            Backend::Memory(store) => {
                store.remote_offer(offer, formats);
                Ok(())
            }
        }
    }
    /// The receiver no longer reads the files of the peer's `offer`.
    fn release(&self, offer: u32) {
        let _ = self.enqueue(Outbound::Cleanup(wire::push(Body::Release(wire::Release {
            offer,
        }))));
    }
}
fn valid_formats(formats: &[Format]) -> bool {
    formats.len() <= 256 && formats.iter().all(|f| f.name.len() <= 1024)
}
impl Drop for Inner {
    fn drop(&mut self) {
        self.active.store(false, Ordering::Release);
        self.cancel_pending();
        if matches!(self.backend, Backend::Native) {
            let _ = native::post(native::Command::Remove(self.id));
        }
    }
}
