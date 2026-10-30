//! The control node's account of every server's egress: per session and
//! node each second, per payer, and the payers' monthly totals
//! (`docs/protocol/egress-policy.md` §2).

use super::meter::Usage;
use super::{Month, Payer};
use std::collections::HashMap;
use std::time::{Duration, Instant};

/// The key of this server among the nodes.
pub const LOCAL: &str = "";
/// A session's payer is kept this long after it ends (its last packets).
const ENDED_KEEP: Duration = Duration::from_secs(120);

/// One session's flow through one node: what the relay and the forwarder
/// sent, and their rates (none until a second sample).
#[derive(Default)]
struct Flow {
    relay: u64,
    forward: u64,
    relay_bps: Option<f64>,
    forward_bps: Option<f64>,
}

impl Flow {
    fn bps(&self) -> f64 {
        self.relay_bps.unwrap_or(0.0) + self.forward_bps.unwrap_or(0.0)
    }
}

/// A session's rates through a node, for the budgets.
#[derive(Clone, Debug, PartialEq)]
pub struct NodeFlow {
    pub node: String,
    pub session: String,
    pub relay_bps: f64,
    pub forward_bps: f64,
}

#[derive(Default)]
struct Node {
    total: u64,
    at: Option<Instant>,
    bps: Option<f64>,
    flows: HashMap<String, Flow>,
}

struct Session {
    payer: Payer,
    ended: Option<Instant>,
}

#[derive(Default)]
struct Account {
    /// This month's bytes (stored and not yet stored).
    month: u64,
}

pub struct Ledger {
    inner: parking_lot::Mutex<Inner>,
}

struct Inner {
    nodes: HashMap<String, Node>,
    sessions: HashMap<String, Session>,
    accounts: HashMap<Payer, Account>,
    month: Month,
    /// Bytes not yet written, by month and payer.
    pending: HashMap<(String, Payer), u64>,
}

/// A rate from bytes sent over `dt`, smoothed over about two samples.
fn smooth(previous: Option<f64>, bytes: u64, dt: f64) -> Option<f64> {
    let now = bytes as f64 * 8.0 / dt.max(0.2);
    Some(previous.map_or(now, |p| (p + now) / 2.0))
}

/// What the web console shows.
#[derive(Clone, Debug, serde::Serialize)]
pub struct View {
    /// Bits per second each node sends now (`""`: this server).
    pub nodes: HashMap<String, u64>,
    /// Bits per second each session sends now, over all nodes.
    pub sessions: HashMap<String, u64>,
    pub accounts: Vec<AccountView>,
    pub month: Month,
}

#[derive(Clone, Debug, serde::Serialize)]
pub struct AccountView {
    /// The user ID; empty for the guests.
    pub payer: String,
    pub bps: u64,
    pub month_bytes: u64,
}

impl Ledger {
    pub fn new(month: Month, stored: Vec<(Payer, u64)>) -> Self {
        Self {
            inner: parking_lot::Mutex::new(Inner {
                nodes: HashMap::new(),
                sessions: HashMap::new(),
                accounts: stored
                    .into_iter()
                    .map(|(payer, month)| (payer, Account { month }))
                    .collect(),
                month,
                pending: HashMap::new(),
            }),
        }
    }

    /// Session `id` starts; its egress is `payer`'s.
    pub fn open(&self, id: &str, payer: Payer) {
        self.inner
            .lock()
            .sessions
            .insert(id.to_owned(), Session { payer, ended: None });
    }

    /// Session `id` ended.
    pub fn close(&self, id: &str) {
        if let Some(s) = self.inner.lock().sessions.get_mut(id) {
            s.ended.get_or_insert_with(Instant::now);
        }
    }

    pub fn payer(&self, id: &str) -> Option<Payer> {
        self.inner.lock().sessions.get(id).map(|s| s.payer.clone())
    }

    /// The payer of session `id` while it runs (not after it ended).
    pub fn open_payer(&self, id: &str) -> Option<Payer> {
        self.inner
            .lock()
            .sessions
            .get(id)
            .filter(|s| s.ended.is_none())
            .map(|s| s.payer.clone())
    }

    /// Node `node` (`LOCAL`: this server) sent `usage` so far, at `now`.
    pub fn report(&self, node: &str, usage: &Usage, now: Instant, month: &Month) {
        let mut inner = self.inner.lock();
        let inner = &mut *inner;
        if inner.month.key != month.key {
            inner.month = month.clone();
            inner.accounts.clear();
        }
        let n = inner.nodes.entry(node.to_owned()).or_default();
        let dt = n.at.map(|at| now.duration_since(at).as_secs_f64());
        n.at = Some(now);
        // A counter that went back started over (a restarted node).
        let grew = |before: u64, after: u64| if after >= before { after - before } else { after };
        if let Some(dt) = dt {
            n.bps = smooth(n.bps, grew(n.total, usage.total), dt);
        }
        n.total = usage.total;
        let mut flows = HashMap::with_capacity(usage.sessions.len());
        for s in &usage.sessions {
            let flow = match (n.flows.remove(&s.session), dt) {
                (Some(old), Some(dt)) => {
                    let (relay, forward) = (grew(old.relay, s.relay), grew(old.forward, s.forward));
                    let flow = Flow {
                        relay: s.relay,
                        forward: s.forward,
                        relay_bps: smooth(old.relay_bps, relay, dt),
                        forward_bps: smooth(old.forward_bps, forward, dt),
                    };
                    (relay + forward, flow)
                }
                // A new flow: what it sent counts, and its rate as if it
                // started with the interval (it is shared out at once).
                (_, dt) => (
                    s.relay + s.forward,
                    Flow {
                        relay: s.relay,
                        forward: s.forward,
                        relay_bps: dt.and_then(|dt| smooth(None, s.relay, dt)),
                        forward_bps: dt.and_then(|dt| smooth(None, s.forward, dt)),
                    },
                ),
            };
            let (sent, flow) = flow;
            if sent > 0
                && let Some(session) = inner.sessions.get(&s.session)
            {
                inner.accounts.entry(session.payer.clone()).or_default().month += sent;
                *inner
                    .pending
                    .entry((inner.month.key.clone(), session.payer.clone()))
                    .or_default() += sent;
            }
            flows.insert(s.session.clone(), flow);
        }
        n.flows = flows;
        inner.sessions.retain(|id, s| {
            s.ended.is_none_or(|at| now.duration_since(at) < ENDED_KEEP)
                || inner_flows(&inner.nodes, id)
        });
    }

    /// Every session's rates through every node.
    pub fn flows(&self) -> Vec<NodeFlow> {
        let inner = self.inner.lock();
        inner
            .nodes
            .iter()
            .flat_map(|(node, n)| {
                n.flows.iter().map(|(session, f)| NodeFlow {
                    node: node.clone(),
                    session: session.clone(),
                    relay_bps: f.relay_bps.unwrap_or(0.0),
                    forward_bps: f.forward_bps.unwrap_or(0.0),
                })
            })
            .collect()
    }

    /// Each node's rate now (bits per second).
    pub fn node_rates(&self) -> HashMap<String, f64> {
        let inner = self.inner.lock();
        inner
            .nodes
            .iter()
            .map(|(node, n)| (node.clone(), n.bps.unwrap_or(0.0)))
            .collect()
    }

    /// A node left (its flows end).
    pub fn forget_node(&self, node: &str) {
        self.inner.lock().nodes.remove(node);
    }

    /// Bytes to store: `(month, payer, bytes)`; they count as stored.
    pub fn take_pending(&self) -> Vec<(String, Payer, u64)> {
        self.inner
            .lock()
            .pending
            .drain()
            .map(|((month, payer), bytes)| (month, payer, bytes))
            .collect()
    }

    /// Storing failed: keep the bytes for the next attempt.
    pub fn restore_pending(&self, rows: Vec<(String, Payer, u64)>) {
        let mut inner = self.inner.lock();
        for (month, payer, bytes) in rows {
            *inner.pending.entry((month, payer)).or_default() += bytes;
        }
    }

    /// `payer`'s bytes this month.
    pub fn month_bytes(&self, payer: &Payer) -> u64 {
        self.inner.lock().accounts.get(payer).map_or(0, |a| a.month)
    }

    pub fn view(&self) -> View {
        let inner = self.inner.lock();
        let mut sessions: HashMap<String, f64> = HashMap::new();
        for node in inner.nodes.values() {
            for (id, flow) in &node.flows {
                *sessions.entry(id.clone()).or_default() += flow.bps();
            }
        }
        let mut rates: HashMap<Payer, f64> = HashMap::new();
        for (id, bps) in &sessions {
            if let Some(s) = inner.sessions.get(id) {
                *rates.entry(s.payer.clone()).or_default() += bps;
            }
        }
        let mut accounts: Vec<AccountView> = inner
            .accounts
            .iter()
            .map(|(payer, a)| AccountView {
                payer: payer.key().to_owned(),
                bps: rates.get(payer).copied().unwrap_or(0.0) as u64,
                month_bytes: a.month,
            })
            .collect();
        for (payer, bps) in &rates {
            if !inner.accounts.contains_key(payer) {
                accounts.push(AccountView {
                    payer: payer.key().to_owned(),
                    bps: *bps as u64,
                    month_bytes: 0,
                });
            }
        }
        accounts.sort_by(|a, b| b.month_bytes.cmp(&a.month_bytes).then(b.bps.cmp(&a.bps)));
        View {
            nodes: inner
                .nodes
                .iter()
                .map(|(id, n)| (id.clone(), n.bps.unwrap_or(0.0) as u64))
                .collect(),
            sessions: sessions.into_iter().map(|(id, bps)| (id, bps as u64)).collect(),
            accounts,
            month: inner.month.clone(),
        }
    }
}

fn inner_flows(nodes: &HashMap<String, Node>, session: &str) -> bool {
    nodes.values().any(|n| n.flows.contains_key(session))
}

#[cfg(test)]
mod tests {
    use super::super::meter::SessionUsage;
    use super::*;

    fn usage(total: u64, sessions: &[(&str, u64)]) -> Usage {
        Usage {
            total,
            sessions: sessions
                .iter()
                .map(|(id, relay)| SessionUsage {
                    session: (*id).to_owned(),
                    relay: *relay,
                    forward: 0,
                })
                .collect(),
        }
    }

    #[test]
    fn rates_and_months_follow_the_payer() {
        let month = Month::at(1_793_000_000);
        let ledger = Ledger::new(month.clone(), vec![(Payer::User("alice".into()), 5)]);
        ledger.open("s1", Payer::User("alice".into()));
        ledger.open("s2", Payer::Guests);
        let t0 = Instant::now();
        ledger.report(LOCAL, &usage(300, &[("s1", 100), ("s2", 200)]), t0, &month);
        ledger.report(
            LOCAL,
            &usage(1300, &[("s1", 1100), ("s2", 200)]),
            t0 + Duration::from_secs(1),
            &month,
        );
        let view = ledger.view();
        assert_eq!(view.nodes[LOCAL], 8000);
        assert_eq!(view.sessions["s1"], 8000);
        let alice = view.accounts.iter().find(|a| a.payer == "alice").unwrap();
        assert_eq!(alice.month_bytes, 5 + 1100);
        assert_eq!(ledger.month_bytes(&Payer::Guests), 200);
        let mut pending = ledger.take_pending();
        pending.sort_by_key(|(_, _, b)| *b);
        assert_eq!(pending, vec![
            (month.key.clone(), Payer::Guests, 200),
            (month.key.clone(), Payer::User("alice".into()), 1100),
        ]);
        assert!(ledger.take_pending().is_empty());
    }

    #[test]
    fn a_restarted_counter_counts_from_zero() {
        let month = Month::at(1_793_000_000);
        let ledger = Ledger::new(month.clone(), Vec::new());
        ledger.open("s", Payer::Guests);
        let t0 = Instant::now();
        ledger.report("n", &usage(1000, &[("s", 1000)]), t0, &month);
        ledger.report("n", &usage(50, &[("s", 50)]), t0 + Duration::from_secs(1), &month);
        assert_eq!(ledger.month_bytes(&Payer::Guests), 1050);
    }

    #[test]
    fn a_new_month_starts_from_zero() {
        let october = Month::at(1_793_000_000);
        let ledger = Ledger::new(october.clone(), vec![(Payer::Guests, 900)]);
        ledger.open("s", Payer::Guests);
        let november = Month::at(october.end + 10);
        ledger.report(LOCAL, &usage(10, &[("s", 10)]), Instant::now(), &november);
        assert_eq!(ledger.month_bytes(&Payer::Guests), 10);
    }
}
