//! Controller side of DIAGNOSTICS v2: one bundle download at a time, written
//! to a partial file, checked against the announced size and SHA-256 and
//! published by rename.
use super::*;
use crate::transport::transfer::{self, Chunk, Flow, Window};
use anyhow::{Result, ensure};
use bytes::Bytes;
use sha2::{Digest, Sha256};
use std::path::PathBuf;
use std::sync::{Arc, Weak};
use tokio::io::AsyncWriteExt;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use webrtc::data_channel::RTCDataChannel;
use webrtc::data_channel::data_channel_state::RTCDataChannelState;

#[derive(Clone, Copy, Default)]
pub(crate) enum Phase {
    #[default]
    Connecting,
    Preparing,
    Downloading,
    Saving,
}
#[derive(Clone, Default)]
pub(crate) struct Snapshot {
    pub phase: Phase,
    pub supported: bool,
    pub allowed: bool,
    pub busy: bool,
    pub cancelled: bool,
    pub progress: bundle::Progress,
    pub path: Option<PathBuf>,
    pub error: Option<String>,
    pub warnings: usize,
}
#[derive(Default)]
struct State {
    generation: u64,
    view: Snapshot,
    session: Weak<RTCDataChannel>,
    bulk: Weak<RTCDataChannel>,
    /// Messages and chunks of the running download.
    inbox: Option<(u32, mpsc::UnboundedSender<Incoming>)>,
    requests: u32,
    stop: CancellationToken,
    job: CancellationToken,
}
enum Incoming {
    Message(Received),
    Chunk(Chunk),
}
#[derive(Clone, Default)]
pub(crate) struct Controller(Arc<Mutex<State>>);
impl Controller {
    pub fn snapshot(&self) -> Snapshot {
        lock(&self.0).view.clone()
    }
    pub fn is_closed(&self) -> bool {
        lock(&self.0).stop.is_cancelled()
    }
    pub fn start(&self) -> Result<()> {
        let mut s = lock(&self.0);
        ensure!(
            s.view.supported && s.view.allowed,
            "{}",
            crate::i18n::t().session.rdiag_unavailable
        );
        ensure!(!s.view.busy, "{}", crate::i18n::t().session.rdiag_busy);
        let session = s.session.upgrade().ok_or_else(|| anyhow::anyhow!("{}", crate::i18n::t().session.rdiag_channel_closed))?;
        let bulk = s.bulk.upgrade().ok_or_else(|| anyhow::anyhow!("{}", crate::i18n::t().session.rdiag_channel_closed))?;
        s.requests = s.requests.wrapping_add(1).max(1);
        let id = s.requests;
        // Unique per connection; the host sends the bundle on this stream.
        let stream = id;
        let (tx, rx) = mpsc::unbounded_channel();
        s.inbox = Some((stream, tx));
        s.job = CancellationToken::new();
        s.view.busy = true;
        s.view.cancelled = false;
        s.view.phase = Phase::Preparing;
        s.view.path = None;
        s.view.error = None;
        s.view.warnings = 0;
        s.view.progress = bundle::Progress {
            stage: crate::i18n::t().session.rdiag_waiting_remote.into(),
            step: bundle::Step::Other,
            completed: 0,
            total: 0,
        };
        let generation = s.generation;
        let (stop, job) = (s.stop.clone(), s.job.clone());
        drop(s);
        let owner = self.clone();
        tokio::spawn(async move {
            let result = tokio::select! {
                _ = stop.cancelled() => Err(anyhow::anyhow!("{}", crate::i18n::t().session.rdiag_connection_closed)),
                _ = job.cancelled() => Err(anyhow::anyhow!("{}", crate::i18n::t().session.rdiag_cancelled)),
                result = download(&owner, generation, &session, &bulk, id, stream, rx, &job) => result,
            };
            if result.is_err() && !stop.is_cancelled() && session.ready_state() == RTCDataChannelState::Open {
                let _ = session
                    .send(&Bytes::from(frame(0, 0, Body::Cancel(Cancel { stream }))))
                    .await;
            }
            owner.update(generation, |v| {
                v.busy = false;
                match result {
                    Ok((path, warnings)) => {
                        v.path = Some(path);
                        v.warnings = warnings;
                        v.progress = bundle::Progress {
                            stage: crate::i18n::t().session.rdiag_saved.into(),
                            step: bundle::Step::Other,
                            completed: 1,
                            total: 1,
                        }
                    }
                    Err(e) => {
                        tracing::warn!(error = %format!("{e:#}"), "remote diagnostics failed");
                        v.error = Some(e.to_string());
                    }
                }
            });
            let mut s = lock(&owner.0);
            if s.generation == generation && s.inbox.as_ref().is_some_and(|(id, _)| *id == stream) {
                s.inbox = None;
            }
        });
        Ok(())
    }
    pub fn cancel(&self) {
        lock(&self.0).job.cancel();
    }
    pub fn close(&self) {
        let mut s = lock(&self.0);
        s.stop.cancel();
        s.inbox = None;
        s.view.supported = false;
        s.view.allowed = false;
        if s.view.busy {
            s.view.error = Some(crate::i18n::t().session.rdiag_closed_incomplete.into());
            s.view.busy = false;
        }
    }
    fn update(&self, generation: u64, f: impl FnOnce(&mut Snapshot)) {
        let mut s = lock(&self.0);
        if s.generation == generation {
            f(&mut s.view)
        }
    }
    /// Bind the session or bulk channel of a (new) connection.
    pub fn bind(&self, channel: &Arc<RTCDataChannel>) {
        let mut s = lock(&self.0);
        match channel.label() {
            crate::protocol::peer::channel::SESSION => {
                s.stop.cancel();
                s.generation = s.generation.wrapping_add(1);
                s.view = Snapshot::default();
                s.inbox = None;
                s.stop = CancellationToken::new();
                s.session = Arc::downgrade(channel);
            }
            crate::protocol::peer::channel::BULK => s.bulk = Arc::downgrade(channel),
            _ => {}
        }
    }
    /// A `DIAGNOSTICS` frame or a `TRANSFER` of the diagnostics stream;
    /// `false` for other frames.
    pub fn receive(&self, bytes: &[u8]) -> Result<bool> {
        if let Some(body) = peer::body_of(FrameType::Transfer, bytes) {
            let chunk = Chunk::decode(body)?;
            if chunk.kind != transfer::kind::DIAGNOSTICS {
                return Ok(false);
            }
            let s = lock(&self.0);
            if let Some((stream, inbox)) = &s.inbox
                && *stream == chunk.stream
            {
                let _ = inbox.send(Incoming::Chunk(chunk));
            }
            return Ok(true);
        }
        let Some(message) = decode(bytes)? else {
            return Ok(false);
        };
        let mut s = lock(&self.0);
        if let Received::Push(Body::Status(status)) = &message {
            s.view.supported = true;
            s.view.allowed = status.allowed;
            return Ok(true);
        }
        if let Some((_, inbox)) = &s.inbox {
            let _ = inbox.send(Incoming::Message(message));
        }
        Ok(true)
    }
}
struct Partial(PathBuf);
impl Drop for Partial {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}
#[allow(clippy::too_many_arguments)]
async fn download(
    owner: &Controller,
    generation: u64,
    session: &RTCDataChannel,
    bulk: &Arc<RTCDataChannel>,
    id: u32,
    stream: u32,
    mut inbox: mpsc::UnboundedReceiver<Incoming>,
    job: &CancellationToken,
) -> Result<(PathBuf, usize)> {
    let directory = bundle::output_directory()?;
    let path = directory.join(format!(
        "Farwing-remote-diagnostics-{}-{}.zip",
        chrono::Utc::now().format("%Y%m%dT%H%M%SZ"),
        uuid::Uuid::new_v4().simple()
    ));
    let temporary = path.with_extension("zip.partial");
    let (partial, file) = tokio::task::spawn_blocking(move || -> Result<_> {
        std::fs::create_dir_all(&directory)?;
        let file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)?;
        // If the awaiting task is cancelled, the returned guard still cleans up.
        Ok((Partial(temporary), file))
    })
    .await??;
    let mut file = tokio::fs::File::from_std(file);
    // Chunks may overtake `Ready` (different channels): accept them within
    // the credit before the size is known.
    session
        .send(&Bytes::from(frame(0, id, Body::Request(Request { stream }))))
        .await?;
    let mut window = Window::new();
    let mut hash = Sha256::new();
    let mut ready: Option<Ready> = None;
    let mut last = false;
    while !(last && ready.is_some()) {
        let incoming = tokio::time::timeout(TIMEOUT, inbox.recv())
            .await
            .map_err(|_| anyhow::anyhow!("{}", crate::i18n::t().session.rdiag_no_response))?
            .ok_or_else(|| anyhow::anyhow!("{}", crate::i18n::t().session.rdiag_connection_closed))?;
        match incoming {
            Incoming::Message(Received::Push(Body::Progress(progress))) if progress.stream == stream => {
                if ready.is_none() {
                    owner.update(generation, |v| {
                        v.phase = Phase::Preparing;
                        v.progress = bundle::Progress {
                            stage: bundle::Step::text_of(&progress.stage, progress.index, progress.count),
                            step: bundle::Step::Other,
                            completed: progress.completed,
                            total: progress.total,
                        };
                    });
                }
            }
            // The host's words stay in its log; these are ours.
            Incoming::Message(Received::Push(Body::Failed(failed))) if failed.stream == stream => {
                anyhow::bail!("{}", crate::i18n::t().session.diag_transfer_stopped)
            }
            Incoming::Message(Received::Failure(other, code)) if other == id => {
                use farwing_protocol::peer::error_code;
                let text = &crate::i18n::t().session;
                anyhow::bail!(
                    "{}",
                    match code.as_str() {
                        error_code::UNSUPPORTED => text.diag_unsupported,
                        error_code::FORBIDDEN => text.diag_forbidden,
                        error_code::BUSY => text.diag_busy,
                        _ => text.rdiag_remote_incomplete,
                    }
                )
            }
            Incoming::Message(Received::Response(other, Body::Ready(value))) if other == id => {
                ensure!(
                    value.size > 0
                        && value.size <= bundle::ARCHIVE_LIMIT
                        && value.sha256.len() == 64
                        && value.sha256.bytes().all(|b| b.is_ascii_hexdigit())
                        && window.received() <= value.size,
                    "{}",
                    crate::i18n::t().session.rdiag_ready_invalid
                );
                owner.update(generation, |v| {
                    v.phase = Phase::Downloading;
                    v.warnings = value.warnings as usize;
                    v.progress = bundle::Progress {
                        stage: crate::i18n::t().session.rdiag_downloading.into(),
                        step: bundle::Step::Other,
                        completed: window.received(),
                        total: value.size,
                    }
                });
                ready = Some(value);
            }
            Incoming::Message(_) => {}
            Incoming::Chunk(chunk) => {
                ensure!(!last && chunk.entry == 0, "{}", crate::i18n::t().session.rdiag_stream_ended);
                window.receive(chunk.offset, chunk.data.len())?;
                ensure!(
                    window.received() <= ready.as_ref().map_or(bundle::ARCHIVE_LIMIT, |r| r.size),
                    "{}",
                    crate::i18n::t().session.rdiag_too_long
                );
                file.write_all(&chunk.data).await?;
                hash.update(&chunk.data);
                last = chunk.last;
                let received = window.received();
                owner.update(generation, |v| v.progress.completed = received);
                if let Some(credit) = window.deliver(chunk.data.len())
                    && !last
                {
                    let flow = Flow {
                        channel: transfer::channel::BULK,
                        stream,
                        credit,
                        kind: transfer::kind::DIAGNOSTICS,
                    };
                    crate::transport::bulk::send(
                        bulk,
                        Bytes::from(peer::message(FrameType::Flow, &flow)),
                        || !job.is_cancelled(),
                    )
                    .await?;
                }
            }
        }
    }
    let ready = ready.expect("loop ends with Ready");
    ensure!(window.received() == ready.size, "{}", crate::i18n::t().session.rdiag_size_mismatch);
    ensure!(format!("{:x}", hash.finalize()) == ready.sha256, "{}", crate::i18n::t().session.rdiag_checksum_failed);
    owner.update(generation, |v| {
        v.phase = Phase::Saving;
        v.progress = bundle::Progress {
            stage: crate::i18n::t().session.rdiag_saving.into(),
            step: bundle::Step::Other,
            completed: 0,
            total: 0,
        }
    });
    file.flush().await?;
    file.sync_all().await?;
    drop(file);
    ensure!(!job.is_cancelled(), "{}", crate::i18n::t().session.rdiag_fetch_cancelled);
    // No suspension at publication: cancellation cannot leave a renamed
    // result whose completion was discarded by the caller.
    std::fs::rename(&partial.0, &path)?;
    Ok((path, ready.warnings as usize))
}
