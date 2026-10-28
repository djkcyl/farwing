//! SESSION requests awaiting the host's answer: one table, each entry with
//! its own deadline. An answer, a failed send, a timeout or a closed channel
//! settles an entry exactly once.
use super::wire::SessionBody;
use super::{OutgoingControlMessage, StreamControlHandle, StreamControlState, protocol};
use crate::i18n::t;
use anyhow::{Result, anyhow};
use std::collections::BTreeMap;
use std::time::{Duration, Instant};

/// What an answer settles.
pub(super) enum Waiting {
    /// A `Capture` request of an awaiting caller.
    Capture(tokio::sync::oneshot::Sender<std::result::Result<(), String>>),
    /// A mode or scale change of display `id`.
    Display(i32),
    /// The display topology operation.
    Topology,
}

#[derive(Default)]
pub(super) struct Requests {
    pending: BTreeMap<u32, (Waiting, Instant)>,
}

impl Requests {
    fn insert(&mut self, id: u32, waiting: Waiting, timeout: Duration) {
        self.pending.insert(id, (waiting, Instant::now() + timeout));
    }
    pub(super) fn take(&mut self, id: u32) -> Option<Waiting> {
        self.pending.remove(&id).map(|(waiting, _)| waiting)
    }
    /// The entries past their deadline, removed.
    pub(super) fn expired(&mut self) -> Vec<Waiting> {
        let now = Instant::now();
        let ids: Vec<_> = self
            .pending
            .iter()
            .filter(|(_, (_, deadline))| *deadline <= now)
            .map(|(id, _)| *id)
            .collect();
        ids.into_iter().filter_map(|id| self.take(id)).collect()
    }
    /// Every entry, removed (the channel closed).
    pub(super) fn drain(&mut self) -> Vec<Waiting> {
        std::mem::take(&mut self.pending)
            .into_values()
            .map(|(waiting, _)| waiting)
            .collect()
    }
}

impl StreamControlHandle {
    /// Send `body` as a request; `waiting` is settled by its answer, or
    /// with an error after `timeout`.
    pub(super) fn send_request(
        &self,
        state: &mut StreamControlState,
        body: SessionBody,
        waiting: Waiting,
        timeout: Duration,
    ) -> Result<i64> {
        let sequence = state.next_sequence;
        state.next_sequence = state.next_sequence.wrapping_add(1);
        self.outgoing
            .send(OutgoingControlMessage {
                annotation_generation: None,
                sequence,
                payload: super::wire::request(sequence, body),
                protocol: protocol(state),
                completion: None,
            })
            .map_err(|_| anyhow!("{}", t().features.stream_settings_sender_stopped))?;
        state
            .requests
            .insert(super::wire::request_id(sequence), waiting, timeout);
        Ok(sequence)
    }

    /// Settle `waiting` with the answer (`Ok`) or why it failed.
    pub(super) fn settle(
        &self,
        state: &mut StreamControlState,
        waiting: Waiting,
        result: std::result::Result<(), String>,
    ) {
        match waiting {
            Waiting::Capture(caller) => {
                let _ = caller.send(result);
            }
            Waiting::Display(id) => state.display_changes.settle(id, result.err()),
            Waiting::Topology => self.topology_settled(state, result),
        }
    }

    /// Settle the requests whose deadline passed.
    pub(super) fn expire_requests(&self, state: &mut StreamControlState) {
        for waiting in state.requests.expired() {
            self.settle(state, waiting, Err(t().features.stream_request_timeout.into()));
        }
    }
}
