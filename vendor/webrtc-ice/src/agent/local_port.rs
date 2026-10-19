//! A logical Port is not an SDP Candidate and not an AllocationSequence socket.
//! Tracks publication and ready/prune/idle state transitions.
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicU8, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use crate::candidate::{Candidate, CandidatePair};
use std::time::Duration;
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;

pub(super) type LocalCandidate = Arc<dyn Candidate + Send + Sync>;

pub(super) const PORT_GATHERING: u8 = 0;
pub(super) const PORT_COMPLETE: u8 = 1;
pub(super) const PORT_ERROR: u8 = 2;
pub(super) const PORT_PRUNED: u8 = 3;
pub(super) const PORT_IDLE_NANOS: u64 = 44_750_000_000;

pub(super) struct LocalPort {
    /// UDPPort::CreateConnection always uses address index zero.
    /// Translated addresses are aliases, not additional receive/ICE owners.
    pub canonical: LocalCandidate,
    pub aliases: Mutex<Vec<LocalCandidate>>,
    pub learned: Mutex<Vec<LocalCandidate>>,
    pub gathering: AtomicU8,
    pub ready: AtomicBool,
    keep_alive: AtomicBool,
    idle_timeout_nanos: AtomicU64,
    pub pruned: AtomicBool,
    pub closed: CancellationToken,
    pub idle_changed: Notify,
    connections: AtomicUsize,
    empty_since: AtomicU64,
}

impl LocalPort {
    pub fn new(canonical: LocalCandidate, created_at: u64) -> Arc<Self> {
        Arc::new(Self {
            canonical,
            aliases: Mutex::new(Vec::new()),
            learned: Mutex::new(Vec::new()),
            gathering: AtomicU8::new(PORT_GATHERING),
            ready: AtomicBool::new(false),
            keep_alive: AtomicBool::new(false),
            idle_timeout_nanos: AtomicU64::new(PORT_IDLE_NANOS),
            pruned: AtomicBool::new(false),
            closed: CancellationToken::new(),
            idle_changed: Notify::new(),
            connections: AtomicUsize::new(0),
            empty_since: AtomicU64::new(created_at),
        })
    }

    pub fn complete(&self, failed: bool) {
        let _ = self.gathering.compare_exchange(
            PORT_GATHERING,
            if failed { PORT_ERROR } else { PORT_COMPLETE },
            Ordering::AcqRel,
            Ordering::Acquire,
        );
    }

    pub fn prune(&self) {
        self.gathering.store(PORT_PRUNED, Ordering::Release);
        self.pruned.store(true, Ordering::Release);
        self.idle_changed.notify_one();
        // No socket close here. Existing Connections continue on this Port.
    }

    pub fn can_create_outbound_connection(&self) -> bool {
        self.ready.load(Ordering::Acquire)
            && !self.pruned.load(Ordering::Acquire)
            && !self.closed.is_cancelled()
    }

    pub fn keep_alive_until_pruned(&self) {
        self.keep_alive.store(true, Ordering::Release);
        self.idle_changed.notify_one();
    }

    pub fn set_idle_timeout(&self, timeout: Duration) {
        self.idle_timeout_nanos.store(
            timeout.as_nanos().min(u128::from(u64::MAX)) as u64,
            Ordering::Release,
        );
        self.idle_changed.notify_one();
    }

    pub fn connection_added(&self) {
        self.connections.fetch_add(1, Ordering::Relaxed);
        self.idle_changed.notify_one();
    }

    pub fn connection_removed(&self) {
        if self.connections.fetch_sub(1, Ordering::Relaxed) == 1 {
            self.empty_since
                .store(CandidatePair::now_nanos(), Ordering::Relaxed);
            self.idle_changed.notify_one();
        }
    }

    pub fn idle_expired(&self) -> bool {
        (!self.keep_alive.load(Ordering::Acquire) || self.pruned.load(Ordering::Acquire))
            && self.connections.load(Ordering::Relaxed) == 0
            && CandidatePair::now_nanos().saturating_sub(self.empty_since.load(Ordering::Relaxed))
                >= self.idle_timeout_nanos.load(Ordering::Acquire)
    }

    pub fn idle_wait(&self) -> Option<Duration> {
        if (self.keep_alive.load(Ordering::Acquire) && !self.pruned.load(Ordering::Acquire))
            || self.connections.load(Ordering::Relaxed) != 0
        {
            return None;
        }
        let elapsed =
            CandidatePair::now_nanos().saturating_sub(self.empty_since.load(Ordering::Relaxed));
        Some(Duration::from_nanos(
            self.idle_timeout_nanos
                .load(Ordering::Acquire)
                .saturating_sub(elapsed),
        ))
    }

    pub fn contains(&self, candidate: &LocalCandidate) -> bool {
        Arc::ptr_eq(&self.canonical, candidate)
            || self.canonical.id() == candidate.id()
            || self
                .aliases
                .lock()
                .expect("port aliases poisoned")
                .iter()
                .any(|alias| alias.id() == candidate.id())
            || self
                .learned
                .lock()
                .expect("learned local candidates poisoned")
                .iter()
                .any(|local| local.id() == candidate.id())
    }

    pub fn candidate_at(&self, address: std::net::SocketAddr) -> Option<LocalCandidate> {
        if self.canonical.addr() == address {
            return Some(self.canonical.clone());
        }
        self.aliases
            .lock()
            .expect("port aliases poisoned")
            .iter()
            .find(|candidate| candidate.addr() == address)
            .cloned()
            .or_else(|| {
                self.learned
                    .lock()
                    .expect("learned local candidates poisoned")
                    .iter()
                    .find(|candidate| candidate.addr() == address)
                    .cloned()
            })
    }
}

impl Drop for LocalPort {
    fn drop(&mut self) {
        self.closed.cancel();
    }
}
