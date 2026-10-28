//! Per-connection desktop-audio quality; never changes the host's saved defaults.
use super::*;
use crate::{
    media::audio::encoder::Quality,
    protocol::audio_control::{self, Body},
};
use std::time::{Duration, Instant};

pub(super) struct State {
    pub available: bool,
    desired: Option<Quality>,
    confirmed: Option<Quality>,
    effective_bps: u32,
    revision: u64,
    next: std::sync::atomic::AtomicU32,
    pending: Option<Pending>,
    error: Option<String>,
}
struct Pending {
    id: u32,
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
            next: std::sync::atomic::AtomicU32::new(1),
            pending: None,
            error: None,
        }
    }
    fn poll(&mut self) {
        let Some(pending) = &mut self.pending else {
            return;
        };
        let failure = if pending.at.elapsed() > Duration::from_secs(8) {
            Some(t().features.stream_audio_quality_unconfirmed.into())
        } else if let Some(sent) = &mut pending.sent {
            match sent.try_recv() {
                Ok(Ok(())) => {
                    pending.sent = None;
                    None
                }
                Ok(Err(error)) => Some(error),
                Err(tokio::sync::oneshot::error::TryRecvError::Closed) => {
                    Some(t().features.stream_audio_quality_cancelled.into())
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
        anyhow::ensure!(
            state.available,
            "{}",
            t().features.stream_audio_quality_unsupported
        );
        anyhow::ensure!(
            state.pending.is_none(),
            "{}",
            t().features.stream_audio_quality_pending
        );
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
        let id = crate::protocol::peer::next_request_id(&state.next);
        let (done, sent) = tokio::sync::oneshot::channel();
        self.outgoing
            .send(OutgoingControlMessage {
                annotation_generation: None,
                sequence: 0,
                payload: audio_control::request(id, quality.kbps),
                completion: Some(done),
            })
            .map_err(|_| anyhow!("{}", t().features.stream_session_ended))?;
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
        let Some(received) = audio_control::decode(bytes)? else {
            return Ok(false);
        };
        let mut state = lock(&self.audio_quality);
        let message = match received.body {
            Ok(message) => message,
            Err(error) => {
                if state
                    .pending
                    .as_ref()
                    .is_some_and(|p| p.id == received.request_id)
                {
                    state.pending = None;
                    state.error = Some(audio_control::reason_text(&error.reason).to_owned());
                    state.desired = state.confirmed;
                }
                return Ok(true);
            }
        };
        let Some(Body::State(audio)) = message.body else {
            anyhow::bail!("unexpected audio request on viewer");
        };
        let quality = Quality { kbps: audio.kbps };
        quality.validate()?;
        anyhow::ensure!(
            audio.effective_bps > 0 && audio.effective_bps <= audio.kbps * 1000,
            "invalid audio rate"
        );
        if received.response {
            if !state
                .pending
                .as_ref()
                .is_some_and(|p| p.id == received.request_id)
            {
                return Ok(true);
            }
            state.pending = None;
            state.error = None;
        }
        if audio.revision < state.revision {
            return Ok(true);
        }
        let first = !state.available;
        state.available = true;
        state.revision = audio.revision;
        state.confirmed = Some(quality);
        state.effective_bps = audio.effective_bps;
        if state.pending.is_none() && !received.response {
            if first {
                self.send_audio_quality(&mut state)?;
            } else if state.desired.is_some() {
                state.desired = Some(quality);
            }
        }
        Ok(true)
    }
}
