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

impl AgentInternal {
    pub(crate) async fn ice_controller_check_interval(&self) -> Duration {
        use crate::agent::agent_config::{
            ICE_RECEIVING_TIMEOUT, ICE_STRONG_PING_INTERVAL, ICE_WEAK_PING_INTERVAL,
        };
        let _serial = self.controller_serial.lock().await;
        let pairs = self.agent_conn.checklist.lock().await;
        for pair in pairs.iter() {
            self.refresh_uu_pair_state(pair);
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

    fn refresh_uu_pair_state(&self, pair: &CandidatePair) {
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

    fn prune_uu_connections(&self, pairs: &[Arc<CandidatePair>], selected: &Arc<CandidatePair>) {
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
        // for its first ten seconds, then keeps the first active relay and the
        // first active relay pair whose two candidate protocols are TLS.  The
        // retained entries are global to this pruning pass, not per Network.
        let now = CandidatePair::now_nanos();
        let relay_grace = Duration::from_secs(10).as_nanos() as u64;
        let mut kept_active_relay = false;
        let mut kept_active_tls_relay = false;

        for pair in &ordered {
            if pair.pruned.load(Ordering::Relaxed) {
                continue;
            }

            let involves_relay = pair.local_candidate().candidate_type() == CandidateType::Relay
                || pair.remote.candidate_type() == CandidateType::Relay;
            if involves_relay {
                let created = pair.created_at_nanos.load(Ordering::Relaxed);
                if now.saturating_sub(created) < relay_grace {
                    continue;
                }

                let active = pair.write_state.load(Ordering::Relaxed) != 3;
                if active && !kept_active_relay {
                    kept_active_relay = true;
                    continue;
                }
                let both_tls = pair
                    .local_candidate()
                    .relay_protocol()
                    .eq_ignore_ascii_case("tls")
                    && pair.remote.relay_protocol().eq_ignore_ascii_case("tls");
                if active && both_tls && !kept_active_tls_relay {
                    kept_active_tls_relay = true;
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
        // A path left for being degraded stays out while the current one works.
        if self.is_controlling.load(Ordering::SeqCst)
            && !Arc::ptr_eq(current, proposed)
            && proposed
                .quality
                .lock()
                .expect("path quality poisoned")
                .avoided(CandidatePair::now_nanos())
            && self.ice_writable(current)
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

    /// The selected direct path keeps passing consent checks but loses many
    /// of them, or is far slower than the relay: move to a healthy relay
    /// backup and avoid the direct path for a while (`PathQuality`).
    async fn leave_degraded_path(&self, pairs: &[Arc<CandidatePair>], now: u64) {
        let relayed = |pair: &CandidatePair| {
            pair.local_candidate().candidate_type() == CandidateType::Relay
                || pair.remote.candidate_type() == CandidateType::Relay
        };
        let Some(selected) = self.agent_conn.get_selected_pair() else {
            return;
        };
        if relayed(&selected) {
            return;
        }
        // Backup relays are pinged rarely and carry no data, so "receiving"
        // says nothing; a writable relay with its last check answered is
        // healthy.
        let Some(backup) = pairs
            .iter()
            .filter(|pair| {
                relayed(pair)
                    && !pair.pruned.load(Ordering::Relaxed)
                    && self.ice_writable(pair)
                    && self.ice_ready_to_send(pair)
                    && pair.consecutive_unanswered_checks.load(Ordering::Relaxed) == 0
            })
            .min_by_key(|pair| pair.smoothed_round_trip_ms.load(Ordering::Relaxed))
        else {
            return;
        };
        let outstanding = selected.outstanding_ping_times().len() as u64;
        let relay_rtt = backup.smoothed_round_trip_ms.load(Ordering::Relaxed);
        let verdict = {
            let mut quality = selected.quality.lock().expect("path quality poisoned");
            let reason = quality.degraded(now, outstanding, relay_rtt);
            reason.map(|reason| (reason, quality.avoid(now)))
        };
        let Some((reason, avoid)) = verdict else {
            return;
        };
        log::info!(
            "selected path degraded ({reason}); using relay {backup} and avoiding {selected} for {}s",
            avoid.as_secs()
        );
        self.set_selected_pair(Some(Arc::clone(backup))).await;
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

    async fn run_uu_controller(&self) {
        use crate::agent::agent_config::{ICE_STRONG_PING_INTERVAL, ICE_WEAK_PING_INTERVAL};
        let mut pairs = self.agent_conn.checklist.lock().await.clone();
        for pair in &pairs {
            self.refresh_uu_pair_state(pair);
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
        for pair in &pairs {
            pair.quality.lock().expect("path quality poisoned").sample(
                now,
                pair.requests_sent.load(Ordering::Relaxed),
                pair.responses_received.load(Ordering::Relaxed),
                pair.smoothed_round_trip_ms.load(Ordering::Relaxed),
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
            self.leave_degraded_path(&pairs, now).await;
        }
        let selected = self.agent_conn.get_selected_pair();
        if let Some(selected) = selected.as_ref().filter(|selected| {
            self.is_controlling.load(Ordering::SeqCst)
                || selected.remote_nomination.load(Ordering::Relaxed) != 0
        }) {
            self.prune_uu_connections(&pairs, selected);
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
        if let Some(pair) = self.find_pair(local, remote).await {
            pair.note_check_sent();
            self.last_ping_sent_nanos
                .store(CandidatePair::now_nanos(), Ordering::Relaxed);
        }
        if self.is_controlling.load(Ordering::SeqCst) {
            ControllingSelector::ping_candidate(self, local, remote).await;
        } else {
            ControlledSelector::ping_candidate(self, local, remote).await;
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
        self.run_uu_controller().await;
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
        self.run_uu_controller().await;
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
