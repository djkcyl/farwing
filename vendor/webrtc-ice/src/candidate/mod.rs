#[cfg(test)]
mod candidate_relay_test;
#[cfg(test)]
mod candidate_server_reflexive_test;
#[cfg(test)]
mod candidate_test;

pub mod candidate_base;
pub mod candidate_host;
pub mod candidate_peer_reflexive;
pub mod candidate_relay;
pub mod candidate_server_reflexive;
pub(crate) mod path_quality;

use std::collections::VecDeque;
use std::fmt;
use std::net::{IpAddr, SocketAddr};
use std::sync::atomic::Ordering;
use std::sync::{Arc, LazyLock, Mutex as StdMutex};
use std::time::{Duration, Instant, SystemTime};

use async_trait::async_trait;
use candidate_base::*;
use portable_atomic::{AtomicBool, AtomicU16, AtomicU32, AtomicU64, AtomicU8};
use serde::{Deserialize, Serialize};
use tokio::sync::{broadcast, Mutex};

use crate::error::Result;
use crate::network_type::*;
use crate::tcp_type::*;

static ICE_MONOTONIC_EPOCH: LazyLock<Instant> = LazyLock::new(Instant::now);

#[derive(Clone, Default, PartialEq, Eq)]
pub struct IceCredentials {
    pub username: String,
    pub password: String,
}

pub(crate) const RECEIVE_MTU: usize = 8192;
pub(crate) const DEFAULT_LOCAL_PREFERENCE: u16 = 65535;

/// Indicates that the candidate is used for RTP.
pub(crate) const COMPONENT_RTP: u16 = 1;
/// Indicates that the candidate is used for RTCP.
pub(crate) const COMPONENT_RTCP: u16 = 0;

/// Candidate represents an ICE candidate
#[async_trait]
pub trait Candidate: fmt::Display {
    /// An arbitrary string used in the freezing algorithm to
    /// group similar candidates.  It is the same for two candidates that
    /// have the same type, base IP address, protocol (UDP, TCP, etc.),
    /// and STUN or TURN server.
    fn foundation(&self) -> String;
    /// Connection::MaybeUpdatePeerReflexiveCandidate.
    fn update_peer_reflexive(&self, _candidate: &dyn Candidate) -> bool {
        false
    }

    /// A unique identifier for just this candidate
    /// Unlike the foundation this is different for each candidate.
    fn id(&self) -> String;

    /// A component is a piece of a data stream.
    /// An example is one for RTP, and one for RTCP
    fn component(&self) -> u16;
    fn set_component(&self, c: u16);

    /// The last time this candidate received traffic
    fn last_received(&self) -> SystemTime;

    /// The last time this candidate sent traffic
    fn last_sent(&self) -> SystemTime;

    fn network_type(&self) -> NetworkType;
    fn address(&self) -> String;
    fn port(&self) -> u16;

    fn priority(&self) -> u32;

    /// A transport address related to candidate,
    /// which is useful for diagnostics and other purposes.
    fn related_address(&self) -> Option<CandidateRelatedAddress>;

    fn candidate_type(&self) -> CandidateType;
    fn tcp_type(&self) -> TcpType;

    /// TURN server transport used to create a local relay candidate.
    fn relay_protocol(&self) -> String;

    /// STUN/TURN URL associated with a local translated candidate.
    fn url(&self) -> String;

    fn network_id(&self) -> u16;
    fn set_network_id(&self, id: u16);
    fn network_cost(&self) -> u16;
    fn set_network_cost(&self, cost: u16);
    /// Adapter type used by the BasicIceController network preference
    /// comparator. A zero value means unknown.
    fn adapter_type(&self) -> u32;
    fn mark_local(&self);
    fn network_key(&self) -> String;
    fn is_any_address_network(&self) -> bool;
    fn generation(&self) -> u32;
    fn set_generation(&self, generation: u32);
    fn credentials(&self) -> IceCredentials;
    fn has_credentials(&self) -> bool;
    fn set_credentials(&self, credentials: IceCredentials);
    fn fill_remote_credentials(&self, credentials: &IceCredentials, generation: Option<u32>);

    fn marshal(&self) -> String;

    fn addr(&self) -> SocketAddr;

    async fn close(&self) -> Result<()>;
    fn seen(&self, outbound: bool);

    async fn write_to(
        &self,
        raw: &[u8],
        dst: &(dyn Candidate + Send + Sync),
        payload: bool,
    ) -> Result<usize>;
    fn equal(&self, other: &dyn Candidate) -> bool;
    fn set_ip(&self, ip: &IpAddr) -> Result<()>;
    fn get_conn(&self) -> Option<&Arc<dyn util::Conn + Send + Sync>>;
    fn get_closed_ch(&self) -> Arc<Mutex<Option<broadcast::Sender<()>>>>;
}

/// Represents the type of candidate `CandidateType` enum.
#[derive(Default, Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum CandidateType {
    #[serde(rename = "unspecified")]
    #[default]
    Unspecified,
    #[serde(rename = "host")]
    Host,
    #[serde(rename = "srflx")]
    ServerReflexive,
    #[serde(rename = "prflx")]
    PeerReflexive,
    #[serde(rename = "relay")]
    Relay,
}

// String makes CandidateType printable
impl fmt::Display for CandidateType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match *self {
            CandidateType::Host => "host",
            CandidateType::ServerReflexive => "srflx",
            CandidateType::PeerReflexive => "prflx",
            CandidateType::Relay => "relay",
            CandidateType::Unspecified => "Unknown candidate type",
        };
        write!(f, "{s}")
    }
}

impl CandidateType {
    /// Returns the preference weight of a `CandidateType`.
    ///
    /// 4.1.2.2.  Guidelines for Choosing Type and Local Preferences
    /// The RECOMMENDED values are 126 for host candidates, 100
    /// for server reflexive candidates, 110 for peer reflexive candidates,
    /// and 0 for relayed candidates.
    #[must_use]
    pub const fn preference(self) -> u16 {
        match self {
            Self::Host => 126,
            Self::PeerReflexive => 110,
            Self::ServerReflexive => 100,
            Self::Relay | CandidateType::Unspecified => 0,
        }
    }
}

pub(crate) fn contains_candidate_type(
    candidate_type: CandidateType,
    candidate_type_list: &[CandidateType],
) -> bool {
    if candidate_type_list.is_empty() {
        return false;
    }
    for ct in candidate_type_list {
        if *ct == candidate_type {
            return true;
        }
    }
    false
}

/// Convey transport addresses related to the candidate, useful for diagnostics and other purposes.
#[derive(PartialEq, Eq, Debug, Clone)]
pub struct CandidateRelatedAddress {
    pub address: String,
    pub port: u16,
}

// String makes CandidateRelatedAddress printable
impl fmt::Display for CandidateRelatedAddress {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, " related {}:{}", self.address, self.port)
    }
}

/// Represent the ICE candidate pair state.
#[derive(Default, Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum CandidatePairState {
    #[serde(rename = "unspecified")]
    #[default]
    Unspecified = 0,

    /// Means a check has not been performed for this pair.
    #[serde(rename = "waiting")]
    Waiting = 1,

    /// Means a check has been sent for this pair, but the transaction is in progress.
    #[serde(rename = "in-progress")]
    InProgress = 2,

    /// Means a check for this pair was already done and failed, either never producing any response
    /// or producing an unrecoverable failure response.
    #[serde(rename = "failed")]
    Failed = 3,

    /// Means a check for this pair was already done and produced a successful result.
    #[serde(rename = "succeeded")]
    Succeeded = 4,
}

impl From<u8> for CandidatePairState {
    fn from(v: u8) -> Self {
        match v {
            1 => Self::Waiting,
            2 => Self::InProgress,
            3 => Self::Failed,
            4 => Self::Succeeded,
            _ => Self::Unspecified,
        }
    }
}

impl fmt::Display for CandidatePairState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match *self {
            Self::Waiting => "waiting",
            Self::InProgress => "in-progress",
            Self::Failed => "failed",
            Self::Succeeded => "succeeded",
            Self::Unspecified => "unspecified",
        };

        write!(f, "{s}")
    }
}

/// Represents a combination of a local and remote candidate.
pub struct CandidatePair {
    pub(crate) tcp: Option<Arc<crate::agent::tcp_port::TcpConnection>>,
    pub(crate) ice_role_controlling: AtomicBool,
    pub remote: Arc<dyn Candidate + Send + Sync>,
    pub(crate) local_port: Arc<dyn Candidate + Send + Sync>,
    local_description: arc_swap::ArcSwapOption<CandidateBase>,
    pub(crate) binding_request_count: AtomicU16,
    pub(crate) state: AtomicU8, // convert it to CandidatePairState,
    /// libwebrtc Connection write state: 0 writable, 1 unreliable,
    /// 2 init, 3 timed out. This is independent of the ICE checklist state.
    pub(crate) write_state: AtomicU8,
    pub(crate) send_blocked: AtomicBool,
    send_failure: StdMutex<Option<(Instant, String)>>,
    pub(crate) nominated: AtomicBool,
    pub(crate) requests_sent: AtomicU64,
    pub(crate) responses_received: AtomicU64,
    pub(crate) total_round_trip_micros: AtomicU64,
    pub(crate) current_round_trip_micros: AtomicU64,
    pub(crate) smoothed_round_trip_ms: AtomicU32,
    pub(crate) pinged_in_round: AtomicBool,
    pub(crate) last_check_sent_nanos: AtomicU64,
    pub(crate) last_check_received_nanos: AtomicU64,
    pub(crate) last_check_response_nanos: AtomicU64,
    pub(crate) last_payload_received_nanos: AtomicU64,
    /// Monotonic timestamp of the last receiving-state transition.
    ///
    /// `Connection::receiving_` stores the time whenever the
    /// derived receiving bit changes. This is
    /// deliberately different from the last packet timestamp: the ICE
    /// controller uses it to avoid switching while the receiving state is
    /// still settling.
    pub(crate) receiving_changed_nanos: AtomicU64,
    pub(crate) receiving_state: AtomicBool,
    /// Binding-request send times retained since the last successful response.
    pub(crate) outstanding_ping_nanos: StdMutex<VecDeque<u64>>,
    /// Creation time used by libwebrtc's relay-pruning grace period.
    pub(crate) created_at_nanos: AtomicU64,
    pub(crate) selected_at_nanos: AtomicU64,
    pub(crate) unanswered_since_nanos: AtomicU64,
    pub(crate) pruned: AtomicBool,
    pub(crate) remote_nomination: AtomicU32,
    pub(crate) consecutive_unanswered_checks: AtomicU16,
    /// Recent check loss and RTT, for leaving a degraded direct path.
    pub(crate) quality: StdMutex<path_quality::PathQuality>,
}

impl Default for CandidatePair {
    fn default() -> Self {
        Self {
            tcp: None,
            ice_role_controlling: AtomicBool::new(false),
            remote: Arc::new(CandidateBase::default()),
            local_port: Arc::new(CandidateBase::default()),
            local_description: arc_swap::ArcSwapOption::empty(),
            state: AtomicU8::new(CandidatePairState::Waiting as u8),
            write_state: AtomicU8::new(2),
            send_blocked: AtomicBool::new(false),
            send_failure: StdMutex::new(None),
            binding_request_count: AtomicU16::new(0),
            nominated: AtomicBool::new(false),
            requests_sent: AtomicU64::new(0),
            responses_received: AtomicU64::new(0),
            total_round_trip_micros: AtomicU64::new(0),
            current_round_trip_micros: AtomicU64::new(0),
            smoothed_round_trip_ms: AtomicU32::new(3000),
            pinged_in_round: AtomicBool::new(false),
            last_check_sent_nanos: AtomicU64::new(0),
            last_check_received_nanos: AtomicU64::new(0),
            last_check_response_nanos: AtomicU64::new(0),
            last_payload_received_nanos: AtomicU64::new(0),
            receiving_changed_nanos: AtomicU64::new(0),
            receiving_state: AtomicBool::new(false),
            outstanding_ping_nanos: StdMutex::new(VecDeque::new()),
            created_at_nanos: AtomicU64::new(Self::now_nanos()),
            selected_at_nanos: AtomicU64::new(0),
            unanswered_since_nanos: AtomicU64::new(0),
            pruned: AtomicBool::new(false),
            remote_nomination: AtomicU32::new(0),
            consecutive_unanswered_checks: AtomicU16::new(0),
            quality: StdMutex::default(),
        }
    }
}

impl fmt::Debug for CandidatePair {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let local = self.local_candidate();
        write!(
            f,
            "prio {} (local, prio {}) {} <-> {} (remote, prio {})",
            self.priority(),
            local.priority(),
            local,
            self.remote,
            self.remote.priority()
        )
    }
}

impl fmt::Display for CandidatePair {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let local = self.local_candidate();
        write!(
            f,
            "prio {} (local, prio {}) {} <-> {} (remote, prio {})",
            self.priority(),
            local.priority(),
            local,
            self.remote,
            self.remote.priority()
        )
    }
}

impl PartialEq for CandidatePair {
    fn eq(&self, other: &Self) -> bool {
        self.local_candidate().equal(&*other.local_candidate()) && self.remote.equal(&*other.remote)
    }
}

impl CandidatePair {
    /// A Connection's reported local candidate may become srflx/prflx after
    /// a Binding success. Its actual Port/socket owner never changes here.
    pub fn local_candidate(&self) -> Arc<dyn Candidate + Send + Sync> {
        self.local_description
            .load_full()
            .map(|candidate| candidate as Arc<dyn Candidate + Send + Sync>)
            .unwrap_or_else(|| self.local_port.clone())
    }

    pub(crate) fn set_local_candidate(&self, candidate: Arc<CandidateBase>) {
        self.local_description.store(Some(candidate));
    }

    pub(crate) fn update_local_cost(&self, cost: u16) {
        if let Some(candidate) = self.local_description.load().as_ref() {
            candidate.set_network_cost(cost);
        }
    }

    #[must_use]
    pub fn new(
        local: Arc<dyn Candidate + Send + Sync>,
        remote: Arc<dyn Candidate + Send + Sync>,
        controlling: bool,
    ) -> Self {
        Self {
            tcp: None,
            ice_role_controlling: AtomicBool::new(controlling),
            remote,
            local_port: local,
            local_description: arc_swap::ArcSwapOption::empty(),
            state: AtomicU8::new(CandidatePairState::Waiting as u8),
            write_state: AtomicU8::new(2),
            send_blocked: AtomicBool::new(false),
            send_failure: StdMutex::new(None),
            binding_request_count: AtomicU16::new(0),
            nominated: AtomicBool::new(false),
            requests_sent: AtomicU64::new(0),
            responses_received: AtomicU64::new(0),
            total_round_trip_micros: AtomicU64::new(0),
            current_round_trip_micros: AtomicU64::new(0),
            smoothed_round_trip_ms: AtomicU32::new(3000),
            pinged_in_round: AtomicBool::new(false),
            last_check_sent_nanos: AtomicU64::new(0),
            last_check_received_nanos: AtomicU64::new(0),
            last_check_response_nanos: AtomicU64::new(0),
            last_payload_received_nanos: AtomicU64::new(0),
            receiving_changed_nanos: AtomicU64::new(0),
            receiving_state: AtomicBool::new(false),
            outstanding_ping_nanos: StdMutex::new(VecDeque::new()),
            created_at_nanos: AtomicU64::new(Self::now_nanos()),
            selected_at_nanos: AtomicU64::new(0),
            unanswered_since_nanos: AtomicU64::new(0),
            pruned: AtomicBool::new(false),
            remote_nomination: AtomicU32::new(0),
            consecutive_unanswered_checks: AtomicU16::new(0),
            quality: StdMutex::default(),
        }
    }

    pub(crate) fn now_nanos() -> u64 {
        ICE_MONOTONIC_EPOCH
            .elapsed()
            .as_nanos()
            .min(u128::from(u64::MAX - 1)) as u64
            + 1
    }

    pub(crate) fn outstanding_ping_times(&self) -> Vec<u64> {
        self.outstanding_ping_nanos
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .iter()
            .copied()
            .collect()
    }

    fn last_receiving_activity_nanos(&self) -> u64 {
        self.last_check_response_nanos
            .load(Ordering::Relaxed)
            .max(self.last_check_received_nanos.load(Ordering::Relaxed))
            .max(self.last_payload_received_nanos.load(Ordering::Relaxed))
    }

    /// Refreshes the derived receiving bit and records transitions exactly
    /// like `Connection::UpdateReceiving`.
    pub(crate) fn refresh_receiving_state(&self, now: u64, timeout: Duration) -> bool {
        let last = self.last_receiving_activity_nanos();
        // Connection::UpdateReceiving: a response newer
        // than the last ping keeps receiving true until another ping is sent.
        let receiving = self.last_check_response_nanos.load(Ordering::Relaxed)
            > self.last_check_sent_nanos.load(Ordering::Relaxed)
            || (last != 0 && now.saturating_sub(last) <= timeout.as_nanos() as u64);
        let previous = self.receiving_state.swap(receiving, Ordering::Relaxed);
        if previous != receiving {
            self.receiving_changed_nanos.store(now, Ordering::Relaxed);
        }
        receiving
    }

    fn note_receiving_activity(&self, now: u64, timeout: Duration) {
        self.refresh_receiving_state(now, timeout);
    }

    pub(crate) fn note_check_sent(&self) {
        let now = Self::now_nanos();
        self.last_check_sent_nanos.store(now, Ordering::Relaxed);
        self.pinged_in_round.store(true, Ordering::Relaxed);
        let mut outstanding = self
            .outstanding_ping_nanos
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        outstanding.push_back(now);
        self.unanswered_since_nanos
            .store(*outstanding.front().unwrap_or(&now), Ordering::Relaxed);
        self.consecutive_unanswered_checks.store(
            outstanding.len().min(usize::from(u16::MAX)) as u16,
            Ordering::Relaxed,
        );
    }

    /// The selected connection's RTT is valid only after a STUN response.
    /// `P2PTransportChannel::GetRttEstimate` exposes this filtered
    /// value, not the 3000 ms constructor default or the last raw sample.
    pub fn rtt_estimate(&self) -> Option<Duration> {
        (self.responses_received.load(Ordering::Acquire) != 0).then(|| {
            Duration::from_millis(u64::from(
                self.smoothed_round_trip_ms.load(Ordering::Relaxed),
            ))
        })
    }

    pub(crate) fn record_round_trip(&self, micros: u64) {
        let sample_ms = (micros / 1000).min(u64::from(u32::MAX)) as u32;
        let old = self.smoothed_round_trip_ms.load(Ordering::Relaxed);
        let filtered = if self.responses_received.load(Ordering::Relaxed) == 0 {
            sample_ms
        } else {
            // Integer truncation of (3*old + sample)/4.
            ((u64::from(old) * 3 + u64::from(sample_ms)) / 4) as u32
        };
        self.smoothed_round_trip_ms
            .store(filtered, Ordering::Relaxed);
        self.current_round_trip_micros
            .store(micros, Ordering::Relaxed);
        self.total_round_trip_micros
            .fetch_add(micros, Ordering::Relaxed);
        self.responses_received.fetch_add(1, Ordering::Release);
    }

    pub(crate) fn note_check_response(&self, receiving_timeout: Duration) {
        if let Some(tcp) = &self.tcp {
            tcp.note_binding_success();
        }
        let now = Self::now_nanos();
        self.last_check_response_nanos.store(now, Ordering::Relaxed);
        self.note_receiving_activity(now, receiving_timeout);
        self.outstanding_ping_nanos
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clear();
        self.write_state.store(0, Ordering::Relaxed);
        self.consecutive_unanswered_checks
            .store(0, Ordering::Relaxed);
        self.unanswered_since_nanos.store(0, Ordering::Relaxed);
    }

    pub(crate) fn note_check_received(&self, receiving_timeout: Duration) {
        let now = Self::now_nanos();
        self.last_check_received_nanos.store(now, Ordering::Relaxed);
        self.note_receiving_activity(now, receiving_timeout);
    }

    /// Returns whether this packet changed receiving, not whether it is true.
    pub(crate) fn note_payload_received(&self, receiving_timeout: Duration) -> bool {
        let previous = self.receiving_state.load(Ordering::Relaxed);
        let now = Self::now_nanos();
        self.last_payload_received_nanos
            .store(now, Ordering::Relaxed);
        self.refresh_receiving_state(now, receiving_timeout) != previous
    }

    pub(crate) fn note_selected(&self) {
        self.selected_at_nanos
            .store(Self::now_nanos(), Ordering::Relaxed);
    }

    pub(crate) fn age_since(&self, timestamp: &AtomicU64) -> Duration {
        let value = timestamp.load(Ordering::Relaxed);
        if value == 0 {
            Duration::MAX
        } else {
            Duration::from_nanos(Self::now_nanos().saturating_sub(value))
        }
    }

    /// RFC 5245 - 5.7.2.  Computing Pair Priority and Ordering Pairs
    /// Let G be the priority for the candidate provided by the controlling
    /// agent.  Let D be the priority for the candidate provided by the
    /// controlled agent.
    /// pair priority = 2^32*MIN(G,D) + 2*MAX(G,D) + (G>D?1:0)
    pub fn priority(&self) -> u64 {
        let view = self.local_description.load();
        let local_priority = view
            .as_ref()
            .map_or_else(|| self.local_port.priority(), |local| local.priority());
        let (g, d) = if self.ice_role_controlling.load(Ordering::SeqCst) {
            (local_priority, self.remote.priority())
        } else {
            (self.remote.priority(), local_priority)
        };

        // High 32 bits are MIN(G,D); the low expression is
        // calculated as uint32 and ORed in. The inherited (2^32-1)*MIN
        // expression is not the native pair priority.
        (u64::from(g.min(d)) << 32)
            | u64::from(g.max(d).wrapping_mul(2).wrapping_add(u32::from(g > d)))
    }

    pub(crate) fn block_failed_send(&self, error: &str) -> bool {
        let mut failure = self.send_failure.lock().unwrap_or_else(|p| p.into_inner());
        if failure.is_some() {
            return false;
        }
        *failure = Some((Instant::now(), error.to_owned()));
        self.send_blocked.store(true, Ordering::Release);
        self.write_state.store(2, Ordering::Release);
        true
    }

    pub(crate) fn blocked_send_reason(&self) -> Option<String> {
        if !self.send_blocked.load(Ordering::Acquire) {
            return None;
        }
        self.send_failure
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .as_ref()
            .map(|(_, error)| error.clone())
    }

    pub(crate) fn confirm_send_path(&self, request_sent: Instant) -> bool {
        let mut failure = self.send_failure.lock().unwrap_or_else(|p| p.into_inner());
        if let Some((failed_at, _)) = failure.as_ref() {
            // A delayed response to a pre-failure ping must not revive the path.
            if request_sent <= *failed_at {
                return false;
            }
            *failure = None;
            self.send_blocked.store(false, Ordering::Release);
        }
        true
    }

    pub async fn write(&self, b: &[u8]) -> Result<usize> {
        if let Some(tcp) = &self.tcp {
            if tcp.connected()
                && (self.write_state.load(Ordering::Acquire) != 0 || !tcp.media_ready())
            {
                return Err(crate::Error::Other(
                    "TCP ICE connection is not media-writable".into(),
                ));
            }
            return tcp
                .send(b)
                .map_err(|error| crate::Error::Other(error.to_string()));
        }
        self.local_port.write_to(b, &*self.remote, true).await
    }
}
