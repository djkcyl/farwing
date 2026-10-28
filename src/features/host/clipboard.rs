//! Clipboard permission and channel ownership (CLIPBOARD v2 on the session
//! channel, its `TRANSFER`/`FLOW` and native drag data on the bulk channel).
//! Native work never runs on media/input threads.
pub(crate) mod agent;
mod frame;
mod wake;

use crate::features::clipboard::Clipboard;
use crate::i18n::t;
use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};
use std::time::Duration;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use webrtc::data_channel::RTCDataChannel;

#[derive(Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Settings {
    pub enabled: bool,
    pub files: bool,
}
impl Default for Settings {
    fn default() -> Self {
        Self {
            enabled: true,
            files: true,
        }
    }
}
#[derive(Clone, Default, Serialize, Deserialize)]
pub(crate) struct Status {
    pub active: bool,
    pub files: bool,
    pub error: Option<String>,
    #[serde(default)]
    pub dragging: bool,
}

#[derive(Clone, Default)]
pub(crate) struct Receiver(Arc<Mutex<Inbox>>);
#[derive(Default)]
struct Inbox {
    generation: u64,
    channel: Option<u16>,
    sender: Option<mpsc::Sender<Vec<u8>>>,
    receiver: Option<mpsc::Receiver<Vec<u8>>>,
    token: u64,
    file: std::sync::Weak<RTCDataChannel>,
}
/// Frames queued towards the clipboard backend. A stream's sender stays
/// within the credit it was granted, so a full window of chunks fits.
pub(super) const INBOX: usize = 2048;
impl Receiver {
    /// Negotiated capability, independent of the initial audio/file/view purpose.
    pub fn bind_file(&self, channel: &Arc<RTCDataChannel>) {
        super::lock(&self.0).file = Arc::downgrade(channel);
    }
    pub fn close_file(&self, channel: &Arc<RTCDataChannel>) {
        let mut s = super::lock(&self.0);
        if s.file.ptr_eq(&Arc::downgrade(channel)) {
            s.file = Default::default();
        }
    }
    /// Bulk channel: `TRANSFER`/`FLOW` of clipboard and drag streams.
    /// Returns `false` for frames that belong elsewhere.
    pub fn receive_bulk(&self, channel: &Arc<RTCDataChannel>, bytes: &[u8]) -> Result<bool> {
        use crate::transport::transfer::{kind, kind_of};
        if !matches!(kind_of(bytes), Some(kind::CLIPBOARD | kind::DRAG)) {
            return Ok(false);
        }
        let s = super::lock(&self.0);
        if s.file.ptr_eq(&Arc::downgrade(channel))
            && let Some(sender) = &s.sender
        {
            sender
                .try_send(bytes.to_vec())
                .map_err(|_| anyhow::anyhow!("{}", t().features.clipboard_data_queue_full))?;
        }
        Ok(true)
    }
    pub fn bind(&self, channel: u16) -> u64 {
        let (sender, receiver) = mpsc::channel(INBOX);
        let mut state = super::lock(&self.0);
        state.generation = state.generation.wrapping_add(1);
        state.token = (uuid::Uuid::new_v4().as_u128() as u64).max(1);
        state.channel = Some(channel);
        state.sender = Some(sender);
        state.receiver = Some(receiver);
        state.generation
    }
    /// Session channel: `CLIPBOARD` and `DRAG` frames.
    pub fn receive(&self, generation: u64, bytes: &[u8]) -> Result<bool> {
        if !crate::protocol::drag_drop::is_packet(bytes)
            && !crate::features::clipboard::wire::is_frame(bytes)
        {
            return Ok(false);
        }
        let state = super::lock(&self.0);
        if state.generation != generation {
            return Ok(true);
        }
        if let Some(sender) = state.sender.as_ref() {
            sender
                .try_send(bytes.to_vec())
                .map_err(|_| anyhow::anyhow!("{}", t().features.clipboard_receive_queue_full))?;
        }
        Ok(true)
    }
}

/// A backend exists only for the current TEXT binding; old queues never migrate to a new channel.
pub(super) async fn run(
    receiver: Receiver,
    mut routes: tokio::sync::watch::Receiver<super::peer::ReportRoutes>,
    lease: super::Lease,
    connected: Arc<AtomicBool>,
    cancel: CancellationToken,
    input: super::input::Receiver,
    screens: impl Fn() -> (Vec<crate::media::capture::Screen>, bool) + Send + Sync + 'static,
) {
    let mut tick = tokio::time::interval(Duration::from_millis(100));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    while !cancel.is_cancelled() && lease.requested() {
        let channel = routes
            .borrow()
            .text
            .as_ref()
            .and_then(std::sync::Weak::upgrade);
        if let Some(channel) = channel {
            if let Err(error) = binding(
                &receiver,
                &mut routes,
                &lease,
                &connected,
                &cancel,
                channel.clone(),
                &input,
                &screens,
            )
            .await
            {
                tracing::warn!(%error, "host clipboard binding stopped");
                lease.clipboard_status(Status {
                    error: Some(error.to_string()),
                    ..Default::default()
                });
                // Do not replay an uncertain clipboard operation after a backend failure.
                loop {
                    let same = routes
                        .borrow()
                        .text
                        .as_ref()
                        .is_some_and(|c| c.ptr_eq(&Arc::downgrade(&channel)));
                    if !same || cancel.is_cancelled() || !lease.requested() {
                        break;
                    }
                    tokio::select! { _=cancel.cancelled()=>break, _=routes.changed()=>{}, _=tick.tick()=>{} }
                }
            }
        } else {
            tokio::select! { _=cancel.cancelled()=>break, _=routes.changed()=>{}, _=tick.tick()=>{} }
        }
    }
    super::lock(&receiver.0).sender.take();
    input.drag_pointer(false);
    lease.clipboard_status(Status::default());
}

async fn binding(
    receiver: &Receiver,
    routes: &mut tokio::sync::watch::Receiver<super::peer::ReportRoutes>,
    lease: &super::Lease,
    connected: &Arc<AtomicBool>,
    cancel: &CancellationToken,
    channel: Arc<RTCDataChannel>,
    input: &super::input::Receiver,
    screens: &impl Fn() -> (Vec<crate::media::capture::Screen>, bool),
) -> Result<()> {
    let (generation, mut rx) = {
        let mut state = super::lock(&receiver.0);
        ensure!(state.channel == Some(channel.id()), "{}", t().features.clipboard_channel_replaced);
        (
            state.generation,
            state
                .receiver
                .take()
                .ok_or_else(|| anyhow::anyhow!("{}", t().features.clipboard_channel_closed))?,
        )
    };
    let stop = cancel.child_token();
    let _stop_guard = stop.clone().drop_guard();
    let backend = agent::Backend::new(stop.clone()).await?;
    let (bulk_tx, bulk_rx) = mpsc::channel(64);
    let (fault_tx, mut faults) = mpsc::channel(1);
    let bulk_receiver = receiver.clone();
    let bulk_lease = lease.clone();
    let bulk_stop = stop.clone();
    let bulk_worker = tokio::spawn(async move {
        bulk(bulk_receiver, bulk_lease, bulk_stop, bulk_rx, fault_tx).await;
    });
    let mut native_failed = false;
    let mut tick = tokio::time::interval(Duration::from_millis(100));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut permission = None;
    let mut native_permission = None;
    let result = async {
        loop {
            if cancel.is_cancelled() || !lease.requested() { break; }
            if !routes.borrow().text.as_ref().is_some_and(|c| c.ptr_eq(&Arc::downgrade(&channel))) { break; }
            let level = routes.borrow().clipboard;
            let online = level >= 1 && connected.load(Ordering::Acquire);
            // Automatic sync: the controller (§4.2); manual `Pull`: any
            // participant holding `clipboard`.
            let settings = lease.clipboard_settings();
            let policy = Settings {
                enabled: settings.enabled && online,
                files: settings.files && level >= 2,
            };
            let pulling = lease.clipboard_pull();
            let pull = Settings {
                enabled: pulling.enabled && online,
                files: pulling.files && level >= 2,
            };
            let (current_screens, viewing) = screens();
            let native = {
                let s = super::lock(&receiver.0);
                crate::features::drag_drop::HostPolicy {
                    token: s.token,
                    enabled: viewing && !native_failed && lease.file_access() && connected.load(Ordering::Acquire)
                        && s.file.upgrade().is_some_and(|c| c.ready_state() == webrtc::data_channel::data_channel_state::RTCDataChannelState::Open),
                    screens: current_screens,
                }
            };
            if native_permission != Some(native.enabled) {
                tracing::info!(enabled=native.enabled, capability=native.token!=0, viewing, "native file drag availability changed");
                native_permission=Some(native.enabled);
            }
            backend.policy(policy, pull, native);
            let files = (policy.files && policy.enabled) || (pull.files && pull.enabled);
            if permission != Some(files) {
                crate::transport::outbox::send(&channel, bytes::Bytes::from(crate::features::stream_control::publisher::clipboard_permission(files))).await?;
                permission = Some(files);
            }
            let status = backend.status();
            input.drag_pointer(status.dragging);
            lease.clipboard_status(status);
            tokio::select! {
                _=cancel.cancelled()=>break,
                _=routes.changed()=>{},
                _=tick.tick()=>{},
                error=faults.recv(), if !native_failed=>{
                    if let Some(error)=error {tracing::warn!(%error,"native drag FILE output failed");}
                    native_failed=true;
                },
                message=rx.recv()=>{
                    let Some(message)=message else {break;};
                    backend.receive(message)?;
                },
                packet=backend.next()=>{
                    let Some(packet)=packet else { anyhow::bail!(backend.status().error.unwrap_or_else(|| t().features.clipboard_user_session_ended.into())); };
                    // The user agent publishes pointer ownership before its
                    // handoff reply. Apply it before the peer can resume input.
                    input.drag_pointer(backend.status().dragging);
                    // Finish each SCTP enqueue, then observe cancellation; never tear a reliable message in half.
                    if packet.bulk {
                        // Stream data is credit-limited; wait for the bulk writer.
                        if bulk_tx.send(packet).await.is_err() { native_failed=true; }
                    } else {
                        crate::transport::outbox::send(&channel, bytes::Bytes::from(packet.data)).await?;
                    }
                },
            }
        }
        Ok(())
    }.await;
    {
        let mut state = super::lock(&receiver.0);
        if state.generation == generation {
            state.sender.take();
        }
    }
    stop.cancel();
    input.drag_pointer(false);
    let _ = bulk_worker.await;
    backend.close().await;
    result
}

/// Writes clipboard and drag stream data on the bulk channel through its
/// shared writer gate.
async fn bulk(
    receiver: Receiver,
    lease: super::Lease,
    stop: CancellationToken,
    mut input: mpsc::Receiver<agent::Outbound>,
    errors: mpsc::Sender<String>,
) {
    loop {
        let packet = tokio::select! { _=stop.cancelled()=>break, p=input.recv()=>match p{Some(p)=>p,None=>break} };
        let current = || {
            let state = super::lock(&receiver.0);
            lease.requested()
                && !stop.is_cancelled()
                && (!packet.native
                    || (state.token == packet.revision && lease.file_access()))
        };
        if !current() {
            continue;
        }
        let channel = super::lock(&receiver.0).file.upgrade();
        let result = match channel {
            Some(channel) => {
                crate::transport::outbox::send_while(&channel, bytes::Bytes::from(packet.data), current)
                    .await
            }
            None => Err(anyhow::anyhow!("剪贴板数据通道已关闭")),
        };
        if let Err(error) = result {
            if current() {
                tracing::warn!(%error, native = packet.native, "clipboard bulk send failed");
                let _ = errors.try_send(if packet.native {
                    "拖放文件发送失败".into()
                } else {
                    "剪贴板数据发送失败".into()
                });
                break;
            }
        }
    }
}

pub(super) fn snapshot(clipboard: &Clipboard) -> Status {
    let state = clipboard.snapshot();
    Status {
        // Shown as "clipboard synchronizing": automatic sync only.
        active: state.auto,
        files: state.auto && state.files,
        error: state.error,
        dragging: false,
    }
}
