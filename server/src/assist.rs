//! In-memory remote-assistance requests (PAKE exchange relayed between the
//! controller and the host; the server never sees the code).
//!
//! Life cycle of a request:
//! 1. `join` pushes `assist.join` and waits for the host's `pake` reply
//!    ([`Stage::AwaitPake`]).
//! 2. The controller checks the host's key confirmations locally and calls
//!    `accept` ([`Stage::AwaitAccept`]).
//! 3. `accept` pushes `assist.accept` and waits for the host's `decision`
//!    ([`Stage::Deciding`]).

use farwing_protocol::api::{AssistDecision, AssistMode, AssistPakeReply};
use parking_lot::Mutex;
use std::collections::HashMap;
use std::time::{Duration, Instant};
use tokio::sync::oneshot;

/// Who asked and whom.
#[derive(Clone, Debug)]
pub struct Parties {
    pub host_device_id: String,
    /// The host's short number (empty when it has none).
    pub connect_id: String,
    /// What the controller asked for: the short number or the device ID.
    pub target: String,
    pub controller_user_id: String,
    pub controller_device_id: String,
}

pub enum DecisionOutcome {
    Decision(AssistDecision),
    Cancelled,
}

enum Stage {
    AwaitPake(Option<oneshot::Sender<AssistPakeReply>>),
    AwaitAccept,
    Deciding(Option<oneshot::Sender<DecisionOutcome>>),
}

struct Pending {
    parties: Parties,
    mode: Option<AssistMode>,
    stage: Stage,
    expires: Instant,
}

#[derive(Debug, PartialEq, Eq)]
pub enum ReplyError {
    NotFound,
    WrongStage,
}

#[derive(Default)]
pub struct AssistRegistry {
    pending: Mutex<HashMap<String, Pending>>,
}

/// How long a request waits for the controller's `accept` after the host
/// answered the PAKE.
const AWAIT_ACCEPT_TTL: Duration = Duration::from_secs(120);
/// Grace added to waits so the waiter, not the sweep, decides the timeout.
const GRACE: Duration = Duration::from_secs(5);

impl AssistRegistry {
    /// Register a new request waiting for the host's PAKE reply.
    pub fn begin(
        &self,
        request_id: &str,
        parties: Parties,
        wait: Duration,
    ) -> oneshot::Receiver<AssistPakeReply> {
        let (tx, rx) = oneshot::channel();
        self.pending.lock().insert(
            request_id.to_owned(),
            Pending {
                parties,
                mode: None,
                stage: Stage::AwaitPake(Some(tx)),
                expires: Instant::now() + wait + GRACE,
            },
        );
        rx
    }

    /// The host answered: wait for the controller's `accept`.
    pub fn await_accept(&self, request_id: &str, mode: AssistMode) {
        if let Some(p) = self.pending.lock().get_mut(request_id) {
            p.mode = Some(mode);
            p.stage = Stage::AwaitAccept;
            p.expires = Instant::now() + AWAIT_ACCEPT_TTL;
        }
    }

    /// Start waiting for the host's decision. The request must belong to
    /// the same controller and wait for `accept`.
    pub fn begin_decision(
        &self,
        request_id: &str,
        controller_user_id: &str,
        controller_device_id: &str,
        wait: Duration,
    ) -> Option<(Parties, oneshot::Receiver<DecisionOutcome>)> {
        let mut map = self.pending.lock();
        let p = map.get_mut(request_id)?;
        if p.parties.controller_user_id != controller_user_id
            || p.parties.controller_device_id != controller_device_id
            || !matches!(p.stage, Stage::AwaitAccept)
        {
            return None;
        }
        let (tx, rx) = oneshot::channel();
        p.stage = Stage::Deciding(Some(tx));
        p.expires = Instant::now() + wait + GRACE;
        Some((p.parties.clone(), rx))
    }

    pub fn remove(&self, request_id: &str) {
        self.pending.lock().remove(request_id);
    }

    fn deliver<T>(
        &self,
        request_id: &str,
        host_device_id: &str,
        take: impl FnOnce(&mut Stage) -> Option<Option<oneshot::Sender<T>>>,
        value: T,
    ) -> Result<(), ReplyError> {
        let mut map = self.pending.lock();
        let p = map
            .get_mut(request_id)
            .filter(|p| p.parties.host_device_id == host_device_id)
            .ok_or(ReplyError::NotFound)?;
        let tx = take(&mut p.stage).ok_or(ReplyError::WrongStage)?;
        match tx.map(|t| t.send(value).is_ok()) {
            Some(true) => Ok(()),
            // Already answered or the waiter gave up.
            _ => Err(ReplyError::NotFound),
        }
    }

    /// Deliver the host's PAKE reply. `host_device_id` must be the request's host.
    pub fn reply_pake(
        &self,
        request_id: &str,
        host_device_id: &str,
        reply: AssistPakeReply,
    ) -> Result<(), ReplyError> {
        self.deliver(
            request_id,
            host_device_id,
            |stage| match stage {
                Stage::AwaitPake(tx) => Some(tx.take()),
                _ => None,
            },
            reply,
        )
    }

    /// Deliver the host's decision. `host_device_id` must be the request's host.
    pub fn reply_decision(
        &self,
        request_id: &str,
        host_device_id: &str,
        decision: AssistDecision,
    ) -> Result<(), ReplyError> {
        self.deliver(
            request_id,
            host_device_id,
            |stage| match stage {
                Stage::Deciding(tx) => Some(tx.take()),
                _ => None,
            },
            DecisionOutcome::Decision(decision),
        )
    }

    /// Cancel a request of this controller. Returns the host to notify.
    pub fn cancel(
        &self,
        request_id: &str,
        controller_user_id: &str,
        controller_device_id: &str,
    ) -> Option<Parties> {
        let mut map = self.pending.lock();
        let p = map.get(request_id)?;
        if p.parties.controller_user_id != controller_user_id
            || p.parties.controller_device_id != controller_device_id
        {
            return None;
        }
        let mut p = map.remove(request_id)?;
        if let Stage::Deciding(tx) = &mut p.stage
            && let Some(tx) = tx.take()
        {
            let _ = tx.send(DecisionOutcome::Cancelled);
        }
        Some(p.parties)
    }

    /// Drop expired requests; returns `(request_id, host_device_id)` of those
    /// the host already answered, so it can be told to forget them.
    pub fn prune(&self) -> Vec<(String, String)> {
        let now = Instant::now();
        let mut expired = Vec::new();
        self.pending.lock().retain(|id, p| {
            let keep = p.expires > now;
            if !keep && !matches!(p.stage, Stage::AwaitPake(_)) {
                expired.push((id.clone(), p.parties.host_device_id.clone()));
            }
            keep
        });
        expired
    }

    /// Mode the host stated for a request.
    pub fn mode(&self, request_id: &str) -> Option<AssistMode> {
        self.pending.lock().get(request_id).and_then(|p| p.mode)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parties() -> Parties {
        Parties {
            host_device_id: "h".into(),
            connect_id: "123456789".into(),
            target: "123456789".into(),
            controller_user_id: "u".into(),
            controller_device_id: "c".into(),
        }
    }

    #[test]
    fn stages_and_ownership() {
        let reg = AssistRegistry::default();
        let mut rx = reg.begin("r", parties(), Duration::from_secs(1));
        let reply = AssistPakeReply {
            allowed: true,
            mode: AssistMode::Password,
            pake: vec![],
            key_confirm: vec![],
            trusted: false,
        };
        assert_eq!(
            reg.reply_pake("r", "other", reply.clone()),
            Err(ReplyError::NotFound)
        );
        let decision = AssistDecision {
            allowed: true,
            refusal: None,
            trusted: false,
        };
        assert_eq!(
            reg.reply_decision("r", "h", decision.clone()),
            Err(ReplyError::WrongStage)
        );
        reg.reply_pake("r", "h", reply.clone()).unwrap();
        assert!(rx.try_recv().is_ok());
        assert!(reg.begin_decision("r", "u", "c", Duration::from_secs(1)).is_none());
        reg.await_accept("r", AssistMode::Password);
        assert_eq!(reg.mode("r"), Some(AssistMode::Password));
        assert!(reg.begin_decision("r", "x", "c", Duration::from_secs(1)).is_none());
        let (_, mut drx) = reg.begin_decision("r", "u", "c", Duration::from_secs(1)).unwrap();
        reg.reply_decision("r", "h", decision).unwrap();
        assert!(matches!(drx.try_recv(), Ok(DecisionOutcome::Decision(_))));
        assert!(reg.cancel("r", "x", "c").is_none());
        assert!(reg.cancel("r", "u", "c").is_some());
        assert!(reg.cancel("r", "u", "c").is_none());
    }
}
