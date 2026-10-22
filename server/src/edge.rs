//! Edge relay nodes, control side (`docs/protocol/deployment-design.md` §4):
//! the nodes' rows, their live connections and health, the devices' measured
//! round trips to them, and the relay servers each session side gets.
//!
//! A node holds no database: it joins with a one-time token, keeps one
//! signed WebSocket to the control node (`/api/v2/edge/ws`), receives its
//! relay settings over it and reports its health every ten seconds. Its TURN
//! credentials are signed with a key derived from the relay secret and its
//! ID, and name it: `<expiry>:<session>:<node>`.

use crate::config::{StunEndpoint, TurnConfig, TurnMode};
use farwing_protocol::realtime::{IceServer, ProbeNode};
use hmac::{Hmac, Mac};
use parking_lot::RwLock;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap};
use std::net::{IpAddr, SocketAddr};
use std::time::{Duration, Instant};
use tokio::sync::mpsc;

/// A node is usable while its last health report is this recent.
const HEALTH_FRESH: Duration = Duration::from_secs(30);
/// Measurements older than this are ignored.
const RTT_FRESH: Duration = Duration::from_secs(2 * 3600);
/// Edge nodes per session side (the control node's own relay comes on top).
pub const NODES_PER_SIDE: usize = 2;

/// The node's relay key: `base64(HMAC-SHA256(relay secret, label ‖ id))`.
pub fn node_secret(master: &str, node_id: &str) -> String {
    let mut mac = Hmac::<sha2::Sha256>::new_from_slice(master.as_bytes()).expect("any key size");
    mac.update(b"farwing edge node\n");
    mac.update(node_id.as_bytes());
    farwing_protocol::crypto::b64(&mac.finalize().into_bytes())
}

/// What the administrator sets for a node.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct NodeSettings {
    /// Written into relayed candidates; empty: the address the node
    /// connects from.
    pub public_ip: Option<IpAddr>,
    pub listen_udp: Option<SocketAddr>,
    pub listen_tcp: Option<SocketAddr>,
    pub listen_tls: Option<SocketAddr>,
    pub relay_port_min: Option<u16>,
    pub relay_port_max: Option<u16>,
    /// Local address relay sockets bind to.
    pub relay_bind: IpAddr,
    pub stun: Vec<StunEndpoint>,
    /// No new sessions above this relayed bandwidth.
    pub bandwidth_limit_mbps: Option<u32>,
}

impl Default for NodeSettings {
    fn default() -> Self {
        let t = TurnConfig::default();
        Self {
            public_ip: None,
            listen_udp: t.listen_udp,
            listen_tcp: t.listen_tcp,
            listen_tls: t.listen_tls,
            relay_port_min: t.relay_port_min,
            relay_port_max: t.relay_port_max,
            relay_bind: t.relay_bind,
            stun: t.stun,
            bandwidth_limit_mbps: None,
        }
    }
}

/// A node's row.
#[derive(Clone, Debug, Serialize)]
pub struct NodeRow {
    pub id: String,
    pub name: String,
    pub region: String,
    /// Not joined yet while empty.
    pub public_key: String,
    pub enabled: bool,
    pub settings: NodeSettings,
    pub created_at: i64,
    pub joined_at: Option<i64>,
    pub last_seen_at: Option<i64>,
    pub last_ip: String,
}

/// Control → node.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ToNode {
    /// After a join: the node's ID.
    Joined { node_id: String },
    /// The relay to run (its `shared_secret` is the node's key).
    Config { turn: Box<TurnConfig> },
    /// The control node presents another certificate from now on.
    ControlFingerprint { fingerprint: String },
}

/// Node → control.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum FromNode {
    Health(Health),
}

/// A node's report.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Health {
    /// Bound relay and discovery addresses (none while the relay is down).
    pub turn_udp: Option<SocketAddr>,
    pub turn_tcp: Option<SocketAddr>,
    pub turn_tls: Option<SocketAddr>,
    pub stun: Vec<SocketAddr>,
    pub allocations: u32,
    /// Sessions with an allocation on the node.
    pub sessions: Vec<String>,
    /// Relayed bytes per second, both ways.
    pub relayed_bps: u64,
    pub cpu_percent: Option<f32>,
    pub memory_mb: Option<u64>,
    /// Why the relay does not run.
    pub error: Option<String>,
    pub version: String,
}

struct Live {
    conn_id: String,
    tx: mpsc::UnboundedSender<ToNode>,
    ip: IpAddr,
    connected_at: i64,
    health: Option<(Health, Instant)>,
}

/// A node as the administration API shows it.
#[derive(Clone, Debug, Serialize)]
pub struct NodeStatus {
    #[serde(flatten)]
    pub row: NodeRow,
    pub online: bool,
    /// Online, enabled, healthy and below its bandwidth limit.
    pub usable: bool,
    pub connected_at: Option<i64>,
    pub address: Option<String>,
    pub health: Option<Health>,
}

#[derive(Default)]
pub struct EdgeHub {
    rows: RwLock<BTreeMap<String, NodeRow>>,
    live: RwLock<HashMap<String, Live>>,
    rtt: RwLock<HashMap<String, (BTreeMap<String, Option<u32>>, Instant)>>,
    relay: RwLock<RelaySecret>,
}

#[derive(Default, Clone)]
struct RelaySecret {
    master: String,
    ttl: Duration,
}

impl EdgeHub {
    pub fn new() -> Self {
        Self::default()
    }

    /// The rows as stored.
    pub fn load(&self, rows: Vec<NodeRow>) {
        *self.rows.write() = rows.into_iter().map(|r| (r.id.clone(), r)).collect();
    }

    pub fn row(&self, id: &str) -> Option<NodeRow> {
        self.rows.read().get(id).cloned()
    }

    /// A row changed (or was added): store it and send the node its relay.
    pub fn upsert(&self, row: NodeRow) {
        let id = row.id.clone();
        self.rows.write().insert(id.clone(), row);
        self.push_config(&id);
    }

    /// A node was removed: forget it and close its connection.
    pub fn remove(&self, id: &str) {
        self.rows.write().remove(id);
        self.live.write().remove(id);
    }

    /// The relay secret and credential lifetime changed: nodes get new keys.
    pub fn reconfigure(&self, turn: &TurnConfig) {
        let next = RelaySecret {
            master: turn.shared_secret.clone(),
            ttl: turn.credential_ttl,
        };
        let changed = {
            let mut current = self.relay.write();
            let changed = current.master != next.master || current.ttl != next.ttl;
            *current = next;
            changed
        };
        if changed {
            let ids: Vec<String> = self.live.read().keys().cloned().collect();
            for id in ids {
                self.push_config(&id);
            }
        }
    }

    /// The relay `id` runs.
    pub fn config_for(&self, id: &str) -> Option<TurnConfig> {
        let row = self.rows.read().get(id)?.clone();
        let ip = self.live.read().get(id).map(|l| l.ip);
        let relay = self.relay.read().clone();
        let s = &row.settings;
        Some(TurnConfig {
            mode: TurnMode::Builtin,
            urls: Vec::new(),
            shared_secret: node_secret(&relay.master, id),
            credential_ttl: relay.ttl,
            force_relay: false,
            relay: true,
            listen_udp: s.listen_udp,
            listen_tcp: s.listen_tcp,
            listen_tls: s.listen_tls,
            public_ip: s.public_ip.or(ip),
            relay_port_min: s.relay_port_min,
            relay_port_max: s.relay_port_max,
            relay_bind: s.relay_bind,
            stun: s.stun.clone(),
            ..TurnConfig::default()
        })
    }

    fn push_config(&self, id: &str) {
        let Some(turn) = self.config_for(id) else {
            return;
        };
        if let Some(live) = self.live.read().get(id) {
            let _ = live.tx.send(ToNode::Config {
                turn: Box::new(turn),
            });
        }
    }

    /// Tell connected nodes about a new control certificate.
    pub fn control_fingerprint(&self, fingerprint: &str) {
        for live in self.live.read().values() {
            let _ = live.tx.send(ToNode::ControlFingerprint {
                fingerprint: fingerprint.to_owned(),
            });
        }
    }

    /// A node connected; replaces an earlier connection of it.
    pub fn attach(
        &self,
        id: &str,
        conn_id: &str,
        ip: IpAddr,
        tx: mpsc::UnboundedSender<ToNode>,
    ) {
        self.live.write().insert(
            id.to_owned(),
            Live {
                conn_id: conn_id.to_owned(),
                tx,
                ip,
                connected_at: crate::db::now_s(),
                health: None,
            },
        );
        self.push_config(id);
    }

    /// The connection `conn_id` of node `id` ended.
    pub fn detach(&self, id: &str, conn_id: &str) {
        let mut live = self.live.write();
        if live.get(id).is_some_and(|l| l.conn_id == conn_id) {
            live.remove(id);
        }
    }

    pub fn report(&self, id: &str, conn_id: &str, health: Health) {
        if let Some(live) = self.live.write().get_mut(id).filter(|l| l.conn_id == conn_id) {
            live.health = Some((health, Instant::now()));
        }
    }

    fn usable_locked(row: &NodeRow, live: Option<&Live>) -> bool {
        let Some((health, at)) = live.and_then(|l| l.health.as_ref()) else {
            return false;
        };
        row.enabled
            && at.elapsed() < HEALTH_FRESH
            && health.turn_udp.is_some()
            && row
                .settings
                .bandwidth_limit_mbps
                .is_none_or(|limit| health.relayed_bps * 8 < u64::from(limit) * 1_000_000)
    }

    /// Public address of a usable node and its health.
    fn usable(&self) -> Vec<(NodeRow, IpAddr, Health)> {
        let rows = self.rows.read();
        let live = self.live.read();
        rows.values()
            .filter_map(|row| {
                let l = live.get(&row.id);
                if !Self::usable_locked(row, l) {
                    return None;
                }
                let l = l?;
                let ip = row.settings.public_ip.unwrap_or(l.ip);
                Some((row.clone(), ip, l.health.as_ref()?.0.clone()))
            })
            .collect()
    }

    /// Where devices measure their round trip to each usable node.
    pub fn probe_list(&self) -> Vec<ProbeNode> {
        self.usable()
            .into_iter()
            .filter_map(|(row, ip, health)| {
                let port = health.stun.first().or(health.turn_udp.as_ref())?.port();
                Some(ProbeNode {
                    id: row.id,
                    stun: SocketAddr::new(ip, port).to_string(),
                })
            })
            .collect()
    }

    /// `stun:` URLs of every usable node's discovery endpoints.
    pub fn nat_discovery(&self) -> Vec<String> {
        self.usable()
            .into_iter()
            .flat_map(|(_, ip, health)| {
                health
                    .stun
                    .iter()
                    .map(move |a| format!("stun:{}", SocketAddr::new(ip, a.port())))
                    .collect::<Vec<_>>()
            })
            .collect()
    }

    /// A device's measured round trips (milliseconds, `None` unanswered).
    pub fn record_rtt(&self, device_id: &str, rtt: BTreeMap<String, Option<u32>>) {
        let known = self.rows.read();
        let rtt = rtt
            .into_iter()
            .filter(|(id, _)| known.contains_key(id))
            .collect();
        self.rtt
            .write()
            .insert(device_id.to_owned(), (rtt, Instant::now()));
    }

    /// The edge relays session `sid` hands to `device_id`: the nearest
    /// usable nodes by its measurement (unanswered ones last; without a
    /// measurement, in name order), each with credentials it accepts.
    pub fn servers_for(&self, sid: &str, device_id: &str) -> Vec<IceServer> {
        let mut usable = self.usable();
        if usable.is_empty() {
            return Vec::new();
        }
        let measured = self
            .rtt
            .read()
            .get(device_id)
            .filter(|(_, at)| at.elapsed() < RTT_FRESH)
            .map(|(m, _)| m.clone())
            .unwrap_or_default();
        usable.sort_by_key(|(row, _, _)| {
            (
                measured
                    .get(&row.id)
                    .map_or(u32::MAX - 1, |r| r.unwrap_or(u32::MAX)),
                row.name.clone(),
            )
        });
        let relay = self.relay.read().clone();
        let expiry = crate::db::now_s() + relay.ttl.as_secs() as i64;
        usable
            .into_iter()
            .take(NODES_PER_SIDE)
            .map(|(row, ip, health)| {
                let host = match ip {
                    IpAddr::V4(v4) => v4.to_string(),
                    IpAddr::V6(v6) => format!("[{v6}]"),
                };
                let mut urls = Vec::new();
                if let Some(udp) = health.turn_udp {
                    urls.push(format!("turn:{host}:{}?transport=udp", udp.port()));
                }
                if let Some(tcp) = health.turn_tcp {
                    urls.push(format!("turn:{host}:{}?transport=tcp", tcp.port()));
                }
                if let Some(tls) = health.turn_tls {
                    urls.push(format!("turns:{host}:{}?transport=tcp", tls.port()));
                }
                let username = format!("{expiry}:{sid}:{}", row.id);
                let credential =
                    crate::crypto::turn_password(&node_secret(&relay.master, &row.id), &username);
                IceServer {
                    urls,
                    username,
                    credential,
                }
            })
            .collect()
    }

    /// Every node with its connection and health.
    pub fn status(&self) -> Vec<NodeStatus> {
        let rows = self.rows.read();
        let live = self.live.read();
        rows.values()
            .map(|row| {
                let l = live.get(&row.id);
                NodeStatus {
                    row: row.clone(),
                    online: l.is_some(),
                    usable: Self::usable_locked(row, l),
                    connected_at: l.map(|l| l.connected_at),
                    address: l.map(|l| l.ip.to_string()),
                    health: l.and_then(|l| l.health.as_ref().map(|(h, _)| h.clone())),
                }
            })
            .collect()
    }

    /// Sessions relayed by an edge node, with the nodes' names.
    pub fn relayed_sessions(&self) -> HashMap<String, Vec<String>> {
        let rows = self.rows.read();
        let mut out: HashMap<String, Vec<String>> = HashMap::new();
        for (id, live) in self.live.read().iter() {
            let Some((health, _)) = &live.health else {
                continue;
            };
            let name = rows.get(id).map_or_else(|| id.clone(), |r| r.name.clone());
            for sid in &health.sessions {
                out.entry(sid.clone()).or_default().push(name.clone());
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(id: &str, name: &str) -> NodeRow {
        NodeRow {
            id: id.into(),
            name: name.into(),
            region: String::new(),
            public_key: "k".into(),
            enabled: true,
            settings: NodeSettings::default(),
            created_at: 0,
            joined_at: Some(0),
            last_seen_at: None,
            last_ip: String::new(),
        }
    }

    fn healthy(hub: &EdgeHub, id: &str, ip: &str) {
        let (tx, _rx) = mpsc::unbounded_channel();
        hub.attach(id, "c", ip.parse().unwrap(), tx);
        hub.report(
            id,
            "c",
            Health {
                turn_udp: Some("0.0.0.0:3478".parse().unwrap()),
                turn_tls: Some("0.0.0.0:5349".parse().unwrap()),
                stun: vec!["0.0.0.0:3479".parse().unwrap()],
                ..Health::default()
            },
        );
    }

    #[test]
    fn nearest_nodes_with_their_own_credentials() {
        let hub = EdgeHub::new();
        hub.reconfigure(&TurnConfig {
            shared_secret: "master".into(),
            ..TurnConfig::default()
        });
        hub.load(vec![row("a", "Beijing"), row("b", "Shanghai"), row("c", "Guangzhou")]);
        healthy(&hub, "a", "203.0.113.1");
        healthy(&hub, "b", "203.0.113.2");
        healthy(&hub, "c", "203.0.113.3");
        // Without a measurement: by name.
        let names = |servers: Vec<IceServer>| {
            servers
                .iter()
                .map(|s| s.username.rsplit(':').next().unwrap().to_owned())
                .collect::<Vec<_>>()
        };
        assert_eq!(names(hub.servers_for("s1", "dev")), ["a", "c"]);
        hub.record_rtt(
            "dev",
            BTreeMap::from([("a".into(), Some(80)), ("b".into(), Some(12)), ("c".into(), None)]),
        );
        let servers = hub.servers_for("s1", "dev");
        assert_eq!(names(servers.clone()), ["b", "a"]);
        let b = &servers[0];
        assert_eq!(
            b.urls,
            ["turn:203.0.113.2:3478?transport=udp", "turns:203.0.113.2:5349?transport=tcp"]
        );
        assert!(b.username.ends_with(":s1:b"));
        assert_eq!(
            b.credential,
            crate::crypto::turn_password(&node_secret("master", "b"), &b.username)
        );
        // A disabled node is skipped; so is one over its bandwidth.
        let mut disabled = row("b", "Shanghai");
        disabled.enabled = false;
        hub.upsert(disabled);
        assert_eq!(names(hub.servers_for("s1", "dev")), ["a", "c"]);
        assert_eq!(hub.probe_list().len(), 2);
        assert_eq!(hub.nat_discovery(), ["stun:203.0.113.1:3479", "stun:203.0.113.3:3479"]);
    }

    #[test]
    fn keys_depend_on_the_secret_and_the_node() {
        assert_ne!(node_secret("m", "a"), node_secret("m", "b"));
        assert_ne!(node_secret("m", "a"), node_secret("n", "a"));
        assert_eq!(node_secret("m", "a"), node_secret("m", "a"));
    }
}
