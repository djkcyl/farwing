//! Each second: every node's flows, their budgets, and where the budgets
//! go — the hosts of relayed sessions, the forwarders, the gauges — and
//! which nodes and payers can take no more (`docs/protocol/egress-policy.md`
//! §4).

use super::allocator::{self, Flow, FlowId, Input};
use super::meter::SessionBudget;
use super::{LOCAL, Payer};
use std::collections::{HashMap, HashSet};

/// What was last decided, for the next round and to tell only changes.
#[derive(Default)]
pub struct State {
    /// Relayed sessions' budgets (bits per second).
    sessions: HashMap<String, Option<f64>>,
    /// Rooms' budgets per viewer.
    rooms: HashMap<String, Option<f64>>,
    /// What each host was told (kbit/s; 0: no limit).
    told: HashMap<String, u32>,
    /// Payers' units now (a relayed session or a forwarded viewer each).
    pub(super) units: HashMap<Payer, f64>,
    /// This server's egress is taken.
    pub(super) local_full: bool,
}

/// A budget change worth telling the host: a limit appears, lifts, or moves
/// by more than a tenth.
fn worth_telling(old: u32, new: u32) -> bool {
    (old == 0) != (new == 0) || old.abs_diff(new) * 10 > old.max(new)
}

fn kbps(bps: Option<f64>) -> u32 {
    bps.map_or(0, |b| (b / 1000.0).round().clamp(1.0, f64::from(u32::MAX)) as u32)
}

/// One round (called every second on the control node).
pub fn tick(app: &crate::http::App) {
    let egress = &app.egress;
    let cfg = app.cfg();
    let mut limits: HashMap<String, Option<f64>> = app
        .edges
        .egress_limits()
        .into_iter()
        .map(|(id, limit)| (id, limit.map(|b| b as f64)))
        .collect();
    limits.insert(
        LOCAL.to_owned(),
        cfg.egress.limit_mbps.map(|m| f64::from(m) * 1_000_000.0),
    );
    // Each forwarded viewer's room, on the room's node.
    let mut room_of: HashMap<String, (String, String)> = HashMap::new();
    for record in app.forwarding.records() {
        let node = record.node.as_ref().map_or_else(|| LOCAL.to_owned(), |(id, _)| id.clone());
        for session in &record.sessions {
            room_of.insert(session.clone(), (record.id.clone(), node.clone()));
        }
    }
    let mut state = egress.budget.lock();
    let mut flows = Vec::new();
    let mut fixed: HashMap<String, f64> = HashMap::new();
    let mut rooms: HashMap<(String, String), (Vec<Payer>, Vec<String>, f64)> = HashMap::new();
    for flow in egress.ledger.flows() {
        let payer = egress.ledger.payer(&flow.session);
        if flow.relay_bps > 0.0 {
            match &payer {
                Some(payer) => flows.push(Flow {
                    id: FlowId::Session(flow.session.clone()),
                    node: flow.node.clone(),
                    payers: vec![payer.clone()],
                    bps: flow.relay_bps,
                    budget: state.sessions.get(&flow.session).copied().flatten(),
                }),
                None => *fixed.entry(flow.node.clone()).or_default() += flow.relay_bps,
            }
        }
        if flow.forward_bps > 0.0 {
            match (&payer, room_of.get(&flow.session)) {
                (Some(payer), Some((room, node))) if *node == flow.node => {
                    let entry = rooms.entry((room.clone(), node.clone())).or_default();
                    entry.0.push(payer.clone());
                    entry.1.push(flow.session.clone());
                    entry.2 += flow.forward_bps;
                }
                _ => *fixed.entry(flow.node.clone()).or_default() += flow.forward_bps,
            }
        }
    }
    let mut viewers: HashMap<String, (String, Vec<String>)> = HashMap::new();
    for ((room, node), (payers, sessions, sum)) in rooms {
        let n = payers.len().max(1) as f64;
        flows.push(Flow {
            id: FlowId::Room(room.clone()),
            node: node.clone(),
            payers,
            bps: sum / n,
            budget: state.rooms.get(&room).copied().flatten(),
        });
        viewers.insert(room, (node, sessions));
    }
    let mut payer_limits = HashMap::new();
    let mut units: HashMap<Payer, f64> = HashMap::new();
    for flow in &flows {
        for payer in &flow.payers {
            payer_limits
                .entry(payer.clone())
                .or_insert_with(|| egress.effective(payer).rate_bps.map(|b| b as f64));
            *units.entry(payer.clone()).or_default() += 1.0;
        }
    }
    let input = Input {
        flows,
        nodes: limits.clone(),
        fixed: fixed.clone(),
        payers: payer_limits,
    };
    let budgets = allocator::allocate(&input);

    // Which nodes can take another flow.
    let mut node_units: HashMap<&str, f64> = HashMap::new();
    for flow in &input.flows {
        *node_units.entry(flow.node.as_str()).or_default() += flow.payers.len() as f64;
    }
    let full = |node: &str| {
        allocator::full(
            limits.get(node).copied().flatten(),
            fixed.get(node).copied().unwrap_or(0.0),
            node_units.get(node).copied().unwrap_or(0.0),
            1.0,
        )
    };
    state.local_full = full(LOCAL);
    let edges_full: HashSet<String> = limits
        .keys()
        .filter(|node| node.as_str() != LOCAL && full(node))
        .cloned()
        .collect();
    app.edges.set_full(edges_full);
    state.units = units;

    // Budgets out.
    let mut per_node: HashMap<String, Vec<SessionBudget>> = HashMap::new();
    let mut room_budgets: HashMap<String, Vec<(String, Option<u64>)>> = HashMap::new();
    let mut sessions = HashMap::new();
    let mut rooms_now = HashMap::new();
    for flow in &input.flows {
        let budget = budgets.get(&flow.id).copied().flatten();
        match &flow.id {
            FlowId::Session(sid) => {
                sessions.insert(sid.clone(), budget);
                per_node.entry(flow.node.clone()).or_default().push(SessionBudget {
                    session: sid.clone(),
                    relay: budget.map(|b| b as u64),
                    forward: None,
                });
            }
            FlowId::Room(room) => {
                rooms_now.insert(room.clone(), budget);
                room_budgets
                    .entry(flow.node.clone())
                    .or_default()
                    .push((room.clone(), budget.map(|b| b as u64)));
                if let Some((node, viewers)) = viewers.get(room) {
                    let list = per_node.entry(node.clone()).or_default();
                    for session in viewers {
                        match list.iter_mut().find(|b| &b.session == session) {
                            Some(entry) => entry.forward = budget.map(|b| b as u64),
                            None => list.push(SessionBudget {
                                session: session.clone(),
                                relay: None,
                                forward: budget.map(|b| b as u64),
                            }),
                        }
                    }
                }
            }
        }
    }
    // Rooms that lost their limit hear so once.
    for (room, old) in &state.rooms {
        if old.is_some() && !rooms_now.contains_key(room) {
            rooms_now.insert(room.clone(), None);
        }
    }
    for room in rooms_now.keys() {
        let node = viewers.get(room).map_or_else(
            || {
                app.forwarding
                    .records()
                    .into_iter()
                    .find(|r| &r.id == room)
                    .and_then(|r| r.node.map(|(id, _)| id))
                    .unwrap_or_default()
            },
            |(node, _)| node.clone(),
        );
        if !room_budgets.get(&node).is_some_and(|l| l.iter().any(|(r, _)| r == room)) {
            room_budgets.entry(node).or_default().push((room.clone(), None));
        }
    }
    for (room, budget) in room_budgets.get(LOCAL).into_iter().flatten() {
        app.forwarding.local.set_budget(room, *budget);
    }
    egress.meters().budgets(
        per_node.get(LOCAL).map_or(&[][..], Vec::as_slice),
        limits.get(LOCAL).copied().flatten().map(|b| b as u64),
    );
    for (node, limit) in &limits {
        if node == LOCAL {
            continue;
        }
        app.edges.send(
            node,
            crate::edge::ToNode::Budgets {
                limit: limit.map(|b| b as u64),
                sessions: per_node.remove(node).unwrap_or_default(),
                rooms: room_budgets.remove(node).unwrap_or_default(),
            },
        );
    }
    // Hosts of relayed sessions send to their budget.
    let mut told = std::mem::take(&mut state.told);
    for (sid, budget) in &sessions {
        let new = kbps(*budget);
        let old = told.get(sid).copied().unwrap_or(0);
        if worth_telling(old, new) {
            app.hub.push_budget(sid, new);
            told.insert(sid.clone(), new);
        }
    }
    told.retain(|sid, kbps| {
        if sessions.contains_key(sid) {
            return true;
        }
        // The flow stopped: its limit lifts.
        if *kbps != 0 {
            app.hub.push_budget(sid, 0);
        }
        false
    });
    state.told = told;
    state.sessions = sessions;
    state.rooms = rooms_now.into_iter().filter(|(_, b)| b.is_some()).collect();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hosts_hear_only_changes_that_matter() {
        assert!(worth_telling(0, 5000));
        assert!(worth_telling(5000, 0));
        assert!(!worth_telling(5000, 5300));
        assert!(worth_telling(5000, 6000));
        assert_eq!(kbps(None), 0);
        assert_eq!(kbps(Some(1_500_000.0)), 1500);
    }
}
