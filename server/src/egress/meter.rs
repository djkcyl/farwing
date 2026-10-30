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

/// A budget's bucket lets this much more through, at least this much more
/// (`docs/protocol/egress-policy.md` §4.3): the host sends to the budget
/// itself; the bucket only stops one that does not.
const BUCKET_SLACK: f64 = 1.3;
const BUCKET_MARGIN_BPS: f64 = 1_000_000.0;
/// The node's bucket lets this much more through than its limit.
const NODE_SLACK: f64 = 1.1;
/// A session's bucket holds this long a burst.
const BURST: f64 = 0.3;
/// The node's bucket holds a second: a session that joins sends before the
/// next round of budgets lowers the others (`docs/protocol/egress-policy.md`
/// §4.3).
const NODE_BURST: f64 = 1.0;

/// A token bucket in bytes.
struct Bucket {
    rate: f64,
    /// Seconds of `rate` it holds.
    burst: f64,
    tokens: f64,
    at: Instant,
}

impl Bucket {
    fn new(bytes_per_second: f64, burst: f64) -> Self {
        Self {
            rate: bytes_per_second,
            burst,
            tokens: bytes_per_second * burst,
            at: Instant::now(),
        }
    }

    fn take(&mut self, bytes: u64) -> bool {
        let now = Instant::now();
        let burst = self.rate * self.burst;
        self.tokens = (self.tokens + now.duration_since(self.at).as_secs_f64() * self.rate).min(burst);
        self.at = now;
        if self.tokens < bytes as f64 {
            return false;
        }
        self.tokens -= bytes as f64;
        true
    }

    fn set(slot: &parking_lot::Mutex<Option<Bucket>>, bytes_per_second: Option<f64>, burst: f64) {
        let mut slot = slot.lock();
        match (bytes_per_second, slot.as_mut()) {
            (None, _) => *slot = None,
            (Some(rate), Some(bucket)) => bucket.rate = rate,
            (Some(rate), None) => *slot = Some(Bucket::new(rate, burst)),
        }
    }

    fn admit(slot: &parking_lot::Mutex<Option<Bucket>>, bytes: u64) -> bool {
        slot.lock().as_mut().is_none_or(|bucket| bucket.take(bytes))
    }
}

/// The bucket rate (bytes per second) for a budget in bits per second.
fn bucket_rate(budget_bps: f64) -> f64 {
    (budget_bps * BUCKET_SLACK).max(budget_bps + BUCKET_MARGIN_BPS) / 8.0
}

/// A session's budgets on a node (bits per second; none: unlimited), as
/// the control node sends them.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct SessionBudget {
    pub session: String,
    #[serde(default)]
    pub relay: Option<u64>,
    #[serde(default)]
    pub forward: Option<u64>,
}

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
    relay_bucket: parking_lot::Mutex<Option<Bucket>>,
    forward_bucket: parking_lot::Mutex<Option<Bucket>>,
    /// The snapshot this gauge last sent something in.
    active: AtomicU32,
    meters: Arc<Shared>,
}

#[derive(Default)]
struct Shared {
    total: AtomicU64,
    /// Snapshots taken so far.
    tick: AtomicU32,
    /// The node's limit.
    bucket: parking_lot::Mutex<Option<Bucket>>,
}

impl SessionGauge {
    fn count(&self, counter: &AtomicU64, bucket: &parking_lot::Mutex<Option<Bucket>>, bytes: usize) -> bool {
        let bytes = bytes as u64 + OVERHEAD;
        if !Bucket::admit(bucket, bytes) || !Bucket::admit(&self.meters.bucket, bytes) {
            return false;
        }
        counter.fetch_add(bytes, Ordering::Relaxed);
        self.meters.total.fetch_add(bytes, Ordering::Relaxed);
        self.active
            .store(self.meters.tick.load(Ordering::Relaxed), Ordering::Relaxed);
        true
    }

    /// The relay sends `bytes` of payload for this session; false: drop.
    pub fn relayed(&self, bytes: usize) -> bool {
        self.relay_allowed.load(Ordering::Relaxed) && self.count(&self.relay, &self.relay_bucket, bytes)
    }

    /// The forwarder sends `bytes` (a packet) to this session's viewer;
    /// false: drop.
    pub fn forwarded(&self, bytes: usize) -> bool {
        self.forward_allowed.load(Ordering::Relaxed)
            && self.count(&self.forward, &self.forward_bucket, bytes)
    }

    fn apply(&self, rule: Rule) {
        self.relay_allowed.store(rule.relay, Ordering::Relaxed);
        self.forward_allowed.store(rule.forward, Ordering::Relaxed);
    }

    fn budget(&self, relay: Option<u64>, forward: Option<u64>) {
        Bucket::set(&self.relay_bucket, relay.map(|b| bucket_rate(b as f64)), BURST);
        Bucket::set(&self.forward_bucket, forward.map(|b| bucket_rate(b as f64)), BURST);
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
    /// Sessions' budgets (`relay`, `forward`; bits per second).
    budgets: parking_lot::Mutex<HashMap<String, (Option<u64>, Option<u64>)>>,
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
            relay_bucket: parking_lot::Mutex::new(None),
            forward_bucket: parking_lot::Mutex::new(None),
            active: AtomicU32::new(self.shared.tick.load(Ordering::Relaxed)),
            meters: Arc::clone(&self.shared),
        });
        if let Some((relay, forward)) = self.budgets.lock().get(id) {
            gauge.budget(*relay, *forward);
        }
        sessions.insert(id.to_owned(), Arc::clone(&gauge));
        gauge
    }

    /// The sessions' budgets from now on (the others: unlimited) and this
    /// node's limit (bits per second).
    pub fn budgets(&self, budgets: &[SessionBudget], limit: Option<u64>) {
        Bucket::set(&self.shared.bucket, limit.map(|l| l as f64 * NODE_SLACK / 8.0), NODE_BURST);
        let map: HashMap<String, (Option<u64>, Option<u64>)> = budgets
            .iter()
            .map(|b| (b.session.clone(), (b.relay, b.forward)))
            .collect();
        for (id, gauge) in self.sessions.lock().iter() {
            let (relay, forward) = map.get(id).copied().unwrap_or_default();
            gauge.budget(relay, forward);
        }
        *self.budgets.lock() = map;
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
    fn a_budget_stops_a_session_far_above_it() {
        let meters = Meters::new();
        let gauge = meters.session("s");
        // 8 Mbit/s: its bucket lets 1.3 MB/s through, 0.39 MB at once.
        meters.budgets(
            &[SessionBudget {
                session: "s".into(),
                relay: Some(8_000_000),
                forward: None,
            }],
            None,
        );
        let sent = (0..1000).filter(|_| gauge.relayed(1200)).count();
        assert!((300..340).contains(&sent), "{sent}");
        assert!(gauge.forwarded(1200));
        meters.budgets(&[], None);
        assert!(gauge.relayed(1200));
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
