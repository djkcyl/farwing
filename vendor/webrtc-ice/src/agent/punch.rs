//! Farwing probe punching (`docs/protocol/v2-design.md` §11), not a generic
//! ICE algorithm. Five seconds into gathering, when no direct path is
//! selected, both peers plan from each side's NAT mapping (as measured by
//! the application and exchanged in `a=x-farwing-nat`, else guessed from
//! srflx candidates), probe each other's mapped ports, and promote a
//! private socket that hears the peer into an ICE host candidate.
//!
//! Probes carry a tag derived from both peers' ICE credentials, so only the
//! peer of this session can make a port answer or promote a socket.
mod actor;

use std::collections::{BTreeMap, HashSet};
use std::net::{IpAddr, SocketAddr};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex as StdMutex, Weak};
use std::time::Duration;

use tokio::sync::{mpsc, Mutex};
use util::{vnet::net::Net, Conn};

use super::agent_gather::GatherCandidatesInternalParams;
use super::agent_internal::AgentInternal;
use super::allocator::AllocationSession;
use super::gather_owner::GatherOwner;
use super::local_port::{LocalCandidate, LocalPort, PORT_GATHERING};
use super::network_catalog::NetworkCatalog;
use super::shared_socket::SharedUdpSocket;
use crate::candidate::{
    candidate_base::{remote_description_copy, CandidateBaseConfig},
    candidate_host::CandidateHostConfig,
    Candidate, CandidatePair, CandidateType,
};
use crate::url::SchemeType;
use crate::util::{ice_candidate_priority, ice_network_cost};

/// First bytes of a probe and its answer: outside every range RFC 7983
/// demultiplexes (STUN, DTLS, TURN channels, RTP/RTCP).
const REQUEST_MAGIC: [u8; 4] = [0x06, b'F', b'W', b'q'];
const RESPONSE_MAGIC: [u8; 4] = [0x06, b'F', b'W', b'r'];
const PROBE_LEN: usize = 12;

/// The probe and answer datagrams of one ICE credential generation: magic
/// plus 8 bytes of SHA-256 over both peers' ufrag and password (in a fixed
/// order, so both peers derive the same bytes).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Probe {
    pub request: [u8; PROBE_LEN],
    pub response: [u8; PROBE_LEN],
}

impl Probe {
    pub fn derive(
        local_ufrag: &str,
        local_pwd: &str,
        remote_ufrag: &str,
        remote_pwd: &str,
    ) -> Option<Self> {
        if [local_ufrag, local_pwd, remote_ufrag, remote_pwd]
            .iter()
            .any(|value| value.is_empty())
        {
            return None;
        }
        // ICE characters exclude ':' and '\n'.
        let local = format!("{local_ufrag}:{local_pwd}");
        let remote = format!("{remote_ufrag}:{remote_pwd}");
        let (first, second) = if local <= remote {
            (local, remote)
        } else {
            (remote, local)
        };
        let digest = ring::digest::digest(
            &ring::digest::SHA256,
            format!("farwing punch probe\n{first}\n{second}").as_bytes(),
        );
        let digest = digest.as_ref();
        let mut request = [0; PROBE_LEN];
        request[..4].copy_from_slice(&REQUEST_MAGIC);
        request[4..].copy_from_slice(&digest[..8]);
        let mut response = [0; PROBE_LEN];
        response[..4].copy_from_slice(&RESPONSE_MAGIC);
        response[4..].copy_from_slice(&digest[8..16]);
        Some(Self { request, response })
    }

    /// Cheap pre-check on the receive path before deriving the tag.
    pub fn looks_like_request(packet: &[u8]) -> bool {
        packet.len() == PROBE_LEN && packet[..4] == REQUEST_MAGIC
    }
}

/// One side's NAT mapping as the application measured it (RFC 5780), for
/// planning probe punching.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum NatMapping {
    /// Not measured: planned from the side's srflx candidates instead.
    #[default]
    Unknown,
    /// Endpoint-independent mapping.
    Cone,
    /// Dependent mapping whose new ports advance by this step.
    Linear(i16),
    /// Dependent mapping without a consistent step.
    Random,
}

struct Allocation {
    generation: u32,
    session: Weak<AllocationSession>,
    agent: Weak<AgentInternal>,
    owner: Weak<GatherOwner>,
    catalog: Arc<Mutex<NetworkCatalog>>,
    net: Arc<Net>,
    stun_servers: usize,
}

#[derive(Clone)]
struct Target {
    base: Arc<LocalPort>,
    hint: LocalCandidate,
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct BaseAddress {
    ip: Option<IpAddr>,
    name: String,
    port: u16,
}
impl BaseAddress {
    fn of(candidate: &dyn Candidate) -> Self {
        let related = candidate.related_address();
        let address = related
            .as_ref()
            .map_or("", |address| address.address.as_str());
        let ip = address.parse().ok();
        Self {
            ip,
            name: if ip.is_some() {
                String::new()
            } else {
                address.into()
            },
            port: related.map_or(0, |address| address.port),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
enum NatKind {
    None = 0,
    Cone = 1,
    Linear = 2,
    Random = 3,
    Unknown = 4,
}

#[derive(Clone)]
struct Group {
    candidates: Vec<LocalCandidate>,
    kind: NatKind,
    step: u16,
}

impl Group {
    fn classify(
        candidates: Vec<LocalCandidate>,
        server_count: usize,
        measured: NatMapping,
    ) -> Self {
        let (kind, step) = match measured {
            NatMapping::Unknown => return Self::guess(candidates, server_count),
            NatMapping::Cone => (NatKind::Cone, 0),
            NatMapping::Linear(step) => (NatKind::Linear, step as u16),
            NatMapping::Random => (NatKind::Random, 0),
        };
        Self {
            candidates,
            kind,
            step,
        }
    }

    /// From the srflx candidates alone: one mapping seen by two or more STUN
    /// servers is a cone; several mappings are linear or random.
    fn guess(mut candidates: Vec<LocalCandidate>, server_count: usize) -> Self {
        let mut step = 0;
        let kind = match candidates.len() {
            0 => NatKind::None,
            1 if server_count >= 2 => NatKind::Cone,
            // One mapping from fewer than two STUN servers says nothing about
            // the NAT's mapping behaviour; do not assume a symmetric NAT and
            // start aggressive random probing (Farwing servers often expose a
            // single STUN endpoint).
            1 => NatKind::Unknown,
            2 => NatKind::Random,
            _ => {
                candidates.sort_by_key(|candidate| candidate.port());
                step = candidates[1].port() - candidates[0].port();
                if candidates
                    .windows(2)
                    .all(|pair| pair[1].port() - pair[0].port() == step)
                {
                    NatKind::Linear
                } else {
                    step = 0;
                    NatKind::Random
                }
            }
        };
        Self {
            candidates,
            kind,
            step,
        }
    }
    fn last(&self) -> &LocalCandidate {
        self.candidates.last().expect("nonempty NAT group")
    }
    fn ips(&self) -> Vec<IpAddr> {
        self.candidates
            .iter()
            .map(|candidate| candidate.addr().ip())
            .collect::<std::collections::BTreeSet<_>>()
            .into_iter()
            .collect()
    }
}

fn pair_score(local: &Group, remote: &Group) -> u32 {
    use NatKind::*;
    let cost = 100
        * (u32::from(local.candidates[0].network_cost())
            + u32::from(remote.candidates[0].network_cost()));
    cost + match (local.kind, remote.kind) {
        (Cone, Linear) | (Linear, Cone) => 10,
        (Cone, Random) | (Random, Cone) => 20,
        (Cone, Cone) => 30,
        (Linear, Linear) => 40,
        (Linear, Random) | (Random, Linear) => 50,
        (Random, Random) => 60,
        (None, _) | (_, None) => 70,
        _ => return 90,
    }
}

#[derive(Clone)]
struct Plan {
    base: Arc<LocalPort>,
    local_bind: Option<IpAddr>,
    remote: Group,
    local_kind: NatKind,
    controlling: bool,
    probe: Probe,
}

/// Both sides' measured mappings and whether the peer takes probes.
#[derive(Clone, Copy, Debug)]
struct Measured {
    local: NatMapping,
    remote: NatMapping,
    peer_allows: bool,
}

impl Default for Measured {
    fn default() -> Self {
        Self {
            local: NatMapping::Unknown,
            remote: NatMapping::Unknown,
            peer_allows: true,
        }
    }
}

#[derive(Default)]
pub(super) struct Coordinator {
    latest: StdMutex<Option<Arc<Allocation>>>,
    target: StdMutex<Option<Target>>,
    actor: StdMutex<Option<mpsc::UnboundedSender<actor::Command>>>,
    running: AtomicBool,
    measured: StdMutex<Measured>,
}

impl Coordinator {
    /// The application's NAT measurements for this session (refreshed with
    /// every remote description). `peer_allows` is false when the peer
    /// turned probe punching off: then this side does not probe it either.
    pub fn set_measured(&self, local: NatMapping, remote: NatMapping, peer_allows: bool) {
        *self.measured.lock().expect("punch measurements poisoned") = Measured {
            local,
            remote,
            peer_allows,
        };
    }

    pub fn register(
        self: &Arc<Self>,
        params: &GatherCandidatesInternalParams,
        session: &Arc<AllocationSession>,
    ) {
        let allocation = Arc::new(Allocation {
            generation: session.generation,
            session: Arc::downgrade(session),
            agent: Arc::downgrade(&params.agent_internal),
            owner: Arc::downgrade(&params.owner),
            catalog: params.network_catalog.clone(),
            net: params.net.clone(),
            stun_servers: params
                .urls
                .iter()
                .filter(|url| url.scheme == SchemeType::Stun)
                .map(|url| (&url.host, url.port))
                .collect::<HashSet<_>>()
                .len(),
        });
        *self.latest.lock().expect("punch allocation poisoned") = Some(allocation.clone());
        let weak = Arc::downgrade(self);
        params.owner.spawn(async move {
            let due = tokio::time::Instant::now() + Duration::from_secs(5);
            let Some(agent) = allocation.agent.upgrade() else {
                return;
            };
            let Some(credentials) = agent
                .credentials_for_generation(allocation.generation)
                .await
            else {
                return;
            };
            drop(agent);
            tokio::time::sleep_until(due).await;
            let Some(coordinator) = weak.upgrade() else {
                return;
            };
            coordinator.analyze(credentials).await;
        });
    }

    async fn analyze(self: &Arc<Self>, snapshot: crate::candidate::IceCredentials) {
        let Some(allocation) = self
            .latest
            .lock()
            .expect("punch allocation poisoned")
            .clone()
        else {
            return;
        };
        let Some(agent) = allocation.agent.upgrade() else {
            return;
        };
        let measured = *self.measured.lock().expect("punch measurements poisoned");
        if !measured.peer_allows {
            log::info!("probe punching skipped: the peer turned it off");
            return;
        }
        let serial = agent.controller_serial.lock().await;
        let probe = {
            let current = agent.ufrag_pwd.lock().await;
            if current.local_ufrag != snapshot.username || current.local_pwd != snapshot.password {
                return;
            }
            let Some(probe) = Probe::derive(
                &current.local_ufrag,
                &current.local_pwd,
                &current.remote_ufrag,
                &current.remote_pwd,
            ) else {
                return;
            };
            probe
        };
        // Native role starts as unspecified. Do not classify
        // a pre-start Agent as controlled merely because its bool is false.
        if !agent.ice_role_assigned.load(Ordering::Acquire) {
            return;
        }
        let mut local = BTreeMap::<BaseAddress, Vec<LocalCandidate>>::new();
        let mut ports = BTreeMap::<BaseAddress, Arc<LocalPort>>::new();
        for port in agent.local_ports.lock().await.iter() {
            if !port.ready.load(Ordering::Acquire)
                || port.pruned.load(Ordering::Acquire)
                || port.closed.is_cancelled()
            {
                continue;
            }
            for candidate in port.aliases.lock().expect("port aliases poisoned").iter() {
                if candidate.candidate_type() == CandidateType::ServerReflexive
                    && candidate.network_type().is_udp()
                    && candidate.generation() == allocation.generation
                {
                    let key = BaseAddress::of(&**candidate);
                    local
                        .entry(key.clone())
                        .or_default()
                        .push(candidate.clone());
                    ports.insert(key, port.clone());
                }
            }
        }
        let mut remote = BTreeMap::<BaseAddress, Vec<LocalCandidate>>::new();
        for candidate in agent.remote_candidates.lock().await.values().flatten() {
            if candidate.candidate_type() == CandidateType::ServerReflexive
                && candidate.network_type().is_udp()
                && candidate.generation() == allocation.generation
            {
                remote
                    .entry(BaseAddress::of(&**candidate))
                    .or_default()
                    .push(candidate.clone());
            }
        }
        // The measurement covers the default route; it stands for every
        // group of that side.
        let local = local
            .into_iter()
            .map(|(key, values)| {
                (
                    key,
                    Group::classify(values, allocation.stun_servers, measured.local),
                )
            })
            .collect::<Vec<_>>();
        let remote = remote
            .into_iter()
            .map(|(key, values)| {
                (
                    key,
                    Group::classify(values, allocation.stun_servers, measured.remote),
                )
            })
            .collect::<Vec<_>>();
        let controlling = agent.is_controlling.load(Ordering::Acquire);
        let mut choices = Vec::new();
        // Normalizes the controlling side to the outer loop, then uses
        // stable score ordering, so both peers agree on equal-cost groups.
        if controlling {
            for (key, l) in &local {
                for (_, r) in &remote {
                    choices.push((pair_score(l, r), key, l, r));
                }
            }
        } else {
            for (_, r) in &remote {
                for (key, l) in &local {
                    choices.push((pair_score(r, l), key, l, r));
                }
            }
        }
        choices.sort_by_key(|choice| choice.0);
        let Some((score, key, l, r)) = choices.first() else {
            log::debug!("NAT analysis skipped: no suitable candidate groups");
            return;
        };
        let direct = agent.agent_conn.get_selected_pair().is_some_and(|pair| {
            pair.local_candidate().candidate_type() != CandidateType::Relay
                && pair.remote.candidate_type() != CandidateType::Relay
        });
        log::info!(
            "NAT groups: local={:?}/{} remote={:?}/{} step={} measured={:?}/{:?} score={} direct={direct}",
            l.kind,
            l.candidates.len(),
            r.kind,
            r.candidates.len(),
            r.step as i16,
            measured.local,
            measured.remote,
            score
        );
        if direct {
            return;
        }
        let Some(base) = ports.get(*key).cloned() else {
            return;
        };
        base.set_idle_timeout(Duration::from_millis(1_728_000_000));
        *self.target.lock().expect("punch target poisoned") = Some(Target {
            base: base.clone(),
            hint: r.last().clone(),
        });
        let plan = Plan {
            base,
            local_bind: key.ip,
            remote: (**r).clone(),
            local_kind: l.kind,
            controlling,
            probe,
        };
        drop(serial);
        let Some(owner) = allocation.owner.upgrade() else {
            return;
        };
        let sender = {
            let mut actor = self.actor.lock().expect("punch actor poisoned");
            actor
                .get_or_insert_with(|| {
                    let (sender, receiver) = mpsc::unbounded_channel();
                    let coordinator = Arc::downgrade(self);
                    owner.spawn(async move {
                        actor::Actor::new(coordinator, receiver).run().await;
                    });
                    sender
                })
                .clone()
        };
        self.running.store(true, Ordering::Release);
        let _ = sender.send(actor::Command::Start(plan));
    }

    pub fn stop_if_direct(&self, agent: &AgentInternal) {
        let direct = agent.agent_conn.get_selected_pair().is_some_and(|pair| {
            pair.local_candidate().candidate_type() != CandidateType::Relay
                && pair.remote.candidate_type() != CandidateType::Relay
        });
        if direct && self.running.swap(false, Ordering::AcqRel) {
            if let Some(actor) = self.actor.lock().expect("punch actor poisoned").as_ref() {
                let _ = actor.send(actor::Command::Direct);
            }
        }
    }

    pub fn clear(&self) {
        self.actor.lock().expect("punch actor poisoned").take();
        self.target.lock().expect("punch target poisoned").take();
        self.latest
            .lock()
            .expect("punch allocation poisoned")
            .take();
        self.running.store(false, Ordering::Release);
    }

    /// Caller holds controller_serial, just like Port's synchronous callback.
    pub async fn on_request(&self, agent: &AgentInternal, peer: SocketAddr) {
        let target = self.target.lock().expect("punch target poisoned").clone();
        if let Some(target) = target {
            Self::attach(agent, &target.base, &target.hint, peer).await;
        }
    }

    async fn attach(
        agent: &AgentInternal,
        port: &Arc<LocalPort>,
        hint: &LocalCandidate,
        peer: SocketAddr,
    ) {
        if port.closed.is_cancelled() || !port.ready.load(Ordering::Acquire) {
            return;
        }
        let mut predicted = remote_description_copy(&**hint);
        predicted.id = crate::rand::generate_cand_id();
        predicted.address = peer.ip().to_string();
        predicted.port = peer.port();
        if predicted.set_ip(&peer.ip()).is_err() {
            return;
        }
        // Directly adds only this Connection; no remembered candidate,
        // candidate broadcast, or extra immediate-sort request.
        agent
            .add_pair(port.canonical.clone(), Arc::new(predicted))
            .await;
    }

    fn promote(self: &Arc<Self>, local_port: u16, peer: SocketAddr) {
        let Some(allocation) = self
            .latest
            .lock()
            .expect("punch allocation poisoned")
            .clone()
        else {
            return;
        };
        let Some(owner) = allocation.owner.upgrade() else {
            return;
        };
        let weak = Arc::downgrade(self);
        owner.spawn(async move {
            let Some(coordinator) = weak.upgrade() else {
                return;
            };
            // Uses the latest session and current template when
            // the network-thread callback runs, not the old probe's epoch.
            let Some(allocation) = coordinator
                .latest
                .lock()
                .expect("punch allocation poisoned")
                .clone()
            else {
                return;
            };
            let Some(target) = coordinator
                .target
                .lock()
                .expect("punch target poisoned")
                .clone()
            else {
                return;
            };
            let Some(session) = allocation.session.upgrade() else {
                return;
            };
            let key = target.base.canonical.network_key();
            if !session.has_punch_network(&key).await {
                return;
            }
            let Some(interface) = allocation.catalog.lock().await.network(&key) else {
                return;
            };
            let Some(agent) = allocation.agent.upgrade() else {
                return;
            };
            let Some(owner) = allocation.owner.upgrade() else {
                return;
            };
            let result = async {
                let raw = allocation
                    .net
                    .bind(SocketAddr::new(interface.ip, local_port))
                    .await?;
                let socket = SharedUdpSocket::new(raw)?;
                let transport: Arc<dyn Conn + Send + Sync> = socket.clone();
                owner.socket(&transport);
                let address = socket.local_addr()?;
                let candidate: LocalCandidate = Arc::new(
                    CandidateHostConfig {
                        base_config: CandidateBaseConfig {
                            network: "udp".into(),
                            address: address.ip().to_string(),
                            port: address.port(),
                            component: 1,
                            priority: ice_candidate_priority(
                                126,
                                interface.network_preference,
                                address.ip(),
                                0,
                            ),
                            conn: Some(transport),
                            network_key: key.clone(),
                            network_id: interface.network_id,
                            network_cost: Some(ice_network_cost(interface.adapter_type)),
                            adapter_type: interface.adapter_type,
                            generation: allocation.generation,
                            ..Default::default()
                        },
                        ..Default::default()
                    }
                    .new_candidate_host()?,
                );
                let port = agent
                    .register_local_port(candidate, false, CandidatePair::now_nanos())
                    .await?;
                session.attach_punch_port(&key, &port).await;
                {
                    let _serial = agent.controller_serial.lock().await;
                    if port.gathering.load(Ordering::Acquire) == PORT_GATHERING
                        && !port.closed.is_cancelled()
                    {
                        port.ready.store(true, Ordering::Release); // No KeepAliveUntilPruned, fan-out or SDP.
                        Self::attach(&agent, &port, &target.hint, peer).await;
                    }
                    port.complete(false);
                }
                socket.start_reader(
                    &agent,
                    &port,
                    Arc::new(turn::client::transaction::TransactionManager::new(
                        port.closed.child_token(),
                    )),
                )?;
                if port.ready.load(Ordering::Acquire) {
                    log::info!("punch promoted private UDP Port: {address} -> {peer}");
                } else {
                    log::debug!(
                        "punch Port was not activated after generation change: {address}"
                    );
                }
                Ok::<_, crate::Error>(())
            }
            .await;
            if let Err(error) = result {
                log::warn!("punch Port promotion failed: {error}");
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn probe_is_shared_and_bound_to_credentials() {
        let a = Probe::derive("ufA", "passwordA", "ufB", "passwordB").unwrap();
        let b = Probe::derive("ufB", "passwordB", "ufA", "passwordA").unwrap();
        // Both peers derive the same datagrams.
        assert_eq!(a, b);
        assert!(Probe::looks_like_request(&a.request));
        assert!(!Probe::looks_like_request(&a.response));
        assert_ne!(a.request[4..], a.response[4..]);
        // Another session (an ICE restart) has other tags.
        let restarted = Probe::derive("ufA2", "passwordA2", "ufB", "passwordB").unwrap();
        assert_ne!(a.request, restarted.request);
        // Without the peer's credentials there is no probe.
        assert!(Probe::derive("ufA", "passwordA", "", "").is_none());
        // Other traffic never matches the cheap check.
        assert!(!Probe::looks_like_request(&[0x00, 0x01, 0x00, 0x00]));
        assert!(!Probe::looks_like_request(&a.request[..4]));
    }
}
