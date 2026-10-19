//! Asynchronous per-Network allocation sequences and continual updates.
//! Socket ownership and candidate publication are deliberately separate.
use std::collections::HashSet;
use std::net::{IpAddr, SocketAddr};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex as StdMutex, Weak};

use stun::addr::MappedAddress;
use stun::agent::TransactionId;
use stun::attributes::*;
use stun::message::*;
use stun::xoraddr::XorMappedAddress;
use tokio::sync::{Mutex, Notify};
use tokio::time::{Duration, Instant};
use tokio_util::sync::CancellationToken;
use turn::client::transaction::{TransactionManager, DEFAULT_RTO_MS};
use util::Conn;

use super::agent_gather::GatherCandidatesInternalParams;
use super::local_port::{LocalCandidate, LocalPort};
use super::shared_socket::SharedUdpSocket;
use crate::candidate::{
    candidate_base::CandidateBaseConfig, candidate_host::CandidateHostConfig,
    candidate_relay::CandidateRelayConfig,
    candidate_server_reflexive::CandidateServerReflexiveConfig, Candidate, CandidatePair,
    CandidateType, COMPONENT_RTP,
};
use crate::error::{Error, Result};
use crate::mdns::MulticastDnsMode;
use crate::state::GatheringState;
use crate::udp_network::UDPNetwork;
use crate::url::{ProtoType, SchemeType, Url};
use crate::util::{
    listen_udp_in_port_range, ice_candidate_priority, ice_network_cost, LocalInterface,
};

pub(super) struct AllocationSession {
    pub generation: u32,
    pub stop: CancellationToken,
    sequences: Mutex<Vec<Arc<Sequence>>>,
    pending: AtomicUsize,
    completed: Notify,
}

impl AllocationSession {
    pub(super) async fn has_punch_network(&self, key: &str) -> bool {
        self.sequences
            .lock()
            .await
            .iter()
            .any(|sequence| sequence.interface.network_key == key)
    }

    pub(super) async fn attach_punch_port(&self, key: &str, port: &Arc<LocalPort>) {
        if let Some(sequence) = self
            .sequences
            .lock()
            .await
            .iter()
            .find(|sequence| sequence.interface.network_key == key)
        {
            // This registers fresh PortData even when the matched sequence
            // is already stopped; it is not ordinary Sequence::add_port.
            sequence
                .ports
                .lock()
                .expect("sequence ports poisoned")
                .push(Arc::downgrade(port));
        }
    }
    pub fn new(generation: u32) -> Arc<Self> {
        Arc::new(Self {
            generation,
            stop: CancellationToken::new(),
            sequences: Mutex::new(Vec::new()),
            pending: AtomicUsize::new(0),
            completed: Notify::new(),
        })
    }
    fn work(self: &Arc<Self>) -> AllocationWork {
        self.pending.fetch_add(1, Ordering::Relaxed);
        AllocationWork(self.clone())
    }
}

struct AllocationWork(Arc<AllocationSession>);
impl Drop for AllocationWork {
    fn drop(&mut self) {
        if self.0.pending.fetch_sub(1, Ordering::AcqRel) == 1 {
            self.0.completed.notify_one();
        }
    }
}

struct Sequence {
    interface: LocalInterface,
    stop_phases: CancellationToken,
    failed: AtomicBool,
    adapter_type: AtomicU32,
    gather_udp: bool,
    gather_tcp: bool,
    gather_relay: bool,
    ports: StdMutex<Vec<Weak<LocalPort>>>,
    socket: Mutex<Option<Arc<SharedUdpSocket>>>,
}

impl Sequence {
    fn add_port(&self, port: &Arc<LocalPort>) {
        if self.failed.load(Ordering::Acquire) || self.stop_phases.is_cancelled() {
            port.prune();
        }
        self.ports
            .lock()
            .expect("sequence ports poisoned")
            .push(Arc::downgrade(port));
    }
    fn fail(&self) {
        self.failed.store(true, Ordering::Release);
        self.stop_phases.cancel();
    }
}

pub(super) async fn run(params: GatherCandidatesInternalParams, session: Arc<AllocationSession>) {
    let params = Arc::new(params);
    let mut network_tick = tokio::time::interval(Duration::from_millis(500));
    network_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut failed_tick = tokio::time::interval_at(
        Instant::now() + super::agent_config::ICE_REGATHER_FAILED_NETWORKS_INTERVAL,
        super::agent_config::ICE_REGATHER_FAILED_NETWORKS_INTERVAL,
    );
    failed_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut current = Arc::new(Vec::<LocalInterface>::new());
    let mut initial = true;
    loop {
        tokio::select! {
            biased;
            _ = session.stop.cancelled() => break,
            _ = network_tick.tick() => {
                let update = params.network_catalog.lock().await.update(
                    &params.net, &params.interface_filter, &params.ip_filter,
                    &params.network_types, params.include_loopback,
                ).await;
                match update {
                    Ok(update) => {
                        if session.stop.is_cancelled() { break; }
                        params.agent_internal.update_network_costs(&update.type_changes).await;
                        for sequence in session.sequences.lock().await.iter() {
                            if let Some((_, adapter_type)) = update.type_changes.iter().find(|(key, _)| *key == sequence.interface.network_key) {
                                sequence.adapter_type.store(*adapter_type, Ordering::Release);
                            }
                        }
                        current = update.interfaces;
                        if initial || update.changed {
                            initial = false;
                            let keys = current.iter().map(|i| i.network_key.clone()).collect::<HashSet<_>>();
                            let mut lost = HashSet::new();
                            let mut lost_types = HashSet::new();
                            for sequence in session.sequences.lock().await.iter() {
                                if !sequence.failed.load(Ordering::Acquire)
                                    && !keys.contains(&sequence.interface.network_key)
                                {
                                    sequence.fail();
                                    lost.insert(sequence.interface.network_key.clone());
                                    lost_types.insert(sequence.adapter_type.load(Ordering::Acquire));
                                }
                            }
                            // The adapter-type failure event is sent
                            // before retiring affected Ports.
                            params.agent_internal.networks_failed(&lost_types, session.generation).await;
                            params.agent_internal.prune_local_candidates_on_networks(&lost, session.generation).await;
                            schedule_networks(&params, &session, &current).await;
                        }
                    }
                    Err(error) => {
                        log::warn!("NetworkManager enumeration failed; retaining previous networks: {error}");
                        if initial {
                            let cached = params.network_catalog.lock().await.snapshot(
                                &params.interface_filter, &params.ip_filter, &params.network_types, params.include_loopback,
                            );
                            if let Some(cached) = cached {
                                current = Arc::new(cached);
                                initial = false;
                                schedule_networks(&params, &session, &current).await;
                            }
                        }
                    },
                }
            }
            _ = failed_tick.tick(), if params.continual_gathering => {
                let failed = params.agent_internal.failed_network_names_for_regather((*current).clone()).await;
                if !failed.is_empty() {
                    for sequence in session.sequences.lock().await.iter() {
                        if failed.contains(&sequence.interface.network_key) { sequence.fail(); }
                    }
                    params.agent_internal.prune_local_candidates_on_networks(&failed, session.generation).await;
                    schedule_networks(&params, &session, &current).await;
                }
            }
            _ = session.completed.notified() => {
                if session.pending.load(Ordering::Acquire) == 0 && !params.continual_gathering {
                    params.gathering_state.store(GatheringState::Complete as u8, Ordering::Release);
                    if let Some(tx) = &*params.chan_candidate_tx.lock().await { let _ = tx.send(None).await; }
                    break;
                }
            }
        }
    }
}

async fn schedule_networks(
    params: &Arc<GatherCandidatesInternalParams>,
    session: &Arc<AllocationSession>,
    interfaces: &[LocalInterface],
) {
    let _work = session.work();
    for interface in interfaces {
        if session.stop.is_cancelled() {
            break;
        }
        let mut sequences = session.sequences.lock().await;
        let matching = sequences
            .iter()
            .filter(|s| {
                !s.failed.load(Ordering::Acquire)
                    && s.interface.network_key == interface.network_key
                    && s.interface.ip == interface.ip
            })
            .collect::<Vec<_>>();
        let port_exists = |tcp: bool| {
            matching.iter().any(|sequence| {
                sequence
                    .ports
                    .lock()
                    .expect("sequence ports poisoned")
                    .iter()
                    .filter_map(Weak::upgrade)
                    .any(|port| {
                        port.canonical.candidate_type() == CandidateType::Host
                            && port.canonical.network_type().is_tcp() == tcp
                            && !port.closed.is_cancelled()
                            && port.gathering.load(Ordering::Acquire)
                                != super::local_port::PORT_ERROR
                            && !port.pruned.load(Ordering::Acquire)
                    })
            })
        };
        let udp_requested = params
            .network_types
            .iter()
            .any(|kind| kind.is_udp() && kind.is_ipv4() == interface.ip.is_ipv4());
        let tcp_requested = params
            .network_types
            .iter()
            .any(|kind| kind.is_tcp() && kind.is_ipv4() == interface.ip.is_ipv4());
        let gather_udp = udp_requested && !port_exists(false);
        let gather_tcp = tcp_requested && !port_exists(true);
        if !matching.is_empty() && !gather_udp && !gather_tcp {
            continue;
        }
        // An existing matching Sequence/config suppresses repeat relay
        // allocation, but a missing/error/pruned UDP Port may still be rebuilt.
        let gather_relay = matching.is_empty();
        let sequence = Arc::new(Sequence {
            interface: interface.clone(),
            stop_phases: session.stop.child_token(),
            failed: AtomicBool::new(false),
            adapter_type: AtomicU32::new(interface.adapter_type),
            gather_udp,
            gather_tcp,
            gather_relay,
            ports: StdMutex::new(Vec::new()),
            socket: Mutex::new(None),
        });
        sequences.push(sequence.clone());
        drop(sequences);
        let params2 = params.clone();
        let session2 = session.clone();
        let work = session.work();
        params.owner.spawn(async move {
            let _work = work;
            if let Err(error) = run_sequence(params2, session2, sequence).await {
                log::warn!("allocation sequence failed: {error}");
            }
        });
    }
}

async fn prepare_udp_port(
    params: Arc<GatherCandidatesInternalParams>,
    session: Arc<AllocationSession>,
    sequence: Arc<Sequence>,
) -> Result<()> {
    let interface = &sequence.interface;
    let UDPNetwork::Ephemeral(config) = &params.udp_network else {
        return Err(Error::Other(
            "allocation sequences require per-network UDP sockets, not an external UDP mux"
                .into(),
        ));
    };
    let raw = if config.is_shared() {
        crate::shared_port::lease(
            &params.net,
            SocketAddr::new(interface.ip, config.port_min()),
            Box::new(Arc::downgrade(&params.agent_internal)),
        )
        .await?
    } else {
        listen_udp_in_port_range(
            &params.net,
            config.port_max(),
            config.port_min(),
            SocketAddr::new(interface.ip, 0),
        )
        .await?
    };
    let shared = SharedUdpSocket::new(raw)?;
    let owned_socket: Arc<dyn Conn + Send + Sync> = shared.clone();
    params.owner.socket(&owned_socket);
    let socket_addr = shared.local_addr()?;
    let mut host_ip = interface.ip;
    if host_ip.is_unspecified() {
        if let Some(ip) = params
            .network_catalog
            .lock()
            .await
            .default_ip(host_ip.is_ipv6())
        {
            host_ip = ip;
        }
    }
    if let Some(mapper) = &*params.ext_ip_mapper {
        if mapper.candidate_type == CandidateType::Host {
            host_ip = mapper.find_external_ip(&host_ip.to_string())?;
        }
    }
    let address = if params.mdns_mode == MulticastDnsMode::QueryAndGather {
        params.mdns_name.clone()
    } else {
        host_ip.to_string()
    };
    let candidate = CandidateHostConfig {
        base_config: CandidateBaseConfig {
            network: "udp".into(),
            address,
            port: socket_addr.port(),
            component: COMPONENT_RTP,
            priority: ice_candidate_priority(126, interface.network_preference, host_ip, 0),
            conn: Some(shared.lease()),
            network_key: interface.network_key.clone(),
            network_id: interface.network_id,
            network_cost: Some(ice_network_cost(interface.adapter_type)),
            adapter_type: interface.adapter_type,
            generation: session.generation,
            ..Default::default()
        },
        ..Default::default()
    }
    .new_candidate_host()?;
    if params.mdns_mode == MulticastDnsMode::QueryAndGather {
        candidate.set_ip(&interface.ip)?;
    }
    let candidate: LocalCandidate = Arc::new(candidate);
    let port = params
        .agent_internal
        .register_local_port(candidate.clone(), false, CandidatePair::now_nanos())
        .await?;
    sequence.add_port(&port);
    let stun = Arc::new(TransactionManager::new(port.closed.child_token()));
    shared.start_reader(&params.agent_internal, &port, stun.clone())?;
    *sequence.socket.lock().await = Some(shared.clone());
    if params.candidate_types.contains(&CandidateType::Host) && !host_ip.is_unspecified() {
        params
            .agent_internal
            .publish_port_candidate(&port, candidate, true)
            .await?;
    }
    schedule_stun(&params, &session, port, shared, stun, interface.clone());
    Ok(())
}

async fn run_sequence(
    params: Arc<GatherCandidatesInternalParams>,
    session: Arc<AllocationSession>,
    sequence: Arc<Sequence>,
) -> Result<()> {
    if sequence.gather_udp {
        if let Err(error) =
            prepare_udp_port(params.clone(), session.clone(), sequence.clone()).await
        {
            log::warn!(
                "UDP Port preparation failed on {}: {error}",
                sequence.interface.network_key
            );
        }
    }
    // AllocationSequence phases are 50ms apart, not a wait for STUN replies.
    tokio::select! {
        biased;
        _ = sequence.stop_phases.cancelled() => return Ok(()),
        _ = tokio::time::sleep(Duration::from_millis(50)) => {},
    }
    if sequence.gather_relay && params.candidate_types.contains(&CandidateType::Relay) {
        let urls = params
            .urls
            .iter()
            .filter(|url| matches!(url.scheme, SchemeType::Turn | SchemeType::Turns))
            .cloned()
            .collect::<Vec<_>>();
        let count = urls.len();
        for (index, url) in urls.into_iter().enumerate() {
            schedule_relay(
                &params,
                &session,
                sequence.clone(),
                url,
                (count - index) as u32,
                true,
            );
        }
    }
    tokio::select! {
        biased;
        _ = sequence.stop_phases.cancelled() => return Ok(()),
        _ = tokio::time::sleep(Duration::from_millis(50)) => {},
    }
    if sequence.gather_tcp {
        prepare_tcp_port(&params, &session, &sequence).await?;
    }
    Ok(())
}

async fn prepare_tcp_port(
    params: &GatherCandidatesInternalParams,
    session: &AllocationSession,
    sequence: &Arc<Sequence>,
) -> Result<()> {
    let (min, max) = match &params.udp_network {
        UDPNetwork::Ephemeral(config) => (config.port_min(), config.port_max()),
        UDPNetwork::Muxed(_) => (0, 0),
    };
    let interface = &sequence.interface;
    let socket = super::tcp_port::TcpPort::new(
        interface.clone(),
        params.network_catalog.clone(),
        &params.agent_internal,
        min,
        max,
    );
    let address = socket.local_addr()?;
    let tcp_type = if socket.passive() {
        crate::tcp_type::TcpType::Passive
    } else {
        crate::tcp_type::TcpType::Active
    };
    let transport: Arc<dyn Conn + Send + Sync> = socket.clone();
    params.owner.socket(&transport);
    let candidate: LocalCandidate = Arc::new(
        CandidateHostConfig {
            base_config: CandidateBaseConfig {
                network: "tcp".into(),
                address: address.ip().to_string(),
                port: address.port(),
                component: COMPONENT_RTP,
                priority: ice_candidate_priority(90, interface.network_preference, address.ip(), 0),
                conn: Some(transport),
                network_key: interface.network_key.clone(),
                network_id: interface.network_id,
                network_cost: Some(ice_network_cost(interface.adapter_type)),
                adapter_type: interface.adapter_type,
                generation: session.generation,
                ..Default::default()
            },
            tcp_type,
        }
        .new_candidate_host()?,
    );
    let port = params
        .agent_internal
        .register_local_port(candidate.clone(), false, CandidatePair::now_nanos())
        .await?;
    sequence.add_port(&port);
    socket.start(&port);
    if params.candidate_types.contains(&CandidateType::Host) && !address.ip().is_unspecified() {
        params
            .agent_internal
            .publish_port_candidate(&port, candidate, true)
            .await?;
    }
    port.complete(false);
    Ok(())
}

fn schedule_stun(
    params: &Arc<GatherCandidatesInternalParams>,
    session: &Arc<AllocationSession>,
    port: Arc<LocalPort>,
    shared: Arc<SharedUdpSocket>,
    manager: Arc<TransactionManager>,
    interface: LocalInterface,
) {
    let work = session.work();
    let params2 = params.clone();
    let session = session.clone();
    params.owner.spawn(async move {
        let _work = work;
        let mut servers = Vec::new();
        for url in params2
            .urls
            .iter()
            .filter(|url| url.scheme == SchemeType::Stun)
        {
            let resolved = tokio::select! {
                biased;
                _ = port.closed.cancelled() => return,
                result = resolve_server(&params2, url, interface.ip) => result,
            };
            if let Ok(server) = resolved {
                if !servers.contains(&server) {
                    servers.push(server);
                    shared.add_stun_server(server);
                }
            }
        }
        let state = Arc::new(StunPortState {
            complete: StdMutex::new(HashSet::new()),
            succeeded: StdMutex::new(HashSet::new()),
            server_count: servers.len(),
        });
        if servers.is_empty() {
            port.complete(false);
        }
        for server in servers {
            let args = StunBinding {
                params: params2.clone(),
                session: session.clone(),
                port: port.clone(),
                state: state.clone(),
                manager: manager.clone(),
                conn: shared.lease(),
                interface: interface.clone(),
                server,
            };
            let work = session.work();
            params2.owner.spawn(async move {
                run_stun_binding(args, work).await;
            });
        }
    });
}

struct StunPortState {
    complete: StdMutex<HashSet<SocketAddr>>,
    succeeded: StdMutex<HashSet<SocketAddr>>,
    server_count: usize,
}

struct StunBinding {
    params: Arc<GatherCandidatesInternalParams>,
    session: Arc<AllocationSession>,
    port: Arc<LocalPort>,
    state: Arc<StunPortState>,
    manager: Arc<TransactionManager>,
    conn: Arc<dyn Conn + Send + Sync>,
    interface: LocalInterface,
    server: SocketAddr,
}

async fn run_stun_binding(args: StunBinding, work: AllocationWork) {
    let started = Instant::now();
    let mut first_work = Some(work);
    loop {
        let mut request = Message::new();
        if request
            .build(&[Box::new(TransactionId::new()), Box::new(BINDING_REQUEST)])
            .is_err()
        {
            break;
        }
        let transaction = match args.manager.start(
            args.conn.clone(),
            &request,
            args.server,
            None,
            DEFAULT_RTO_MS,
            false,
        ) {
            Ok(transaction) => transaction,
            Err(_) => break,
        };
        let response = transaction.wait().await;
        let mut stop = false;
        match response {
            Ok(response) if response.msg.typ.class == CLASS_SUCCESS_RESPONSE => {
                if let Ok(address) = mapped_address(&response.msg) {
                    let first = args
                        .state
                        .succeeded
                        .lock()
                        .expect("STUN results poisoned")
                        .insert(args.server);
                    if first
                        && address != args.conn.local_addr().unwrap_or(args.port.canonical.addr())
                    {
                        let alias = CandidateServerReflexiveConfig {
                            base_config: CandidateBaseConfig {
                                network: "udp".into(),
                                address: address.ip().to_string(),
                                port: address.port(),
                                component: COMPONENT_RTP,
                                priority: ice_candidate_priority(
                                    100,
                                    args.interface.network_preference,
                                    address.ip(),
                                    0,
                                ),
                                network_key: args.interface.network_key.clone(),
                                network_id: args.interface.network_id,
                                network_cost: Some(args.port.canonical.network_cost()),
                                adapter_type: args.interface.adapter_type,
                                generation: args.session.generation,
                                url: format!("stun:{}", args.server),
                                ..Default::default()
                            },
                            rel_addr: args.interface.ip.to_string(),
                            rel_port: args.port.canonical.port(),
                        }
                        .new_candidate_server_reflexive();
                        if args
                            .params
                            .candidate_types
                            .contains(&CandidateType::ServerReflexive)
                        {
                            if let Ok(alias) = alias {
                                let _ = args
                                    .params
                                    .agent_internal
                                    .publish_port_candidate(&args.port, Arc::new(alias), true)
                                    .await;
                            }
                        }
                    }
                    stun_first_completed(&args);
                    first_work.take();
                } else {
                    log::warn!(
                        "Binding response missing valid mapped address from {}",
                        args.server
                    );
                }
            }
            Ok(response) => {
                log::debug!(
                    "Binding error response from {}: {}",
                    args.server,
                    response.msg.typ
                );
                stun_first_completed(&args);
                first_work.take();
                stop = started.elapsed() >= Duration::from_secs(50);
            }
            Err(error) => {
                if !args.port.closed.is_cancelled() {
                    log::debug!("STUN binding ended for {}: {error}", args.server);
                    stun_first_completed(&args);
                }
                break;
            }
        }
        if stop
            || (args.port.canonical.network_cost() >= 900
                && started.elapsed() > Duration::from_secs(120))
        {
            break;
        }
        tokio::select! {
            biased;
            _ = args.port.closed.cancelled() => break,
            _ = tokio::time::sleep(Duration::from_secs(10)) => {},
        }
    }
}

fn stun_first_completed(args: &StunBinding) {
    let mut completed = args.state.complete.lock().expect("STUN results poisoned");
    completed.insert(args.server);
    if completed.len() == args.state.server_count {
        args.port.complete(false);
    }
}

fn mapped_address(message: &Message) -> std::result::Result<SocketAddr, stun::Error> {
    if message.contains(ATTR_MAPPED_ADDRESS) {
        let mut address = MappedAddress::default();
        address.get_from(message)?;
        Ok(SocketAddr::new(address.ip, address.port))
    } else {
        let mut address = XorMappedAddress::default();
        address.get_from(message)?;
        Ok(SocketAddr::new(address.ip, address.port))
    }
}

async fn resolve_server(
    params: &GatherCandidatesInternalParams,
    url: &Url,
    local: IpAddr,
) -> Result<SocketAddr> {
    let host_port = if url.host.contains(':') {
        format!("[{}]:{}", url.host, url.port)
    } else {
        format!("{}:{}", url.host, url.port)
    };
    Ok(params.net.resolve_addr(local.is_ipv4(), &host_port).await?)
}

fn schedule_relay(
    params: &Arc<GatherCandidatesInternalParams>,
    session: &Arc<AllocationSession>,
    sequence: Arc<Sequence>,
    url: Url,
    preference: u32,
    share_udp: bool,
) {
    // A literal server of the other address family is skipped before
    // constructing a Port. Unresolved names follow the per-family resolver.
    if url
        .host
        .parse::<IpAddr>()
        .is_ok_and(|server| server.is_ipv4() != sequence.interface.ip.is_ipv4())
    {
        log::debug!(
            "skipping TURN server with incompatible address family: {} on {}",
            url.host,
            sequence.interface.ip
        );
        return;
    }
    let params2 = params.clone();
    let session2 = session.clone();
    let work = session.work();
    params.owner.spawn(async move {
        let _work = work;
        if let Err(error) =
            allocate_relay(params2, session2, sequence, url, preference, share_udp).await
        {
            log::warn!("TURN Port allocation failed: {error}");
        }
    });
}

#[derive(Default)]
struct PendingRelay {
    base: Option<Arc<dyn Conn + Send + Sync>>,
    client: Option<Arc<turn::client::Client>>,
    relay: Option<Arc<dyn Conn + Send + Sync>>,
}

impl PendingRelay {
    async fn close_current(&mut self, close_transport: bool) {
        if let Some(relay) = self.relay.take() {
            let _ = relay.close().await;
        }
        if let Some(client) = self.client.take() {
            if close_transport {
                let _ = client.close().await;
            } else {
                client.detach_transport().await;
            }
        }
        if close_transport {
            if let Some(base) = self.base.take() {
                let _ = base.close().await;
            }
        }
    }
}

type ReadyRelay = (
    Arc<turn::client::Client>,
    Arc<dyn Conn + Send + Sync>,
    SocketAddr,
    SocketAddr,
);

async fn prepare_relay_transport(
    params: &GatherCandidatesInternalParams,
    sequence: &Sequence,
    url: &Url,
    share_udp: bool,
    pending: &mut PendingRelay,
) -> Result<ReadyRelay> {
    let local_ip = sequence.interface.ip;
    let mut remote = resolve_server(params, url, local_ip).await?;
    let mut visited = HashSet::from([remote]);
    let mut shared = if share_udp && url.proto == ProtoType::Udp {
        sequence.socket.lock().await.clone()
    } else {
        None
    };
    let mut udp_socket: Option<Arc<dyn Conn + Send + Sync>> = None;
    let mut auth = turn::client::AllocationAuth::default();
    let mut mismatch_retries = 0;
    loop {
        let base = if let Some(shared) = &shared {
            shared.lease()
        } else if url.proto == ProtoType::Udp {
            if udp_socket.is_none() {
                let socket = params.net.bind(SocketAddr::new(local_ip, 0)).await?;
                crate::socket_options::configure_udp_media_buffers(socket.as_ref());
                udp_socket = Some(socket);
            }
            udp_socket.as_ref().expect("UDP socket was created").clone()
        } else {
            crate::turn_stream_conn::TurnStreamConn::connect(
                remote,
                Some(local_ip),
                url.scheme == SchemeType::Turns,
                params.agent_internal.insecure_skip_verify,
            )
            .await?
        };
        params.owner.socket(&base);
        pending.base = Some(base.clone());
        let local = base.local_addr()?;
        let client = Arc::new(
            turn::client::Client::new(turn::client::ClientConfig {
                transport: if url.proto == ProtoType::Udp {
                    turn::client::ClientTransport::Udp
                } else {
                    turn::client::ClientTransport::Tcp
                },
                stun_serv_addr: String::new(),
                turn_serv_addr: remote.to_string(),
                username: url.username.clone(),
                password: url.password.clone(),
                realm: String::new(),
                software: String::new(),
                rto_in_ms: 0,
                conn: base,
                vnet: Some(params.net.clone()),
            })
            .await
            .map_err(|error| Error::Other(error.to_string()))?,
        );
        params.owner.client(&client);
        pending.client = Some(client.clone());
        client.set_allocation_auth(auth.clone()).await;
        if let Some(shared) = &shared {
            shared.add_turn(&client.shared_ingress());
        } else {
            client
                .listen()
                .await
                .map_err(|error| Error::Other(error.to_string()))?;
        }
        match client.allocate().await {
            Ok(allocation) => {
                let allocation: Arc<dyn Conn + Send + Sync> = Arc::new(allocation);
                params.owner.relay(&allocation);
                pending.relay = Some(allocation.clone());
                return Ok((client, allocation, local, remote));
            }
            Err(turn::Error::AllocationRedirect {
                target,
                realm,
                nonce,
            }) => {
                // Visited, loopback and incompatible-family
                // alternates fail as 300; protocol is never changed by redirect.
                let compatible = local_ip.is_ipv4() == target.ip().is_ipv4()
                    && match (local_ip, target.ip()) {
                        (IpAddr::V6(a), IpAddr::V6(b)) => {
                            a.is_unicast_link_local() == b.is_unicast_link_local()
                        }
                        _ => true,
                    };
                if visited.contains(&target) || target.ip().is_loopback() || !compatible {
                    return Err(Error::Other(
                        "TURN Allocate rejected alternate server (300)".into(),
                    ));
                }
                auth = client.allocation_auth().await;
                if let Some(realm) = realm {
                    if realm != auth.realm {
                        auth.realm = realm;
                        auth.signed = true;
                    }
                }
                if let Some(nonce) = nonce {
                    auth.nonce = nonce;
                }
                log::info!(
                    "TURN Allocate redirect: {remote} -> {target}, transport={:?}",
                    url.proto
                );
                pending.close_current(url.proto == ProtoType::Tcp).await;
                remote = target;
                visited.insert(target);
            }
            Err(turn::Error::AllocationMismatch) if mismatch_retries < 2 => {
                mismatch_retries += 1;
                log::info!("TURN Allocate 437: replacing socket, attempt={mismatch_retries}");
                pending.close_current(true).await;
                // Resetting auth leaves the Sequence's shared UDP
                // socket untouched. This TurnPort's replacement is private.
                shared = None;
                udp_socket = None;
                auth = turn::client::AllocationAuth::default();
            }
            Err(error) => return Err(Error::Other(error.to_string())),
        }
    }
}

async fn allocate_relay(
    params: Arc<GatherCandidatesInternalParams>,
    session: Arc<AllocationSession>,
    sequence: Arc<Sequence>,
    url: Url,
    preference: u32,
    share_udp: bool,
) -> Result<()> {
    let interface = &sequence.interface;
    let created_at = CandidatePair::now_nanos();
    let expiry = Instant::now() + Duration::from_nanos(super::local_port::PORT_IDLE_NANOS);
    let mut pending = PendingRelay::default();
    let ready = tokio::time::timeout_at(
        expiry,
        prepare_relay_transport(&params, &sequence, &url, share_udp, &mut pending),
    )
    .await;
    let (client, allocation, local, server) = match ready {
        Ok(Ok(ready)) => ready,
        Ok(Err(error)) => {
            pending.close_current(true).await;
            return Err(error);
        }
        Err(_) => {
            pending.close_current(true).await;
            return Err(Error::Other(
                "unready TURN Port reached its idle expiry".into(),
            ));
        }
    };
    let protocol = if url.scheme == SchemeType::Turns {
        "tls"
    } else if url.proto == ProtoType::Tcp {
        "tcp"
    } else {
        "udp"
    };
    let mut actual_url = url.clone();
    actual_url.host = server.ip().to_string();
    actual_url.port = server.port();
    let finish = async {
        let address = allocation.local_addr()?;
        let candidate: LocalCandidate = Arc::new(
            CandidateRelayConfig {
                base_config: CandidateBaseConfig {
                    network: "udp".into(),
                    address: address.ip().to_string(),
                    port: address.port(),
                    component: COMPONENT_RTP,
                    priority: ice_candidate_priority(
                        match protocol {
                            "udp" => 2,
                            "tcp" => 1,
                            _ => 0,
                        },
                        interface.network_preference,
                        address.ip(),
                        preference,
                    ),
                    conn: Some(allocation),
                    relay_protocol: protocol.into(),
                    url: actual_url.to_string(),
                    network_key: interface.network_key.clone(),
                    network_id: interface.network_id,
                    network_cost: Some(ice_network_cost(interface.adapter_type)),
                    adapter_type: interface.adapter_type,
                    generation: session.generation,
                    ..Default::default()
                },
                rel_addr: local.ip().to_string(),
                rel_port: local.port(),
                relay_client: Some(client),
            }
            .new_candidate_relay()?,
        );
        let port = params
            .agent_internal
            .register_local_port(candidate.clone(), true, created_at)
            .await?;
        sequence.add_port(&port);
        params
            .agent_internal
            .publish_port_candidate(&port, candidate, true)
            .await?;
        port.complete(false);
        Ok::<_, Error>(())
    }
    .await;
    if finish.is_err() {
        pending.close_current(true).await;
    }
    finish
}
