//! An asynchronous STUN request manager. Waiting never owns the media sender.

use std::collections::HashMap;
use std::net::{Ipv4Addr, SocketAddr};
use std::sync::{Arc, Mutex, Weak};

use stun::integrity::MessageIntegrity;
use stun::message::*;
use tokio::sync::oneshot;
use tokio::time::Duration;
use tokio_util::{sync::CancellationToken, task::TaskTracker};
use util::Conn;

use crate::error::*;

const MAX_SENDS: u16 = 9;
const MAX_INTERVAL_MS: u64 = 8000;
pub const DEFAULT_RTO_MS: u16 = 250;
type RequestId = [u8; 12];

#[derive(Debug)]
pub struct TransactionResult {
    pub msg: Message,
    pub from: SocketAddr,
    pub retries: u16,
}

impl Default for TransactionResult {
    fn default() -> Self {
        Self {
            msg: Message::default(),
            from: (Ipv4Addr::UNSPECIFIED, 0).into(),
            retries: 0,
        }
    }
}

enum Outcome {
    Response(Result<TransactionResult>),
    // The request is removed without invoking any response callback.
    Discarded,
}

struct PendingRequest {
    method: Method,
    destination: SocketAddr,
    integrity: Option<MessageIntegrity>,
    retries: u16,
    cancel: CancellationToken,
    result: Option<oneshot::Sender<Outcome>>,
}

impl Drop for PendingRequest {
    fn drop(&mut self) {
        self.cancel.cancel();
    }
}

pub struct Transaction {
    id: RequestId,
    manager: Weak<TransactionManager>,
    receiver: Option<oneshot::Receiver<Outcome>>,
    first_send: Option<oneshot::Receiver<()>>,
    closed: CancellationToken,
    remove_on_drop: bool,
}

impl Transaction {
    /// Wait for the initial send attempt, not its server response. A zero
    /// delay sends synchronously, before the caller sends peer data.
    pub async fn wait_first_send(&mut self) -> Result<()> {
        if let Some(first_send) = self.first_send.take() {
            tokio::select! {
                biased;
                _ = self.closed.cancelled() => return Err(Error::ErrTransactionClosed),
                result = first_send => { result.map_err(|_| Error::ErrTransactionClosed)?; }
            }
        }
        Ok(())
    }

    pub async fn wait(mut self) -> Result<TransactionResult> {
        self.wait_first_send().await?;
        let Some(receiver) = self.receiver.take() else {
            return Ok(TransactionResult::default());
        };
        tokio::select! {
            biased;
            _ = self.closed.cancelled() => Err(Error::ErrTransactionClosed),
            result = receiver => match result {
                Ok(Outcome::Response(result)) => result,
                Ok(Outcome::Discarded) => {
                    self.closed.cancelled().await;
                    Err(Error::ErrTransactionClosed)
                }
                Err(_) => Err(Error::ErrTransactionClosed),
            },
        }
    }
}

impl Drop for Transaction {
    fn drop(&mut self) {
        if self.remove_on_drop {
            if let Some(manager) = self.manager.upgrade() {
                manager
                    .pending
                    .lock()
                    .expect("TURN request mutex poisoned")
                    .remove(&self.id);
            }
        }
    }
}

pub struct TransactionManager {
    pending: Mutex<HashMap<RequestId, PendingRequest>>,
    tasks: TaskTracker,
    closed: CancellationToken,
}

impl Drop for TransactionManager {
    fn drop(&mut self) {
        self.closed.cancel();
    }
}

impl TransactionManager {
    pub fn new(closed: CancellationToken) -> Self {
        Self {
            pending: Mutex::new(HashMap::new()),
            tasks: TaskTracker::new(),
            closed,
        }
    }

    pub fn start(
        self: &Arc<Self>,
        conn: Arc<dyn Conn + Send + Sync>,
        msg: &Message,
        destination: SocketAddr,
        integrity: Option<MessageIntegrity>,
        initial_rto: u16,
        ignore_result: bool,
    ) -> Result<Transaction> {
        self.start_after(
            conn,
            msg,
            destination,
            integrity,
            initial_rto,
            ignore_result,
            Duration::ZERO,
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub fn start_after(
        self: &Arc<Self>,
        conn: Arc<dyn Conn + Send + Sync>,
        msg: &Message,
        destination: SocketAddr,
        integrity: Option<MessageIntegrity>,
        initial_rto: u16,
        ignore_result: bool,
        initial_delay: Duration,
    ) -> Result<Transaction> {
        let mut pending = self.pending.lock().expect("TURN request mutex poisoned");
        if self.closed.is_cancelled() {
            return Err(Error::ErrTransactionClosed);
        }
        let id = msg.transaction_id.0;
        if pending.contains_key(&id) {
            return Err(Error::Other("duplicate TURN transaction ID".to_owned()));
        }
        let cancel = self.closed.child_token();
        let (result, receiver) = if ignore_result {
            (None, None)
        } else {
            let (tx, rx) = oneshot::channel();
            (Some(tx), Some(rx))
        };
        pending.insert(
            id,
            PendingRequest {
                method: msg.typ.method,
                destination,
                integrity,
                retries: 0,
                cancel: cancel.clone(),
                result,
            },
        );
        let manager = Arc::downgrade(self);
        let task_manager = manager.clone();
        let raw = msg.raw.clone();
        let typ = msg.typ;
        let transport_closed = self.closed.clone();
        let (first_sent, first_send) = oneshot::channel();
        // Serialize registration with close's drain. Tasks have no strong cycle.
        self.tasks.spawn(async move {
            let mut first_sent = Some(first_sent);
            if !initial_delay.is_zero() {
                tokio::select! {
                    biased;
                    _ = cancel.cancelled() => return,
                    _ = tokio::time::sleep(initial_delay) => {},
                }
            }
            let mut delay = u64::from(initial_rto).min(MAX_INTERVAL_MS);
            for send_index in 0..MAX_SENDS {
                if let Some(manager) = task_manager.upgrade() {
                    if let Some(request) = manager
                        .pending
                        .lock()
                        .expect("TURN request mutex poisoned")
                        .get_mut(&id)
                    {
                        request.retries = send_index;
                    } else {
                        return;
                    }
                } else {
                    return;
                }
                tokio::select! {
                    biased;
                    // A response/cancel may stop retries, but must not truncate
                    // a TCP/TLS frame already being written. Only final client
                    // shutdown may abandon an in-flight transport write.
                    _ = transport_closed.cancelled() => return,
                    result = conn.send_to(&raw, destination) => {
                        // Failed writes are logged without ending the request.
                        if let Err(error) = result {
                            log::warn!("TURN {typ} write to {destination} failed: {error}");
                        }
                    }
                }
                if let Some(first_sent) = first_sent.take() {
                    let _ = first_sent.send(());
                }
                log::trace!("TURN {typ} to {destination}, send={}", send_index + 1);
                tokio::select! {
                    biased;
                    _ = cancel.cancelled() => return,
                    _ = tokio::time::sleep(Duration::from_millis(delay)) => {},
                }
                delay = (delay * 2).min(MAX_INTERVAL_MS);
            }
            if let Some(manager) = task_manager.upgrade() {
                manager.finish(
                    &id,
                    Outcome::Response(Err(Error::ErrAllRetransmissionsFailed)),
                );
            }
        });
        Ok(Transaction {
            id,
            manager,
            receiver,
            first_send: Some(first_send),
            closed: self.closed.clone(),
            remove_on_drop: !ignore_result,
        })
    }

    fn finish(&self, id: &RequestId, outcome: Outcome) {
        if let Some(mut request) = self
            .pending
            .lock()
            .expect("TURN request mutex poisoned")
            .remove(id)
        {
            if let Some(result) = request.result.take() {
                let _ = result.send(outcome);
            }
        }
    }

    pub fn handle_response(&self, msg: Message, from: SocketAddr) {
        let id = msg.transaction_id.0;
        let mut pending = self.pending.lock().expect("TURN request mutex poisoned");
        let Some(request) = pending.get(&id) else {
            return;
        };
        if from != request.destination {
            log::debug!("discarded TURN response from unexpected source {from}");
            return;
        }
        // Unknown attributes are preserved only when bit 0x4000 is set, then
        // the retained, non-optional ones are rejected. Unknown 0x0000..0x3fff
        // attributes are skipped during parsing, not rejected.
        // None of the deployed base/TURN attribute tables defines 0x4000..0x7fff.
        if msg.attributes.0.iter().any(|a| a.typ.0 & 0xc000 == 0x4000) {
            let mut request = pending.remove(&id).expect("request still present");
            log::warn!("TURN response contains unknown required attribute; request discarded");
            if let Some(result) = request.result.take() {
                let _ = result.send(Outcome::Discarded);
            }
            return;
        }
        if msg.typ.method != request.method
            || !matches!(msg.typ.class, CLASS_SUCCESS_RESPONSE | CLASS_ERROR_RESPONSE)
        {
            log::debug!("discarded TURN response with wrong type {}", msg.typ);
            return;
        }
        if msg.typ.class == CLASS_SUCCESS_RESPONSE {
            if let Some(integrity) = &request.integrity {
                if !super::integrity::verify(&msg, &integrity.0) {
                    log::warn!("discarded TURN success response with missing/invalid integrity");
                    return;
                }
            }
        }
        let mut request = pending.remove(&id).expect("request still present");
        if let Some(result) = request.result.take() {
            let _ = result.send(Outcome::Response(Ok(TransactionResult {
                msg,
                from,
                retries: request.retries,
            })));
        }
    }

    pub async fn close(&self) {
        self.closed.cancel();
        self.pending
            .lock()
            .expect("TURN request mutex poisoned")
            .clear();
        self.tasks.close();
        self.tasks.wait().await;
        log::debug!("TURN request tasks joined");
    }

    #[cfg(test)]
    pub(super) fn size(&self) -> usize {
        self.pending
            .lock()
            .expect("TURN request mutex poisoned")
            .len()
    }
}
