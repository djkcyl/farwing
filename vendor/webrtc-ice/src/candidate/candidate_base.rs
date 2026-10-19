use std::fmt;
use std::ops::Add;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use crc::{Crc, CRC_32_ISCSI};
use portable_atomic::{AtomicBool, AtomicU16, AtomicU32, AtomicU64, AtomicU8};
use tokio::sync::{broadcast, Mutex};
use util::sync::Mutex as SyncMutex;

use super::*;
use crate::candidate::candidate_host::CandidateHostConfig;
use crate::candidate::candidate_peer_reflexive::CandidatePeerReflexiveConfig;
use crate::candidate::candidate_relay::CandidateRelayConfig;
use crate::candidate::candidate_server_reflexive::CandidateServerReflexiveConfig;
use crate::error::*;
use crate::util::*;

#[derive(Default)]
pub struct CandidateBaseConfig {
    pub candidate_id: String,
    pub network: String,
    pub address: String,
    pub port: u16,
    pub component: u16,
    pub priority: u32,
    pub foundation: String,
    pub conn: Option<Arc<dyn util::Conn + Send + Sync>>,
    pub initialized_ch: Option<broadcast::Receiver<()>>,
    pub relay_protocol: String,
    pub url: String,
    pub network_id: u16,
    pub network_cost: Option<u16>,
    /// Adapter classification for local candidates. Remote candidates do
    /// not carry this as a separate SDP token and derive it from network-cost.
    pub adapter_type: u32,
    pub network_key: String,
    pub generation: u32,
    pub credentials: IceCredentials,
}

#[derive(Clone)]
pub(crate) struct SignaledCandidateDescription {
    foundation: String,
    priority: u32,
    candidate_type: CandidateType,
    related_address: Option<CandidateRelatedAddress>,
    tcp_type: TcpType,
    relay_protocol: String,
    url: String,
}

pub struct CandidateBase {
    pub(crate) local_origin: AtomicBool,
    pub(crate) id: String,
    pub(crate) network_type: AtomicU8,
    pub(crate) candidate_type: CandidateType,
    pub(crate) signaled: std::sync::RwLock<Option<SignaledCandidateDescription>>,

    pub(crate) component: AtomicU16,
    pub(crate) address: String,
    pub(crate) port: u16,
    pub(crate) related_address: Option<CandidateRelatedAddress>,
    pub(crate) tcp_type: TcpType,

    pub(crate) resolved_addr: SyncMutex<SocketAddr>,

    pub(crate) last_sent: AtomicU64,
    pub(crate) last_received: AtomicU64,

    pub(crate) conn: Option<Arc<dyn util::Conn + Send + Sync>>,
    pub(crate) closed_ch: Arc<Mutex<Option<broadcast::Sender<()>>>>,

    pub(crate) foundation_override: String,
    pub(crate) priority_override: u32,

    //CandidateHost
    pub(crate) network: String,
    //CandidateRelay
    pub(crate) relay_client: Option<Arc<turn::client::Client>>,
    pub(crate) relay_protocol: String,
    pub(crate) url: String,
    pub(crate) network_id: AtomicU16,
    pub(crate) network_cost: AtomicU16,
    pub(crate) adapter_type: u32,
    pub(crate) network_key: String,
    pub(crate) generation: AtomicU32,
    pub(crate) credentials: std::sync::RwLock<IceCredentials>,
}

/// Recover the adapter type encoded by the network-cost table for a
/// remote candidate. Local candidates carry the explicit platform adapter
/// type; SDP only transports network-cost, so this mapping is the only
/// evidence-backed reconstruction available on the receiving side.
pub const fn adapter_type_from_network_cost(cost: u16) -> u32 {
    match cost {
        0 => 1,
        10 => 2,
        250 | 2250 => 512,
        500 | 2500 => 256,
        900 | 2900 => 4,
        910 | 2910 => 128,
        980 | 2980 => 64,
        999 | 2999 => 32,
        2000 | 2050 => 8,
        2010 => 2,
        50 => 0,
        _ => 0,
    }
}

impl Default for CandidateBase {
    fn default() -> Self {
        Self {
            local_origin: AtomicBool::new(false),
            id: String::new(),
            network_type: AtomicU8::new(0),
            candidate_type: CandidateType::default(),
            signaled: std::sync::RwLock::new(None),

            component: AtomicU16::new(0),
            address: String::new(),
            port: 0,
            related_address: None,
            tcp_type: TcpType::default(),

            resolved_addr: SyncMutex::new(SocketAddr::new(IpAddr::from([0, 0, 0, 0]), 0)),

            last_sent: AtomicU64::new(0),
            last_received: AtomicU64::new(0),

            conn: None,
            closed_ch: Arc::new(Mutex::new(None)),

            foundation_override: String::new(),
            priority_override: 0,
            network: String::new(),
            relay_client: None,
            relay_protocol: String::new(),
            url: String::new(),
            network_id: AtomicU16::new(0),
            // libwebrtc's Candidate parser initializes an omitted
            // `network-cost` extension to zero.  A non-zero fallback here is
            // observable by BasicIceController before the first inbound STUN
            // request and can incorrectly demote a directly reachable host
            // candidate.
            network_cost: AtomicU16::new(0),
            adapter_type: 0,
            network_key: String::new(),
            generation: AtomicU32::new(0),
            credentials: std::sync::RwLock::new(IceCredentials::default()),
        }
    }
}

/// Candidate inputs are descriptions, not ownership transfers of another
/// Agent's socket. The Candidate value is copied before being enriched.
pub(crate) fn remote_description_copy(candidate: &dyn Candidate) -> CandidateBase {
    CandidateBase {
        id: candidate.id(),
        network_type: AtomicU8::new(candidate.network_type() as u8),
        candidate_type: candidate.candidate_type(),
        component: AtomicU16::new(candidate.component()),
        address: candidate.address(),
        port: candidate.port(),
        related_address: candidate.related_address(),
        tcp_type: candidate.tcp_type(),
        resolved_addr: SyncMutex::new(candidate.addr()),
        foundation_override: candidate.foundation(),
        priority_override: candidate.priority(),
        network: candidate.network_type().network_short(),
        relay_protocol: candidate.relay_protocol(),
        url: candidate.url(),
        network_id: AtomicU16::new(candidate.network_id()),
        network_cost: AtomicU16::new(candidate.network_cost()),
        adapter_type: candidate.adapter_type(),
        network_key: candidate.network_key(),
        generation: AtomicU32::new(candidate.generation()),
        credentials: std::sync::RwLock::new(candidate.credentials()),
        ..Default::default()
    }
}

pub(crate) fn local_description_copy(candidate: &dyn Candidate) -> CandidateBase {
    let copy = remote_description_copy(candidate);
    copy.mark_local();
    copy
}

// String makes the candidateBase printable
impl fmt::Display for CandidateBase {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if let Some(related_address) = self.related_address() {
            write!(
                f,
                "{} {} {}:{}{}",
                self.network_type(),
                self.candidate_type(),
                self.address(),
                self.port(),
                related_address,
            )
        } else {
            write!(
                f,
                "{} {} {}:{}",
                self.network_type(),
                self.candidate_type(),
                self.address(),
                self.port(),
            )
        }
    }
}

#[async_trait]
impl Candidate for CandidateBase {
    fn update_peer_reflexive(&self, candidate: &dyn Candidate) -> bool {
        if self.candidate_type() != CandidateType::PeerReflexive
            || candidate.candidate_type() == CandidateType::PeerReflexive
            || self.network_type() != candidate.network_type()
            || self.addr() != candidate.addr()
            || self.generation() != candidate.generation()
            || self.credentials() != candidate.credentials()
        {
            return false;
        }
        let description = SignaledCandidateDescription {
            foundation: candidate.foundation(),
            priority: candidate.priority(),
            candidate_type: candidate.candidate_type(),
            related_address: candidate.related_address(),
            tcp_type: candidate.tcp_type(),
            relay_protocol: candidate.relay_protocol(),
            url: candidate.url(),
        };
        *self.signaled.write().unwrap_or_else(|p| p.into_inner()) = Some(description);
        self.set_network_id(candidate.network_id());
        self.set_network_cost(candidate.network_cost());
        true
    }

    fn foundation(&self) -> String {
        if let Some(description) = self
            .signaled
            .read()
            .unwrap_or_else(|p| p.into_inner())
            .as_ref()
        {
            return description.foundation.clone();
        }
        if !self.foundation_override.is_empty() {
            return self.foundation_override.clone();
        }

        let mut buf = vec![];
        buf.extend_from_slice(self.candidate_type().to_string().as_bytes());
        buf.extend_from_slice(self.address.as_bytes());
        buf.extend_from_slice(self.network_type().to_string().as_bytes());

        let checksum = Crc::<u32>::new(&CRC_32_ISCSI).checksum(&buf);

        format!("{checksum}")
    }

    /// Returns Candidate ID.
    fn id(&self) -> String {
        self.id.clone()
    }

    /// Returns candidate component.
    fn component(&self) -> u16 {
        self.component.load(Ordering::SeqCst)
    }

    fn set_component(&self, component: u16) {
        self.component.store(component, Ordering::SeqCst);
    }

    /// Returns a time indicating the last time this candidate was received.
    fn last_received(&self) -> SystemTime {
        UNIX_EPOCH.add(Duration::from_nanos(
            self.last_received.load(Ordering::SeqCst),
        ))
    }

    /// Returns a time indicating the last time this candidate was sent.
    fn last_sent(&self) -> SystemTime {
        UNIX_EPOCH.add(Duration::from_nanos(self.last_sent.load(Ordering::SeqCst)))
    }

    /// Returns candidate NetworkType.
    fn network_type(&self) -> NetworkType {
        NetworkType::from(self.network_type.load(Ordering::SeqCst))
    }

    /// Returns Candidate Address.
    fn address(&self) -> String {
        self.address.clone()
    }

    /// Returns Candidate Port.
    fn port(&self) -> u16 {
        self.port
    }

    /// Computes the priority for this ICE Candidate.
    fn priority(&self) -> u32 {
        if let Some(description) = self
            .signaled
            .read()
            .unwrap_or_else(|p| p.into_inner())
            .as_ref()
        {
            return description.priority.clone();
        }
        if self.priority_override != 0 {
            return self.priority_override;
        }

        // The local preference MUST be an integer from 0 (lowest preference) to
        // 65535 (highest preference) inclusive.  When there is only a single IP
        // address, this value SHOULD be set to 65535.  If there are multiple
        // candidates for a particular component for a particular data stream
        // that have the same type, the local preference MUST be unique for each
        // one.
        (1 << 24) * u32::from(self.candidate_type().preference())
            + (1 << 8) * u32::from(self.local_preference())
            + (256 - u32::from(self.component()))
    }

    /// Returns `Option<CandidateRelatedAddress>`.
    fn related_address(&self) -> Option<CandidateRelatedAddress> {
        if let Some(description) = self
            .signaled
            .read()
            .unwrap_or_else(|p| p.into_inner())
            .as_ref()
        {
            return description.related_address.clone();
        }
        self.related_address.as_ref().cloned()
    }

    /// Returns candidate type.
    fn candidate_type(&self) -> CandidateType {
        if let Some(description) = self
            .signaled
            .read()
            .unwrap_or_else(|p| p.into_inner())
            .as_ref()
        {
            return description.candidate_type.clone();
        }
        self.candidate_type
    }

    fn tcp_type(&self) -> TcpType {
        if let Some(description) = self
            .signaled
            .read()
            .unwrap_or_else(|p| p.into_inner())
            .as_ref()
        {
            return description.tcp_type.clone();
        }
        self.tcp_type
    }

    fn relay_protocol(&self) -> String {
        if let Some(description) = self
            .signaled
            .read()
            .unwrap_or_else(|p| p.into_inner())
            .as_ref()
        {
            return description.relay_protocol.clone();
        }
        self.relay_protocol.clone()
    }

    fn url(&self) -> String {
        if let Some(description) = self
            .signaled
            .read()
            .unwrap_or_else(|p| p.into_inner())
            .as_ref()
        {
            return description.url.clone();
        }
        self.url.clone()
    }

    fn network_id(&self) -> u16 {
        self.network_id.load(Ordering::Relaxed)
    }

    fn set_network_id(&self, id: u16) {
        self.network_id.store(id, Ordering::Relaxed);
    }

    fn network_cost(&self) -> u16 {
        self.network_cost.load(Ordering::Relaxed)
    }

    fn set_network_cost(&self, cost: u16) {
        self.network_cost.store(cost, Ordering::Relaxed);
    }

    fn adapter_type(&self) -> u32 {
        if self.local_origin.load(Ordering::Acquire) || self.adapter_type != 0 {
            self.adapter_type
        } else {
            adapter_type_from_network_cost(self.network_cost())
        }
    }

    fn mark_local(&self) {
        self.local_origin.store(true, Ordering::Release);
    }

    fn network_key(&self) -> String {
        self.network_key.clone()
    }

    fn is_any_address_network(&self) -> bool {
        self.adapter_type == 32
            || self.network_key.is_empty()
            || matches!(self.network_key.as_str(), "0.0.0.0" | "::")
    }

    fn generation(&self) -> u32 {
        self.generation.load(Ordering::Relaxed)
    }

    fn set_generation(&self, generation: u32) {
        self.generation.store(generation, Ordering::Relaxed);
    }

    fn credentials(&self) -> IceCredentials {
        self.credentials
            .read()
            .expect("candidate credentials poisoned")
            .clone()
    }

    fn has_credentials(&self) -> bool {
        let credentials = self
            .credentials
            .read()
            .expect("candidate credentials poisoned");
        !credentials.username.is_empty() && !credentials.password.is_empty()
    }

    fn set_credentials(&self, credentials: IceCredentials) {
        *self
            .credentials
            .write()
            .expect("candidate credentials poisoned") = credentials;
    }

    fn fill_remote_credentials(&self, parameters: &IceCredentials, generation: Option<u32>) {
        let mut credentials = self
            .credentials
            .write()
            .expect("candidate credentials poisoned");
        if credentials.username != parameters.username {
            return;
        }
        if credentials.password.is_empty() {
            credentials.password.clone_from(&parameters.password);
        }
        // Only promotes generation zero when both credentials match.
        if *credentials == *parameters && self.generation.load(Ordering::Relaxed) == 0 {
            if let Some(generation) = generation {
                self.generation.store(generation, Ordering::Relaxed);
            }
        }
    }

    /// Returns the string representation of the ICECandidate.
    fn marshal(&self) -> String {
        let mut val = format!(
            "{} {} {} {} {} {} typ {}",
            self.foundation(),
            self.component(),
            self.network_type().network_short(),
            self.priority(),
            self.address(),
            self.port(),
            self.candidate_type()
        );

        if self.tcp_type != TcpType::Unspecified {
            val += format!(" tcptype {}", self.tcp_type()).as_str();
        }

        if let Some(related_address) = self.related_address() {
            val += format!(
                " raddr {} rport {}",
                related_address.address, related_address.port,
            )
            .as_str();
        }

        val += format!(
            " generation {} network-id {} network-cost {}",
            self.generation(),
            self.network_id(),
            self.network_cost()
        )
        .as_str();

        let username = self.credentials().username;
        if !username.is_empty() {
            val += format!(" ufrag {username}").as_str();
        }
        let relay_protocol = self.relay_protocol();
        if self.candidate_type() == CandidateType::Relay && !relay_protocol.is_empty() {
            val += format!(" relay-protocol {relay_protocol}").as_str();
        }

        val
    }

    fn addr(&self) -> SocketAddr {
        *self.resolved_addr.lock()
    }

    /// Stops the recvLoop.
    async fn close(&self) -> Result<()> {
        let already_closed = self.closed_ch.lock().await.take().is_none();

        if let Some(conn) = &self.conn {
            let _ = conn.close().await;
        }

        if let Some(relay_client) = &self.relay_client {
            let _ = relay_client.close().await;
        }

        if already_closed {
            Err(Error::ErrClosed)
        } else {
            Ok(())
        }
    }

    fn seen(&self, outbound: bool) {
        let d = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_else(|_| Duration::from_secs(0));

        if outbound {
            self.set_last_sent(d);
        } else {
            self.set_last_received(d);
        }
    }

    async fn write_to(
        &self,
        raw: &[u8],
        dst: &(dyn Candidate + Send + Sync),
        payload: bool,
    ) -> Result<usize> {
        let n = if let Some(conn) = &self.conn {
            let addr = dst.addr();
            if payload {
                conn.send_to(raw, addr).await?
            } else {
                conn.send_control_to(raw, addr).await?
            }
        } else {
            0
        };
        self.seen(true);
        Ok(n)
    }

    /// Used to compare two candidateBases.
    fn equal(&self, other: &dyn Candidate) -> bool {
        self.component() == other.component()
            && self.network_type() == other.network_type()
            && self.candidate_type() == other.candidate_type()
            && self.addr() == other.addr()
            && self.credentials() == other.credentials()
            && self.foundation() == other.foundation()
            && self.related_address() == other.related_address()
            && self.generation() == other.generation()
            && self.network_id() == other.network_id()
    }

    fn set_ip(&self, ip: &IpAddr) -> Result<()> {
        let network_type = determine_network_type(&self.network, ip)?;

        self.network_type
            .store(network_type as u8, Ordering::SeqCst);

        let addr = create_addr(network_type, *ip, self.port);
        *self.resolved_addr.lock() = addr;

        Ok(())
    }

    fn get_conn(&self) -> Option<&Arc<dyn util::Conn + Send + Sync>> {
        self.conn.as_ref()
    }

    fn get_closed_ch(&self) -> Arc<Mutex<Option<broadcast::Sender<()>>>> {
        self.closed_ch.clone()
    }
}

impl CandidateBase {
    pub fn set_last_received(&self, d: Duration) {
        #[allow(clippy::cast_possible_truncation)]
        self.last_received
            .store(d.as_nanos() as u64, Ordering::SeqCst);
    }

    pub fn set_last_sent(&self, d: Duration) {
        #[allow(clippy::cast_possible_truncation)]
        self.last_sent.store(d.as_nanos() as u64, Ordering::SeqCst);
    }

    /// Returns the local preference for this candidate.
    pub fn local_preference(&self) -> u16 {
        if self.network_type().is_tcp() {
            // RFC 6544, section 4.2
            //
            // In Section 4.1.2.1 of [RFC5245], a recommended formula for UDP ICE
            // candidate prioritization is defined.  For TCP candidates, the same
            // formula and candidate type preferences SHOULD be used, and the
            // RECOMMENDED type preferences for the new candidate types defined in
            // this document (see Section 5) are 105 for NAT-assisted candidates and
            // 75 for UDP-tunneled candidates.
            //
            // (...)
            //
            // With TCP candidates, the local preference part of the recommended
            // priority formula is updated to also include the directionality
            // (active, passive, or simultaneous-open) of the TCP connection.  The
            // RECOMMENDED local preference is then defined as:
            //
            //     local preference = (2^13) * direction-pref + other-pref
            //
            // The direction-pref MUST be between 0 and 7 (both inclusive), with 7
            // being the most preferred.  The other-pref MUST be between 0 and 8191
            // (both inclusive), with 8191 being the most preferred.  It is
            // RECOMMENDED that the host, UDP-tunneled, and relayed TCP candidates
            // have the direction-pref assigned as follows: 6 for active, 4 for
            // passive, and 2 for S-O.  For the NAT-assisted and server reflexive
            // candidates, the RECOMMENDED values are: 6 for S-O, 4 for active, and
            // 2 for passive.
            //
            // (...)
            //
            // If any two candidates have the same type-preference and direction-
            // pref, they MUST have a unique other-pref.  With this specification,
            // this usually only happens with multi-homed hosts, in which case
            // other-pref is the preference for the particular IP address from which
            // the candidate was obtained.  When there is only a single IP address,
            // this value SHOULD be set to the maximum allowed value (8191).
            let other_pref: u16 = 8191;

            let direction_pref: u16 = match self.candidate_type() {
                CandidateType::Host | CandidateType::Relay => match self.tcp_type() {
                    TcpType::Active => 6,
                    TcpType::Passive => 4,
                    TcpType::SimultaneousOpen => 2,
                    TcpType::Unspecified => 0,
                },
                CandidateType::PeerReflexive | CandidateType::ServerReflexive => {
                    match self.tcp_type() {
                        TcpType::SimultaneousOpen => 6,
                        TcpType::Active => 4,
                        TcpType::Passive => 2,
                        TcpType::Unspecified => 0,
                    }
                }
                CandidateType::Unspecified => 0,
            };

            (1 << 13) * direction_pref + other_pref
        } else {
            DEFAULT_LOCAL_PREFERENCE
        }
    }
}

/// Creates a Candidate from its string representation.
pub fn unmarshal_candidate(raw: &str) -> Result<impl Candidate> {
    let split: Vec<&str> = raw.split_whitespace().collect();
    if split.len() < 8 {
        return Err(Error::Other(format!(
            "{:?} ({})",
            Error::ErrAttributeTooShortIceCandidate,
            split.len()
        )));
    }

    // Foundation
    let foundation = split[0].to_owned();

    // Component
    let component: u16 = split[1].parse()?;

    // Network
    let network = split[2].to_owned();

    // Priority
    let priority: u32 = split[3].parse()?;

    // Address
    let address = split[4].to_owned();

    // Port
    let port: u16 = split[5].parse()?;

    let typ = split[7];

    let mut rel_addr = String::new();
    let mut rel_port = 0;
    let mut tcp_type = TcpType::Unspecified;
    let mut generation = 0;
    let mut network_id = 0;
    let mut network_cost = None;
    let mut relay_protocol = String::new();
    let mut username_fragment = String::new();

    if split.len() > 8 {
        if (split.len() - 8) % 2 != 0 {
            return Err(Error::Other(format!(
                "{:?}: incomplete candidate extension",
                Error::ErrParseRelatedAddr
            )));
        }
        let mut index = 8;
        while index + 1 < split.len() {
            match split[index] {
                "raddr" => split[index + 1].clone_into(&mut rel_addr),
                "rport" => rel_port = split[index + 1].parse()?,
                "tcptype" => tcp_type = TcpType::from(split[index + 1]),
                "generation" => generation = split[index + 1].parse()?,
                "network-id" => network_id = split[index + 1].parse()?,
                "network-cost" => network_cost = Some(split[index + 1].parse()?),
                "relay-protocol" => split[index + 1].clone_into(&mut relay_protocol),
                "ufrag" => split[index + 1].clone_into(&mut username_fragment),
                _ => {}
            }
            index += 2;
        }
    }

    match typ {
        "host" => {
            let config = CandidateHostConfig {
                base_config: CandidateBaseConfig {
                    network,
                    address,
                    port,
                    component,
                    priority,
                    foundation,
                    generation,
                    network_id,
                    network_cost,
                    relay_protocol,
                    credentials: IceCredentials {
                        username: username_fragment,
                        password: String::new(),
                    },
                    ..CandidateBaseConfig::default()
                },
                tcp_type,
            };
            config.new_candidate_host()
        }
        "srflx" => {
            let config = CandidateServerReflexiveConfig {
                base_config: CandidateBaseConfig {
                    network,
                    address,
                    port,
                    component,
                    priority,
                    foundation,
                    generation,
                    network_id,
                    network_cost,
                    relay_protocol,
                    credentials: IceCredentials {
                        username: username_fragment,
                        password: String::new(),
                    },
                    ..CandidateBaseConfig::default()
                },
                rel_addr,
                rel_port,
            };
            config.new_candidate_server_reflexive()
        }
        "prflx" => {
            let config = CandidatePeerReflexiveConfig {
                base_config: CandidateBaseConfig {
                    network,
                    address,
                    port,
                    component,
                    priority,
                    foundation,
                    generation,
                    network_id,
                    network_cost,
                    relay_protocol,
                    credentials: IceCredentials {
                        username: username_fragment,
                        password: String::new(),
                    },
                    ..CandidateBaseConfig::default()
                },
                rel_addr,
                rel_port,
            };

            config.new_candidate_peer_reflexive()
        }
        "relay" => {
            let config = CandidateRelayConfig {
                base_config: CandidateBaseConfig {
                    network,
                    address,
                    port,
                    component,
                    priority,
                    foundation,
                    generation,
                    network_id,
                    network_cost,
                    relay_protocol,
                    credentials: IceCredentials {
                        username: username_fragment,
                        password: String::new(),
                    },
                    ..CandidateBaseConfig::default()
                },
                rel_addr,
                rel_port,
                ..CandidateRelayConfig::default()
            };
            config.new_candidate_relay()
        }
        _ => Err(Error::Other(format!(
            "{:?} ({})",
            Error::ErrUnknownCandidateType,
            typ
        ))),
    }
}
