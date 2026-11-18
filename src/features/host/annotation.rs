//! Session-owned Draw execution. Network callbacks never paint or query Windows.
pub(crate) mod agent;
// The model only drives the Windows overlay; Linux uses its validation alone.
#[cfg_attr(not(windows), allow(dead_code))]
mod model;

use crate::features::stream_control::{annotation::wire::PbDrawRequestKind, publisher};
use crate::media::capture::Screen;
use crate::protocol::annotation as native;
use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::sync::{Arc, Mutex, Weak, mpsc};
use std::time::Duration;
use tokio_util::sync::CancellationToken;
use webrtc::data_channel::RTCDataChannel;

#[derive(Clone, Default, Serialize, Deserialize)]
pub(crate) struct Context {
    pub allowed: bool,
    pub current: i32,
    pub screens: Vec<Screen>,
}

#[derive(Clone, Serialize, Deserialize)]
pub(crate) enum Action {
    Official(PbDrawRequestKind),
    Native(native::Command),
}
impl Action {
    pub fn toggle(&self) -> Option<bool> {
        let command = match self {
            Self::Official(c) => Some(c),
            Self::Native(c) => match &c.operation {
                Some(native::Operation::Draw(d)) => d.payload.as_ref(),
                _ => None,
            },
        };
        match command {
            Some(PbDrawRequestKind::Toggle(t)) => Some(t.enable),
            _ => None,
        }
    }
}
struct Work {
    generation: u64,
    submitted: std::time::Instant,
    command: Action,
    reply: tokio::sync::oneshot::Sender<i32>,
}
struct Binding {
    generation: u64,
    channel: Weak<RTCDataChannel>,
    viewing: bool,
    extension: bool,
    token: u64,
    last_request: i64,
    completed: std::collections::VecDeque<(i64, i32)>,
}
#[derive(Clone)]
pub(crate) struct Receiver {
    binding: Arc<Mutex<Binding>>,
    input: mpsc::SyncSender<Work>,
}
impl Receiver {
    pub fn start(
        viewing: bool,
        extension: bool,
        context: impl Fn() -> Context + Send + 'static,
        cancel: CancellationToken,
    ) -> (Self, tokio::task::JoinHandle<()>) {
        let binding = Arc::new(Mutex::new(Binding {
            generation: 0,
            channel: Weak::new(),
            viewing,
            extension,
            token: 0,
            last_request: 0,
            completed: Default::default(),
        }));
        let (input, incoming) = mpsc::sync_channel::<Work>(64);
        let state = binding.clone();
        let activity = crate::platform::host_service::activity::Work::new();
        let worker = tokio::task::spawn_blocking(move || {
            let _activity = activity;
            let mut backend = agent::Backend::default();
            let mut generation = 0;
            while !cancel.is_cancelled() {
                let work = match incoming.recv_timeout(backend.interval()) {
                    Ok(work) => Some(work),
                    Err(mpsc::RecvTimeoutError::Timeout) => None,
                    Err(mpsc::RecvTimeoutError::Disconnected) => break,
                };
                let mut context = context();
                let current = {
                    let b = super::lock(&state);
                    context.allowed &= b.viewing && b.channel.strong_count() != 0;
                    b.generation
                };
                if generation != current || !context.allowed {
                    backend = agent::Backend::default();
                    generation = current;
                }
                let command = work.as_ref().filter(|w| {
                    w.generation == current
                        && context.allowed
                        && !w.reply.is_closed()
                        && w.submitted.elapsed() < Duration::from_secs(2)
                });
                let accepted = command.is_some();
                let code = match backend.exchange(
                    context,
                    command.map(|w| w.command.clone()),
                    &cancel,
                ) {
                    Ok(code) => code,
                    Err(error) => {
                        tracing::warn!(%error, "host annotation renderer failed; overlay cleared");
                        backend = agent::Backend::default();
                        4
                    }
                };
                if let Some(work) = work {
                    let valid =
                        work.generation == super::lock(&state).generation && !cancel.is_cancelled();
                    let _ = work.reply.send(if valid && accepted { code } else { 4 });
                }
            }
            // Session-owned native windows die before this worker completes.
        });
        (Self { binding, input }, worker)
    }
    pub fn bind(&self, channel: &Arc<RTCDataChannel>) {
        let mut b = super::lock(&self.binding);
        b.generation = b.generation.wrapping_add(1);
        b.channel = Arc::downgrade(channel);
        b.token = (uuid::Uuid::new_v4().as_u128() as u64).max(1);
        b.last_request = 0;
        b.completed.clear();
    }
    pub fn close(&self, channel: &Arc<RTCDataChannel>) {
        let mut b = super::lock(&self.binding);
        if b.channel.ptr_eq(&Arc::downgrade(channel)) {
            b.generation = b.generation.wrapping_add(1);
            b.channel = Weak::new();
        }
    }
    pub fn invalidate(&self) {
        let mut b = super::lock(&self.binding);
        b.generation = b.generation.wrapping_add(1);
        b.token = (uuid::Uuid::new_v4().as_u128() as u64).max(1);
        b.last_request = 0;
        b.completed.clear();
    }
    pub async fn notify_hello(&self) {
        let channel = super::lock(&self.binding).channel.upgrade();
        if let Some(channel) = channel {
            if let Some(bytes) = self.hello(&channel) {
                let _ = channel.send_text_bytes(&bytes::Bytes::from(bytes)).await;
            }
        }
    }
    pub fn hello(&self, channel: &Arc<RTCDataChannel>) -> Option<Vec<u8>> {
        let b = super::lock(&self.binding);
        if !b.extension || !b.channel.ptr_eq(&Arc::downgrade(channel)) {
            return None;
        }
        native::encode(native::Packet {
            token: b.token,
            request: 0,
            payload: Some(native::Payload::Hello(1)),
        })
        .ok()
    }

    pub fn viewing(&self, enabled: bool) {
        let mut b = super::lock(&self.binding);
        if b.viewing != enabled {
            b.viewing = enabled;
            b.generation = b.generation.wrapping_add(1);
        }
    }
    pub async fn receive(&self, channel: &Arc<RTCDataChannel>, bytes: &[u8]) -> Result<bool> {
        if let Some(packet) = native::decode(bytes)? {
            self.receive_native(channel, packet).await?;
            return Ok(true);
        }
        // Other bounded TEXT protocols retain their own size rules.
        if bytes.len() > 128 * 1024 {
            return Ok(false);
        }
        let Some(request) = publisher::draw_request(bytes)? else {
            return Ok(false);
        };
        let Some(generation) = self.generation(channel) else {
            return Ok(true);
        };
        self.receive_official(channel, request, generation).await?;
        Ok(true)
    }
    pub fn generation(&self, channel: &Arc<RTCDataChannel>) -> Option<u64> {
        let b = super::lock(&self.binding);
        b.channel
            .ptr_eq(&Arc::downgrade(channel))
            .then_some(b.generation)
    }
    /// Both official ingress carriers execute against the same TEXT-bound owner.
    /// Capture the epoch at admission, not after an asynchronous queue wait.
    pub async fn receive_official(
        &self,
        channel: &Arc<RTCDataChannel>,
        request: publisher::DrawRequest,
        generation: u64,
    ) -> Result<()> {
        if self.generation(channel) != Some(generation) {
            return Ok(());
        }
        let (reply, result) = tokio::sync::oneshot::channel();
        let code = if !model::valid(&request.command) {
            4
        } else if self
            .input
            .try_send(Work {
                generation,
                submitted: std::time::Instant::now(),
                command: Action::Official(request.command.clone()),
                reply,
            })
            .is_ok()
        {
            result.await.unwrap_or(4)
        } else {
            4
        };
        let current = {
            let b = super::lock(&self.binding);
            b.generation == generation && b.channel.ptr_eq(&Arc::downgrade(channel))
        };
        if current {
            if let PbDrawRequestKind::Toggle(toggle) = &request.command {
                tracing::info!(
                    enabled = toggle.enable,
                    error_code = code,
                    "host annotation toggle completed"
                );
            } else if code != 0 {
                tracing::debug!(error_code = code, "host annotation operation rejected");
            }
            // Never cancel a reliable SCTP message halfway through enqueue.
            channel
                .send_text_bytes(&bytes::Bytes::from(request.response(code)))
                .await?;
        }
        Ok(())
    }
    async fn receive_native(
        &self,
        channel: &Arc<RTCDataChannel>,
        packet: native::Packet,
    ) -> Result<()> {
        let Some(native::Payload::Command(command)) = packet.payload else {
            return Ok(());
        };
        let toggle = match &command.operation {
            Some(native::Operation::Draw(draw)) => match &draw.payload {
                Some(PbDrawRequestKind::Toggle(t)) => Some(t.enable),
                _ => None,
            },
            _ => None,
        };
        let (generation, previous) = {
            let mut b = super::lock(&self.binding);
            if !b.extension
                || b.token != packet.token
                || !b.channel.ptr_eq(&Arc::downgrade(channel))
            {
                return Ok(());
            }
            let old = b
                .completed
                .iter()
                .find(|(id, _)| *id == packet.request)
                .map(|(_, code)| *code)
                .or_else(|| (packet.request <= b.last_request || packet.request <= 0).then_some(4));
            if old.is_none() {
                b.last_request = packet.request;
            }
            (b.generation, old)
        };
        let (reply, result) = tokio::sync::oneshot::channel();
        let code = if let Some(code) = previous {
            code
        } else if !model::valid_native(&command) {
            4
        } else if self
            .input
            .try_send(Work {
                generation,
                submitted: std::time::Instant::now(),
                command: Action::Native(command),
                reply,
            })
            .is_ok()
        {
            result.await.unwrap_or(4)
        } else {
            4
        };
        let current = {
            let mut b = super::lock(&self.binding);
            let current = b.generation == generation
                && b.token == packet.token
                && b.channel.ptr_eq(&Arc::downgrade(channel));
            if current && previous.is_none() {
                b.completed.push_back((packet.request, code));
                if b.completed.len() > 64 {
                    b.completed.pop_front();
                }
            }
            current
        };
        if current {
            if let Some(enabled) = toggle {
                tracing::info!(
                    enabled,
                    error_code = code,
                    "host native annotation toggle completed"
                );
            }
            channel
                .send_text_bytes(&bytes::Bytes::from(native::encode(native::Packet {
                    token: packet.token,
                    request: packet.request,
                    payload: Some(native::Payload::Result(code)),
                })?))
                .await?;
        }
        Ok(())
    }
}
