//! Each node's egress shared among its flows each second
//! (`docs/protocol/egress-policy.md` §4.1): weighted max-min fairness under
//! the nodes' limits and the payers' rate limits, never below a floor.

use super::Payer;
use std::collections::HashMap;

/// No flow is squeezed below this (per viewer for a forwarding room).
pub const FLOOR_BPS: f64 = 1_000_000.0;
/// A flow using this share of its budget wants more.
const WANTS_MORE: f64 = 0.9;
/// Room to grow above what a flow uses now.
const GROWTH: f64 = 1.5;
const HEADROOM_BPS: f64 = 1_000_000.0;

/// What a flow is.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum FlowId {
    /// A relayed session.
    Session(String),
    /// A forwarding room's main stream (one unit per viewer).
    Room(String),
}

/// One flow through one node.
#[derive(Clone, Debug)]
pub struct Flow {
    pub id: FlowId,
    pub node: String,
    /// The payer of each unit: one for a session, each viewer for a room.
    pub payers: Vec<Payer>,
    /// Bits per second per unit now.
    pub bps: f64,
    /// The budget per unit it had (none: unlimited).
    pub budget: Option<f64>,
}

#[derive(Default)]
pub struct Input {
    pub flows: Vec<Flow>,
    /// Each node's limit (absent or none: unlimited).
    pub nodes: HashMap<String, Option<f64>>,
    /// What else each node sends that no budget governs (bits per second).
    pub fixed: HashMap<String, f64>,
    /// Each payer's rate limit (absent or none: unlimited).
    pub payers: HashMap<Payer, Option<f64>>,
}

impl Flow {
    fn demand(&self) -> f64 {
        match self.budget {
            Some(budget) if self.bps >= budget * WANTS_MORE => f64::INFINITY,
            None if self.bps <= 0.0 => f64::INFINITY,
            _ => self.bps * GROWTH + HEADROOM_BPS,
        }
    }
}

/// A limit and the flows it binds, with each flow's units under it.
struct Constraint {
    capacity: f64,
    members: Vec<(usize, f64)>,
}

/// Each flow's budget per unit: `None` where nothing binds it.
pub fn allocate(input: &Input) -> HashMap<FlowId, Option<f64>> {
    let flows = &input.flows;
    let mut constraints = Vec::new();
    for (node, limit) in &input.nodes {
        let Some(limit) = limit else { continue };
        let members: Vec<(usize, f64)> = flows
            .iter()
            .enumerate()
            .filter(|(_, f)| &f.node == node)
            .map(|(i, f)| (i, f.payers.len() as f64))
            .collect();
        if !members.is_empty() {
            let fixed = input.fixed.get(node).copied().unwrap_or(0.0);
            constraints.push(Constraint {
                capacity: (limit - fixed).max(0.0),
                members,
            });
        }
    }
    for (payer, limit) in &input.payers {
        let Some(limit) = limit else { continue };
        let members: Vec<(usize, f64)> = flows
            .iter()
            .enumerate()
            .filter_map(|(i, f)| {
                let units = f.payers.iter().filter(|p| *p == payer).count();
                (units > 0).then_some((i, units as f64))
            })
            .collect();
        if !members.is_empty() {
            constraints.push(Constraint {
                capacity: *limit,
                members,
            });
        }
    }
    let demand: Vec<f64> = flows.iter().map(Flow::demand).collect();
    let mut alloc = vec![0.0; flows.len()];
    // Frozen: satisfied (`Some(false)`) or held by a full limit (`Some(true)`).
    let mut frozen: Vec<Option<bool>> = vec![None; flows.len()];
    loop {
        let active: Vec<usize> = (0..flows.len()).filter(|i| frozen[*i].is_none()).collect();
        if active.is_empty() {
            break;
        }
        let mut step = active
            .iter()
            .map(|i| demand[*i] - alloc[*i])
            .fold(f64::INFINITY, f64::min);
        for c in &constraints {
            let used: f64 = c.members.iter().map(|(i, w)| alloc[*i] * w).sum();
            let weight: f64 = c
                .members
                .iter()
                .filter(|(i, _)| frozen[*i].is_none())
                .map(|(_, w)| w)
                .sum();
            if weight > 0.0 {
                step = step.min(((c.capacity - used) / weight).max(0.0));
            }
        }
        if step.is_infinite() {
            // Nothing binds what is left: unlimited.
            for i in active {
                frozen[i] = Some(false);
            }
            break;
        }
        for i in &active {
            alloc[*i] += step;
        }
        for i in &active {
            if alloc[*i] >= demand[*i] - 1.0 {
                frozen[*i] = Some(false);
            }
        }
        for c in &constraints {
            let used: f64 = c.members.iter().map(|(i, w)| alloc[*i] * w).sum();
            if used >= c.capacity - 1.0 {
                for (i, _) in &c.members {
                    if frozen[*i].is_none() {
                        frozen[*i] = Some(true);
                    }
                }
            }
        }
    }
    // A flow satisfied under a full limit keeps a budget of what it got (its
    // use and room to grow): unlimited, a dip in its rate would let it
    // jump past the others' share.
    let mut bound = vec![false; flows.len()];
    for c in &constraints {
        let used: f64 = c.members.iter().map(|(i, w)| alloc[*i] * w).sum();
        if used >= c.capacity - 1.0 {
            for (i, _) in &c.members {
                bound[*i] = true;
            }
        }
    }
    flows
        .iter()
        .enumerate()
        .map(|(i, f)| {
            let budget = (frozen[i] == Some(true) || bound[i]).then(|| alloc[i].max(FLOOR_BPS));
            (f.id.clone(), budget)
        })
        .collect()
}

/// Whether one more flow of `units` at the floor would exceed `limit` on
/// top of `flows` (their units) and `fixed`.
pub fn full(limit: Option<f64>, fixed: f64, units: f64, more: f64) -> bool {
    limit.is_some_and(|limit| fixed + (units + more) * FLOOR_BPS > limit)
}

#[cfg(test)]
mod tests {
    use super::*;

    const M: f64 = 1_000_000.0;

    fn session(id: &str, node: &str, payer: &str, bps: f64, budget: Option<f64>) -> Flow {
        Flow {
            id: FlowId::Session(id.into()),
            node: node.into(),
            payers: vec![Payer::User(payer.into())],
            bps,
            budget,
        }
    }

    #[test]
    fn nothing_limited_stays_unlimited() {
        let input = Input {
            flows: vec![session("a", "", "u", 5.0 * M, None)],
            ..Input::default()
        };
        assert_eq!(allocate(&input)[&FlowId::Session("a".into())], None);
    }

    #[test]
    fn a_full_node_is_shared_fairly() {
        let input = Input {
            flows: vec![
                session("a", "", "u", 8.0 * M, None),
                session("b", "", "v", 8.0 * M, None),
            ],
            nodes: HashMap::from([(String::new(), Some(10.0 * M))]),
            ..Input::default()
        };
        let budgets = allocate(&input);
        assert_eq!(budgets[&FlowId::Session("a".into())], Some(5.0 * M));
        assert_eq!(budgets[&FlowId::Session("b".into())], Some(5.0 * M));
    }

    #[test]
    fn a_small_flow_leaves_the_rest_to_others() {
        let input = Input {
            flows: vec![
                // Uses 1 Mbit/s of its 5: wants at most 2.5.
                session("small", "", "u", 1.0 * M, Some(5.0 * M)),
                session("big", "", "v", 5.0 * M, Some(5.0 * M)),
            ],
            nodes: HashMap::from([(String::new(), Some(10.0 * M))]),
            ..Input::default()
        };
        let budgets = allocate(&input);
        // It keeps a budget of what it wants: the node is full.
        assert_eq!(budgets[&FlowId::Session("small".into())], Some(2.5 * M));
        assert_eq!(budgets[&FlowId::Session("big".into())], Some(7.5 * M));
    }

    #[test]
    fn a_room_counts_each_viewer_and_each_viewer_s_payer() {
        let room = Flow {
            id: FlowId::Room("r".into()),
            node: String::new(),
            payers: vec![Payer::User("u".into()), Payer::User("v".into())],
            bps: 6.0 * M,
            budget: None,
        };
        let input = Input {
            flows: vec![room, session("s", "", "u", 6.0 * M, None)],
            nodes: HashMap::from([(String::new(), Some(30.0 * M))]),
            payers: HashMap::from([(Payer::User("u".into()), Some(8.0 * M))]),
            ..Input::default()
        };
        let budgets = allocate(&input);
        // u's 8 Mbit/s: one viewer of the room and the session, 4 each.
        assert_eq!(budgets[&FlowId::Room("r".into())], Some(4.0 * M));
        assert_eq!(budgets[&FlowId::Session("s".into())], Some(4.0 * M));
    }

    #[test]
    fn nobody_goes_below_the_floor() {
        let flows = (0..4)
            .map(|i| session(&i.to_string(), "", &i.to_string(), 5.0 * M, None))
            .collect();
        let input = Input {
            flows,
            nodes: HashMap::from([(String::new(), Some(2.0 * M))]),
            ..Input::default()
        };
        assert!(allocate(&input).values().all(|b| *b == Some(FLOOR_BPS)));
        assert!(!full(Some(10.0 * M), 0.0, 9.0, 1.0));
        assert!(full(Some(10.0 * M), 0.5 * M, 9.0, 1.0));
    }

    #[test]
    fn room_under_the_limit_leaves_flows_unlimited() {
        let input = Input {
            flows: vec![
                session("a", "", "u", 1.0 * M, Some(2.0 * M)),
                session("b", "", "v", 1.0 * M, None),
            ],
            nodes: HashMap::from([(String::new(), Some(10.0 * M))]),
            ..Input::default()
        };
        let budgets = allocate(&input);
        assert_eq!(budgets[&FlowId::Session("a".into())], None);
        assert_eq!(budgets[&FlowId::Session("b".into())], None);
    }
}
