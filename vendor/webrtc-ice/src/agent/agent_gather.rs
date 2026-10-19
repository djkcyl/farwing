//! Immutable input to one allocator session. Network polling, phased allocation
//! and socket lifetimes live in allocator.rs, not nested blocking gather passes.
use super::network_catalog::NetworkCatalog;
use super::*;
use crate::udp_network::UDPNetwork;

pub(crate) struct GatherCandidatesInternalParams {
    pub(super) owner: Arc<super::gather_owner::GatherOwner>,
    pub(super) udp_network: UDPNetwork,
    pub(super) candidate_types: Vec<CandidateType>,
    pub(super) urls: Vec<Url>,
    pub(super) network_types: Vec<NetworkType>,
    pub(super) mdns_mode: MulticastDnsMode,
    pub(super) mdns_name: String,
    pub(super) net: Arc<Net>,
    pub(super) interface_filter: Arc<Option<InterfaceFilterFn>>,
    pub(super) ip_filter: Arc<Option<IpFilterFn>>,
    pub(super) ext_ip_mapper: Arc<Option<ExternalIpMapper>>,
    pub(super) agent_internal: Arc<AgentInternal>,
    pub(super) gathering_state: Arc<AtomicU8>,
    pub(super) chan_candidate_tx: ChanCandidateTx,
    pub(super) include_loopback: bool,
    pub(super) continual_gathering: bool,
    pub(super) network_catalog: Arc<Mutex<NetworkCatalog>>,
}
