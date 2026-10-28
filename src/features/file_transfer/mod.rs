//! File management and transfer, FILE v2 (`docs/protocol/v2-design.md` §8.1):
//! typed requests on the session channel, file data as `TRANSFER`/`FLOW` on
//! the bulk channel.
pub(crate) mod host;
pub(crate) mod model;
mod storage;
pub(crate) use storage::Store;
pub(crate) mod service;
mod tasks;
pub(crate) mod ui;
pub(crate) mod wire;

use anyhow::{Context, Result, ensure};
use bytes::Bytes;
use model::FileEntry;
use std::{
    collections::HashMap,
    sync::{
        Arc, Mutex, Weak,
        atomic::{AtomicBool, AtomicI32, AtomicU32, Ordering},
    },
    time::Duration,
};
use tokio::sync::{Notify, mpsc, oneshot};
use tokio_util::sync::CancellationToken;
use webrtc::data_channel::{RTCDataChannel, data_channel_state::RTCDataChannelState};

use crate::i18n::t;
use crate::transport::transfer::{Chunk, Credit, INITIAL_WINDOW, Window};
use wire::{Body, Frame};

pub(crate) fn lock<T>(v: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    v.lock().unwrap_or_else(|e| e.into_inner())
}
/// How long a request waits for its response.
pub(crate) const TIMEOUT: Duration = Duration::from_secs(60);
/// Queue limit of the session channel for (possibly fragmented) requests.
const SESSION_QUEUE: usize = 4 * 1024 * 1024;

/// What arrives for a running transfer.
pub(crate) enum Inbound {
    Chunk(Chunk),
    ItemDone(wire::ItemDone),
}

/// One transfer stream of this connection.
struct Stream {
    inbound: mpsc::UnboundedSender<Inbound>,
    /// Upload: what the host allows us to send.
    credit: Mutex<Credit>,
    /// Download: what we allowed the host to send.
    window: Mutex<Window>,
    changed: Notify,
    stop: CancellationToken,
    fault: Mutex<Option<String>>,
}

impl Stream {
    fn fail(&self, message: impl Into<String>) {
        lock(&self.fault).get_or_insert_with(|| message.into());
        self.stop.cancel();
        self.changed.notify_waiters();
    }
}

/// A registered transfer; unregisters on drop.
pub(crate) struct Registration {
    pub id: u32,
    stream: Arc<Stream>,
    transport: Arc<Transport>,
    pub inbound: mpsc::UnboundedReceiver<Inbound>,
}

impl Drop for Registration {
    fn drop(&mut self) {
        lock(&self.transport.streams).remove(&self.id);
    }
}

impl Registration {
    /// Why the transfer stopped: a connection or protocol fault, else a pause.
    pub(crate) fn failure(&self) -> String {
        lock(&self.stream.fault)
            .clone()
            .unwrap_or_else(|| t().features.transfer_task_paused.into())
    }
    pub(crate) fn stopped(&self) -> bool {
        self.stream.stop.is_cancelled()
    }
    pub(crate) async fn cancelled(&self) {
        self.stream.stop.cancelled().await
    }
}

#[derive(Default)]
pub(crate) struct Transport {
    session: Mutex<Weak<RTCDataChannel>>,
    bulk: Mutex<Weak<RTCDataChannel>>,
    pending: Mutex<HashMap<u32, oneshot::Sender<std::result::Result<Body, wire::Failure>>>>,
    streams: Mutex<HashMap<u32, Arc<Stream>>>,
    fragments: Mutex<wire::Reassembly>,
    /// The host declared the `files` feature.
    supported: AtomicBool,
    /// 0 unknown, 1 revoked, 2 granted (`PERMISSIONS`).
    permission: AtomicI32,
    changed: Notify,
    requests: AtomicU32,
    transfers: AtomicU32,
    session_lock: tokio::sync::Mutex<()>,
}

/// Removes an unanswered request when its waiter goes away.
struct Waiting<'a>(&'a Transport, u32);
impl Drop for Waiting<'_> {
    fn drop(&mut self) {
        lock(&self.0.pending).remove(&self.1);
    }
}

impl Transport {
    pub(crate) fn allowed(&self) -> bool {
        self.permission.load(Ordering::Acquire) != 1
    }
    /// Whether this participant holds the `files` capability (`PERMISSIONS`).
    /// Losing it aborts every running transfer.
    pub(crate) fn permit(&self, allowed: bool) {
        self.permission
            .store(if allowed { 2 } else { 1 }, Ordering::Release);
        if !allowed {
            for stream in lock(&self.streams).values() {
                stream.fail(t().features.transfer_permission_revoked);
            }
        }
        self.wake();
    }
    pub(crate) fn bind(&self, c: &Arc<RTCDataChannel>) {
        match c.label() {
            crate::protocol::peer::channel::SESSION => *lock(&self.session) = Arc::downgrade(c),
            crate::protocol::peer::channel::BULK => *lock(&self.bulk) = Arc::downgrade(c),
            _ => {}
        }
    }
    pub(crate) fn capabilities(&self, files: bool) {
        self.supported.store(files, Ordering::Release);
        self.changed.notify_waiters();
    }
    pub(crate) fn supported(&self) -> bool {
        self.supported.load(Ordering::Acquire)
    }
    /// Something a sender may wait for changed (queue drained, credit).
    pub(crate) fn wake(&self) {
        self.changed.notify_waiters();
        for stream in lock(&self.streams).values() {
            stream.changed.notify_waiters();
        }
    }
    pub(crate) fn close(&self) {
        self.permission.store(0, Ordering::Release);
        self.supported.store(false, Ordering::Release);
        // Dropping the senders fails every waiting request.
        lock(&self.pending).clear();
        *lock(&self.fragments) = Default::default();
        for (_, stream) in lock(&self.streams).drain() {
            stream.fail(t().features.transfer_disconnected_paused);
        }
        self.wake();
    }
    fn ready(&self) -> Result<()> {
        ensure!(self.supported(), "{}", t().features.transfer_host_not_ready);
        ensure!(self.allowed(), "{}", t().features.transfer_permission_off);
        Ok(())
    }

    /// Send `body` and wait for its response; a refusal is an error
    /// ([`wire::Failure`]).
    pub(crate) async fn request(&self, body: Body, stop: &CancellationToken) -> Result<Body> {
        self.ready()?;
        let name = body.name();
        let id = loop {
            let id = self.requests.fetch_add(1, Ordering::Relaxed).wrapping_add(1);
            if id != 0 && !lock(&self.pending).contains_key(&id) {
                break id;
            }
        };
        let (tx, rx) = oneshot::channel();
        lock(&self.pending).insert(id, tx);
        let _waiting = Waiting(self, id);
        tokio::select! {
            biased;
            _ = stop.cancelled() => anyhow::bail!("{}", t().features.transfer_task_paused),
            sent = self.send_session(wire::request(id, body)) => sent?,
        }
        let reply = tokio::select! {
            biased;
            _ = stop.cancelled() => anyhow::bail!("{}", t().features.transfer_task_paused),
            reply = tokio::time::timeout(TIMEOUT, rx) => reply
                .with_context(|| (t().features.transfer_request_timed_out)(name))?
                .context(t().features.transfer_connection_ended)?,
        };
        Ok(reply?)
    }

    async fn send_session(&self, frames: Vec<Vec<u8>>) -> Result<()> {
        let channel = lock(&self.session).upgrade().context(t().features.transfer_channel_not_connected)?;
        // Fragments of one message go out back to back.
        let _gate = tokio::time::timeout(Duration::from_secs(30), self.session_lock.lock())
            .await
            .context(t().features.transfer_send_queue_timeout)?;
        for frame in frames {
            let frame = Bytes::from(frame);
            tokio::time::timeout(Duration::from_secs(30), async {
                // The session writer owns this channel's low-water callback; poll.
                while channel.buffered_amount().await + frame.len() > SESSION_QUEUE {
                    ensure!(
                        channel.ready_state() == RTCDataChannelState::Open,
                        "{}", t().features.transfer_channel_closed
                    );
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
                ensure!(
                    channel.ready_state() == RTCDataChannelState::Open,
                    "{}", t().features.transfer_channel_closed
                );
                channel.send(&frame).await?;
                Ok::<_, anyhow::Error>(())
            })
            .await
            .context(t().features.transfer_send_timeout)??;
        }
        Ok(())
    }

    /// Send one frame on the bulk channel through its shared writer gate
    /// (at most [`crate::transport::bulk::QUEUE`] bytes queued in SCTP).
    async fn send_bulk(&self, frame: Vec<u8>) -> Result<()> {
        let channel = lock(&self.bulk).upgrade().context(t().features.transfer_data_channel_not_connected)?;
        crate::transport::bulk::send(&channel, Bytes::from(frame), || self.supported())
            .await
            .context(t().features.transfer_data_send_failed)
    }

    /// Register a new transfer stream of this connection.
    pub(crate) fn register(self: &Arc<Self>, stop: CancellationToken) -> Result<Registration> {
        self.ready()?;
        let (inbound, receiver) = mpsc::unbounded_channel();
        let stream = Arc::new(Stream {
            inbound,
            credit: Mutex::new(Credit::new(INITIAL_WINDOW)),
            window: Mutex::new(Window::new()),
            changed: Notify::new(),
            stop,
            fault: Mutex::new(None),
        });
        let mut streams = lock(&self.streams);
        ensure!(streams.len() < 64, "{}", t().features.transfer_too_many_tasks);
        let id = loop {
            let id = self.transfers.fetch_add(1, Ordering::Relaxed).wrapping_add(1);
            if id != 0 && !streams.contains_key(&id) {
                break id;
            }
        };
        streams.insert(id, stream.clone());
        Ok(Registration {
            id,
            stream,
            transport: Arc::clone(self),
            inbound: receiver,
        })
    }

    /// Wait until the upload may send at least one byte; returns the credit.
    pub(crate) async fn credit(&self, transfer: &Registration) -> Result<u64> {
        let stream = &transfer.stream;
        loop {
            let changed = stream.changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            ensure!(!stream.stop.is_cancelled(), "{}", transfer.failure());
            let available = lock(&stream.credit).available();
            if available > 0 {
                return Ok(available);
            }
            tokio::select! {
                _ = stream.stop.cancelled() => {},
                _ = changed => {},
                // A FLOW never got lost on a reliable channel; this only
                // bounds a host that stopped granting.
                _ = tokio::time::sleep(TIMEOUT) => anyhow::bail!("{}", t().features.transfer_host_not_receiving),
            }
        }
    }

    /// Send one chunk of an upload within its credit.
    pub(crate) async fn send_chunk(&self, transfer: &Registration, chunk: Chunk) -> Result<()> {
        ensure!(!transfer.stopped(), "{}", transfer.failure());
        lock(&transfer.stream.credit).spend(chunk.data.len())?;
        tokio::select! {
            biased;
            _ = transfer.cancelled() => anyhow::bail!(transfer.failure()),
            sent = self.send_bulk(wire::chunk(&chunk)) => sent,
        }
    }

    /// `len` downloaded bytes were written; grant the host more credit.
    pub(crate) async fn delivered(&self, transfer: &Registration, len: usize) -> Result<()> {
        let credit = lock(&transfer.stream.window).deliver(len);
        if let Some(credit) = credit {
            self.send_bulk(wire::flow(transfer.id, credit)).await?;
        }
        Ok(())
    }

    /// Handle a `FILE`, `TRANSFER` or `FLOW` frame; `false` for other frames.
    /// Never waits for the transfer that owns the frame.
    pub(crate) async fn receive(&self, bytes: &Bytes) -> Result<bool> {
        let Some(frame) = wire::classify(bytes)? else {
            return Ok(false);
        };
        match frame {
            Frame::Chunk(chunk) => {
                let stream = lock(&self.streams).get(&chunk.stream).cloned();
                let Some(stream) = stream else {
                    // Chunks still in flight after a pause or cancel.
                    return Ok(true);
                };
                let accounted = {
                    let mut window = lock(&stream.window);
                    let received = window.received();
                    window.receive(received, chunk.data.len())
                };
                match accounted {
                    Ok(()) => {
                        let _ = stream.inbound.send(Inbound::Chunk(chunk));
                    }
                    Err(error) => stream.fail((t().features.transfer_host_flow_violation)(&error.to_string())),
                }
            }
            Frame::Flow(flow) => {
                if let Some(stream) = lock(&self.streams).get(&flow.stream)
                    && lock(&stream.credit).grant(flow.credit)
                {
                    stream.changed.notify_waiters();
                }
            }
            Frame::File(header, body) => {
                let received = lock(&self.fragments).push(&header, body)?;
                let Some(received) = received else {
                    return Ok(true);
                };
                if received.response {
                    match lock(&self.pending).remove(&received.request_id) {
                        Some(waiter) => {
                            let _ = waiter.send(received.body);
                        }
                        None => tracing::debug!(
                            request_id = received.request_id,
                            body = received.body.as_ref().map_or("refusal", Body::name),
                            "file response has no waiting request"
                        ),
                    }
                } else if let Ok(Body::ItemDone(done)) = received.body {
                    if let Some(stream) = lock(&self.streams).get(&done.transfer) {
                        let _ = stream.inbound.send(Inbound::ItemDone(done));
                    }
                } else {
                    tracing::debug!(
                        body = received.body.as_ref().map_or("refusal", Body::name),
                        "unexpected file request ignored"
                    );
                }
            }
        }
        Ok(true)
    }
}
