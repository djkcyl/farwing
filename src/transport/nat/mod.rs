//! NAT traversal aids around the ICE agent (`docs/protocol/v2-design.md` §11):
//! RFC 5780 behaviour discovery, the `a=x-farwing-nat` SDP attribute, port
//! prediction for address(-and-port)-dependent mappings, and gateway port
//! mappings (PCP / NAT-PMP / UPnP-IGD) advertised as extra srflx candidates.
//!
//! None of this replaces ICE: every candidate goes through the normal
//! `add_ice_candidate` / local candidate paths, and a session never waits for
//! discovery longer than [`LOCAL_INFO_BUDGET`].

mod candidates;
pub(crate) mod discovery;
mod portmap;
pub(crate) mod route;
pub(crate) mod settings;
mod stun;

use std::sync::{Arc, Mutex};
use std::time::Duration;
use webrtc::ice_transport::ice_candidate::RTCIceCandidateInit;

pub(crate) use settings::Settings;

/// Session-level SDP attribute carrying this side's NAT behaviour.
pub(crate) const ATTRIBUTE: &str = "x-farwing-nat";
/// The longest a new session waits for a discovery result.
pub(crate) const LOCAL_INFO_BUDGET: Duration = Duration::from_millis(300);
/// Predicted remote ports per remote srflx candidate (k = 1..=8).
const PREDICTED_PORTS: u16 = 8;

/// RFC 4787 / RFC 5780 behaviour class, used for both mapping and filtering.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub(crate) enum Behavior {
    #[default]
    Unknown,
    /// Endpoint-independent.
    EndpointIndependent,
    /// Address-dependent.
    AddressDependent,
    /// Address-and-port-dependent.
    AddressAndPortDependent,
}

impl Behavior {
    pub(crate) fn token(self) -> &'static str {
        match self {
            Self::Unknown => "unknown",
            Self::EndpointIndependent => "eim",
            Self::AddressDependent => "adm",
            Self::AddressAndPortDependent => "apdm",
        }
    }

    fn parse(token: &str) -> Self {
        match token.trim().to_ascii_lowercase().as_str() {
            "eim" => Self::EndpointIndependent,
            "adm" => Self::AddressDependent,
            "apdm" => Self::AddressAndPortDependent,
            _ => Self::Unknown,
        }
    }

    fn dependent(self) -> bool {
        matches!(self, Self::AddressDependent | Self::AddressAndPortDependent)
    }
}

/// One side's NAT behaviour as discovered over IPv4.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub(crate) struct NatInfo {
    pub mapping: Behavior,
    pub filtering: Behavior,
    /// Step between consecutive new mappings of a dependent mapping; 0 when
    /// unknown, inconsistent or not applicable.
    pub port_delta: i32,
}

impl NatInfo {
    /// The attribute value: `mapping=<..>;filtering=<..>;delta=<i32>`.
    pub(crate) fn encode(&self) -> String {
        format!(
            "mapping={};filtering={};delta={}",
            self.mapping.token(),
            self.filtering.token(),
            self.port_delta
        )
    }

    pub(crate) fn decode(value: &str) -> Self {
        let mut info = Self::default();
        for field in value.split(';') {
            let Some((key, value)) = field.split_once('=') else {
                continue;
            };
            match key.trim() {
                "mapping" => info.mapping = Behavior::parse(value),
                "filtering" => info.filtering = Behavior::parse(value),
                "delta" => info.port_delta = value.trim().parse().unwrap_or(0),
                _ => {}
            }
        }
        if !info.mapping.dependent() {
            info.port_delta = 0;
        }
        info
    }

    /// The mapping as the ICE agent's probe punching plans with it.
    fn punch_mapping(&self) -> webrtc::ice::agent::NatMapping {
        use webrtc::ice::agent::NatMapping;
        match self.mapping {
            Behavior::Unknown => NatMapping::Unknown,
            Behavior::EndpointIndependent => NatMapping::Cone,
            _ if self.port_delta != 0 => {
                NatMapping::Linear(self.port_delta.clamp(-64, 64) as i16)
            }
            _ => NatMapping::Random,
        }
    }

    /// The side accepts a peer's new mapping without predicting it: ICE
    /// learns the peer's port as a peer-reflexive candidate.
    fn admits_new_peer_ports(&self) -> bool {
        self.mapping == Behavior::EndpointIndependent
            && matches!(
                self.filtering,
                Behavior::EndpointIndependent | Behavior::AddressDependent
            )
    }
}

fn attribute_value(sdp: &str) -> Option<&str> {
    let prefix = format!("a={ATTRIBUTE}:");
    sdp.lines()
        .find_map(|line| line.trim_end().strip_prefix(prefix.as_str()))
}

/// Whether the attribute's sender takes probe punching (`punch=off` when it
/// turned aggressive punching off).
fn takes_probes(value: &str) -> bool {
    !value
        .split(';')
        .any(|field| field.trim().eq_ignore_ascii_case("punch=off"))
}

/// Whether predicting the remote's ports can help: the remote's mapping is
/// dependent with a consistent step, and this side would not already admit
/// its new ports. Never true for EIM/EIM.
pub(crate) fn should_predict(local: &NatInfo, remote: &NatInfo) -> bool {
    remote.mapping.dependent() && remote.port_delta != 0 && !local.admits_new_peer_ports()
}

/// Insert a session-level attribute line before the first media section,
/// replacing an earlier copy.
pub(crate) fn insert_attribute(sdp: &mut String, line: &str) {
    let newline = if sdp.contains("\r\n") { "\r\n" } else { "\n" };
    let prefix = format!("a={ATTRIBUTE}:");
    if sdp.contains(&prefix) {
        *sdp = sdp
            .split_inclusive('\n')
            .filter(|existing| !existing.starts_with(&prefix))
            .collect();
    }
    let block = format!("{line}{newline}");
    match sdp.find(&format!("{newline}m=")) {
        Some(index) => sdp.insert_str(index + newline.len(), &block),
        None => {
            if !sdp.is_empty() && !sdp.ends_with(newline) {
                sdp.push_str(newline);
            }
            sdp.push_str(&block);
        }
    }
}

/// Publishes a local candidate on the role's signaling path.
pub(crate) type Emit = Arc<dyn Fn(RTCIceCandidateInit) + Send + Sync>;

/// NAT traversal state of one peer connection, shared by both roles.
pub(crate) struct Traversal {
    settings: Settings,
    local: Mutex<Option<NatInfo>>,
    remote: Mutex<Option<NatInfo>>,
    /// The peer takes probe punching (its attribute has no `punch=off`).
    remote_probes: std::sync::atomic::AtomicBool,
    mappings: portmap::Session,
    summarized: std::sync::atomic::AtomicBool,
}

impl Traversal {
    /// Snapshot settings and this network's NAT behaviour; waits at most
    /// [`LOCAL_INFO_BUDGET`] for a discovery that is still running.
    pub(crate) async fn new() -> Arc<Self> {
        let settings = settings::load();
        let local = discovery::current(LOCAL_INFO_BUDGET).await;
        tracing::debug!(?settings, ?local, "NAT traversal state for new peer");
        Arc::new(Self {
            settings,
            local: Mutex::new(local),
            remote: Mutex::new(None),
            remote_probes: std::sync::atomic::AtomicBool::new(true),
            mappings: portmap::Session::new(settings.port_mapping),
            summarized: std::sync::atomic::AtomicBool::new(false),
        })
    }

    fn local(&self) -> Option<NatInfo> {
        *self.local.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn remote(&self) -> Option<NatInfo> {
        *self.remote.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// The attribute line for a local offer or answer. A newer cached result
    /// (e.g. after a network change before an ICE restart) replaces the
    /// snapshot. With aggressive punching off, `delta=0;punch=off` asks the
    /// peer neither to predict this side's ports nor to probe them.
    pub(crate) fn attribute(&self) -> String {
        let mut info = {
            let mut local = self.local.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(fresh) = discovery::cached() {
                *local = Some(fresh);
            }
            local.unwrap_or_default()
        };
        if !self.settings.aggressive_punching {
            info.port_delta = 0;
            return format!("a={ATTRIBUTE}:{};punch=off", info.encode());
        }
        format!("a={ATTRIBUTE}:{}", info.encode())
    }

    /// Record the peer's attribute from its offer or answer.
    pub(crate) fn remote_description(&self, sdp: &str) {
        let Some(value) = attribute_value(sdp) else {
            return;
        };
        let info = NatInfo::decode(value);
        let probes = takes_probes(value);
        let local = self.local().unwrap_or_default();
        tracing::info!(
            local = %local.encode(),
            remote = %info.encode(),
            predict = self.settings.aggressive_punching && should_predict(&local, &info),
            peer_takes_probes = probes,
            "peer NAT behaviour"
        );
        *self.remote.lock().unwrap_or_else(|e| e.into_inner()) = Some(info);
        self.remote_probes
            .store(probes, std::sync::atomic::Ordering::Release);
    }

    /// Both sides' mappings for the ICE agent's probe punching, and whether
    /// the peer takes probes. The agent only probes with aggressive
    /// punching on (`rtc::core`).
    pub(crate) fn punch_plan(
        &self,
    ) -> (
        webrtc::ice::agent::NatMapping,
        webrtc::ice::agent::NatMapping,
        bool,
    ) {
        (
            self.local().unwrap_or_default().punch_mapping(),
            self.remote().unwrap_or_default().punch_mapping(),
            self.remote_probes.load(std::sync::atomic::Ordering::Acquire),
        )
    }

    /// Predicted candidates to add after a real remote candidate. Empty
    /// unless aggressive punching is on and both sides' info says it helps.
    pub(crate) fn predictions(&self, candidate: &RTCIceCandidateInit) -> Vec<RTCIceCandidateInit> {
        if !self.settings.aggressive_punching {
            return Vec::new();
        }
        let Some(remote) = self.remote() else {
            return Vec::new();
        };
        if !should_predict(&self.local().unwrap_or_default(), &remote) {
            return Vec::new();
        }
        candidates::predicted(candidate, remote.port_delta, PREDICTED_PORTS)
    }

    /// Predictions for the `a=candidate` lines embedded in a remote SDP.
    pub(crate) fn sdp_predictions(&self, sdp: &str) -> Vec<RTCIceCandidateInit> {
        sdp.lines()
            .filter_map(|line| line.trim_end().strip_prefix("a="))
            .filter(|line| line.starts_with("candidate:"))
            .flat_map(|line| {
                self.predictions(&RTCIceCandidateInit {
                    candidate: line.to_owned(),
                    ..Default::default()
                })
            })
            .collect()
    }

    /// Observe a local candidate the role is about to publish: a host UDP
    /// candidate on the default route gets a gateway mapping, published as
    /// an extra srflx candidate through `emit`.
    pub(crate) fn local_candidate(&self, candidate: &RTCIceCandidateInit, emit: &Emit) {
        self.mappings.observe(candidate, emit);
    }

    /// Log once how this connection went: both sides' NAT behaviour,
    /// whether ports were predicted, the gateway mapping and the path the
    /// connection ended on (`route`: `LAN`, `P2P`, `relay`…; empty when
    /// unknown). Diagnostic bundles carry it with the logs.
    pub(crate) fn summarize(&self, route: &str, pair: &str) {
        if self.summarized.swap(true, std::sync::atomic::Ordering::AcqRel) {
            return;
        }
        let local = self.local().unwrap_or_default();
        let remote = self.remote();
        tracing::info!(
            route = if route.is_empty() { "-" } else { route },
            pair = if pair.is_empty() { "-" } else { pair },
            local_nat = %local.encode(),
            remote_nat = %remote.map_or_else(|| "-".to_owned(), |r| r.encode()),
            aggressive_punching = self.settings.aggressive_punching,
            predicted = self.settings.aggressive_punching
                && remote.is_some_and(|r| should_predict(&local, &r)),
            probing = self.settings.aggressive_punching
                && self.remote_probes.load(std::sync::atomic::Ordering::Acquire),
            gateway_mapping = %self.mappings.outcome(),
            "connection path summary"
        );
    }

    /// Delete gateway mappings (bounded wait).
    pub(crate) async fn close(&self) {
        self.mappings.close().await;
    }

    /// Stop refreshing; the mapping tasks delete their mappings on their own.
    pub(crate) fn close_detached(&self) {
        self.mappings.stop();
    }
}

/// Start background discovery (idempotent): at presence start-up and on
/// network change.
pub(crate) fn start() {
    discovery::start();
}

/// The vendored ICE agent's choice between direct and relay paths, tested
/// here: the vendored crate's own tests do not run in this workspace.
#[cfg(test)]
#[allow(dead_code)]
#[path = "../../../vendor/webrtc-ice/src/candidate/path_quality.rs"]
mod path_quality;

#[cfg(test)]
mod tests {
    use super::*;
    use Behavior::*;

    fn info(mapping: Behavior, filtering: Behavior, port_delta: i32) -> NatInfo {
        NatInfo {
            mapping,
            filtering,
            port_delta,
        }
    }

    #[test]
    fn attribute_round_trip() {
        let value = info(AddressAndPortDependent, AddressDependent, -2);
        assert_eq!(value.encode(), "mapping=apdm;filtering=adm;delta=-2");
        assert_eq!(NatInfo::decode(&value.encode()), value);
        // Unknown tokens and a delta without a dependent mapping are dropped.
        assert_eq!(
            NatInfo::decode("mapping=eim;filtering=weird;delta=4;x=1"),
            info(EndpointIndependent, Unknown, 0)
        );
        let sdp = "v=0\r\na=x-farwing-nat:mapping=adm;filtering=apdm;delta=1\r\nm=audio 9\r\n";
        assert_eq!(
            attribute_value(sdp).map(NatInfo::decode),
            Some(info(AddressDependent, AddressAndPortDependent, 1))
        );
        assert_eq!(attribute_value("v=0\r\n"), None);
    }

    #[test]
    fn attribute_is_session_level_and_replaced() {
        let mut sdp = "v=0\r\ns=-\r\nm=audio 9\r\na=mid:0\r\n".to_owned();
        insert_attribute(
            &mut sdp,
            "a=x-farwing-nat:mapping=eim;filtering=eim;delta=0",
        );
        insert_attribute(
            &mut sdp,
            "a=x-farwing-nat:mapping=apdm;filtering=apdm;delta=1",
        );
        assert_eq!(
            sdp,
            "v=0\r\ns=-\r\na=x-farwing-nat:mapping=apdm;filtering=apdm;delta=1\r\nm=audio 9\r\na=mid:0\r\n"
        );
    }

    #[test]
    fn punch_opt_out_and_mappings() {
        use webrtc::ice::agent::NatMapping;
        let sdp = "v=0\r\na=x-farwing-nat:mapping=apdm;filtering=apdm;delta=0;punch=off\r\n";
        let value = attribute_value(sdp).unwrap();
        assert!(!takes_probes(value));
        // The opt-out does not disturb the behaviour fields.
        assert_eq!(
            NatInfo::decode(value),
            info(AddressAndPortDependent, AddressAndPortDependent, 0)
        );
        assert!(takes_probes("mapping=eim;filtering=eim;delta=0"));
        assert_eq!(NatInfo::default().punch_mapping(), NatMapping::Unknown);
        assert_eq!(
            info(EndpointIndependent, AddressAndPortDependent, 0).punch_mapping(),
            NatMapping::Cone
        );
        assert_eq!(
            info(AddressAndPortDependent, AddressAndPortDependent, -2).punch_mapping(),
            NatMapping::Linear(-2)
        );
        assert_eq!(
            info(AddressDependent, AddressDependent, 0).punch_mapping(),
            NatMapping::Random
        );
    }

    #[test]
    fn prediction_rule() {
        let open = info(EndpointIndependent, EndpointIndependent, 0);
        let cone_restricted = info(EndpointIndependent, AddressAndPortDependent, 0);
        let symmetric = info(AddressAndPortDependent, AddressAndPortDependent, 1);
        let random = info(AddressAndPortDependent, AddressAndPortDependent, 0);
        // Never for EIM/EIM.
        assert!(!should_predict(&open, &open));
        assert!(!should_predict(&cone_restricted, &cone_restricted));
        // A symmetric peer with a consistent step behind a port-restricted
        // or symmetric local NAT.
        assert!(should_predict(&cone_restricted, &symmetric));
        assert!(should_predict(&symmetric, &symmetric));
        assert!(should_predict(&NatInfo::default(), &symmetric));
        // This side admits the peer's new port as peer-reflexive already.
        assert!(!should_predict(&open, &symmetric));
        // No consistent step, or nothing known about the peer.
        assert!(!should_predict(&cone_restricted, &random));
        assert!(!should_predict(&cone_restricted, &NatInfo::default()));
    }
}
