use std::cmp::Ordering as CmpOrdering;
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::atomic::Ordering;
use std::sync::Arc;

use async_trait::async_trait;
use stun::agent::*;
use stun::attributes::*;
use stun::fingerprint::*;
use stun::integrity::*;
use stun::message::*;
use stun::textattrs::*;
use tokio::time::Duration;

use crate::agent::agent_internal::*;
use crate::candidate::path_quality::{
    better, cheapest_better, Measure, PathClass, HOLD, PROBE_INTERVAL, TRIAL,
};
use crate::candidate::*;
use crate::control::*;
use crate::priority::*;
use crate::state::ConnectionState;
use crate::use_candidate::*;

const ATTR_GOOG_NETWORK_INFO: AttrType = AttrType(0xC057);
const ATTR_GOOG_GENERATION: AttrType = AttrType(0xC070);

fn goog_generation(candidate: &Arc<dyn Candidate + Send + Sync>) -> RawAttribute {
    RawAttribute {
        typ: ATTR_GOOG_GENERATION,
        length: 4,
        value: candidate.generation().to_be_bytes().to_vec(),
    }
}
/// Google ICE renomination attribute used by libwebrtc. It is only emitted
/// when `a=ice-options:renomination` was negotiated; parsing it is still
/// required on the controlled side even when the normal USE-CANDIDATE path
/// is in use.
const ATTR_NOMINATION: AttrType = AttrType(0xC001);

fn remote_nomination(message: &Message) -> Option<u32> {
    let value = message.get(ATTR_NOMINATION).ok()?;
    let bytes: [u8; 4] = value.as_slice().try_into().ok()?;
    Some(u32::from_be_bytes(bytes))
}

/// Applies the nomination signal carried by an inbound Binding request. The
/// RFC USE-CANDIDATE marker is accepted as nomination 1, but when the optional
/// `renomination` mode is negotiated it carries an increasing 32-bit value in
/// STUN attribute 0xC001.  A present zero value is invalid and must not fall
/// back to USE-CANDIDATE (it is logged and ignored).
pub(super) fn apply_remote_nomination(message: &Message, pair: &CandidatePair) {
    if let Some(value) = remote_nomination(message) {
        if value == 0 {
            log::warn!("ignoring invalid zero STUN nomination");
            return;
        }
        pair.remote_nomination.fetch_max(value, Ordering::Relaxed);
    } else if message.contains(ATTR_USE_CANDIDATE) {
        pair.remote_nomination.fetch_max(1, Ordering::Relaxed);
    }
}

fn goog_network_info(candidate: &Arc<dyn Candidate + Send + Sync>) -> RawAttribute {
    let value = (u32::from(candidate.network_id()) << 16) | u32::from(candidate.network_cost());
    RawAttribute {
        typ: ATTR_GOOG_NETWORK_INFO,
        length: 4,
        value: value.to_be_bytes().to_vec(),
    }
}

pub(super) fn remote_network_info(message: &Message) -> Option<(u16, u16)> {
    let value = message.get(ATTR_GOOG_NETWORK_INFO).ok()?;
    let bytes: [u8; 4] = value.as_slice().try_into().ok()?;
    let network_info = u32::from_be_bytes(bytes);
    Some(((network_info >> 16) as u16, network_info as u16))
}

pub(super) fn apply_remote_network_info(
    message: &Message,
    candidate: &Arc<dyn Candidate + Send + Sync>,
) {
    if let Some((_, network_cost)) = remote_network_info(message) {
        // This updates cost, not the previously signaled network id.
        candidate.set_network_cost(network_cost);
    }
}

#[async_trait]
trait ControllingSelector {
    async fn start(&self);
    async fn contact_candidates(&self);
    async fn ping_candidate(
        &self,
        local: &Arc<dyn Candidate + Send + Sync>,
        remote: &Arc<dyn Candidate + Send + Sync>,
    );
    async fn handle_success_response(
        &self,
        m: &Message,
        local: &Arc<dyn Candidate + Send + Sync>,
        remote: &Arc<dyn Candidate + Send + Sync>,
        remote_addr: SocketAddr,
    );
}

#[async_trait]
trait ControlledSelector {
    async fn start(&self);
    async fn contact_candidates(&self);
    async fn ping_candidate(
        &self,
        local: &Arc<dyn Candidate + Send + Sync>,
        remote: &Arc<dyn Candidate + Send + Sync>,
    );
    async fn handle_success_response(
        &self,
        m: &Message,
        local: &Arc<dyn Candidate + Send + Sync>,
        remote: &Arc<dyn Candidate + Send + Sync>,
        remote_addr: SocketAddr,
    );
}

/// The kind of path a pair is (`candidate::path_quality::PathClass`).
pub(super) fn path_class(pair: &CandidatePair) -> PathClass {
    let local = pair.local_candidate();
    let local_relay = local.candidate_type() == CandidateType::Relay;
    PathClass::of(
        local_relay || pair.remote.candidate_type() == CandidateType::Relay,
        if local_relay { local.relay_protocol() } else { String::new() }.as_str(),
    )
}

/// The controlling agent's path choice (`AgentInternal::choose_path`).
#[derive(Default)]
pub(crate) struct PathChoice {
    /// Consecutive one-second verdicts for `holding`.
    pub(crate) hold: u32,
    holding: std::sync::Weak<CandidatePair>,
    decided_at: u64,
    /// The path a retry came from, and when the retry's trial ends.
    trial: Option<(std::sync::Weak<CandidatePair>, u64)>,
    /// A relay chosen over a better-ranked path.
    pinned: Option<std::sync::Weak<CandidatePair>>,
    /// A load test of the alternative under way.
    load: Option<LoadTest>,
}

/// A load test the peer is sending over `pair` (`load_test`).
struct LoadTest {
    pair: std::sync::Weak<CandidatePair>,
    until: u64,
    total: u32,
    rate_kbps: u32,
}

impl LoadTest {
    fn of(&self, pair: &Arc<CandidatePair>) -> bool {
        self.pair
            .upgrade()
            .is_some_and(|tested| Arc::ptr_eq(&tested, pair))
    }
}

impl PathChoice {
    fn pinned(&self, pair: &Arc<CandidatePair>) -> bool {
        self.pinned
            .as_ref()
            .and_then(std::sync::Weak::upgrade)
            .is_some_and(|pinned| Arc::ptr_eq(&pinned, pair))
    }

    /// `pair` is where a retry on trial came from.
    fn trial_from(&mut self, pair: &Arc<CandidatePair>, now: u64) -> bool {
        if self.trial.as_ref().is_some_and(|(_, until)| now >= *until) {
            self.trial = None;
        }
        self.trial
            .as_ref()
            .and_then(|(from, _)| from.upgrade())
            .is_some_and(|from| Arc::ptr_eq(&from, pair))
    }

    /// Count a verdict for `pair`; true once it has held for [`HOLD`].
    fn holds(&mut self, pair: &Arc<CandidatePair>) -> bool {
        if !self
            .holding
            .upgrade()
            .is_some_and(|holding| Arc::ptr_eq(&holding, pair))
        {
            self.holding = Arc::downgrade(pair);
            self.hold = 0;
        }
        self.hold += 1;
        if self.hold < HOLD {
            return false;
        }
        self.hold = 0;
        true
    }
}

impl AgentInternal {
    pub(crate) async fn ice_controller_check_interval(&self) -> Duration {
        use crate::agent::agent_config::{
            ICE_RECEIVING_TIMEOUT, ICE_STRONG_PING_INTERVAL, ICE_WEAK_PING_INTERVAL,
        };
        let _serial = self.controller_serial.lock().await;
        let pairs = self.agent_conn.checklist.lock().await;
        for pair in pairs.iter() {
            self.refresh_pair_state(pair);
        }
        let selected = self.agent_conn.get_selected_pair();
        let weak = selected
            .as_ref()
            .is_none_or(|pair| !self.ice_selected_receiving(pair) || !self.ice_writable(pair));
        let need_more_weak_pings = pairs.iter().any(|pair| {
            !pair.pruned.load(Ordering::Relaxed)
                && CandidatePairState::from(pair.state.load(Ordering::SeqCst))
                    != CandidatePairState::Failed
                && self.ice_write_state(pair) != 3
                && pair.requests_sent.load(Ordering::Relaxed)
                    < crate::agent::agent_config::ICE_MIN_PINGS_AT_WEAK_INTERVAL
        });
        if weak || need_more_weak_pings {
            return ICE_WEAK_PING_INTERVAL;
        }
        let receiving_check = (ICE_RECEIVING_TIMEOUT / 10).max(Duration::from_millis(50));
        ICE_STRONG_PING_INTERVAL.min(receiving_check)
    }

    fn ice_last_activity_nanos(&self, pair: &CandidatePair) -> u64 {
        pair.last_payload_received_nanos
            .load(Ordering::Relaxed)
            .max(pair.last_check_received_nanos.load(Ordering::Relaxed))
            .max(pair.last_check_response_nanos.load(Ordering::Relaxed))
    }

    fn refresh_pair_state(&self, pair: &CandidatePair) {
        use crate::agent::agent_config::{
            ICE_INACTIVE_TIMEOUT, ICE_UNWRITABLE_MIN_CHECKS, ICE_UNWRITABLE_TIMEOUT,
        };

        pair.refresh_receiving_state(
            CandidatePair::now_nanos(),
            crate::agent::agent_config::ICE_RECEIVING_TIMEOUT,
        );
        if CandidatePairState::from(pair.state.load(Ordering::SeqCst)) == CandidatePairState::Failed
            || pair.pruned.load(Ordering::Relaxed)
        {
            pair.write_state.store(3, Ordering::Relaxed);
            return;
        }

        let write_state = pair.write_state.load(Ordering::Relaxed);
        let outstanding = pair.outstanding_ping_times();
        let Some(oldest) = outstanding.first().copied() else {
            return;
        };
        let now = CandidatePair::now_nanos();
        if write_state == 0 && outstanding.len() >= usize::from(ICE_UNWRITABLE_MIN_CHECKS) {
            let response_timeout = Duration::from_millis(u64::from(
                pair.smoothed_round_trip_ms.load(Ordering::Relaxed),
            ))
            .saturating_mul(2)
            .clamp(Duration::from_millis(100), Duration::from_secs(60));
            let fifth = outstanding[usize::from(ICE_UNWRITABLE_MIN_CHECKS) - 1];
            if now.saturating_sub(fifth) > response_timeout.as_nanos() as u64
                && now.saturating_sub(oldest) > ICE_UNWRITABLE_TIMEOUT.as_nanos() as u64
            {
                pair.write_state.store(1, Ordering::Relaxed);
            }
        }
        let state = pair.write_state.load(Ordering::Relaxed);
        let timeout = if state == 1 {
            ICE_INACTIVE_TIMEOUT.min(Duration::from_secs(9))
        } else {
            ICE_INACTIVE_TIMEOUT
        };
        if (state == 1 || state == 2) && now.saturating_sub(oldest) > timeout.as_nanos() as u64 {
            pair.write_state.store(3, Ordering::Relaxed);
        }
    }

    fn ice_pair_is_dead(&self, pair: &CandidatePair) -> bool {
        // Terminal Connection removal does not
        // destroy its Port/socket or the other Connections on that Network.
        let now = CandidatePair::now_nanos();
        let last_received = self.ice_last_activity_nanos(pair);
        if last_received == 0 {
            return self.ice_write_state(pair) == 3
                && now.saturating_sub(pair.created_at_nanos.load(Ordering::Relaxed))
                    > 10_000_000_000;
        }
        if now.saturating_sub(last_received) <= 30_000_000_000 {
            return false;
        }
        pair.outstanding_ping_times()
            .first()
            .is_none_or(|oldest| now.saturating_sub(*oldest) > 30_000_000_000)
    }

    fn ice_receiving(&self, pair: &CandidatePair) -> bool {
        pair.receiving_state.load(Ordering::Relaxed)
    }

    fn ice_receiving_unchanged_since_nanos(&self, pair: &CandidatePair) -> u64 {
        pair.receiving_changed_nanos.load(Ordering::Relaxed)
    }

    fn ice_writable(&self, pair: &CandidatePair) -> bool {
        if pair.send_blocked.load(Ordering::Acquire) {
            return false;
        }
        if CandidatePairState::from(pair.state.load(Ordering::SeqCst))
            != CandidatePairState::Succeeded
            || pair.pruned.load(Ordering::Relaxed)
        {
            return false;
        }
        pair.write_state.load(Ordering::Relaxed) == 0
    }

    fn ice_selected_receiving(&self, pair: &CandidatePair) -> bool {
        self.ice_receiving(pair)
    }

    fn ice_write_state(&self, pair: &CandidatePair) -> u8 {
        pair.write_state.load(Ordering::Relaxed)
    }

    fn ice_ready_to_send(&self, pair: &CandidatePair) -> bool {
        !pair.send_blocked.load(Ordering::Acquire)
            && !pair.pruned.load(Ordering::Relaxed)
            && (self.ice_writable(pair)
                || self.ice_write_state(pair) == 1
                || self.ice_presumed_writable(pair))
    }

    fn ice_connected(&self, pair: &CandidatePair) -> bool {
        // Connection-level "connected" is distinct from ICE/write state.
        pair.tcp.as_ref().map_or_else(
            || pair.local_port.get_conn().is_some(),
            |tcp| tcp.connected(),
        )
    }

    /// Compare the connection's network preference and cost before the
    /// writable/receiving state comparator. `BasicIceController::ShouldSwitchConnection`
    /// checks this first; a candidate on a less-preferred network is rejected
    /// immediately unless it is already receiving (or the fully-relayed
    /// presumed-writable mode applies). Keeping this separate from the later
    /// generation/priority comparator is important: a better state on a
    /// costly backup must not displace a healthy cheap path.
    fn ice_pair_network_cost_cmp(&self, a: &CandidatePair, b: &CandidatePair) -> CmpOrdering {
        let a_cost =
            u32::from(a.local_candidate().network_cost()) + u32::from(a.remote.network_cost());
        let b_cost =
            u32::from(b.local_candidate().network_cost()) + u32::from(b.remote.network_cost());
        b_cost.cmp(&a_cost)
    }

    fn ice_pair_network_policy_cmp(&self, a: &CandidatePair, b: &CandidatePair) -> CmpOrdering {
        if let Some(desired) = self.network_preference {
            let a_matches = a.local_candidate().adapter_type() == desired;
            let b_matches = b.local_candidate().adapter_type() == desired;
            match (a_matches, b_matches) {
                (true, false) => return CmpOrdering::Greater,
                (false, true) => return CmpOrdering::Less,
                _ => {}
            }
        }

        let a_vpn = a.local_candidate().adapter_type() == 8;
        let b_vpn = b.local_candidate().adapter_type() == 8;
        match self.vpn_preference {
            1 | 3 if a_vpn != b_vpn => return a_vpn.cmp(&b_vpn),
            2 | 4 if a_vpn != b_vpn => return b_vpn.cmp(&a_vpn),
            _ => {}
        }
        self.ice_pair_network_cost_cmp(a, b)
    }

    /// Presumed-writable applies only to an in-progress pair
    /// whose local candidate is relay and whose remote candidate is relay or
    /// peer-reflexive. This is left disabled by default.
    fn ice_presumed_writable(&self, pair: &CandidatePair) -> bool {
        !pair.send_blocked.load(Ordering::Acquire)
            && self.presume_writable_when_fully_relayed
            && self.ice_write_state(pair) == 2
            && pair.local_candidate().candidate_type() == CandidateType::Relay
            && matches!(
                pair.remote.candidate_type(),
                CandidateType::Relay | CandidateType::PeerReflexive
            )
    }

    fn ice_pair_state_cmp(
        &self,
        a: &CandidatePair,
        b: &CandidatePair,
        receiving_unchanged_threshold_nanos: Option<u64>,
        missed_receiving_unchanged_threshold: &mut bool,
    ) -> CmpOrdering {
        // BasicIceController treats the relay/prflx in-progress pair as
        // writable when the presume-writable flag is enabled.  Compare the
        // real writable bit first, then write-state so a genuinely writable
        // pair still outranks a presumed one.
        let a_writable = self.ice_writable(a) || self.ice_presumed_writable(a);
        let b_writable = self.ice_writable(b) || self.ice_presumed_writable(b);
        let writable = a_writable.cmp(&b_writable);
        if writable != CmpOrdering::Equal {
            return writable;
        }
        let write_state = self.ice_write_state(b).cmp(&self.ice_write_state(a));
        if write_state != CmpOrdering::Equal {
            return write_state;
        }
        let a_receiving = self.ice_receiving(a);
        let b_receiving = self.ice_receiving(b);
        if a_receiving && !b_receiving {
            return CmpOrdering::Greater;
        }
        if !a_receiving && b_receiving {
            let Some(threshold) = receiving_unchanged_threshold_nanos else {
                return CmpOrdering::Less;
            };
            // BasicIceController compares the last receiving-state
            // transition rather than the last packet timestamp. If
            // the currently non-receiving pair changed state after the
            // switching threshold, the controller defers the decision. When
            // both transitions are old it may switch immediately; otherwise
            // it marks the recheck as pending and continues with the remaining
            // state tie-breakers.
            if self.ice_receiving_unchanged_since_nanos(a) > threshold {
                *missed_receiving_unchanged_threshold = true;
            } else if self.ice_receiving_unchanged_since_nanos(b) <= threshold {
                return CmpOrdering::Less;
            } else {
                *missed_receiving_unchanged_threshold = true;
            }
        }
        if self.ice_write_state(a) == 0 && self.ice_write_state(b) == 0 {
            let connected = self.ice_connected(a).cmp(&self.ice_connected(b));
            if connected != CmpOrdering::Equal {
                return connected;
            }
        }
        CmpOrdering::Equal
    }

    fn ice_pair_candidate_cmp(&self, a: &CandidatePair, b: &CandidatePair) -> CmpOrdering {
        // The combined ICE generation is intentionally promoted
        // before network cost and pair priority. This differs from
        // upstream libwebrtc and lets a restart generation replace the old
        // selected path while that old path continues carrying media.
        let a_generation = a
            .local_candidate()
            .generation()
            .saturating_add(a.remote.generation());
        let b_generation = b
            .local_candidate()
            .generation()
            .saturating_add(b.remote.generation());
        let generation = a_generation.cmp(&b_generation);
        if generation != CmpOrdering::Equal {
            return generation;
        }
        self.ice_pair_network_policy_cmp(a, b)
            .then_with(|| a.priority().cmp(&b.priority()))
            .then_with(|| {
                b.pruned
                    .load(Ordering::Relaxed)
                    .cmp(&a.pruned.load(Ordering::Relaxed))
            })
    }

    fn ice_pair_cmp_at(
        &self,
        a: &CandidatePair,
        b: &CandidatePair,
        receiving_unchanged_threshold_nanos: Option<u64>,
        missed_receiving_unchanged_threshold: &mut bool,
    ) -> CmpOrdering {
        self.ice_pair_state_cmp(
            a,
            b,
            receiving_unchanged_threshold_nanos,
            missed_receiving_unchanged_threshold,
        )
        .then_with(|| {
            if self.is_controlling.load(Ordering::SeqCst) {
                CmpOrdering::Equal
            } else {
                a.remote_nomination
                    .load(Ordering::Relaxed)
                    .cmp(&b.remote_nomination.load(Ordering::Relaxed))
                    .then_with(|| {
                        a.last_payload_received_nanos
                            .load(Ordering::Relaxed)
                            .cmp(&b.last_payload_received_nanos.load(Ordering::Relaxed))
                    })
            }
        })
        .then_with(|| self.ice_pair_candidate_cmp(a, b))
    }

    fn ice_pair_cmp(&self, a: &CandidatePair, b: &CandidatePair) -> CmpOrdering {
        let mut missed = false;
        self.ice_pair_cmp_at(a, b, None, &mut missed)
    }

    fn ice_pair_sort_cmp(&self, a: &CandidatePair, b: &CandidatePair) -> CmpOrdering {
        self.ice_pair_cmp(a, b).then_with(|| {
            let a_rtt = a.smoothed_round_trip_ms.load(Ordering::Relaxed);
            let b_rtt = b.smoothed_round_trip_ms.load(Ordering::Relaxed);
            b_rtt.cmp(&a_rtt)
        })
    }

    fn ice_active_writable_ping_interval(&self, pair: &CandidatePair, weak: bool) -> Duration {
        use crate::agent::agent_config::{
            ICE_STABLE_WRITABLE_PING_INTERVAL, ICE_WEAK_OR_STABILIZING_PING_INTERVAL,
            ICE_WEAK_PING_INTERVAL,
        };
        if pair.requests_sent.load(Ordering::Relaxed)
            < crate::agent::agent_config::ICE_MIN_PINGS_AT_WEAK_INTERVAL
        {
            return ICE_WEAK_PING_INTERVAL;
        }
        let rtt_micros = u64::from(pair.smoothed_round_trip_ms.load(Ordering::Relaxed)) * 1000;
        let outstanding = pair.outstanding_ping_times();
        let stable = pair.responses_received.load(Ordering::Relaxed) >= 5
            && (outstanding.is_empty()
                || (CandidatePair::now_nanos().saturating_sub(outstanding[0])
                    <= Duration::from_micros(rtt_micros.saturating_mul(2))
                        .as_nanos()
                        .min(u128::from(u64::MAX)) as u64));
        if weak || !stable {
            return ICE_STABLE_WRITABLE_PING_INTERVAL.min(ICE_WEAK_OR_STABILIZING_PING_INTERVAL);
        }
        ICE_STABLE_WRITABLE_PING_INTERVAL
    }

    fn ice_pingable(&self, pair: &CandidatePair, selected: Option<&CandidatePair>) -> bool {
        if !pair.remote.has_credentials() {
            return false;
        }
        if pair.pruned.load(Ordering::Relaxed)
            || CandidatePairState::from(pair.state.load(Ordering::SeqCst))
                == CandidatePairState::Failed
            || (!self.ice_connected(pair) && !self.ice_writable(pair))
            || self
                .max_outstanding_pings
                .is_some_and(|limit| pair.outstanding_ping_times().len() >= usize::from(limit))
        {
            return false;
        }
        let weak = selected.is_none_or(|selected| {
            !self.ice_writable(selected)
                || !self.ice_receiving(selected)
                || !self.ice_connected(selected)
        });
        if weak {
            return true;
        }
        // IsBackupConnection requires channel STATE_COMPLETED: at most one
        // active Connection on each Network. The caller supplies that flag below.
        if self.ice_write_state(pair) == 3 {
            return false;
        }
        !self.ice_writable(pair)
            || pair.age_since(&pair.last_check_sent_nanos)
                >= self.ice_active_writable_ping_interval(pair, false)
    }

    fn prune_connections(&self, pairs: &[Arc<CandidatePair>], selected: &Arc<CandidatePair>) {
        if !self.ice_connected(selected)
            || !self.ice_writable(selected)
            || !self.ice_receiving(selected)
        {
            return;
        }

        // BasicIceController::PruneConnections runs after the connection list
        // has been sorted.  Keep that ordering explicit: the first connection
        // for a Network is the top connection unless the selected connection
        // belongs to that Network, in which case the selected connection is
        // pinned as top.
        let mut ordered = pairs
            .iter()
            .filter(|pair| !pair.pruned.load(Ordering::Relaxed))
            .cloned()
            .collect::<Vec<_>>();
        ordered.sort_by(|a, b| self.ice_pair_sort_cmp(b, a));

        let mut best_by_network = HashMap::<String, Arc<CandidatePair>>::new();
        best_by_network.insert(selected.local_port.network_key(), Arc::clone(selected));
        for pair in &ordered {
            let network = pair.local_port.network_key();
            best_by_network
                .entry(network)
                .or_insert_with(|| Arc::clone(pair));
        }

        // PruneConnections protects every relay-involving connection
        // for its first ten seconds. Then Farwing keeps the first active relay
        // pair of each relay kind (UDP, and TCP/TLS) as `choose_path`'s
        // alternatives, instead of libwebrtc's first active relay and first
        // all-TLS relay. The retained entries are global to this pruning pass.
        let now = CandidatePair::now_nanos();
        let relay_grace = Duration::from_secs(10).as_nanos() as u64;
        let mut kept_udp_relay = false;
        let mut kept_stream_relay = false;

        for pair in &ordered {
            if pair.pruned.load(Ordering::Relaxed) {
                continue;
            }

            let class = path_class(pair);
            if class != PathClass::Direct {
                let created = pair.created_at_nanos.load(Ordering::Relaxed);
                if now.saturating_sub(created) < relay_grace {
                    continue;
                }
                let active = pair.write_state.load(Ordering::Relaxed) != 3;
                let kept = if class == PathClass::UdpRelay {
                    &mut kept_udp_relay
                } else {
                    &mut kept_stream_relay
                };
                if active && !*kept {
                    *kept = true;
                    continue;
                }
            }

            let best = if pair.local_port.is_any_address_network() {
                selected
            } else if let Some(best) = best_by_network.get(&pair.local_port.network_key()) {
                best
            } else {
                continue;
            };
            if !Arc::ptr_eq(pair, best)
                && self.ice_writable(best)
                && self.ice_receiving(best)
                && self.ice_connected(best)
                && best.local_candidate().adapter_type() == pair.local_candidate().adapter_type()
                && self.ice_pair_candidate_cmp(best, pair) != CmpOrdering::Less
            {
                pair.write_state.store(3, Ordering::Relaxed);
                pair.pruned.store(true, Ordering::Relaxed);
                log::trace!("ICE pruned lower-ranked same-network connection: {pair}");
            }
        }
    }

    /// The same ShouldSwitchConnection decision is used for sorted candidates
    /// and the controlled-side nonselected-data proposal.
    fn ice_should_switch_connection(
        &self,
        proposed: &Arc<CandidatePair>,
        selected: Option<&Arc<CandidatePair>>,
    ) -> bool {
        if !self.ice_ready_to_send(proposed) {
            return false;
        }
        let Some(current) = selected else {
            return true;
        };
        // A relay chosen over a better-ranked path for its measured quality
        // stays while it works; `choose_path` decides when to leave it.
        if self.is_controlling.load(Ordering::SeqCst)
            && !Arc::ptr_eq(current, proposed)
            && self.ice_writable(current)
            && self
                .path_choice
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .pinned(current)
        {
            return false;
        }
        if Arc::ptr_eq(current, proposed)
            || (self.ice_pair_network_policy_cmp(proposed, current) == CmpOrdering::Less
                && !self.ice_receiving(proposed))
        {
            return false;
        }
        let threshold = CandidatePair::now_nanos().saturating_sub(
            crate::agent::agent_config::ICE_RECEIVING_SWITCHING_DELAY.as_nanos() as u64,
        );
        let mut missed = false;
        // CURRENT first, proposed second. The controller's existing periodic
        // check revisits a missed receiving threshold; media need not wake it.
        let cmp = self.ice_pair_cmp_at(current, proposed, Some(threshold), &mut missed);
        cmp == CmpOrdering::Less
            || (cmp == CmpOrdering::Equal
                && proposed
                    .smoothed_round_trip_ms
                    .load(Ordering::Relaxed)
                    .saturating_add(10)
                    <= current.smoothed_round_trip_ms.load(Ordering::Relaxed))
    }

    /// Keep the session on the cheapest of the selected path and the best
    /// path of each other kind (direct, UDP relay, TCP/TLS relay), by
    /// measured RTT and loss (`candidate::path_quality`). Probes those
    /// alternatives meanwhile.
    async fn choose_path(&self, pairs: &[Arc<CandidatePair>], now: u64) {
        let Some(selected) = self.agent_conn.get_selected_pair() else {
            return;
        };
        if !self.ice_writable(&selected) {
            // A failing path is left by the ordinary rules.
            return;
        }
        let on = path_class(&selected);
        let mut alternatives: Vec<Arc<CandidatePair>> = Vec::new();
        for class in [PathClass::Direct, PathClass::UdpRelay, PathClass::StreamRelay] {
            if class == on {
                continue;
            }
            let best = pairs
                .iter()
                .filter(|pair| {
                    path_class(pair) == class
                        && !pair.pruned.load(Ordering::Relaxed)
                        && self.ice_writable(pair)
                        && self.ice_ready_to_send(pair)
                })
                .min_by_key(|pair| pair.smoothed_round_trip_ms.load(Ordering::Relaxed));
            if let Some(best) = best {
                alternatives.push(Arc::clone(best));
            }
        }
        for alternative in alternatives
            .iter()
            .filter(|pair| pair.age_since(&pair.last_check_sent_nanos) >= PROBE_INTERVAL)
        {
            // Outside the consent-check pacing, which serves the selected path.
            alternative.note_check_sent();
            ControllingSelector::ping_candidate(self, &alternative.local_port, &alternative.remote)
                .await;
        }
        {
            let mut choice = self.path_choice.lock().unwrap_or_else(|e| e.into_inner());
            if now.saturating_sub(choice.decided_at) < Duration::from_secs(1).as_nanos() as u64 {
                return;
            }
            choice.decided_at = now;
            if alternatives.is_empty() {
                choice.hold = 0;
            }
        }
        if alternatives.is_empty() {
            log::debug!(
                target: "webrtc_ice::agent::path",
                "path choice: no usable path besides {selected} ({})",
                on.name()
            );
            return;
        }
        let in_flight = |pair: &CandidatePair| {
            let rtt = u64::from(pair.smoothed_round_trip_ms.load(Ordering::Relaxed).min(1000));
            let recent = Duration::from_millis(2 * rtt + 100).as_nanos() as u64;
            pair.outstanding_ping_times()
                .iter()
                .filter(|sent| now.saturating_sub(**sent) < recent)
                .count() as u64
        };
        let current = selected
            .quality
            .lock()
            .expect("path quality poisoned")
            .measure(now, in_flight(&selected));
        // Each alternative's measure, whether it waits out a retry, and
        // whether the session left it before.
        let measured: Vec<(Option<Measure>, bool, bool)> = alternatives
            .iter()
            .map(|pair| {
                let quality = pair.quality.lock().expect("path quality poisoned");
                (
                    quality.measure(now, in_flight(pair)),
                    quality.blocked(now),
                    quality.was_left(now),
                )
            })
            .collect();
        log::debug!(
            target: "webrtc_ice::agent::path",
            "path choice: {selected} ({}) {current:?} against {}",
            on.name(),
            alternatives
                .iter()
                .zip(&measured)
                .map(|(pair, (measure, blocked, _))| format!(
                    "{pair} ({}) {measure:?}{}",
                    path_class(pair).name(),
                    if *blocked { " (not retried yet)" } else { "" }
                ))
                .collect::<Vec<_>>()
                .join(", ")
        );
        // A load test of an alternative: still running, finished, or about a
        // path that is no longer an alternative.
        let finished = {
            let mut choice = self.path_choice.lock().unwrap_or_else(|e| e.into_inner());
            match choice.load.take() {
                Some(test) => match alternatives.iter().position(|pair| test.of(pair)) {
                    None => {
                        self.end_load_test();
                        None
                    }
                    Some(_) if now < test.until => {
                        choice.load = Some(test);
                        return;
                    }
                    Some(index) => Some((test, index)),
                },
                None => None,
            }
        };
        if let Some((test, index)) = finished {
            let alternative = &alternatives[index];
            let (probed, _, was_left) = measured[index];
            let received = self.end_load_test();
            let loss = 1.0 - (f64::from(received) / f64::from(test.total.max(1))).min(1.0);
            let Some(probed) = probed else {
                return;
            };
            let loaded = Measure {
                loss: Some(loss),
                media: true,
                ..probed
            };
            let preferred = path_class(alternative) == PathClass::Direct;
            let verdict = current.and_then(|current| better(&current, &loaded, preferred));
            match verdict {
                Some(reason) => {
                    let reason = format!(
                        "{reason}; load test at {} kbps lost {:.1}%",
                        test.rate_kbps,
                        loss * 100.0
                    );
                    self.switch_path(&selected, alternative, current, reason, false, was_left, now)
                        .await;
                }
                None if loaded.lossy_under_load() => {
                    let span = alternative
                        .quality
                        .lock()
                        .expect("path quality poisoned")
                        .left(now);
                    log::info!(
                        target: "webrtc_ice::agent::path",
                        "path load test of {alternative}: {received}/{} packets at {} kbps ({:.1}% lost); staying on {selected}, not retrying for {}s",
                        test.total,
                        test.rate_kbps,
                        loss * 100.0,
                        span.as_secs()
                    );
                }
                None => log::info!(
                    target: "webrtc_ice::agent::path",
                    "path load test of {alternative}: {:.1}% lost at {} kbps; staying on {selected}",
                    loss * 100.0,
                    test.rate_kbps
                ),
            }
            return;
        }
        let decision = {
            let mut choice = self.path_choice.lock().unwrap_or_else(|e| e.into_inner());
            // A trial of a retried path falls back to where it came from,
            // even if that path is itself waiting out a retry.
            let fallback: Vec<bool> = alternatives
                .iter()
                .map(|pair| choice.trial_from(pair, now))
                .collect();
            let eligible: Vec<(Option<Measure>, bool)> = alternatives
                .iter()
                .zip(&measured)
                .zip(&fallback)
                .map(|((pair, (measure, blocked, _)), fallback)| {
                    (
                        measure.filter(|_| *fallback || !*blocked),
                        path_class(pair) == PathClass::Direct,
                    )
                })
                .collect();
            let picked = current.and_then(|current| {
                // Falling back from a failed trial comes first.
                fallback
                    .iter()
                    .position(|f| *f)
                    .and_then(|i| {
                        let (measure, preferred) = eligible[i];
                        measure
                            .and_then(|m| better(&current, &m, preferred))
                            .map(|reason| (i, reason))
                    })
                    .or_else(|| cheapest_better(&current, &eligible))
            });
            match picked {
                None => {
                    choice.hold = 0;
                    None
                }
                // Falling back from a failed trial needs no second opinion.
                Some((index, _)) if !fallback[index] && !choice.holds(&alternatives[index]) => None,
                Some((index, reason)) => Some((index, reason, fallback[index])),
            }
        };
        let Some((index, reason, fallback)) = decision else {
            return;
        };
        let alternative = &alternatives[index];
        let was_left = measured[index].2;
        // Light checks cannot tell whether a path a carrier throttles under
        // load has recovered: load-test a direct path (and a UDP relay the
        // session left for its loss) before moving onto it.
        let class = path_class(alternative);
        let udp = alternative.local_candidate().network_type().is_udp()
            && alternative.remote.network_type().is_udp()
            && class != PathClass::StreamRelay;
        if !fallback && udp && (class == PathClass::Direct || was_left) {
            let session_kbps = selected
                .quality
                .lock()
                .expect("path quality poisoned")
                .media_kbps(now);
            self.start_load_test(alternative, session_kbps, now).await;
            return;
        }
        self.switch_path(&selected, alternative, current, reason, fallback, was_left, now)
            .await;
    }

    /// Ask the peer for a load test over `alternative` (`load_test`), at half
    /// the session's bitrate.
    async fn start_load_test(&self, alternative: &Arc<CandidatePair>, session_kbps: Option<u32>, now: u64) {
        let tag = {
            let credentials = self.ufrag_pwd.lock().await;
            super::punch::Probe::derive(
                &credentials.local_ufrag,
                &credentials.local_pwd,
                &credentials.remote_ufrag,
                &credentials.remote_pwd,
            )
            .map(|probe| probe.tag())
        };
        let (Some(tag), Some(conn)) = (tag, alternative.local_port.get_conn().cloned()) else {
            return;
        };
        let request = super::load_test::Request {
            id: rand::random::<u32>() | 1,
            rate_kbps: (session_kbps.unwrap_or(0) / 2)
                .clamp(super::load_test::MIN_KBPS, super::load_test::MAX_KBPS),
            duration_ms: super::load_test::DURATION.as_millis() as u32,
            size: super::load_test::SIZE,
        };
        *self.load_receiving.lock().unwrap_or_else(|e| e.into_inner()) = Some((tag, request.id));
        self.load_received.store(0, Ordering::Relaxed);
        {
            let mut choice = self.path_choice.lock().unwrap_or_else(|e| e.into_inner());
            choice.load = Some(LoadTest {
                pair: Arc::downgrade(alternative),
                // Room for the request and the last packets to arrive.
                until: now + (super::load_test::DURATION + Duration::from_millis(1500)).as_nanos() as u64,
                total: request.total(),
                rate_kbps: request.rate_kbps,
            });
            choice.hold = 0;
        }
        log::info!(
            target: "webrtc_ice::agent::path",
            "path load test of {alternative}: {} kbps for {} ms",
            request.rate_kbps,
            request.duration_ms
        );
        let packet = request.encode(&tag);
        // The peer ignores repeats of the same request.
        for _ in 0..3 {
            let _ = conn.send_to(&packet, alternative.remote.addr()).await;
        }
    }

    /// Stop counting a load test; returns the packets that arrived.
    fn end_load_test(&self) -> u32 {
        self.load_receiving
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take();
        self.load_received.swap(0, Ordering::Relaxed)
    }

    #[allow(clippy::too_many_arguments)]
    async fn switch_path(
        &self,
        selected: &Arc<CandidatePair>,
        alternative: &Arc<CandidatePair>,
        current: Option<Measure>,
        reason: String,
        fallback: bool,
        was_left: bool,
        now: u64,
    ) {
        let relayed = path_class(alternative) != PathClass::Direct;
        let retry = {
            let mut choice = self.path_choice.lock().unwrap_or_else(|e| e.into_inner());
            choice.trial = if !fallback && was_left {
                Some((Arc::downgrade(selected), now + TRIAL.as_nanos() as u64))
            } else {
                None
            };
            choice.pinned = relayed.then(|| Arc::downgrade(alternative));
            choice.hold = 0;
            choice.trial.is_some()
        };
        let left_for = current
            .filter(|current| current.lossy_under_load())
            .map(|_| {
                selected
                    .quality
                    .lock()
                    .expect("path quality poisoned")
                    .left(now)
            });
        log::info!(
            target: "webrtc_ice::agent::path",
            "path switch: {reason}; {selected} ({}) -> {alternative} ({}){}{}",
            path_class(selected).name(),
            path_class(alternative).name(),
            if fallback { " (trial failed)" } else if retry { " (retry)" } else { "" },
            left_for.map_or_else(String::new, |span| format!(
                "; not retrying the old path for {}s",
                span.as_secs()
            ))
        );
        self.set_selected_pair(Some(Arc::clone(alternative))).await;
        // Nominate it now rather than at its next consent check.
        self.ping_candidate(&alternative.local_port, &alternative.remote)
            .await;
    }

    pub(super) async fn ice_on_nonselected_payload(&self, pair: &Arc<CandidatePair>) {
        // This proposes the Connection with reason 6 (data received),
        // without an extra direct ping; ordinary controller checks still
        // own connectivity probing.
        let selected = self.agent_conn.get_selected_pair();
        if self.ice_should_switch_connection(pair, selected.as_ref()) {
            log::info!("selected connection after nonselected payload: {pair}");
            self.set_selected_pair(Some(pair.clone())).await;
        }
    }

    async fn run_controller(&self) {
        use crate::agent::agent_config::{ICE_STRONG_PING_INTERVAL, ICE_WEAK_PING_INTERVAL};
        let mut pairs = self.agent_conn.checklist.lock().await.clone();
        for pair in &pairs {
            self.refresh_pair_state(pair);
        }
        let dead = pairs
            .iter()
            .filter(|p| self.ice_pair_is_dead(p))
            .cloned()
            .collect::<Vec<_>>();
        if !dead.is_empty() {
            for pair in &dead {
                log::debug!("destroying dead Connection: {pair}");
            }
            self.agent_conn
                .checklist
                .lock()
                .await
                .retain(|p| !dead.iter().any(|d| Arc::ptr_eq(p, d)));
            pairs.retain(|p| !dead.iter().any(|d| Arc::ptr_eq(p, d)));
            self.note_connections_removed(&dead).await;
            if self
                .agent_conn
                .get_selected_pair()
                .as_ref()
                .is_some_and(|p| dead.iter().any(|d| Arc::ptr_eq(p, d)))
            {
                self.set_selected_pair(None).await;
            }
        }
        let now = CandidatePair::now_nanos();
        let selected = self.agent_conn.get_selected_pair();
        let media = *self.media_counts.lock().unwrap_or_else(|e| e.into_inner());
        for pair in &pairs {
            let carries = selected.as_ref().is_some_and(|s| Arc::ptr_eq(s, pair));
            pair.quality.lock().expect("path quality poisoned").sample(
                now,
                pair.requests_sent.load(Ordering::Relaxed),
                pair.responses_received.load(Ordering::Relaxed),
                pair.smoothed_round_trip_ms.load(Ordering::Relaxed),
                media.filter(|_| carries),
            );
        }
        // SortConnectionsAndUpdateState switches before pruning.
        pairs.sort_by(|a, b| self.ice_pair_sort_cmp(b, a));
        let selected = self.agent_conn.get_selected_pair();
        if let Some(best) = pairs.first().filter(|pair| self.ice_ready_to_send(pair)) {
            if self.ice_should_switch_connection(best, selected.as_ref()) {
                log::info!("selected connection after sort: {best}");
                self.set_selected_pair(Some(Arc::clone(best))).await;
            }
        }
        if self.is_controlling.load(Ordering::SeqCst) {
            self.choose_path(&pairs, now).await;
        }
        let selected = self.agent_conn.get_selected_pair();
        if let Some(selected) = selected.as_ref().filter(|selected| {
            self.is_controlling.load(Ordering::SeqCst)
                || selected.remote_nomination.load(Ordering::Relaxed) != 0
        }) {
            self.prune_connections(&pairs, selected);
        }
        let active = pairs
            .iter()
            .filter(|p| !p.pruned.load(Ordering::Relaxed) && self.ice_write_state(p) != 3)
            .collect::<Vec<_>>();
        let writable = selected
            .as_ref()
            .is_some_and(|p| self.ice_writable(p) || self.ice_presumed_writable(p));
        if writable {
            self.ever_writable.store(true, Ordering::Relaxed);
        }
        let state = if self.ever_had_pair.load(Ordering::Relaxed) && active.is_empty() {
            ConnectionState::Failed
        } else if !writable && self.ever_writable.load(Ordering::Relaxed) {
            ConnectionState::Disconnected
        } else if !self.ever_had_pair.load(Ordering::Relaxed) && active.is_empty() {
            ConnectionState::New
        } else if writable {
            ConnectionState::Connected
        } else {
            ConnectionState::Checking
        };
        self.update_connection_state(state).await;
        if self.lite.load(Ordering::SeqCst) && !self.is_controlling.load(Ordering::SeqCst) {
            return;
        }

        let weak = selected
            .as_ref()
            .is_none_or(|p| !self.ice_writable(p) || !self.ice_receiving(p) || !self.ice_connected(p));
        let needs_initial_pings = active
            .iter()
            .any(|p| p.requests_sent.load(Ordering::Relaxed) < 3);
        let interval = if weak || needs_initial_pings {
            ICE_WEAK_PING_INTERVAL
        } else {
            ICE_STRONG_PING_INTERVAL
        };
        let now = CandidatePair::now_nanos();
        let last_ping = self.last_ping_sent_nanos.load(Ordering::Relaxed);
        if last_ping != 0 && now.saturating_sub(last_ping) < interval.as_nanos() as u64 {
            return;
        }

        let mut networks = std::collections::HashSet::new();
        let completed = !active.is_empty()
            && active
                .iter()
                .all(|p| networks.insert(p.local_port.network_key()));
        let pingable = |pair: &Arc<CandidatePair>| {
            if !self.ice_pingable(pair, selected.as_deref()) {
                return false;
            }
            if !weak && completed && selected.as_ref().is_some_and(|s| !Arc::ptr_eq(s, pair)) {
                pair.responses_received.load(Ordering::Relaxed) == 0
                    || pair.age_since(&pair.last_check_response_nanos)
                        >= crate::agent::agent_config::ICE_BACKUP_CONNECTION_PING_INTERVAL
            } else {
                true
            }
        };
        // FindNextPingableConnection: selected due -> per-Network best in weak
        // state -> oldest triggered check -> fair unpinged round.
        let mut next = selected
            .as_ref()
            .filter(|p| {
                self.ice_writable(p)
                    && self.ice_connected(p)
                    && p.age_since(&p.last_check_sent_nanos)
                        >= self.ice_active_writable_ping_interval(p, weak)
            })
            .cloned();
        if next.is_none() && weak {
            let mut by_network = HashMap::<String, Arc<CandidatePair>>::new();
            if let Some(selected) = selected.as_ref() {
                by_network.insert(selected.local_port.network_key(), Arc::clone(selected));
            }
            for pair in &pairs {
                by_network
                    .entry(pair.local_port.network_key())
                    .or_insert_with(|| Arc::clone(pair));
            }
            next = by_network
                .into_values()
                .filter(|p| {
                    self.ice_writable(p)
                        && self.ice_connected(p)
                        && p.age_since(&p.last_check_sent_nanos)
                            >= self.ice_active_writable_ping_interval(p, weak)
                })
                .min_by_key(|p| p.last_check_sent_nanos.load(Ordering::Relaxed));
        }
        if next.is_none() {
            next = pairs
                .iter()
                .filter(|p| {
                    pingable(p)
                        && !self.ice_writable(p)
                        && p.last_check_received_nanos.load(Ordering::Relaxed)
                            > p.last_check_sent_nanos.load(Ordering::Relaxed)
                })
                .min_by_key(|p| p.last_check_received_nanos.load(Ordering::Relaxed))
                .cloned();
        }
        if next.is_none() {
            if !pairs
                .iter()
                .any(|p| !p.pinged_in_round.load(Ordering::Relaxed) && pingable(p))
            {
                for pair in &pairs {
                    pair.pinged_in_round.store(false, Ordering::Relaxed);
                }
            }
            next = pairs
                .iter()
                .filter(|p| !p.pinged_in_round.load(Ordering::Relaxed) && pingable(p))
                .min_by_key(|p| p.last_check_sent_nanos.load(Ordering::Relaxed))
                .cloned();
        }
        if let Some(pair) = next {
            if CandidatePairState::from(pair.state.load(Ordering::SeqCst))
                == CandidatePairState::Waiting
            {
                pair.state
                    .store(CandidatePairState::InProgress as u8, Ordering::SeqCst);
            }
            self.ping_candidate(&pair.local_port, &pair.remote).await;
        }
    }

    pub(crate) async fn start(&self) {
        if self.is_controlling.load(Ordering::SeqCst) {
            ControllingSelector::start(self).await;
        } else {
            ControlledSelector::start(self).await;
        }
    }

    pub(crate) async fn contact_candidates(&self) {
        if self.is_controlling.load(Ordering::SeqCst) {
            ControllingSelector::contact_candidates(self).await;
        } else {
            ControlledSelector::contact_candidates(self).await;
        }
    }

    pub(crate) async fn ping_candidate(
        &self,
        local: &Arc<dyn Candidate + Send + Sync>,
        remote: &Arc<dyn Candidate + Send + Sync>,
    ) {
        if self.connection_credentials(local, remote).await.is_none() {
            return;
        }
        let previous_ping = self.last_ping_sent_nanos.load(Ordering::Relaxed);
        if let Some(pair) = self.find_pair(local, remote).await {
            pair.note_check_sent();
            self.last_ping_sent_nanos
                .store(CandidatePair::now_nanos(), Ordering::Relaxed);
        }
        self.ping_send_failed.store(false, Ordering::Relaxed);
        if self.is_controlling.load(Ordering::SeqCst) {
            ControllingSelector::ping_candidate(self, local, remote).await;
        } else {
            ControlledSelector::ping_candidate(self, local, remote).await;
        }
        // A request that never left (no route from this interface) does not
        // use the pacing slot: check the next connection now instead of one
        // interval later. Bounded so unreachable networks cannot spin.
        if self.ping_send_failed.load(Ordering::Relaxed) {
            if self.ping_send_failures.fetch_add(1, Ordering::Relaxed) < 4 {
                self.last_ping_sent_nanos
                    .store(previous_ping, Ordering::Relaxed);
                let _ = self.force_candidate_contact_tx.try_send(true);
            }
        } else {
            self.ping_send_failures.store(0, Ordering::Relaxed);
        }
    }

    pub(crate) async fn handle_success_response(
        &self,
        m: &Message,
        local: &Arc<dyn Candidate + Send + Sync>,
        remote: &Arc<dyn Candidate + Send + Sync>,
        remote_addr: SocketAddr,
    ) {
        if self.is_controlling.load(Ordering::SeqCst) {
            ControllingSelector::handle_success_response(self, m, local, remote, remote_addr).await;
        } else {
            ControlledSelector::handle_success_response(self, m, local, remote, remote_addr).await;
        }
    }
}

#[async_trait]
impl ControllingSelector for AgentInternal {
    async fn start(&self) {}

    async fn contact_candidates(&self) {
        self.run_controller().await;
    }

    async fn ping_candidate(
        &self,
        local: &Arc<dyn Candidate + Send + Sync>,
        remote: &Arc<dyn Candidate + Send + Sync>,
    ) {
        let Some(pair) = self.find_pair(local, remote).await else {
            return;
        };
        let local_description = pair.local_candidate();
        let selected = self.agent_conn.get_selected_pair();
        // The default semi-aggressive nomination mode is used.  The
        // Connection's USE-CANDIDATE bit is configured only after it becomes
        // the local selected connection, so initial and backup checks remain
        // ordinary Binding requests.
        let use_candidate = self.is_controlling.load(Ordering::SeqCst)
            && selected
                .as_ref()
                .is_some_and(|selected| Arc::ptr_eq(&pair, selected))
            && pair.nominated.load(Ordering::SeqCst);
        let (msg, result) = {
            let Some(ufrag_pwd) = self.connection_credentials(local, remote).await else {
                return;
            };
            let username = ufrag_pwd.remote_ufrag.clone() + ":" + ufrag_pwd.local_ufrag.as_str();
            let mut msg = Message::new();
            let mut attributes: Vec<Box<dyn Setter>> = vec![
                Box::new(BINDING_REQUEST),
                Box::new(TransactionId::new()),
                Box::new(Username::new(ATTR_USERNAME, username)),
                Box::new(AttrControlling(self.tie_breaker.load(Ordering::SeqCst))),
                Box::new(PriorityAttr(
                    (local_description.priority() & 0x00ff_ffff)
                        | (if local.network_type().is_tcp() {
                            80
                        } else {
                            110
                        } << 24),
                )),
                Box::new(goog_network_info(local)),
                Box::new(goog_generation(local)),
            ];
            if use_candidate {
                attributes.push(Box::<UseCandidateAttr>::default());
            }
            attributes.extend([
                Box::new(MessageIntegrity::new_short_term_integrity(
                    ufrag_pwd.remote_pwd.clone(),
                )) as Box<dyn Setter>,
                Box::new(FINGERPRINT) as Box<dyn Setter>,
            ]);
            let result = msg.build(&attributes);
            (msg, result)
        };

        if let Err(err) = result {
            log::error!("{err}");
        } else {
            self.send_binding_request(&msg, local, remote).await;
        }
    }

    async fn handle_success_response(
        &self,
        m: &Message,
        local: &Arc<dyn Candidate + Send + Sync>,
        remote: &Arc<dyn Candidate + Send + Sync>,
        remote_addr: SocketAddr,
    ) {
        if let Some(pending_request) = self
            .handle_inbound_binding_success(m.transaction_id, &local.id(), remote_addr)
            .await
        {
            let transaction_addr = pending_request.destination;

            // Assert that NAT is not symmetric
            // https://tools.ietf.org/html/rfc8445#section-7.2.5.2.1
            if transaction_addr != remote_addr {
                log::debug!("discard message: transaction source and destination does not match expected({transaction_addr}), actual({remote})");
                return;
            }

            log::trace!("inbound STUN (SuccessResponse) from {remote} to {local}");
            if let Some(p) = self.find_pair(local, remote).await {
                if !p.confirm_send_path(pending_request.timestamp.into_std()) {
                    return;
                }
                p.state
                    .store(CandidatePairState::Succeeded as u8, Ordering::SeqCst);
                p.binding_request_count.store(0, Ordering::SeqCst);
                p.note_check_response(crate::agent::agent_config::ICE_RECEIVING_TIMEOUT);
                let round_trip_micros = pending_request
                    .timestamp
                    .elapsed()
                    .as_micros()
                    .min(u128::from(u64::MAX)) as u64;
                p.record_round_trip(round_trip_micros);
                self.update_local_candidate_from_response(&p, m, pending_request.priority)
                    .await;
                log::trace!(
                    "Found valid candidate pair: {}, p.state: {}, isUseCandidate: {}",
                    p,
                    p.state.load(Ordering::SeqCst),
                    pending_request.is_use_candidate
                );
                // Connection::SignalStateChange posts the
                // candidate-pair-state-changed sort request.  Do not wait for
                // a second nominated round trip before selecting locally.
                self.request_connectivity_check();
            } else {
                // This shouldn't happen
                log::error!("Success response from invalid candidate pair");
            }
        } else {
            log::warn!(
                "discard message from ({}), unknown TransactionID 0x{:?}",
                remote,
                m.transaction_id
            );
        }
    }
}

#[async_trait]
impl ControlledSelector for AgentInternal {
    async fn start(&self) {}

    async fn contact_candidates(&self) {
        self.run_controller().await;
    }

    async fn ping_candidate(
        &self,
        local: &Arc<dyn Candidate + Send + Sync>,
        remote: &Arc<dyn Candidate + Send + Sync>,
    ) {
        let Some(pair) = self.find_pair(local, remote).await else {
            return;
        };
        let local_description = pair.local_candidate();
        let (msg, result) = {
            let Some(ufrag_pwd) = self.connection_credentials(local, remote).await else {
                return;
            };
            let username = ufrag_pwd.remote_ufrag.clone() + ":" + ufrag_pwd.local_ufrag.as_str();
            let mut msg = Message::new();
            let result = msg.build(&[
                Box::new(BINDING_REQUEST),
                Box::new(TransactionId::new()),
                Box::new(Username::new(ATTR_USERNAME, username)),
                Box::new(AttrControlled(self.tie_breaker.load(Ordering::SeqCst))),
                Box::new(PriorityAttr(
                    (local_description.priority() & 0x00ff_ffff)
                        | (if local.network_type().is_tcp() {
                            80
                        } else {
                            110
                        } << 24),
                )),
                Box::new(goog_network_info(local)),
                Box::new(goog_generation(local)),
                Box::new(MessageIntegrity::new_short_term_integrity(
                    ufrag_pwd.remote_pwd.clone(),
                )),
                Box::new(FINGERPRINT),
            ]);
            (msg, result)
        };

        if let Err(err) = result {
            log::error!("{err}");
        } else {
            self.send_binding_request(&msg, local, remote).await;
        }
    }

    async fn handle_success_response(
        &self,
        m: &Message,
        local: &Arc<dyn Candidate + Send + Sync>,
        remote: &Arc<dyn Candidate + Send + Sync>,
        remote_addr: SocketAddr,
    ) {
        // https://tools.ietf.org/html/rfc8445#section-7.3.1.5
        // If the controlled agent does not accept the request from the
        // controlling agent, the controlled agent MUST reject the nomination
        // request with an appropriate error code response (e.g., 400)
        // [RFC5389].

        if let Some(pending_request) = self
            .handle_inbound_binding_success(m.transaction_id, &local.id(), remote_addr)
            .await
        {
            let transaction_addr = pending_request.destination;

            // Assert that NAT is not symmetric
            // https://tools.ietf.org/html/rfc8445#section-7.2.5.2.1
            if transaction_addr != remote_addr {
                log::debug!("discard message: transaction source and destination does not match expected({transaction_addr}), actual({remote})");
                return;
            }

            log::trace!("inbound STUN (SuccessResponse) from {remote} to {local}");

            if let Some(p) = self.find_pair(local, remote).await {
                if !p.confirm_send_path(pending_request.timestamp.into_std()) {
                    return;
                }
                p.state
                    .store(CandidatePairState::Succeeded as u8, Ordering::SeqCst);
                p.binding_request_count.store(0, Ordering::SeqCst);
                p.note_check_response(crate::agent::agent_config::ICE_RECEIVING_TIMEOUT);
                let round_trip_micros = pending_request
                    .timestamp
                    .elapsed()
                    .as_micros()
                    .min(u128::from(u64::MAX)) as u64;
                p.record_round_trip(round_trip_micros);
                self.update_local_candidate_from_response(&p, m, pending_request.priority)
                    .await;
                log::trace!("Found valid candidate pair: {p}");

                self.request_connectivity_check();
            } else {
                // This shouldn't happen
                log::error!("Success response from invalid candidate pair");
            }
        } else {
            log::warn!(
                "discard message from ({}), unknown TransactionID 0x{:?}",
                remote,
                m.transaction_id
            );
        }
    }
}
