//! Clipboard permission and TEXT-channel ownership. Native work never runs on media/input threads.
pub(crate) mod agent;
#[cfg(windows)]
mod frame;
mod send;
#[cfg(windows)]
mod wake;
#[cfg(not(windows))]
#[path = "clipboard/wake_linux.rs"]
mod wake;

use crate::features::clipboard::Clipboard;
use crate::protocol::peer_platform::PeerPlatform;
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
    native: bool,
    official_drop: Option<crate::account::feature_ability::FeatureCatalog>,
    token: u64,
    file: std::sync::Weak<RTCDataChannel>,
}
impl Receiver {
    pub fn official_drop(&self, catalog: Option<crate::account::feature_ability::FeatureCatalog>) {
        super::lock(&self.0).official_drop = catalog;
    }
    /// Negotiated capability, independent of the initial audio/file/view purpose.
    pub fn native(&self, enabled: bool) {
        super::lock(&self.0).native = enabled;
    }
    pub fn bind_file(&self, channel: &Arc<RTCDataChannel>) {
        super::lock(&self.0).file = Arc::downgrade(channel);
    }
    pub fn close_file(&self, channel: &Arc<RTCDataChannel>) {
        let mut s = super::lock(&self.0);
        if s.file.ptr_eq(&Arc::downgrade(channel)) {
            s.file = Default::default();
        }
    }
    pub fn receive_file(&self, channel: &Arc<RTCDataChannel>, bytes: &[u8]) -> Result<()> {
        let packet = crate::protocol::drag_drop::decode(bytes)?
            .ok_or_else(|| anyhow::anyhow!("拖放文件帧无效"))?;
        ensure!(
            matches!(
                packet.payload,
                Some(crate::protocol::drag_drop::Payload::Data(_))
            ),
            "FILE只接收拖放数据"
        );
        let s = super::lock(&self.0);
        if s.file.ptr_eq(&Arc::downgrade(channel))
            && let Some(sender) = &s.sender
        {
            sender
                .try_send(bytes.to_vec())
                .map_err(|_| anyhow::anyhow!("拖放接收队列已满"))?;
        }
        Ok(())
    }
    pub fn bind(&self, channel: u16) -> u64 {
        let (sender, receiver) = mpsc::channel(32);
        let mut state = super::lock(&self.0);
        state.generation = state.generation.wrapping_add(1);
        state.token = (uuid::Uuid::new_v4().as_u128() as u64).max(1);
        state.channel = Some(channel);
        state.sender = Some(sender);
        state.receiver = Some(receiver);
        state.generation
    }
    pub fn receive(&self, generation: u64, bytes: &[u8]) -> Result<bool> {
        ensure!(bytes.len() < 524288, "剪贴板消息过大");
        if !crate::protocol::drag_drop::is_packet(bytes)
            && !crate::features::clipboard::is_message(bytes)?
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
                .map_err(|_| anyhow::anyhow!("剪贴板接收队列已满或关闭"))?;
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
    platform: PeerPlatform,
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
                platform,
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
    platform: PeerPlatform,
    channel: Arc<RTCDataChannel>,
    input: &super::input::Receiver,
    screens: &impl Fn() -> (Vec<crate::media::capture::Screen>, bool),
) -> Result<()> {
    let (generation, mut rx) = {
        let mut state = super::lock(&receiver.0);
        ensure!(state.channel == Some(channel.id()), "剪贴板通道已被替换");
        (
            state.generation,
            state
                .receiver
                .take()
                .ok_or_else(|| anyhow::anyhow!("剪贴板通道已关闭"))?,
        )
    };
    let stop = cancel.child_token();
    let _stop_guard = stop.clone().drop_guard();
    let backend = agent::Backend::new(platform, stop.clone()).await?;
    let (bulk_tx, bulk_rx) = mpsc::channel(8);
    let (fault_tx, mut faults) = mpsc::channel(1);
    let bulk_receiver = receiver.clone();
    let bulk_lease = lease.clone();
    let bulk_stop = stop.clone();
    let bulk_enabled = Arc::new(AtomicBool::new(false));
    let bulk_policy = bulk_enabled.clone();
    let bulk_connected = connected.clone();
    let bulk_worker = tokio::spawn(async move {
        bulk(
            bulk_receiver,
            bulk_lease,
            generation,
            bulk_policy,
            bulk_connected,
            bulk_stop,
            bulk_rx,
            fault_tx,
        )
        .await;
    });
    let mut native_failed = false;
    let mut tick = tokio::time::interval(Duration::from_millis(100));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut permission = None;
    let mut send_permission = None;
    let mut native_permission = None;
    let result = async {
        loop {
            if cancel.is_cancelled() || !lease.requested() { break; }
            if !routes.borrow().text.as_ref().is_some_and(|c| c.ptr_eq(&Arc::downgrade(&channel))) { break; }
            let settings = lease.clipboard_settings();
            let level = routes.borrow().clipboard;
            let file_allowed = lease.file_access();
            let link_connected = connected.load(Ordering::Acquire);
            let policy = Settings {
                enabled: settings.enabled && level >= 1 && link_connected,
                files: settings.files && level >= 2,
            };
            let (current_screens, viewing) = screens();
            let native = {
                let s = super::lock(&receiver.0);
                crate::features::drag_drop::HostPolicy {
                    token: if s.native { s.token } else { 0 },
                    enabled: s.native && viewing && !native_failed && file_allowed && link_connected
                        && s.file.upgrade().is_some_and(|c| c.ready_state() == webrtc::data_channel::data_channel_state::RTCDataChannelState::Open),
                    screens: current_screens,
                }
            };
            if native_permission != Some(native.enabled) {
                tracing::info!(enabled=native.enabled, capability=native.token!=0, viewing, "native file drag availability changed");
                native_permission=Some(native.enabled);
            }
            bulk_enabled.store(native.enabled, Ordering::Release);
            let official_drop = viewing && !native.enabled && policy.enabled && policy.files && file_allowed
                && super::lock(&receiver.0).official_drop.as_ref().is_some_and(|c|
                    c.published(platform, crate::account::feature_ability::Feature::FileDrop));
            if send_permission != Some(official_drop) {
                tracing::info!(?platform, enabled=official_drop, "official file send permission");
                send_permission=Some(official_drop);
            }
            let send_routes = routes.clone();
            let current = || {
                lease.requested()
                    && lease.clipboard_settings() == settings
                    && lease.file_access() == file_allowed
                    && connected.load(Ordering::Acquire) == link_connected
                    && screens().1 == viewing
                    && super::lock(&receiver.0).generation == generation
                    && send_routes.borrow().clipboard == level
                    && send_routes.borrow().text.as_ref().is_some_and(|c| c.ptr_eq(&Arc::downgrade(&channel)))
            };
            backend.policy(policy, native, official_drop);
            if permission != Some(policy.files && policy.enabled) {
                let files = policy.files && policy.enabled;
                let data = bytes::Bytes::from(crate::features::stream_control::publisher::clipboard_permission(files));
                let Some(sent) = crate::lifecycle::while_current(cancel, Duration::from_millis(100), &current,
                    tokio::time::timeout(Duration::from_secs(30), channel.send_text_bytes(&data))).await else { continue; };
                sent??;
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
                    let Some(packet)=packet else { anyhow::bail!(backend.status().error.unwrap_or_else(|| "剪贴板用户会话已结束".into())); };
                    // The user agent publishes pointer ownership before its
                    // handoff reply. Apply it before the peer can resume input.
                    input.drag_pointer(backend.status().dragging);
                    // SCTP reserves capacity before its non-suspending message
                    // commit, so policy changes may cancel admission safely.
                    if packet.bulk {
                        if bulk_tx.try_send(packet).is_err() { native_failed=true; }
                    } else {
                        let data = bytes::Bytes::from(packet.data);
                        if let Some(sent) = crate::lifecycle::while_current(cancel, Duration::from_millis(100), &current,
                            tokio::time::timeout(Duration::from_secs(30), channel.send_text_bytes(&data))).await { sent??; }
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

async fn bulk(
    receiver: Receiver,
    lease: super::Lease,
    generation: u64,
    enabled: Arc<AtomicBool>,
    connected: Arc<AtomicBool>,
    stop: CancellationToken,
    mut input: mpsc::Receiver<agent::Outbound>,
    errors: mpsc::Sender<String>,
) {
    loop {
        let packet = tokio::select! { _=stop.cancelled()=>break, p=input.recv()=>match p{Some(p)=>p,None=>break} };
        let current = || {
            let state = super::lock(&receiver.0);
            state.token == packet.revision
                && state.generation == generation
                && state.native
                && enabled.load(Ordering::Acquire)
                && connected.load(Ordering::Acquire)
                && lease.file_access()
                && lease.requested()
        };
        if !current() {
            continue;
        }
        let channel = super::lock(&receiver.0).file.upgrade();
        let result = tokio::time::timeout(Duration::from_secs(30), async {
            let channel = channel.ok_or_else(|| anyhow::anyhow!("拖放文件通道已关闭"))?;
            while channel.buffered_amount().await + packet.data.len() > 2 * 1024 * 1024 {
                if !current() { return Ok(()); }
                tokio::select! { _=stop.cancelled()=>return Ok(()), _=tokio::time::sleep(Duration::from_millis(5))=>{} }
            }
            let data = bytes::Bytes::copy_from_slice(&packet.data);
            if let Some(result) = crate::lifecycle::while_current(&stop, Duration::from_millis(5), &current, channel.send(&data)).await { result?; }
            Ok::<_,anyhow::Error>(())
        }).await;
        if !matches!(result, Ok(Ok(()))) {
            let _ = errors.try_send("拖放文件发送失败".into());
            break;
        }
    }
}

pub(super) fn snapshot(clipboard: &Clipboard) -> Status {
    let state = clipboard.snapshot();
    Status {
        active: state.active,
        files: state.active && state.files,
        error: state.error,
        dragging: clipboard.dragging(),
    }
}
