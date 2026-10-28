//! Session-owned Draw execution. Network callbacks never paint or query Windows.
pub(crate) mod agent;
mod model;

use crate::media::capture::Screen;
use crate::protocol::annotation::{self as protocol, Op, reason};
use anyhow::Result;
use std::sync::{Arc, Mutex, Weak, mpsc};
use std::time::Duration;
use tokio_util::sync::CancellationToken;
use webrtc::data_channel::RTCDataChannel;

#[derive(Clone, Default)]
pub(crate) struct Context {
    pub allowed: bool,
    pub current: i32,
    pub screens: Vec<Screen>,
}

/// Why a request was not applied.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Refusal {
    SignedOut,
    Locked,
    NotAllowed,
    Off,
    Invalid,
    Failed,
}

impl Refusal {
    fn reason(self) -> (&'static str, &'static str) {
        match self {
            Self::SignedOut => reason::SIGNED_OUT,
            Self::Locked => reason::LOCKED,
            Self::NotAllowed => reason::NOT_ALLOWED,
            Self::Off => reason::OFF,
            Self::Invalid => reason::INVALID,
            Self::Failed => reason::FAILED,
        }
    }
}

pub(crate) type Outcome = std::result::Result<(), Refusal>;

struct Work {
    generation: u64,
    submitted: std::time::Instant,
    op: Op,
    reply: tokio::sync::oneshot::Sender<Outcome>,
}
struct Binding {
    generation: u64,
    channel: Weak<RTCDataChannel>,
    viewing: bool,
    /// The overlay was dropped while the controller could not hear it.
    reset: bool,
}
#[derive(Clone)]
pub(crate) struct Receiver {
    binding: Arc<Mutex<Binding>>,
    input: mpsc::SyncSender<Work>,
}
impl Receiver {
    pub fn start(
        viewing: bool,
        context: impl Fn() -> Context + Send + 'static,
        cancel: CancellationToken,
    ) -> (Self, tokio::task::JoinHandle<()>) {
        let binding = Arc::new(Mutex::new(Binding {
            generation: 0,
            channel: Weak::new(),
            viewing,
            reset: false,
        }));
        let (input, incoming) = mpsc::sync_channel::<Work>(64);
        let state = binding.clone();
        let activity = crate::platform::windows::host_service::activity::Work::new();
        let worker = tokio::task::spawn_blocking(move || {
            let _activity = activity;
            let mut backend = agent::Backend::default();
            let mut generation = 0;
            while !cancel.is_cancelled() {
                let work = match incoming.recv_timeout(Duration::from_millis(50)) {
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
                let refused = work.as_ref().and_then(|w| {
                    if !context.allowed {
                        Some(Refusal::NotAllowed)
                    } else if w.generation != current {
                        Some(Refusal::Off)
                    } else if w.reply.is_closed() || w.submitted.elapsed() >= Duration::from_secs(2) {
                        Some(Refusal::Failed)
                    } else {
                        None
                    }
                });
                let op = work.as_ref().filter(|_| refused.is_none()).map(|w| &w.op);
                let outcome = match backend.exchange(context, op, &cancel) {
                    Ok(outcome) => outcome,
                    Err(error) => {
                        tracing::warn!(%error, "host annotation renderer failed; overlay cleared");
                        backend = agent::Backend::default();
                        Err(Refusal::Failed)
                    }
                };
                if let Some(work) = work {
                    let stale =
                        work.generation != super::lock(&state).generation || cancel.is_cancelled();
                    let _ = work.reply.send(match refused {
                        Some(refusal) => Err(refusal),
                        None if stale => Err(Refusal::Off),
                        None => outcome,
                    });
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
        b.reset = false;
    }
    pub fn close(&self, channel: &Arc<RTCDataChannel>) {
        let mut b = super::lock(&self.binding);
        if b.channel.ptr_eq(&Arc::downgrade(channel)) {
            b.generation = b.generation.wrapping_add(1);
            b.channel = Weak::new();
        }
    }
    /// Drop the overlay (the transport is gone); [`Self::notify_reset`]
    /// tells the controller once it is back.
    pub fn invalidate(&self) {
        let mut b = super::lock(&self.binding);
        b.generation = b.generation.wrapping_add(1);
        b.reset = true;
    }
    /// Push `Reset` if the overlay was dropped since the controller last
    /// heard.
    pub async fn notify_reset(&self) {
        let channel = {
            let mut b = super::lock(&self.binding);
            if !std::mem::take(&mut b.reset) {
                return;
            }
            b.channel.upgrade()
        };
        if let Some(channel) = channel {
            let _ = crate::transport::outbox::send(&channel, bytes::Bytes::from(protocol::reset())).await;
        }
    }
    /// Whether this participant views the screens; the overlay goes when
    /// it stops. Returns whether that dropped it.
    pub fn viewing(&self, enabled: bool) -> bool {
        let mut b = super::lock(&self.binding);
        if b.viewing == enabled {
            return false;
        }
        b.viewing = enabled;
        b.generation = b.generation.wrapping_add(1);
        b.reset |= !enabled;
        !enabled
    }
    /// An `ANNOTATION` frame on `channel`; a request is answered once applied.
    pub async fn receive(&self, channel: &Arc<RTCDataChannel>, bytes: &[u8]) -> Result<bool> {
        let Some(received) = protocol::decode(bytes)? else {
            return Ok(false);
        };
        let id = received.request_id;
        anyhow::ensure!(id != 0 && !received.response, "annotation frame that is not a request");
        let current = || super::lock(&self.binding).channel.ptr_eq(&Arc::downgrade(channel));
        let generation = {
            let b = super::lock(&self.binding);
            if !b.channel.ptr_eq(&Arc::downgrade(channel)) {
                return Ok(true);
            }
            b.generation
        };
        let outcome = match received.body.ok().and_then(|message| message.op) {
            Some(op) if model::valid(&op) => {
                let show = match op {
                    Op::Show(on) => Some(on),
                    _ => None,
                };
                let (reply, result) = tokio::sync::oneshot::channel();
                let outcome = match self.input.try_send(Work {
                    generation,
                    submitted: std::time::Instant::now(),
                    op,
                    reply,
                }) {
                    Ok(()) => result.await.unwrap_or(Err(Refusal::Failed)),
                    Err(_) => Err(Refusal::Failed),
                };
                if let Some(enabled) = show {
                    tracing::info!(enabled, ?outcome, "host annotation toggle completed");
                }
                outcome
            }
            _ => Err(Refusal::Invalid),
        };
        if current() {
            crate::transport::outbox::send(
                channel,
                bytes::Bytes::from(protocol::response(id, outcome.map_err(Refusal::reason))),
            )
            .await?;
        }
        Ok(true)
    }
}
