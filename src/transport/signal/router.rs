//! Direct participants inside the host's signalling loop
//! (`docs/protocol/v2-design.md` §12.2).
//!
//! A direct connection presents itself to the host session like the server
//! does: it injects `session.incoming`, `rtc.*` and `session.ended` envelopes,
//! and receives every envelope the host sends with its session ID. Server and
//! direct participants share one host session and one display layout.

use farwing_protocol::realtime::Envelope;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use tokio::sync::mpsc;

pub(crate) struct Router {
    inbound_tx: mpsc::UnboundedSender<Envelope>,
    inbound: tokio::sync::Mutex<mpsc::UnboundedReceiver<Envelope>>,
    routes: Mutex<HashMap<String, mpsc::UnboundedSender<Envelope>>>,
    /// UDP ports of direct sessions (`port .. port + UDP_PORTS - 1`).
    udp_ports: Mutex<Option<(u16, u16)>>,
    /// PAKE keys of paired or assisted direct sessions, by session.
    keys: Mutex<HashMap<String, [u8; 32]>>,
}

impl Router {
    pub(crate) fn new() -> Arc<Self> {
        let (inbound_tx, inbound) = mpsc::unbounded_channel();
        Arc::new(Self {
            inbound_tx,
            inbound: tokio::sync::Mutex::new(inbound),
            routes: Mutex::new(HashMap::new()),
            udp_ports: Mutex::new(None),
            keys: Mutex::new(HashMap::new()),
        })
    }

    /// The direct endpoint's port: direct sessions use the UDP ports from it.
    pub(crate) fn set_port(&self, port: Option<u16>) {
        *self.udp_ports.lock().unwrap_or_else(|e| e.into_inner()) = port.map(|port| {
            (
                port,
                port.saturating_add(farwing_protocol::direct::UDP_PORTS - 1),
            )
        });
    }

    /// The UDP ports of session `sid` when it is direct.
    pub(crate) fn session_ports(&self, sid: &str) -> Option<(u16, u16)> {
        if !self.is_direct(sid) {
            return None;
        }
        *self.udp_ports.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub(crate) fn set_session_key(&self, sid: &str, key: [u8; 32]) {
        self.keys
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(sid.to_owned(), key);
    }

    pub(crate) fn take_session_key(&self, sid: &str) -> Option<[u8; 32]> {
        self.keys
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(sid)
    }

    fn routes(&self) -> std::sync::MutexGuard<'_, HashMap<String, mpsc::UnboundedSender<Envelope>>> {
        self.routes.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Deliver the host's envelopes of session `sid` to a direct connection.
    pub(crate) fn register(&self, sid: &str, sender: mpsc::UnboundedSender<Envelope>) {
        self.routes().insert(sid.to_owned(), sender);
    }

    pub(crate) fn unregister(&self, sid: &str) {
        self.routes().remove(sid);
        self.keys.lock().unwrap_or_else(|e| e.into_inner()).remove(sid);
    }

    pub(crate) fn is_direct(&self, sid: &str) -> bool {
        self.routes().contains_key(sid)
    }

    /// Hand an envelope to the host session.
    pub(crate) fn inject(&self, envelope: Envelope) {
        let _ = self.inbound_tx.send(envelope);
    }

    /// Send a host envelope to its direct connection; gives it back when its
    /// session is not direct.
    pub(crate) fn route(&self, envelope: Envelope) -> Result<(), Envelope> {
        let Some(sid) = envelope.sid.as_deref() else {
            return Err(envelope);
        };
        let sender = self.routes().get(sid).cloned();
        match sender {
            Some(sender) => {
                // A closed connection drops its late envelopes.
                let _ = sender.send(envelope);
                Ok(())
            }
            None => Err(envelope),
        }
    }

    /// The next envelope from a direct connection.
    pub(crate) async fn recv(&self) -> Option<Envelope> {
        self.inbound.lock().await.recv().await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use farwing_protocol::realtime::types;

    #[tokio::test]
    async fn routes_by_session() {
        let router = Router::new();
        let (tx, mut rx) = mpsc::unbounded_channel();
        router.register("s1", tx);
        assert!(router.is_direct("s1"));
        assert!(router.route(Envelope::new(types::RTC_ANSWER, ()).with_sid("s1")).is_ok());
        assert_eq!(rx.recv().await.unwrap().kind, types::RTC_ANSWER);
        assert!(router.route(Envelope::new(types::RTC_ANSWER, ()).with_sid("s2")).is_err());
        assert!(router.route(Envelope::new(types::PING, ())).is_err());
        router.inject(Envelope::new(types::SESSION_INCOMING, ()).with_sid("s1"));
        assert_eq!(router.recv().await.unwrap().kind, types::SESSION_INCOMING);
        router.unregister("s1");
        assert!(!router.is_direct("s1"));
    }
}
