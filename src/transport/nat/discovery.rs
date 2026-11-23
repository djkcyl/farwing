//! RFC 5780 NAT behaviour discovery over one local UDP socket, against the
//! server's `nat_discovery` endpoints (`ServerFeatures::nat_discovery`).
//!
//! Order matters on a single socket: the filtering tests (CHANGE-REQUEST to
//! the primary endpoint) run before the mapping tests send to any other
//! server address, which would open the NAT's filter towards it.
//!
//! Results are cached per network (default-route local IP and gateway) for
//! ten minutes. A monitor task probes at start-up and after network changes;
//! sessions only read the cache (see [`current`]).
use super::stun::{self, CHANGE_IP, CHANGE_PORT, Reply};
use super::{Behavior, NatInfo, route};
use anyhow::{Context, Result};
use std::collections::HashMap;
use std::net::{Ipv4Addr, SocketAddr};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{LazyLock, Mutex};
#[cfg(windows)]
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::net::UdpSocket;
use tokio::sync::Notify;

/// Validity of a result for its network.
const CACHE_TTL: Duration = Duration::from_secs(10 * 60);
/// Validity of a failed attempt (server unreachable, no endpoints).
const FAILURE_TTL: Duration = Duration::from_secs(60);
/// STUN retransmissions (RFC 5389 style, shortened for a background probe).
const RETRANSMITS: [Duration; 3] = [
    Duration::from_millis(250),
    Duration::from_millis(500),
    Duration::from_millis(1000),
];
/// Time for the network to settle after a change notification.
const SETTLE: Duration = Duration::from_secs(2);
/// Larger steps are treated as random allocation.
const MAX_PORT_DELTA: i32 = 64;
/// Mapping test destinations besides the primary endpoint.
const MAX_MAPPING_TARGETS: usize = 5;

/// What one filtering test (CHANGE-REQUEST) observed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Change {
    /// A response arrived from the changed address.
    Received,
    /// No response: the NAT filtered it (or it was lost three times).
    Silent,
    /// The server has no such alternate address (420) or ignored the request.
    Unsupported,
}

/// Raw results of one probe.
#[derive(Clone, Debug, Default)]
pub(super) struct Observations {
    /// The probe socket's local address.
    pub local: Option<SocketAddr>,
    /// (server endpoint, mapped address) in the order the mappings were
    /// created; the first is the primary endpoint.
    pub mappings: Vec<(SocketAddr, SocketAddr)>,
    /// CHANGE-REQUEST change IP and port.
    pub change_both: Option<Change>,
    /// CHANGE-REQUEST change port.
    pub change_port: Option<Change>,
}

/// Classify a probe. Where the server cannot separate two classes, the less
/// permissive one is reported (e.g. APDM without a port test, ADF when
/// change-IP is unavailable); prediction treats both members alike.
pub(super) fn classify(observations: &Observations) -> NatInfo {
    let mapping = mapping(observations);
    NatInfo {
        mapping,
        filtering: filtering(observations),
        port_delta: if matches!(
            mapping,
            Behavior::AddressDependent | Behavior::AddressAndPortDependent
        ) {
            port_delta(&observations.mappings)
        } else {
            0
        },
    }
}

fn mapping(observations: &Observations) -> Behavior {
    let Some(&(primary, first)) = observations.mappings.first() else {
        return Behavior::Unknown;
    };
    if observations.local == Some(first) {
        // No NAT on the path.
        return Behavior::EndpointIndependent;
    }
    let mappings = &observations.mappings;
    if mappings.iter().all(|(_, mapped)| *mapped == first) {
        // Endpoint independence needs a second server IP; a second port
        // alone cannot separate EIM from ADM.
        return if mappings
            .iter()
            .any(|(server, _)| server.ip() != primary.ip())
        {
            Behavior::EndpointIndependent
        } else {
            Behavior::Unknown
        };
    }
    let mut port_tested = false;
    for (index, (a, mapped_a)) in mappings.iter().enumerate() {
        for (b, mapped_b) in &mappings[index + 1..] {
            if a.ip() == b.ip() && a.port() != b.port() {
                if mapped_a != mapped_b {
                    return Behavior::AddressAndPortDependent;
                }
                port_tested = true;
            }
        }
    }
    if port_tested {
        Behavior::AddressDependent
    } else {
        Behavior::AddressAndPortDependent
    }
}

fn filtering(observations: &Observations) -> Behavior {
    use Change::*;
    match (observations.change_both, observations.change_port) {
        (Some(Received), _) => Behavior::EndpointIndependent,
        (_, Some(Received)) => Behavior::AddressDependent,
        (_, Some(Silent)) | (Some(Silent), _) => Behavior::AddressAndPortDependent,
        _ => Behavior::Unknown,
    }
}

/// The step between consecutive new mappings, when at least two steps agree
/// and all mappings share one external IP.
fn port_delta(mappings: &[(SocketAddr, SocketAddr)]) -> i32 {
    let mut distinct: Vec<SocketAddr> = Vec::new();
    for (_, mapped) in mappings {
        if !distinct.contains(mapped) {
            distinct.push(*mapped);
        }
    }
    if distinct.len() < 3 || distinct.iter().any(|m| m.ip() != distinct[0].ip()) {
        return 0;
    }
    let steps: Vec<i32> = distinct
        .windows(2)
        .map(|pair| i32::from(pair[1].port()) - i32::from(pair[0].port()))
        .collect();
    let step = steps[0];
    if step != 0 && step.abs() <= MAX_PORT_DELTA && steps.iter().all(|s| *s == step) {
        step
    } else {
        0
    }
}

/// Mapping test destinations, RFC 5780 order: other IP with the primary
/// port, other IP and port, primary IP with another port; then any other
/// configured endpoint (more samples for the port step).
fn mapping_targets(
    primary: SocketAddr,
    other: Option<SocketAddr>,
    endpoints: &[SocketAddr],
) -> Vec<SocketAddr> {
    let mut candidates = Vec::new();
    if let Some(other) = other.filter(|o| o.ip() != primary.ip() && o.port() != primary.port()) {
        candidates.extend([
            SocketAddr::new(other.ip(), primary.port()),
            other,
            SocketAddr::new(primary.ip(), other.port()),
        ]);
    }
    let rank = |e: &SocketAddr| match (e.ip() == primary.ip(), e.port() == primary.port()) {
        (false, true) => 0,
        (false, false) => 1,
        (true, false) => 2,
        (true, true) => 3,
    };
    let mut configured: Vec<_> = endpoints
        .iter()
        .copied()
        .filter(|e| e.is_ipv4() == primary.is_ipv4())
        .collect();
    configured.sort_by_key(rank);
    candidates.extend(configured);
    let mut targets = Vec::new();
    for target in candidates {
        if target != primary && !targets.contains(&target) {
            targets.push(target);
        }
    }
    targets.truncate(MAX_MAPPING_TARGETS);
    targets
}

/// One transaction with retransmissions: the reply and its source.
async fn transact(
    socket: &UdpSocket,
    server: SocketAddr,
    change: u32,
) -> Result<Option<(Reply, SocketAddr)>> {
    let id: stun::TransactionId = rand::random();
    let request = stun::request(&id, change);
    let mut buffer = [0u8; 1500];
    for wait in RETRANSMITS {
        socket.send_to(&request, server).await?;
        let deadline = tokio::time::Instant::now() + wait;
        loop {
            match tokio::time::timeout_at(deadline, socket.recv_from(&mut buffer)).await {
                Err(_) => break,
                Ok(Err(error)) if error.kind() == std::io::ErrorKind::ConnectionReset => {
                    // ICMP unreachable from an earlier send (Windows).
                    continue;
                }
                Ok(Err(error)) => return Err(error.into()),
                Ok(Ok((length, from))) => {
                    if let Some(reply) = stun::parse(&buffer[..length], &id) {
                        return Ok(Some((reply, from)));
                    }
                }
            }
        }
    }
    Ok(None)
}

/// A filtering test: a success must come from the requested other address.
async fn change_test(socket: &UdpSocket, server: SocketAddr, change: u32) -> Result<Change> {
    Ok(match transact(socket, server, change).await? {
        None => Change::Silent,
        Some((Reply::Success { origin, .. }, from)) => {
            let source = origin.unwrap_or(from);
            let ip_changed = source.ip() != server.ip();
            let port_changed = source.port() != server.port();
            if (change & CHANGE_IP == 0 || ip_changed)
                && (change & CHANGE_PORT == 0 || port_changed)
            {
                Change::Received
            } else {
                Change::Unsupported
            }
        }
        Some((Reply::Error(_), _)) => Change::Unsupported,
    })
}

/// Run the tests from `local` against `endpoints` (IPv4).
async fn probe(local: Ipv4Addr, endpoints: &[SocketAddr]) -> Result<Observations> {
    let primary = *endpoints.first().context("no IPv4 discovery endpoint")?;
    let socket = UdpSocket::bind((local, 0))
        .await
        .context("bind probe socket")?;
    let mut observations = Observations {
        local: socket.local_addr().ok(),
        ..Default::default()
    };
    let Some((Reply::Success { mapped, other, .. }, _)) = transact(&socket, primary, 0).await?
    else {
        tracing::debug!(%primary, "NAT discovery: no binding response");
        return Ok(observations);
    };
    observations.mappings.push((primary, mapped));
    observations.change_both = Some(change_test(&socket, primary, CHANGE_IP | CHANGE_PORT).await?);
    observations.change_port = Some(change_test(&socket, primary, CHANGE_PORT).await?);
    for target in mapping_targets(primary, other, endpoints) {
        if let Some((Reply::Success { mapped, .. }, _)) = transact(&socket, target, 0).await? {
            observations.mappings.push((target, mapped));
        }
    }
    Ok(observations)
}

/// `stun:host:port`, `host:port` or `[v6]:port` entries, resolved; IPv4 only
/// (the NATs that matter for punching are IPv4).
async fn resolve(entries: &[String]) -> Vec<SocketAddr> {
    let mut endpoints = Vec::new();
    for entry in entries {
        let entry = entry.trim();
        let entry = entry
            .strip_prefix("stun:")
            .or_else(|| entry.strip_prefix("STUN:"))
            .unwrap_or(entry);
        let resolved: Vec<SocketAddr> = match entry.parse::<SocketAddr>() {
            Ok(address) => vec![address],
            Err(_) => match tokio::net::lookup_host(entry).await {
                Ok(addresses) => addresses.collect(),
                Err(error) => {
                    tracing::debug!(%error, entry, "NAT discovery endpoint not resolved");
                    continue;
                }
            },
        };
        for address in resolved.into_iter().filter(SocketAddr::is_ipv4) {
            if !endpoints.contains(&address) {
                endpoints.push(address);
            }
        }
    }
    endpoints
}

async fn discover(network: route::Network) -> Result<NatInfo> {
    let server = crate::account::server::ServerConfig::require()?;
    let info = crate::account::api::ServerApi::new(&server)?
        .server_info()
        .await
        .context("server information")?;
    let endpoints = resolve(&info.features.nat_discovery).await;
    anyhow::ensure!(
        !endpoints.is_empty(),
        "server offers no IPv4 NAT discovery endpoint"
    );
    let observations = probe(network.local, &endpoints).await?;
    let result = classify(&observations);
    tracing::info!(
        mapping = result.mapping.token(),
        filtering = result.filtering.token(),
        port_delta = result.port_delta,
        endpoints = endpoints.len(),
        mappings = observations.mappings.len(),
        "NAT behaviour discovered"
    );
    Ok(result)
}

#[derive(Default)]
struct State {
    entries: HashMap<route::Network, (NatInfo, Instant)>,
    probing: Option<route::Network>,
}

impl State {
    fn fresh(&self, network: &route::Network) -> Option<NatInfo> {
        self.entries
            .get(network)
            .filter(|(_, expires)| Instant::now() < *expires)
            .map(|(info, _)| *info)
    }
}

static STATE: LazyLock<Mutex<State>> = LazyLock::new(Mutex::default);
static STARTED: AtomicBool = AtomicBool::new(false);
/// A probe finished.
static DONE: Notify = Notify::const_new();
/// A session found no fresh result.
static KICK: Notify = Notify::const_new();

fn state() -> std::sync::MutexGuard<'static, State> {
    STATE.lock().unwrap_or_else(|e| e.into_inner())
}

/// Probe the current network unless it has a fresh result.
async fn refresh() {
    let Some(network) = route::current() else {
        return;
    };
    {
        let mut state = state();
        if state.fresh(&network).is_some() {
            return;
        }
        state.probing = Some(network);
    }
    let result = tokio::time::timeout(Duration::from_secs(15), discover(network))
        .await
        .unwrap_or_else(|_| Err(anyhow::anyhow!("NAT discovery timed out")));
    let entry = match result {
        Ok(info) => (info, Instant::now() + CACHE_TTL),
        Err(error) => {
            tracing::debug!(error = %format!("{error:#}"), "NAT discovery unavailable");
            (NatInfo::default(), Instant::now() + FAILURE_TTL)
        }
    };
    {
        let mut state = state();
        state
            .entries
            .retain(|_, (_, expires)| Instant::now() < *expires);
        state.entries.insert(network, entry);
        state.probing = None;
    }
    DONE.notify_waiters();
}

/// Lets [`start`] spawn again, and sessions stop waiting, if the runtime
/// drops the monitor (e.g. a short-lived command runtime).
struct Running;

impl Drop for Running {
    fn drop(&mut self) {
        state().probing = None;
        STARTED.store(false, Ordering::Release);
        DONE.notify_waiters();
    }
}

async fn monitor() {
    let _running = Running;
    #[cfg(windows)]
    let changes = Arc::new(crate::platform::windows::wol::Changes::default());
    #[cfg(windows)]
    let _watcher = crate::platform::windows::wol::Watcher::new(changes.clone())
        .map_err(|error| tracing::debug!(%error, "NAT discovery without network notifications"))
        .ok();
    #[cfg(windows)]
    let change = || changes.notify.notified();
    #[cfg(target_os = "linux")]
    let change = std::future::pending::<()>;
    loop {
        refresh().await;
        tokio::select! {
            _ = change() => {
                // Coalesce the burst of interface/address/route events.
                tokio::time::sleep(SETTLE).await;
            }
            _ = KICK.notified() => {}
            _ = tokio::time::sleep(CACHE_TTL) => {}
        }
    }
}

/// Start the background monitor on the current Tokio runtime (idempotent).
pub(crate) fn start() {
    let Ok(runtime) = tokio::runtime::Handle::try_current() else {
        return;
    };
    if !STARTED.swap(true, Ordering::AcqRel) {
        runtime.spawn(monitor());
    }
}

/// A fresh result for the current network, without waiting.
pub(crate) fn cached() -> Option<NatInfo> {
    let network = route::current()?;
    state().fresh(&network)
}

/// A fresh result for the current network. Waits at most `budget` for a
/// probe already in progress; otherwise requests one and returns `None`.
pub(crate) async fn current(budget: Duration) -> Option<NatInfo> {
    start();
    let network = route::current()?;
    let deadline = tokio::time::Instant::now() + budget;
    loop {
        let done = DONE.notified();
        tokio::pin!(done);
        done.as_mut().enable();
        {
            let state = state();
            if let Some(info) = state.fresh(&network) {
                return Some(info);
            }
            if state.probing.is_none() {
                // A new probe takes seconds; this session goes without.
                KICK.notify_one();
                return None;
            }
        }
        tokio::time::timeout_at(deadline, done).await.ok()?;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use Behavior::*;

    fn sa(value: &str) -> SocketAddr {
        value.parse().unwrap()
    }

    /// A NAT simulator answering the RFC 5780 tests for a full server set
    /// A:3478, A:3479, B:3478, B:3479.
    struct Nat {
        mapping: Behavior,
        filtering: Behavior,
        /// Port of the first mapping and step between new mappings.
        base: u16,
        step: u16,
    }

    impl Nat {
        fn run(&self, endpoints: &[SocketAddr]) -> Observations {
            let local = sa("192.168.1.5:50000");
            let primary = endpoints[0];
            let mut created: Vec<(SocketAddr, SocketAddr)> = Vec::new();
            let mut map = |server: SocketAddr| {
                let key = |s: SocketAddr| match self.mapping {
                    EndpointIndependent => sa("0.0.0.0:0"),
                    AddressDependent => SocketAddr::new(s.ip(), 0),
                    _ => s,
                };
                if let Some((_, mapped)) = created.iter().find(|(s, _)| key(*s) == key(server)) {
                    return *mapped;
                }
                let port = self.base + self.step * created.len() as u16;
                let mapped = SocketAddr::new("203.0.113.7".parse().unwrap(), port);
                created.push((server, mapped));
                mapped
            };
            let mut observations = Observations {
                local: Some(local),
                ..Default::default()
            };
            observations.mappings.push((primary, map(primary)));
            let has = |ip_changed: bool, port_changed: bool| {
                endpoints.iter().any(|e| {
                    (e.ip() != primary.ip()) == ip_changed
                        && (e.port() != primary.port()) == port_changed
                })
            };
            // Only the primary server address has been contacted so far.
            let admitted = |ip_changed: bool, port_changed: bool| match self.filtering {
                EndpointIndependent => true,
                AddressDependent => !ip_changed,
                _ => !ip_changed && !port_changed,
            };
            let test = |ip: bool, port: bool| {
                if !has(ip, port) {
                    Change::Unsupported
                } else if admitted(ip, port) {
                    Change::Received
                } else {
                    Change::Silent
                }
            };
            observations.change_both = Some(test(true, true));
            observations.change_port = Some(test(false, true));
            let other = endpoints
                .iter()
                .copied()
                .find(|e| e.ip() != primary.ip() && e.port() != primary.port());
            for target in mapping_targets(primary, other, endpoints) {
                observations.mappings.push((target, map(target)));
            }
            observations
        }
    }

    fn full() -> Vec<SocketAddr> {
        [
            "198.51.100.1:3478",
            "198.51.100.1:3479",
            "198.51.100.2:3478",
            "198.51.100.2:3479",
        ]
        .into_iter()
        .map(sa)
        .collect()
    }

    fn nat(mapping: Behavior, filtering: Behavior, step: u16) -> Nat {
        Nat {
            mapping,
            filtering,
            base: 40000,
            step,
        }
    }

    #[test]
    fn classifies_full_server_set() {
        for (mapping, filtering, step, expected_delta) in [
            (EndpointIndependent, EndpointIndependent, 1, 0),
            (EndpointIndependent, AddressDependent, 1, 0),
            (EndpointIndependent, AddressAndPortDependent, 1, 0),
            // Two server IPs give an ADM NAT only two mappings: no
            // consistent step can be shown.
            (AddressDependent, AddressAndPortDependent, 1, 0),
            (AddressAndPortDependent, AddressAndPortDependent, 1, 1),
            (AddressAndPortDependent, AddressAndPortDependent, 4, 4),
        ] {
            let result = classify(&nat(mapping, filtering, step).run(&full()));
            assert_eq!(
                result,
                NatInfo {
                    mapping,
                    filtering,
                    port_delta: expected_delta
                },
                "{mapping:?}/{filtering:?}/{step}"
            );
        }
    }

    #[test]
    fn single_ip_two_ports() {
        let endpoints = vec![sa("198.51.100.1:3478"), sa("198.51.100.1:3479")];
        // EIM and ADM look alike without a second IP.
        let result = classify(&nat(EndpointIndependent, EndpointIndependent, 1).run(&endpoints));
        assert_eq!(result.mapping, Unknown);
        // Change-IP is unavailable (420): a passing port change reports ADF.
        assert_eq!(result.filtering, AddressDependent);
        let result = classify(&nat(AddressDependent, AddressDependent, 1).run(&endpoints));
        assert_eq!(result.mapping, Unknown);
        let result =
            classify(&nat(AddressAndPortDependent, AddressAndPortDependent, 1).run(&endpoints));
        assert_eq!(result.mapping, AddressAndPortDependent);
        assert_eq!(result.filtering, AddressAndPortDependent);
        // Two mappings: one step is not a consistent delta.
        assert_eq!(result.port_delta, 0);
    }

    #[test]
    fn two_ips_one_port() {
        let endpoints = vec![sa("198.51.100.1:3478"), sa("198.51.100.2:3478")];
        let result = classify(&nat(EndpointIndependent, AddressDependent, 1).run(&endpoints));
        assert_eq!(result.mapping, EndpointIndependent);
        // Neither alternate port exists: filtering cannot be tested.
        assert_eq!(result.filtering, Unknown);
        // Dependent on the IP, port dependence untested: the stricter class.
        let result = classify(&nat(AddressDependent, AddressAndPortDependent, 1).run(&endpoints));
        assert_eq!(result.mapping, AddressAndPortDependent);
    }

    #[test]
    fn single_endpoint_and_no_response() {
        let one = vec![sa("198.51.100.1:3478")];
        let result = classify(&nat(AddressAndPortDependent, AddressAndPortDependent, 1).run(&one));
        assert_eq!(result.mapping, Unknown);
        assert_eq!(result.filtering, Unknown);
        assert_eq!(classify(&Observations::default()), NatInfo::default());
    }

    #[test]
    fn no_nat_is_endpoint_independent() {
        let local = sa("203.0.113.7:50000");
        let observations = Observations {
            local: Some(local),
            mappings: vec![(sa("198.51.100.1:3478"), local)],
            change_both: Some(Change::Received),
            change_port: Some(Change::Received),
        };
        let result = classify(&observations);
        assert_eq!(result.mapping, EndpointIndependent);
        assert_eq!(result.filtering, EndpointIndependent);
    }

    #[test]
    fn port_delta_needs_agreeing_steps() {
        let server = sa("198.51.100.1:3478");
        let at = |port: u16| {
            (
                server,
                SocketAddr::new("203.0.113.7".parse().unwrap(), port),
            )
        };
        assert_eq!(port_delta(&[at(100), at(102), at(104), at(106)]), 2);
        assert_eq!(port_delta(&[at(106), at(104), at(102)]), -2);
        assert_eq!(port_delta(&[at(100), at(102), at(105)]), 0);
        assert_eq!(port_delta(&[at(100), at(102)]), 0);
        assert_eq!(port_delta(&[at(100), at(300), at(500)]), 0);
        // Mappings on different external IPs (paired pooling off).
        let other = (server, sa("203.0.113.8:102"));
        assert_eq!(port_delta(&[at(100), other, at(104)]), 0);
    }

    #[test]
    fn mapping_targets_follow_rfc5780_order() {
        let primary = sa("198.51.100.1:3478");
        let other = sa("198.51.100.2:3479");
        assert_eq!(
            mapping_targets(primary, Some(other), &full()),
            vec![sa("198.51.100.2:3478"), other, sa("198.51.100.1:3479")]
        );
        // Without OTHER-ADDRESS: configured endpoints by the same ranking.
        let endpoints = vec![primary, sa("198.51.100.1:3479"), sa("198.51.100.9:3478")];
        assert_eq!(
            mapping_targets(primary, None, &endpoints),
            vec![sa("198.51.100.9:3478"), sa("198.51.100.1:3479")]
        );
    }

    #[tokio::test]
    async fn resolves_configured_entries() {
        let entries = vec![
            "stun:198.51.100.1:3478".to_owned(),
            "198.51.100.2:3479".to_owned(),
            "stun:[2001:db8::1]:3478".to_owned(),
            "stun:198.51.100.1:3478".to_owned(),
        ];
        assert_eq!(
            resolve(&entries).await,
            vec![sa("198.51.100.1:3478"), sa("198.51.100.2:3479")]
        );
    }

    #[tokio::test]
    async fn probe_against_local_responder() {
        // A loopback server answering Binding requests like the Farwing STUN
        // endpoints without an alternate address (420 for CHANGE-REQUEST).
        let server = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let address = server.local_addr().unwrap();
        tokio::spawn(async move {
            let mut buffer = [0u8; 1500];
            while let Ok((length, from)) = server.recv_from(&mut buffer).await {
                let id: stun::TransactionId = buffer[8..20].try_into().unwrap();
                let reply = if length > 20 {
                    let mut error = vec![0x01, 0x11, 0, 8, 0x21, 0x12, 0xa4, 0x42];
                    error.extend_from_slice(&id);
                    error.extend_from_slice(&[0, 9, 0, 4, 0, 0, 4, 20]);
                    error
                } else {
                    stun::tests::success(&id, from, address, None)
                };
                let _ = server.send_to(&reply, from).await;
            }
        });
        let observations = probe(Ipv4Addr::LOCALHOST, &[address]).await.unwrap();
        assert_eq!(observations.mappings.len(), 1);
        assert_eq!(observations.change_both, Some(Change::Unsupported));
        assert_eq!(observations.change_port, Some(Change::Unsupported));
        let result = classify(&observations);
        // The mapped address equals the local one: no NAT.
        assert_eq!(result.mapping, EndpointIndependent);
        assert_eq!(result.filtering, Unknown);
    }
}
