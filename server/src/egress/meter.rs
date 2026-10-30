//! What one server sends for sessions: the relay's packets to clients and
//! peers elsewhere, the forwarder's packets to viewers
//! (`docs/protocol/egress-policy.md` §2).

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::time::{Duration, Instant};

/// Counted per packet on top of its payload: IPv4, UDP and the TURN or
/// SRTP framing, roughly.
const OVERHEAD: u64 = 36;
/// A session's counters are dropped this many snapshots (seconds) after
/// its last packet once nothing refers to them.
const IDLE: u32 = 60;
/// An ended session's relay credentials stay valid this long (the TURN
/// credential lifetime): its traffic is refused as long.
const ENDED: Duration = Duration::from_secs(12 * 3600);

/// What a session may send through this server
/// (`docs/protocol/egress-policy.md` §3.4).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Rule {
    pub relay: bool,
    pub forward: bool,
}

impl Rule {
    pub const ALLOWED: Self = Self {
        relay: true,
        forward: true,
    };
    pub const NOTHING: Self = Self {
        relay: false,
        forward: false,
    };
}

/// A rule for one session, as the control node sends it to edge nodes.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RuleUpdate {
    pub session: String,
    pub rule: Rule,
    /// The session ended: the rule lapses after the credential lifetime.
    #[serde(default)]
    pub ended: bool,
}

/// One session's egress on this server.
pub struct SessionGauge {
    relay: AtomicU64,
    forward: AtomicU64,
    relay_allowed: AtomicBool,
    forward_allowed: AtomicBool,
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
        self.relay_allowed.load(Ordering::Relaxed) && self.count(&self.relay, bytes)
    }

    /// The forwarder sends `bytes` (a packet) to this session's viewer;
    /// false: drop.
    pub fn forwarded(&self, bytes: usize) -> bool {
        self.forward_allowed.load(Ordering::Relaxed) && self.count(&self.forward, bytes)
    }

    fn apply(&self, rule: Rule) {
        self.relay_allowed.store(rule.relay, Ordering::Relaxed);
        self.forward_allowed.store(rule.forward, Ordering::Relaxed);
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
    /// Sessions that may not send everything, and when ended ones lapse.
    rules: parking_lot::Mutex<HashMap<String, (Rule, Option<Instant>)>>,
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
        let rule = self.rules.lock().get(id).map_or(Rule::ALLOWED, |(r, _)| *r);
        let gauge = Arc::new(SessionGauge {
            relay: AtomicU64::new(0),
            forward: AtomicU64::new(0),
            relay_allowed: AtomicBool::new(rule.relay),
            forward_allowed: AtomicBool::new(rule.forward),
            active: AtomicU32::new(self.shared.tick.load(Ordering::Relaxed)),
            meters: Arc::clone(&self.shared),
        });
        sessions.insert(id.to_owned(), Arc::clone(&gauge));
        gauge
    }

    /// What session `update.session` may send from now on.
    pub fn rule(&self, update: &RuleUpdate) {
        let mut rules = self.rules.lock();
        if update.rule == Rule::ALLOWED && !update.ended {
            rules.remove(&update.session);
        } else {
            let lapses = update.ended.then(|| Instant::now() + ENDED);
            rules.insert(update.session.clone(), (update.rule, lapses));
        }
        if let Some(gauge) = self.sessions.lock().get(&update.session) {
            gauge.apply(update.rule);
        }
    }

    /// The rules in force (for an edge node that just connected).
    pub fn rules(&self) -> Vec<RuleUpdate> {
        self.rules
            .lock()
            .iter()
            .map(|(session, (rule, lapses))| RuleUpdate {
                session: session.clone(),
                rule: *rule,
                ended: lapses.is_some(),
            })
            .collect()
    }

    /// The counters now; drops those of sessions gone quiet that no
    /// allocation or viewer connection holds any more.
    pub fn usage(&self) -> Usage {
        let now = Instant::now();
        self.rules
            .lock()
            .retain(|_, (_, lapses)| lapses.is_none_or(|at| now < at));
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
    fn a_session_sends_what_its_rule_allows() {
        let meters = Meters::new();
        let early = meters.session("s");
        meters.rule(&RuleUpdate {
            session: "s".into(),
            rule: Rule { relay: false, forward: true },
            ended: false,
        });
        assert!(!early.relayed(100));
        assert!(early.forwarded(100));
        meters.rule(&RuleUpdate {
            session: "t".into(),
            rule: Rule::NOTHING,
            ended: true,
        });
        assert!(!meters.session("t").forwarded(100));
        meters.rule(&RuleUpdate {
            session: "s".into(),
            rule: Rule::ALLOWED,
            ended: false,
        });
        assert!(early.relayed(100));
        assert_eq!(meters.rules().len(), 1);
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
