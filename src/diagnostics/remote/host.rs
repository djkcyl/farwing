//! Host side of DIAGNOSTICS v2 for one peer connection: answers `Request`
//! with this host's redacted bundle, streamed as `TRANSFER{kind: DIAGNOSTICS}`
//! within the controller's credit.
use super::*;
use crate::features::host::Lease;
use crate::protocol::peer::flags;
use crate::transport::transfer::{self, Chunk, Credit, Flow};
use anyhow::{ensure, Result};
use bytes::Bytes;
use sha2::{Digest, Sha256};
use std::sync::{
    Arc, Weak,
    atomic::{AtomicBool, Ordering},
};
use tokio::io::{AsyncReadExt, AsyncSeekExt};
use tokio::sync::{Notify, mpsc};
use tokio_util::sync::CancellationToken;
use webrtc::data_channel::RTCDataChannel;
use webrtc::data_channel::data_channel_state::RTCDataChannelState;

#[derive(Clone)]
pub(crate) struct Host(Arc<Shared>);
struct Shared {
    session: Mutex<Weak<RTCDataChannel>>,
    bulk: Mutex<Weak<RTCDataChannel>>,
    input: mpsc::Sender<Received>,
    /// Credit of the stream being sent.
    credit: Mutex<Option<(u32, Credit)>>,
    changed: Notify,
}

impl Host {
    /// Serve this connection until `stop`. `enabled`: the controller
    /// declared the `diagnostics` feature.
    pub(crate) fn start(
        enabled: bool,
        lease: Lease,
        connected: Arc<AtomicBool>,
        stop: CancellationToken,
    ) -> Self {
        let (input, requests) = mpsc::channel(16);
        let host = Self(Arc::new(Shared {
            session: Mutex::new(Weak::new()),
            bulk: Mutex::new(Weak::new()),
            input,
            credit: Mutex::new(None),
            changed: Notify::new(),
        }));
        let worker = host.clone();
        let activity = crate::platform::windows::host_service::activity::Work::new();
        tokio::spawn(async move {
            let _activity = activity;
            worker.run(enabled, lease, connected, stop, requests).await;
        });
        host
    }
    /// Bind the session or bulk channel of this connection.
    pub(crate) fn attach(&self, channel: &Arc<RTCDataChannel>) {
        match channel.label() {
            crate::protocol::peer::channel::SESSION => {
                *lock(&self.0.session) = Arc::downgrade(channel)
            }
            crate::protocol::peer::channel::BULK => *lock(&self.0.bulk) = Arc::downgrade(channel),
            _ => {}
        }
    }
    /// A `DIAGNOSTICS` frame or a `FLOW` of the diagnostics stream; `false`
    /// for other frames.
    pub(crate) fn receive(&self, bytes: &[u8]) -> Result<bool> {
        if let Some(body) = peer::body_of(FrameType::Flow, bytes) {
            let flow = Flow::decode(body)?;
            if flow.kind != transfer::kind::DIAGNOSTICS {
                return Ok(false);
            }
            if let Some((stream, credit)) = lock(&self.0.credit).as_mut()
                && *stream == flow.stream
                && credit.grant(flow.credit)
            {
                self.0.changed.notify_waiters();
            }
            return Ok(true);
        }
        let Some(message) = decode(bytes)? else {
            return Ok(false);
        };
        self.0
            .input
            .try_send(message)
            .map_err(|_| anyhow::anyhow!(crate::i18n::t().session.diag_too_many_requests))?;
        Ok(true)
    }
    async fn send(&self, frame: Vec<u8>) -> Result<()> {
        let channel = lock(&self.0.session)
            .upgrade()
            .filter(|c| c.ready_state() == RTCDataChannelState::Open)
            .ok_or_else(|| anyhow::anyhow!(crate::i18n::t().session.diag_channel_closed))?;
        tokio::time::timeout(TIMEOUT, channel.send(&Bytes::from(frame))).await??;
        Ok(())
    }
    async fn refuse(&self, id: u32, code: &str, message: &str) {
        let _ = self
            .send(peer::error_response(FrameType::Diagnostics, id, code, message))
            .await;
    }
    async fn run(
        &self,
        enabled: bool,
        lease: Lease,
        connected: Arc<AtomicBool>,
        stop: CancellationToken,
        mut requests: mpsc::Receiver<Received>,
    ) {
        let allowed = || enabled && lease.diagnostics_access() && connected.load(Ordering::Acquire);
        let mut announced: Option<(Weak<RTCDataChannel>, bool)> = None;
        let mut tick = tokio::time::interval(std::time::Duration::from_millis(200));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            let message = tokio::select! {
                _ = stop.cancelled() => break,
                _ = tick.tick() => {
                    // Status on every (re)opened session channel and change.
                    let session = lock(&self.0.session).clone();
                    let open = session
                        .upgrade()
                        .is_some_and(|c| c.ready_state() == RTCDataChannelState::Open);
                    let now = allowed();
                    if open
                        && !announced.as_ref().is_some_and(|(c, a)| c.ptr_eq(&session) && *a == now)
                        && self.send(frame(0, 0, Body::Status(Status { allowed: now }))).await.is_ok()
                    {
                        announced = Some((session, now));
                    }
                    continue;
                }
                message = requests.recv() => match message { Some(m) => m, None => break },
            };
            let Received::Request(id, Body::Request(request)) = message else {
                continue;
            };
            if !enabled {
                self.refuse(id, farwing_protocol::peer::error_code::UNSUPPORTED, crate::i18n::t().session.diag_unsupported).await;
                continue;
            }
            if !allowed() {
                self.refuse(id, farwing_protocol::peer::error_code::FORBIDDEN, crate::i18n::t().session.diag_forbidden).await;
                continue;
            }
            let job = stop.child_token();
            let result = {
                let work = self.transfer(id, request.stream, &lease, &allowed, &job, &mut requests);
                tokio::pin!(work);
                loop {
                    tokio::select! {
                        _ = stop.cancelled() => break Err(Outcome::Before(anyhow::anyhow!(crate::i18n::t().session.diag_connection_closed))),
                        _ = tick.tick() => if !allowed() {
                            job.cancel();
                            break Err(Outcome::After(anyhow::anyhow!(crate::i18n::t().session.diag_access_lost)));
                        },
                        result = &mut work => break result,
                    }
                }
            };
            *lock(&self.0.credit) = None;
            match result {
                Ok(()) => tracing::info!(stream = request.stream, "remote diagnostic bundle sent"),
                Err(Outcome::Cancelled) => tracing::info!("remote diagnostic export cancelled by the controller"),
                Err(Outcome::Before(error)) => {
                    tracing::debug!(error = %format!("{error:#}"), "remote diagnostic export failed");
                    if !stop.is_cancelled() {
                        self.refuse(id, farwing_protocol::peer::error_code::INTERNAL, &format!("{error:#}")).await;
                    }
                }
                Err(Outcome::After(error)) => {
                    tracing::debug!(error = %format!("{error:#}"), "remote diagnostic transfer failed");
                    if !stop.is_cancelled() {
                        let _ = self
                            .send(frame(
                                0,
                                0,
                                Body::Failed(Failed {
                                    stream: request.stream,
                                    message: format!("{error:#}"),
                                }),
                            ))
                            .await;
                    }
                }
            }
        }
    }
    /// Messages that arrive while a bundle is prepared or sent: `Cancel`
    /// stops it; another request is refused.
    async fn interrupted(
        &self,
        stream: u32,
        message: Option<Received>,
    ) -> std::result::Result<(), Outcome> {
        match message {
            None => Err(Outcome::Before(anyhow::anyhow!(crate::i18n::t().session.diag_channel_closed))),
            Some(Received::Push(Body::Cancel(cancel))) if cancel.stream == stream => {
                Err(Outcome::Cancelled)
            }
            Some(Received::Request(id, _)) => {
                self.refuse(id, farwing_protocol::peer::error_code::BUSY, crate::i18n::t().session.diag_busy).await;
                Ok(())
            }
            Some(_) => Ok(()),
        }
    }
    async fn transfer(
        &self,
        id: u32,
        stream: u32,
        lease: &Lease,
        allowed: &impl Fn() -> bool,
        job: &CancellationToken,
        requests: &mut mpsc::Receiver<Received>,
    ) -> std::result::Result<(), Outcome> {
        let before = Outcome::Before;
        let task = bundle::Task::remote(lease.diagnostic_input()).map_err(before)?;
        let mut tick = tokio::time::interval(std::time::Duration::from_millis(200));
        let exported = loop {
            match task.receiver.try_recv() {
                Ok(result) => break result.map_err(|e| before(anyhow::Error::msg(e)))?,
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    return Err(before(anyhow::anyhow!(crate::i18n::t().session.diag_export_interrupted)));
                }
                Err(_) => {}
            }
            tokio::select! {
                _ = job.cancelled() => return Err(before(anyhow::anyhow!(crate::i18n::t().session.diag_export_stopped))),
                _ = tick.tick() => {
                    let progress = task.progress();
                    self.send(frame(0, 0, Body::Progress(Progress {
                        stream,
                        stage: progress.stage,
                        completed: progress.completed,
                        total: progress.total,
                    })))
                    .await
                    .map_err(before)?;
                }
                message = requests.recv() => self.interrupted(stream, message).await?,
            }
        };
        // `exported` owns the temporary archive and deletes it on every exit.
        let (mut file, size, sha256) = async {
            let mut file = tokio::fs::File::open(&exported.path).await?;
            let size = file.metadata().await?.len();
            ensure!(size > 0 && size <= bundle::ARCHIVE_LIMIT, "{}", crate::i18n::t().session.diag_size_invalid);
            let mut digest = Sha256::new();
            let mut buffer = vec![0; transfer::CHUNK];
            loop {
                let count = file.read(&mut buffer).await?;
                if count == 0 {
                    break;
                }
                digest.update(&buffer[..count]);
            }
            file.seek(std::io::SeekFrom::Start(0)).await?;
            Ok::<_, anyhow::Error>((file, size, format!("{:x}", digest.finalize())))
        }
        .await
        .map_err(before)?;
        *lock(&self.0.credit) = Some((stream, Credit::new(transfer::INITIAL_WINDOW)));
        self.send(frame(
            flags::RESPONSE,
            id,
            Body::Ready(Ready {
                size,
                sha256,
                logs: exported.logs as u32,
                warnings: exported.warnings as u32,
            }),
        ))
        .await
        .map_err(before)?;
        let after = Outcome::After;
        let bulk = lock(&self.0.bulk)
            .upgrade()
            .ok_or_else(|| after(anyhow::anyhow!(crate::i18n::t().session.diag_data_closed)))?;
        let mut buffer = vec![0; transfer::CHUNK];
        let mut offset = 0u64;
        while offset < size {
            let count = file.read(&mut buffer).await.map_err(|e| after(e.into()))?;
            if count == 0 || offset + count as u64 > size {
                return Err(after(anyhow::anyhow!(crate::i18n::t().session.diag_length_mismatch)));
            }
            // Wait for the controller's credit.
            loop {
                let changed = self.0.changed.notified();
                tokio::pin!(changed);
                changed.as_mut().enable();
                let available = lock(&self.0.credit)
                    .as_ref()
                    .map_or(0, |(_, credit)| credit.available());
                if available >= count as u64 {
                    break;
                }
                tokio::select! {
                    _ = job.cancelled() => return Err(after(anyhow::anyhow!(crate::i18n::t().session.diag_transfer_stopped))),
                    _ = &mut changed => {},
                    _ = tokio::time::sleep(TIMEOUT) => return Err(after(anyhow::anyhow!(crate::i18n::t().session.diag_not_acknowledged))),
                    message = requests.recv() => self.interrupted(stream, message).await?,
                }
            }
            if let Some((_, credit)) = lock(&self.0.credit).as_mut() {
                credit.spend(count).map_err(after)?;
            }
            let chunk = Chunk {
                stream,
                kind: transfer::kind::DIAGNOSTICS,
                offset,
                data: Bytes::copy_from_slice(&buffer[..count]),
                last: offset + count as u64 == size,
                entry: 0,
            };
            crate::transport::bulk::send(
                &bulk,
                Bytes::from(peer::message(FrameType::Transfer, &chunk)),
                || allowed() && !job.is_cancelled(),
            )
            .await
            .map_err(after)?;
            offset += count as u64;
        }
        Ok(())
    }
}

enum Outcome {
    /// The controller cancelled.
    Cancelled,
    /// Failed before `Ready`: answered with an error response.
    Before(anyhow::Error),
    /// Failed while streaming: reported with a `Failed` push.
    After(anyhow::Error),
}
