//! What one server sends for sessions: the relay's packets to clients and
//! peers elsewhere, the forwarder's packets to viewers
//! (`docs/protocol/egress-policy.md` §2).

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};

/// Counted per packet on top of its payload: IPv4, UDP and the TURN or
/// SRTP framing, roughly.
const OVERHEAD: u64 = 36;
/// A session's counters are dropped this many snapshots (seconds) after
/// its last packet once nothing refers to them.
const IDLE: u32 = 60;

/// One session's egress on this server.
pub struct SessionGauge {
    relay: AtomicU64,
    forward: AtomicU64,
    /// The snapshot this gauge last sent something in.
    active: AtomicU32,
    meters: Arc<Shared>,
}

#[derive(Default)]
struct Shared {
    total: AtomicU64,
    /// Snapshots taken so far.
    tick: AtomicU32,
}

impl SessionGauge {
    fn count(&self, counter: &AtomicU64, bytes: usize) -> bool {
        let bytes = bytes as u64 + OVERHEAD;
        counter.fetch_add(bytes, Ordering::Relaxed);
        self.meters.total.fetch_add(bytes, Ordering::Relaxed);
        self.active
            .store(self.meters.tick.load(Ordering::Relaxed), Ordering::Relaxed);
        true
    }

    /// The relay sends `bytes` of payload for this session; false: drop.
    pub fn relayed(&self, bytes: usize) -> bool {
        self.count(&self.relay, bytes)
    }

    /// The forwarder sends `bytes` (a packet) to this session's viewer.
    pub fn forwarded(&self, bytes: usize) -> bool {
        self.count(&self.forward, bytes)
    }
}

/// Bytes sent so far, as a node reports them each second.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Usage {
    /// Everything this server sent for sessions.
    pub total: u64,
    pub sessions: Vec<SessionUsage>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct SessionUsage {
    pub session: String,
    pub relay: u64,
    pub forward: u64,
}

/// The gauges of this server's sessions.
#[derive(Default)]
pub struct Meters {
    shared: Arc<Shared>,
    sessions: parking_lot::Mutex<HashMap<String, Arc<SessionGauge>>>,
}

impl Meters {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// Session `id`'s gauge (created on first use).
    pub fn session(&self, id: &str) -> Arc<SessionGauge> {
        let mut sessions = self.sessions.lock();
        if let Some(gauge) = sessions.get(id) {
            return Arc::clone(gauge);
        }
        let gauge = Arc::new(SessionGauge {
            relay: AtomicU64::new(0),
            forward: AtomicU64::new(0),
            active: AtomicU32::new(self.shared.tick.load(Ordering::Relaxed)),
            meters: Arc::clone(&self.shared),
        });
        sessions.insert(id.to_owned(), Arc::clone(&gauge));
        gauge
    }

    /// The counters now; drops those of sessions gone quiet that no
    /// allocation or viewer connection holds any more.
    pub fn usage(&self) -> Usage {
        let tick = self.shared.tick.fetch_add(1, Ordering::Relaxed).wrapping_add(1);
        let mut sessions = self.sessions.lock();
        sessions.retain(|_, g| {
            Arc::strong_count(g) > 1 || tick.wrapping_sub(g.active.load(Ordering::Relaxed)) < IDLE
        });
        Usage {
            total: self.shared.total.load(Ordering::Relaxed),
            sessions: sessions
                .iter()
                .map(|(id, g)| SessionUsage {
                    session: id.clone(),
                    relay: g.relay.load(Ordering::Relaxed),
                    forward: g.forward.load(Ordering::Relaxed),
                })
                .collect(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counts_per_session_and_in_total() {
        let meters = Meters::new();
        let a = meters.session("a");
        assert!(a.relayed(1000));
        assert!(meters.session("a").forwarded(500));
        assert!(meters.session("b").relayed(64));
        let usage = meters.usage();
        assert_eq!(usage.total, 1000 + 500 + 64 + 3 * OVERHEAD);
        let a = usage.sessions.iter().find(|s| s.session == "a").unwrap();
        assert_eq!((a.relay, a.forward), (1000 + OVERHEAD, 500 + OVERHEAD));
    }

    #[test]
    fn quiet_unheld_sessions_are_dropped() {
        let meters = Meters::new();
        let held = meters.session("held");
        drop(meters.session("quiet"));
        for _ in 0..IDLE {
            meters.usage();
        }
        let left: Vec<String> = meters.usage().sessions.into_iter().map(|s| s.session).collect();
        assert_eq!(left, vec!["held".to_owned()]);
        drop(held);
    }
}
