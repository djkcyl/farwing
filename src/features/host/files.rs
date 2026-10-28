//! FILE v2 admission and session/bulk channel ownership for the
//! ordinary-user executor. Frames pass through unchanged in both directions.
use super::{Lease, lock};
use crate::features::file_transfer::host::{self as executor, agent::Backend};
use crate::i18n::t;
use anyhow::{Result, ensure};
use std::{
    collections::HashMap,
    sync::{
        Arc, Mutex, Weak,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use webrtc::data_channel::{RTCDataChannel, data_channel_state::RTCDataChannelState};
#[derive(Clone, Default, serde::Serialize, serde::Deserialize)]
pub(crate) struct Status {
    pub active: bool,
    pub error: Option<String>,
}
#[derive(Default)]
struct State {
    capabilities: executor::Capabilities,
    channels: HashMap<String, Weak<RTCDataChannel>>,
    generation: u64,
    incoming: Option<mpsc::Sender<(u64, Vec<u8>)>>,
    receiver: Option<mpsc::Receiver<(u64, Vec<u8>)>>,
}
#[derive(Clone)]
pub(crate) struct Receiver(Arc<Mutex<State>>);
impl Default for Receiver {
    fn default() -> Self {
        let (tx, rx) = mpsc::channel(32);
        Self(Arc::new(Mutex::new(State {
            incoming: Some(tx),
            receiver: Some(rx),
            ..Default::default()
        })))
    }
}
impl Receiver {
    pub fn capabilities(&self, capabilities: executor::Capabilities) {
        let mut state = lock(&self.0);
        if state.capabilities != capabilities {
            tracing::debug!(files = capabilities.files, "host file capabilities negotiated");
            state.capabilities = capabilities;
        }
    }
    pub fn bind(&self, channel: &Arc<RTCDataChannel>) {
        let mut s = lock(&self.0);
        if s.channels
            .get(channel.label())
            .is_some_and(|old| !old.ptr_eq(&Arc::downgrade(channel)))
        {
            s.generation = s.generation.wrapping_add(1);
        }
        s.channels
            .insert(channel.label().to_owned(), Arc::downgrade(channel));
    }
    pub fn close(&self, channel: &Arc<RTCDataChannel>) {
        let mut s = lock(&self.0);
        if s.channels
            .get(channel.label())
            .is_some_and(|old| old.ptr_eq(&Arc::downgrade(channel)))
        {
            s.channels.remove(channel.label());
            s.generation = s.generation.wrapping_add(1);
        }
    }
    pub async fn receive(&self, channel: &Arc<RTCDataChannel>, bytes: &[u8]) -> Result<bool> {
        if !executor::claims(bytes) {
            return Ok(false);
        }
        let delivery = {
            let s = lock(&self.0);
            if !s
                .channels
                .get(channel.label())
                .is_some_and(|old| old.ptr_eq(&Arc::downgrade(channel)))
            {
                return Ok(true);
            }
            s.incoming.clone().map(|tx| (tx, s.generation))
        };
        if let Some((tx, generation)) = delivery {
            tx.send((generation, bytes.to_vec()))
                .await
                .map_err(|_| anyhow::anyhow!(t().features.host_file_queue_closed))?;
        }
        Ok(true)
    }
}
pub(super) async fn run(
    receiver: Receiver,
    lease: Lease,
    connected: Arc<AtomicBool>,
    cancel: CancellationToken,
    scope: String,
) {
    let mut input = lock(&receiver.0)
        .receiver
        .take()
        .expect("file receiver has one owner");
    let mut backend: Option<(u64, Backend)> = None;
    let mut tick = tokio::time::interval(Duration::from_millis(25));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut pending: Option<(u64, Vec<u8>)> = None;
    loop {
        if cancel.is_cancelled() || !lease.requested() {
            break;
        }
        let (generation, text_ready, file_ready) = {
            let s = lock(&receiver.0);
            let open = |name| {
                s.channels
                    .get(name)
                    .and_then(Weak::upgrade)
                    .is_some_and(|c| c.ready_state() == RTCDataChannelState::Open)
            };
            (
                s.generation,
                open(crate::protocol::peer::channel::SESSION),
                open(crate::protocol::peer::channel::BULK),
            )
        };
        let ready = text_ready && file_ready;
        if backend
            .as_ref()
            .is_some_and(|(g, _)| *g != generation || !ready)
        {
            lease.file_notices(backend.take().unwrap().1.close().await);
            pending = None;
        }
        if backend.is_none() && ready && pending.is_some() {
            backend = Some((
                generation,
                Backend::new(scope.clone(), cancel.child_token()),
            ));
        }
        let allowed = lease.file_access() && connected.load(Ordering::Acquire);
        let revision = backend.as_ref().map_or(0, |(_, b)| {
            b.policy(allowed, lock(&receiver.0).capabilities)
        });
        let sender = backend.as_ref().map(|(_, b)| b.input.clone());
        if let Some((_, b)) = &backend {
            lease.file_notices(b.notices.take());
            lease.file_status(Status {
                active: allowed && b.ready.load(Ordering::Acquire),
                error: lock(&b.error).clone(),
            });
        }

        tokio::select! {
            _=cancel.cancelled()=>break,
            _=tick.tick()=>{},
            packet=input.recv(), if pending.is_none() && text_ready=>{
                let Some((g,packet))=packet else{break};
                if g != generation {continue;}
                let _ = file_ready;
                pending=Some((if backend.is_some(){revision}else{u64::MAX},packet));
            },
            permit=async{match &sender{Some(tx)=>tx.reserve().await,None=>std::future::pending().await}}, if pending.is_some()=>{
                let Ok(permit)=permit else{break};
                let packet=pending.take().unwrap();
                if packet.0==revision||packet.0==u64::MAX{permit.send((revision,packet.1));}
            },
            packet=async{match &mut backend{Some((_,b))=>b.output.recv().await,None=>std::future::pending().await}}=>{
                if let Some((packet_revision,packet))=packet {
                    if packet_revision!=revision {continue;}
                    if let Err(e)=send(&receiver,generation,packet,&cancel,&lease).await{lease.file_status(Status{active:false,error:Some(e.to_string())});break;}
                } else {break;}
            }
        }
    }
    if let Some((_, b)) = backend {
        lease.file_notices(b.close().await);
    }
    lock(&receiver.0).incoming = None;
    lease.file_status(Status::default());
}
async fn send(
    receiver: &Receiver,
    generation: u64,
    packet: executor::Packet,
    cancel: &CancellationToken,
    lease: &Lease,
) -> Result<()> {
    let rejection = executor::is_failure(&packet);
    if !lease.requested() || (!lease.file_access() && !rejection) {
        return Ok(());
    }
    let channel = {
        let s = lock(&receiver.0);
        ensure!(s.generation == generation, "{}", t().features.host_file_channel_replaced);
        s.channels
            .get(if packet.bulk {
                crate::protocol::peer::channel::BULK
            } else {
                crate::protocol::peer::channel::SESSION
            })
            .and_then(Weak::upgrade)
            .ok_or_else(|| anyhow::anyhow!(t().features.host_file_channel_closed))?
    };
    let permitted = || lease.requested() && (lease.file_access() || rejection);
    // Both channels go through their outboxes: file data shares the bulk
    // one with clipboard, drag and diagnostics data.
    let current = || {
        permitted() && !cancel.is_cancelled() && lock(&receiver.0).generation == generation
    };
    if !current() {
        return Ok(());
    }
    crate::transport::outbox::send_while(&channel, bytes::Bytes::from(packet.data), current).await
}
