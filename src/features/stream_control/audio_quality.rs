//! Per-connection desktop-audio quality; never changes the host's saved defaults.
use super::*;
use crate::{
    media::audio::encoder::Quality,
    protocol::audio_control::{self, Message},
};
use std::time::{Duration, Instant};

pub(super) struct State {
    pub available: bool,
    desired: Option<Quality>,
    confirmed: Option<Quality>,
    effective_bps: u32,
    revision: u64,
    next: u64,
    pending: Option<Pending>,
    error: Option<String>,
}
struct Pending {
    id: u64,
    quality: Quality,
    at: Instant,
    sent: Option<tokio::sync::oneshot::Receiver<std::result::Result<(), String>>>,
}
pub(crate) struct Snapshot {
    pub available: bool,
    pub quality: Quality,
    pub effective_bps: u32,
    pub pending: bool,
    pub error: Option<String>,
}
impl State {
    pub fn new(audio_only: bool) -> Self {
        Self {
            available: false,
            desired: audio_only.then_some(Quality { kbps: 256 }),
            confirmed: None,
            effective_bps: 0,
            revision: 0,
            next: 1,
            pending: None,
            error: None,
        }
    }
    fn poll(&mut self) {
        let Some(pending) = &mut self.pending else {
            return;
        };
        let failure = if pending.at.elapsed() > Duration::from_secs(8) {
            Some("被控端未确认音质设置".into())
        } else if let Some(sent) = &mut pending.sent {
            match sent.try_recv() {
                Ok(Ok(())) => {
                    pending.sent = None;
                    None
                }
                Ok(Err(error)) => Some(error),
                Err(tokio::sync::oneshot::error::TryRecvError::Closed) => {
                    Some("音质请求发送已取消".into())
                }
                Err(tokio::sync::oneshot::error::TryRecvError::Empty) => None,
            }
        } else {
            None
        };
        if let Some(error) = failure {
            self.pending = None;
            self.error = Some(error);
            self.desired = self.confirmed;
        }
    }
}
impl StreamControlHandle {
    pub(crate) fn audio_quality_snapshot(&self) -> Snapshot {
        let mut state = lock(&self.audio_quality);
        state.poll();
        Snapshot {
            available: state.available,
            quality: state
                .pending
                .as_ref()
                .map(|p| p.quality)
                .or(state.confirmed)
                .unwrap_or_default(),
            effective_bps: state.effective_bps,
            pending: state.pending.is_some(),
            error: state.error.clone(),
        }
    }
    pub(crate) fn set_audio_quality(&self, quality: Quality) -> Result<()> {
        quality.validate()?;
        let mut state = lock(&self.audio_quality);
        state.poll();
        anyhow::ensure!(state.available, "被控端不支持远程音质切换");
        anyhow::ensure!(state.pending.is_none(), "正在等待音质设置确认");
        state.desired = Some(quality);
        if let Err(error) = self.send_audio_quality(&mut state) {
            state.desired = state.confirmed;
            state.error = Some(error.to_string());
            return Err(error);
        }
        Ok(())
    }
    pub(super) fn default_audio_quality(&self) {
        let mut state = lock(&self.audio_quality);
        if state.desired.is_none() {
            state.desired = Some(Quality { kbps: 256 });
        }
        if state.available && state.pending.is_none() {
            let _ = self.send_audio_quality(&mut state);
        }
    }
    fn send_audio_quality(&self, state: &mut State) -> Result<()> {
        let Some(quality) = state.desired else {
            return Ok(());
        };
        if state.confirmed == Some(quality) {
            return Ok(());
        }
        let id = state.next;
        state.next = state.next.wrapping_add(1);
        let (done, sent) = tokio::sync::oneshot::channel();
        self.outgoing
            .send(OutgoingControlMessage {
                annotation_generation: None,
                sequence: 0,
                payload: audio_control::encode(Message::Set {
                    request: id,
                    kbps: quality.kbps,
                }),
                protocol: StreamControlProtocol::CaptureSetting,
                completion: Some(done),
            })
            .map_err(|_| anyhow!("会话已结束"))?;
        state.pending = Some(Pending {
            id,
            quality,
            at: Instant::now(),
            sent: Some(sent),
        });
        state.error = None;
        Ok(())
    }
    pub(super) fn disconnect_audio_quality(&self) {
        let mut state = lock(&self.audio_quality);
        state.available = false;
        state.revision = 0;
        state.pending = None;
        state.confirmed = None;
        state.effective_bps = 0;
        state.error = None;
    }
    pub(super) fn handle_audio_quality(&self, bytes: &[u8]) -> Result<bool> {
        let Some(message) = audio_control::decode(bytes)? else {
            return Ok(false);
        };
        let mut state = lock(&self.audio_quality);
        match message {
            Message::Hello {
                revision,
                kbps,
                effective_bps,
            } => {
                let quality = Quality { kbps };
                quality.validate()?;
                anyhow::ensure!(
                    effective_bps > 0 && effective_bps <= kbps * 1000,
                    "invalid audio rate"
                );
                if revision < state.revision {
                    return Ok(true);
                }
                let first = !state.available;
                state.available = true;
                state.revision = revision;
                state.confirmed = Some(quality);
                state.effective_bps = effective_bps;
                if state.pending.is_none() {
                    if first {
                        self.send_audio_quality(&mut state)?;
                    } else if state.desired.is_some() {
                        state.desired = Some(quality);
                    }
                }
            }
            Message::Applied {
                request,
                revision,
                kbps,
                effective_bps,
            } => {
                if state
                    .pending
                    .as_ref()
                    .is_some_and(|p| p.id == request && p.quality.kbps == kbps)
                {
                    anyhow::ensure!(
                        effective_bps > 0 && effective_bps <= kbps * 1000,
                        "invalid audio rate"
                    );
                    if revision >= state.revision {
                        state.revision = revision;
                        state.confirmed = Some(Quality { kbps });
                        state.effective_bps = effective_bps;
                    }
                    state.pending = None;
                    state.error = None;
                }
            }
            Message::Rejected { request, message } => {
                if state.pending.as_ref().is_some_and(|p| p.id == request) {
                    state.pending = None;
                    state.error = Some(message);
                    state.desired = state.confirmed;
                }
            }
            Message::Set { .. } => anyhow::bail!("unexpected audio request on viewer"),
        }
        Ok(true)
    }
}
