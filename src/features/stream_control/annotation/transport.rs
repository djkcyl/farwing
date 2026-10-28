//! `ANNOTATION` requests and the host's answers. Editor state only observes
//! completed effects.
use super::*;
use crate::features::stream_control::wire::request_id;
use crate::protocol::annotation::{self as ann, Op};

pub(super) struct InFlight {
    pub effect: Pending,
    pub sent: Instant,
}

/// The host's refusal `reason`, worded here.
pub(super) fn refusal_text(reason: &str) -> String {
    let t = t();
    if reason == ann::reason::SIGNED_OUT.1 {
        t.features.annotation_host_signed_out
    } else if reason == ann::reason::LOCKED.1 {
        t.features.annotation_host_locked
    } else {
        t.features.annotation_rejected
    }
    .into()
}

impl StreamControlHandle {
    /// An `ANNOTATION` frame: the answer to one of our requests, or the
    /// host's `Reset`.
    pub(crate) fn handle_annotation(&self, bytes: &[u8]) -> Result<bool> {
        let Some(received) = ann::decode(bytes)? else {
            return Ok(false);
        };
        let mut s = lock(&self.shared);
        if !s.text_channel_open {
            return Ok(true);
        }
        if received.response {
            let result = received.body.map(drop).map_err(|error| error.reason);
            s.annotation.complete(received.request_id, result);
        } else if matches!(
            received.body,
            Ok(ann::Annotation {
                op: Some(Op::Reset(_))
            })
        ) && (s.annotation.enabled || s.annotation.toggling() || s.annotation.uncertain)
        {
            tracing::info!("the host dropped its annotation overlay");
            s.annotation.disconnect();
        }
        drop(s);
        self.mouse.repaint();
        Ok(true)
    }
    /// Send `op`; `effect` completes with the host's answer.
    pub(super) fn send_op(&self, s: &mut StreamControlState, op: Op, effect: Pending) -> Result<()> {
        ensure_ready(s)?;
        if !annotation_supported(s) {
            bail!("{}", t().features.annotation_unsupported);
        }
        anyhow::ensure!(
            s.annotation.pending.len() < MAX_PENDING,
            "{}",
            t().features.annotation_requests_pending
        );
        let sequence = s.next_sequence;
        let id = request_id(sequence);
        let payload = ann::request(id, op)?;
        s.next_sequence = sequence.wrapping_add(1);
        s.annotation.pending.insert(
            id,
            InFlight {
                effect,
                sent: Instant::now(),
            },
        );
        let outgoing = OutgoingControlMessage {
            sequence,
            payload,
            protocol: protocol(s),
            completion: None,
            annotation_generation: Some(s.annotation.generation),
        };
        if self.outgoing.send(outgoing).is_err() {
            s.annotation
                .uncertain(t().features.annotation_sender_stopped.into());
            bail!("{}", t().features.annotation_sender_stopped);
        }
        Ok(())
    }
    pub(crate) fn annotation_message_current(&self, sequence: i64, generation: u64) -> bool {
        let s = lock(&self.shared);
        s.annotation.generation == generation
            && s.annotation.pending.contains_key(&request_id(sequence))
    }
    pub(crate) fn annotation_send_failed(&self, sequence: i64, error: &str) {
        let mut s = lock(&self.shared);
        if let Some(p) = s.annotation.pending.remove(&request_id(sequence)) {
            if let Pending::Toggle(enable) = p.effect {
                s.annotation.enabled = !enable;
            }
            s.annotation
                .uncertain((t().features.annotation_send_failed)(error));
        }
    }
}
