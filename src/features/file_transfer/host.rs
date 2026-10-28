//! Authorized host side of FILE v2. Disk work lives in the user's executor
//! (`agent`); the network owner only moves complete frames in and out.
pub(crate) mod agent;
mod filesystem;
pub(crate) mod notices;
mod transfer;

use super::{
    model::FileInfo,
    storage,
    wire::{self, Body, Failure, Frame, code},
};
use crate::i18n::t;
use crate::transport::transfer::{Chunk, Credit, INITIAL_WINDOW, Window};
use anyhow::{Context, Result, ensure};
use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
};
use tokio::sync::{Notify, mpsc};
use tokio_util::sync::CancellationToken;

/// Largest frame the executor accepts or emits.
const MAX_FRAME: usize = farwing_protocol::limits::MAX_PEER_FRAME;

/// What the controller declared about file transfer in its `HELLO`.
#[derive(Clone, Copy, PartialEq, Eq, prost::Message)]
pub(crate) struct Capabilities {
    #[prost(bool, tag = "1")]
    pub files: bool,
}
impl Capabilities {
    /// Capabilities of a peer that declared (or not) the `files` feature.
    pub fn from_feature(files: bool) -> Self {
        Self { files }
    }
}

/// A complete outgoing frame of the executor.
#[derive(Clone, prost::Message)]
pub(crate) struct Packet {
    #[prost(bytes = "vec", tag = "1")]
    pub data: Vec<u8>,
    /// Sent on the bulk channel (`TRANSFER`, `FLOW`) instead of the session channel.
    #[prost(bool, tag = "2")]
    pub bulk: bool,
}
impl Packet {
    fn session(data: Vec<u8>) -> Self {
        Self { data, bulk: false }
    }
    fn bulk(data: Vec<u8>) -> Self {
        Self { data, bulk: true }
    }
}

/// Whether `bytes` is a file-transfer frame (`FILE`, or `TRANSFER`/`FLOW` of
/// a file stream).
pub(crate) fn claims(bytes: &[u8]) -> bool {
    wire::claims(bytes)
}

/// Whether an outgoing packet reports a failure; those still reach a
/// controller whose permission was just revoked.
pub(crate) fn is_failure(packet: &Packet) -> bool {
    use crate::protocol::peer::FrameType;
    let Ok((header, body)) = crate::protocol::peer::decode(&packet.data) else {
        return false;
    };
    if header.kind != FrameType::File as u16 {
        return false;
    }
    use prost::Message as _;
    header.is_error()
        || matches!(
            wire::FileMessage::decode(body).map(|m| m.body),
            Ok(Some(Body::ItemDone(done))) if !done.error.is_empty()
        )
}

/// The failure code of `error` for the controller.
pub(super) fn failure(error: &anyhow::Error) -> Failure {
    let message = format!("{error:#}");
    if let Some(failure) = error.downcast_ref::<Failure>() {
        return failure.clone();
    }
    let code = match error
        .chain()
        .find_map(|e| e.downcast_ref::<std::io::Error>())
    {
        Some(e) => match e.raw_os_error() {
            Some(2 | 3) => code::NOT_FOUND,
            Some(5 | 32 | 33) => code::DENIED,
            Some(39 | 112) => code::NO_SPACE,
            Some(80 | 183) => code::EXISTS,
            _ => match e.kind() {
                std::io::ErrorKind::NotFound => code::NOT_FOUND,
                std::io::ErrorKind::PermissionDenied => code::DENIED,
                std::io::ErrorKind::AlreadyExists => code::EXISTS,
                _ => code::IO,
            },
        },
        None => code::DENIED,
    };
    Failure::new(code, message)
}

/// A refusal for a request frame the executor cannot take (permission
/// gone, no user session, malformed); nothing for other frames.
pub(crate) fn reject(bytes: &[u8], failure: &Failure) -> Vec<Packet> {
    let Ok(Some(Frame::File(header, _))) = wire::classify(bytes) else {
        return Vec::new();
    };
    // A fragmented request is answered once, at its last fragment.
    if header.request_id == 0
        || header.is_response()
        || header.flags & crate::protocol::peer::flags::MORE != 0
    {
        return Vec::new();
    }
    wire::failed(header.request_id, failure)
        .into_iter()
        .map(Packet::session)
        .collect()
}

/// Sends of one transfer task.
#[derive(Clone)]
struct Output {
    output: mpsc::Sender<Packet>,
}
impl Output {
    async fn send(&self, packet: Packet) -> Result<()> {
        ensure!(packet.data.len() <= MAX_FRAME, "{}", t().features.transfer_response_too_large);
        self.output
            .send(packet)
            .await
            .map_err(|_| anyhow::anyhow!("{}", t().features.transfer_connection_ended))
    }
    /// Answer request `request_id`; fragments are never interleaved with a
    /// cancellation.
    async fn respond(&self, request_id: u32, body: Body) -> Result<()> {
        for frame in wire::response(request_id, body) {
            self.send(Packet::session(frame)).await?;
        }
        Ok(())
    }
    async fn fail(&self, request_id: u32, error: &anyhow::Error) -> Result<()> {
        let failure = failure(error);
        for frame in wire::failed(request_id, &failure) {
            self.send(Packet::session(frame)).await?;
        }
        Ok(())
    }
}

/// What the dispatcher hands a running transfer.
enum Event {
    /// `UploadStart` (offsets) or `DownloadStart` (plans) with its request ID.
    Start(u32, Body),
    Chunk(Chunk),
}

/// Shared state of one running transfer.
struct Shared {
    /// Download: what the controller allows us to send.
    credit: Mutex<Credit>,
    /// Upload: what we allowed the controller to send.
    window: Mutex<Window>,
    changed: Notify,
    /// Pause/cancel request (`Cancel`), answered once the task stopped.
    cancel: Mutex<Option<(u32, bool)>>,
    /// Why the dispatcher stopped the transfer (a protocol violation).
    fault: Mutex<Option<String>>,
    stop: CancellationToken,
}

struct Active {
    generation: u64,
    key: String,
    /// The entry being transferred, for a revocation notice.
    entry: Arc<std::sync::atomic::AtomicU32>,
    events: mpsc::UnboundedSender<Event>,
    shared: Arc<Shared>,
}

pub(crate) struct Engine {
    journal: notices::Journal,
    store: Arc<filesystem::Store>,
    output: mpsc::Sender<Packet>,
    transfers: HashMap<u32, Active>,
    work: tokio::task::JoinSet<Option<(u32, u64)>>,
    generation: u64,
    fragments: wire::Reassembly,
    stop: CancellationToken,
}

impl Engine {
    pub fn new(
        scope: &str,
        output: mpsc::Sender<Packet>,
        journal: notices::Journal,
    ) -> Result<Self> {
        Ok(Self::with_store(
            filesystem::Store::new(scope)?,
            output,
            journal,
        ))
    }
    fn with_store(
        store: filesystem::Store,
        output: mpsc::Sender<Packet>,
        journal: notices::Journal,
    ) -> Self {
        Self {
            journal,
            store: Arc::new(store),
            output,
            transfers: HashMap::new(),
            work: tokio::task::JoinSet::new(),
            generation: 0,
            fragments: Default::default(),
            stop: CancellationToken::new(),
        }
    }

    fn reap(&mut self) {
        while let Some(done) = self.work.try_join_next() {
            if let Ok(Some((id, generation))) = done
                && self
                    .transfers
                    .get(&id)
                    .is_some_and(|t| t.generation == generation)
            {
                self.transfers.remove(&id);
            }
        }
        self.transfers.retain(|_, t| !t.events.is_closed());
    }

    /// Handle one frame from the controller. Never waits for disk work.
    pub async fn receive(&mut self, bytes: &[u8]) -> Result<()> {
        self.reap();
        let Some(frame) = wire::classify(bytes)? else {
            return Ok(());
        };
        match frame {
            Frame::Chunk(chunk) => {
                let Some(active) = self.transfers.get(&chunk.stream) else {
                    // In flight after a cancel or a failure; dropped.
                    return Ok(());
                };
                let accounted = {
                    let mut window = super::lock(&active.shared.window);
                    let received = window.received();
                    window.receive(received, chunk.data.len())
                };
                if let Err(error) = accounted {
                    *super::lock(&active.shared.fault) = Some((t().features.transfer_controller_exceeded_window)(&error.to_string()));
                    active.shared.stop.cancel();
                    return Ok(());
                }
                let _ = active.events.send(Event::Chunk(chunk));
            }
            Frame::Flow(flow) => {
                if let Some(active) = self.transfers.get(&flow.stream)
                    && super::lock(&active.shared.credit).grant(flow.credit)
                {
                    active.shared.changed.notify_waiters();
                }
            }
            Frame::File(header, body) => {
                if header.is_response() {
                    return Ok(());
                }
                let Some(received) = self.fragments.push(&header, body)? else {
                    return Ok(());
                };
                ensure!(received.request_id != 0, "{}", t().features.transfer_controller_sent_push);
                let Ok(body) = received.body else {
                    return Ok(());
                };
                self.request(received.request_id, body)?;
            }
        }
        Ok(())
    }

    fn request(&mut self, id: u32, body: Body) -> Result<()> {
        let output = Output {
            output: self.output.clone(),
        };
        match body {
            Body::List(_)
            | Body::MakeDir(_)
            | Body::Rename(_)
            | Body::Remove(_)
            | Body::Exists(_) => {
                ensure!(self.work.len() < 32, "{}", t().features.transfer_too_many_operations);
                let store = self.store.clone();
                let stop = self.stop.child_token();
                self.work.spawn(async move {
                    let name = body.name();
                    let blocking = stop.clone();
                    let result = tokio::task::spawn_blocking(move || {
                        filesystem::operation(body, &store, &blocking)
                    })
                    .await
                    .unwrap_or_else(|e| Err(e.into()));
                    let sent = match result {
                        Ok(reply) => output.respond(id, reply).await,
                        Err(error) => {
                            tracing::warn!(request = name, error = %format!("{error:#}"), "host file operation failed");
                            output.fail(id, &error).await
                        }
                    };
                    if let Err(error) = sent {
                        tracing::debug!(%error, "host file reply not sent");
                    }
                    None
                });
            }
            Body::Upload(upload) => {
                let transfer = upload.transfer;
                let key = upload.key.clone();
                let (task, run) = self.admit(transfer, key)?;
                let store = self.store.clone();
                let journal = self.journal.clone();
                self.work.spawn(async move {
                    transfer::upload(id, upload, run, output, store, journal).await;
                    Some(task)
                });
            }
            Body::Download(download) => {
                let transfer = download.transfer;
                let key = download.key.clone();
                let (task, run) = self.admit(transfer, key)?;
                let journal = self.journal.clone();
                self.work.spawn(async move {
                    transfer::download(id, download, run, output, journal).await;
                    Some(task)
                });
            }
            Body::UploadStart(start) => self.start(start.transfer, id, Body::UploadStart(start))?,
            Body::DownloadStart(start) => {
                self.start(start.transfer, id, Body::DownloadStart(start))?
            }
            Body::Cancel(cancel) => {
                if let Some(active) = self
                    .transfers
                    .get(&cancel.transfer)
                    .filter(|_| cancel.transfer != 0)
                    .or_else(|| {
                        self.transfers
                            .values()
                            .find(|t| !cancel.key.is_empty() && t.key == cancel.key)
                    })
                {
                    // The task answers once it stopped and saved its state;
                    // one that already released its queue left it to us.
                    *super::lock(&active.shared.cancel) = Some((id, cancel.discard));
                    active.shared.stop.cancel();
                    active.shared.changed.notify_waiters();
                    if !active.events.is_closed()
                        || super::lock(&active.shared.cancel).take().is_none()
                    {
                        return Ok(());
                    }
                }
                let store = self.store.clone();
                self.work.spawn(async move {
                    let result = if cancel.discard {
                        tokio::task::spawn_blocking(move || store.clear(&cancel.key))
                            .await
                            .unwrap_or_else(|e| Err(e.into()))
                    } else {
                        Ok(())
                    };
                    let _ = match result {
                        Ok(()) => output.respond(id, Body::Done(wire::Done::default())).await,
                        Err(error) => output.fail(id, &error).await,
                    };
                    None
                });
            }
            other => {
                return Err(Failure::new(
                    code::PROTOCOL,
                    (t().features.transfer_unsupported_request)(other.name()),
                )
                .into());
            }
        }
        Ok(())
    }

    /// Admit a new transfer `id`.
    fn admit(&mut self, id: u32, key: String) -> Result<((u32, u64), transfer::Run)> {
        ensure!(
            self.transfers.len() < 8,
            Failure::new(code::BUSY, t().features.transfer_too_many_transfers)
        );
        ensure!(
            id != 0 && !self.transfers.contains_key(&id),
            Failure::new(code::PROTOCOL, t().features.transfer_duplicate_id)
        );
        ensure!(
            key.is_empty() || !self.transfers.values().any(|t| t.key == key),
            Failure::new(code::BUSY, t().features.transfer_task_in_progress)
        );
        self.generation = self.generation.checked_add(1).context(t().features.transfer_generations_exhausted)?;
        let (events, receiver) = mpsc::unbounded_channel();
        let shared = Arc::new(Shared {
            credit: Mutex::new(Credit::new(INITIAL_WINDOW)),
            window: Mutex::new(Window::new()),
            changed: Notify::new(),
            cancel: Mutex::new(None),
            fault: Mutex::new(None),
            stop: self.stop.child_token(),
        });
        let entry = Arc::new(std::sync::atomic::AtomicU32::new(wire::WHOLE_TRANSFER));
        self.transfers.insert(
            id,
            Active {
                generation: self.generation,
                key,
                entry: entry.clone(),
                events,
                shared: shared.clone(),
            },
        );
        Ok((
            (id, self.generation),
            transfer::Run {
                id,
                events: receiver,
                shared,
                entry,
            },
        ))
    }

    fn start(&mut self, transfer: u32, id: u32, body: Body) -> Result<()> {
        let active = self
            .transfers
            .get(&transfer)
            .ok_or_else(|| Failure::new(code::CANCELLED, t().features.transfer_ended))?;
        active
            .events
            .send(Event::Start(id, body))
            .map_err(|_| Failure::new(code::CANCELLED, t().features.transfer_ended))?;
        Ok(())
    }

    pub async fn close(mut self) {
        self.stop.cancel();
        self.transfers.clear();
        while self.work.join_next().await.is_some() {}
    }

    /// Notices for the controller that every running transfer lost its
    /// permission.
    fn revoked(&self) -> Vec<Packet> {
        self.transfers
            .iter()
            .map(|(&transfer, active)| {
                Packet::session(wire::item_done(wire::ItemDone::new(
                    transfer,
                    active.entry.load(std::sync::atomic::Ordering::Acquire),
                    code::DENIED,
                )))
            })
            .collect()
    }
}

impl Drop for Engine {
    fn drop(&mut self) {
        self.stop.cancel();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transport::transfer::{CHUNK, kind};
    use std::path::{Path, PathBuf};
    use std::time::Duration;

    /// The controller's view of the engine's output.
    enum Out {
        Response(u32, std::result::Result<Body, Failure>),
        Push(Body),
        Chunk(Chunk),
        Flow(u64),
    }

    struct Controller {
        engine: Engine,
        output: mpsc::Receiver<Packet>,
        fragments: wire::Reassembly,
        requests: u32,
    }

    impl Controller {
        async fn send(&mut self, frame: &[u8]) {
            self.engine.receive(frame).await.unwrap();
        }
        async fn request(&mut self, body: Body) -> u32 {
            self.requests += 1;
            for frame in wire::request(self.requests, body) {
                self.send(&frame).await;
            }
            self.requests
        }
        async fn next(&mut self) -> Out {
            loop {
                let packet = tokio::time::timeout(Duration::from_secs(10), self.output.recv())
                    .await
                    .expect("engine output")
                    .expect("engine open");
                match wire::classify(&packet.data).unwrap().expect("file frame") {
                    Frame::Chunk(chunk) => {
                        assert!(packet.bulk);
                        return Out::Chunk(chunk);
                    }
                    Frame::Flow(flow) => {
                        assert!(packet.bulk);
                        return Out::Flow(flow.credit);
                    }
                    Frame::File(header, body) => {
                        assert!(!packet.bulk);
                        if let Some(m) = self.fragments.push(&header, body).unwrap() {
                            return if m.response {
                                Out::Response(m.request_id, m.body)
                            } else {
                                Out::Push(m.body.expect("a push"))
                            };
                        }
                    }
                }
            }
        }
        async fn answer(&mut self, id: u32) -> Body {
            self.result(id).await.expect("an answer, not a refusal")
        }
        async fn result(&mut self, id: u32) -> std::result::Result<Body, Failure> {
            match self.next().await {
                Out::Response(rid, body) if rid == id => body,
                _ => panic!("expected the response to {id}"),
            }
        }
    }

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("farwing-engine-{name}-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        dunce::canonicalize(dir).unwrap()
    }

    fn pattern(len: usize, seed: u8) -> Vec<u8> {
        (0..len).map(|i| (i as u8).wrapping_mul(31).wrapping_add(seed)).collect()
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn upload_then_download_within_credit() {
        let base = scratch("round");
        let source = base.join("src/tree");
        std::fs::create_dir_all(source.join("sub/empty")).unwrap();
        std::fs::write(source.join("sub/small.txt"), b"hello").unwrap();
        std::fs::write(source.join("zero.bin"), b"").unwrap();
        let big = pattern(5 * 1024 * 1024 + 123, 7);
        std::fs::write(source.join("big.bin"), &big).unwrap();
        let destination = base.join("dst");
        std::fs::create_dir_all(&destination).unwrap();
        let (tx, output) = mpsc::channel(64);
        let engine = Engine::with_store(
            filesystem::Store::at(base.join("records")),
            tx,
            notices::Journal::default(),
        );
        let mut c = Controller {
            engine,
            output,
            fragments: Default::default(),
            requests: 0,
        };

        // ----------------------------------------------------- upload
        let (_, folder, items) =
            storage::scan(&source, &CancellationToken::new()).unwrap();
        let key = uuid::Uuid::new_v4().to_string();
        let id = c
            .request(Body::Upload(wire::Upload {
                transfer: 1,
                dir: destination.to_string_lossy().into_owned(),
                folder: folder.clone(),
                items: items.iter().map(wire::Item::from).collect(),
                conflict: wire::conflict::RENAME,
                key: key.clone(),
            }))
            .await;
        let Body::Plan(plan) = c.answer(id).await else {
            panic!("plan expected")
        };
        assert_eq!(plan.items.len(), items.len());
        assert!(plan.items.iter().all(|p| p.offset == 0 && !p.skip));
        let id = c
            .request(Body::UploadStart(wire::UploadStart {
                transfer: 1,
                offsets: vec![0; items.len()],
            }))
            .await;
        assert!(matches!(c.answer(id).await, Body::Done(_)));
        let mut credit = Credit::new(INITIAL_WINDOW);
        let mut saved = 0;
        for (entry, item) in items.iter().enumerate() {
            let data = if item.directory {
                Vec::new()
            } else {
                std::fs::read(source.join(&item.rel_path)).unwrap()
            };
            let mut position = 0;
            loop {
                while credit.available() == 0 {
                    match c.next().await {
                        Out::Flow(granted) => {
                            credit.grant(granted);
                        }
                        Out::Push(Body::ItemDone(done)) => {
                            assert_eq!(done.error, "");
                            saved += 1;
                        }
                        _ => panic!("unexpected output while waiting for credit"),
                    }
                }
                let len = (data.len() - position)
                    .min(CHUNK)
                    .min(credit.available() as usize);
                credit.spend(len).unwrap();
                let last = position + len == data.len();
                c.send(&wire::chunk(&Chunk {
                    stream: 1,
                    kind: kind::FILE,
                    offset: position as u64,
                    data: bytes::Bytes::copy_from_slice(&data[position..position + len]),
                    last,
                    entry: entry as u32,
                }))
                .await;
                position += len;
                if last {
                    break;
                }
            }
        }
        assert!(credit.sent() > INITIAL_WINDOW, "the host granted more credit");
        while saved < items.len() {
            match c.next().await {
                Out::Push(Body::ItemDone(done)) => {
                    assert_eq!(done.error, "");
                    saved += 1;
                }
                Out::Flow(_) => {}
                _ => panic!("unexpected output after upload"),
            }
        }
        let received = destination.join(&folder);
        assert_eq!(std::fs::read(received.join("big.bin")).unwrap(), big);
        assert_eq!(std::fs::read(received.join("sub/small.txt")).unwrap(), b"hello");
        assert_eq!(std::fs::metadata(received.join("zero.bin")).unwrap().len(), 0);
        assert!(received.join("sub/empty").is_dir());
        let mtime = |p: &Path| storage::modified_ns(&std::fs::metadata(p).unwrap());
        assert_eq!(mtime(&received.join("big.bin")), mtime(&source.join("big.bin")));
        // A completed upload leaves no partial files behind.
        let leftovers = std::fs::read_dir(&received)
            .unwrap()
            .filter(|e| {
                e.as_ref()
                    .unwrap()
                    .file_name()
                    .to_string_lossy()
                    .ends_with(".downloading")
            })
            .count();
        assert_eq!(leftovers, 0);

        // ----------------------------------------------------- download
        let id = c
            .request(Body::Download(wire::Download {
                transfer: 2,
                paths: vec![received.to_string_lossy().into_owned()],
                key: uuid::Uuid::new_v4().to_string(),
            }))
            .await;
        let Body::Manifest(manifest) = c.answer(id).await else {
            panic!("manifest expected")
        };
        assert_eq!(manifest.folder, "tree");
        assert_eq!(manifest.items.len(), items.len());
        let id = c
            .request(Body::DownloadStart(wire::DownloadStart {
                transfer: 2,
                items: vec![Default::default(); manifest.items.len()],
            }))
            .await;
        let Body::Accepted(accepted) = c.answer(id).await else {
            panic!("accepted expected")
        };
        assert!(accepted.offsets.iter().all(|o| *o == 0));
        let mut files: Vec<Vec<u8>> = vec![Vec::new(); manifest.items.len()];
        let mut window = Window::new();
        let mut done = 0;
        // Deliver nothing until the host stalls at its initial credit.
        let mut withheld = true;
        while done < manifest.items.len() {
            let chunk = match tokio::time::timeout(Duration::from_millis(500), c.next()).await {
                Ok(Out::Chunk(chunk)) => chunk,
                Ok(_) => panic!("unexpected output during download"),
                Err(_) => {
                    // The host stopped at its credit; deliver what arrived.
                    assert!(withheld && window.received() == INITIAL_WINDOW);
                    withheld = false;
                    let credit = window.deliver(INITIAL_WINDOW as usize).expect("a grant");
                    c.send(&wire::flow(2, credit)).await;
                    continue;
                }
            };
            let received = window.received();
            window.receive(received, chunk.data.len()).expect("within credit");
            if !withheld && let Some(credit) = window.deliver(chunk.data.len()) {
                c.send(&wire::flow(2, credit)).await;
            }
            let entry = chunk.entry as usize;
            assert_eq!(chunk.offset as usize, files[entry].len());
            files[entry].extend_from_slice(&chunk.data);
            if chunk.last {
                done += 1;
            }
        }
        assert!(!withheld, "the host waited for FLOW");
        let big_entry = manifest.items.iter().position(|i| i.path == "big.bin").unwrap();
        assert_eq!(files[big_entry], big);
        let empty = manifest.items.iter().position(|i| i.path == "sub/empty").unwrap();
        assert!(manifest.items[empty].dir && files[empty].is_empty());

        // ----------------------------------------- management and errors
        let id = c
            .request(Body::MakeDir(wire::MakeDir {
                parent: received.to_string_lossy().into_owned(),
                name: "sub".into(),
            }))
            .await;
        assert!(matches!(c.result(id).await, Err(f) if f.code == wire::code::EXISTS));
        let id = c
            .request(Body::List(wire::List {
                path: received.to_string_lossy().into_owned(),
            }))
            .await;
        let Body::Listing(listing) = c.answer(id).await else {
            panic!("listing expected")
        };
        assert_eq!(listing.entries.len(), 3);
        assert!(listing.entries.iter().any(|e| e.name == "sub" && e.kind == wire::kind::DIR));
        // A start for a transfer that is not running fails that request.
        let frame = wire::request(
            99,
            Body::UploadStart(wire::UploadStart {
                transfer: 9,
                offsets: vec![],
            }),
        )
        .remove(0);
        let failed = failure(&c.engine.receive(&frame).await.unwrap_err());
        assert_eq!(failed.code, wire::code::CANCELLED);
        assert_eq!(reject(&frame, &failed).len(), 1);
        assert!(c.engine.receive(b"not a frame").await.is_ok());
        c.engine.close().await;
        let _ = std::fs::remove_dir_all(base);
    }
}
