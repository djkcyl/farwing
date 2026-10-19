use portable_atomic::{AtomicU16, AtomicU32, AtomicU8};
use std::sync::Arc;

use util::sync::Mutex as SyncMutex;

use super::candidate_base::*;
use super::*;
use crate::error::*;
use crate::rand::generate_cand_id;
use crate::util::*;

/// The config required to create a new `CandidateRelay`.
#[derive(Default)]
pub struct CandidateRelayConfig {
    pub base_config: CandidateBaseConfig,

    pub rel_addr: String,
    pub rel_port: u16,
    pub relay_client: Option<Arc<turn::client::Client>>,
}

impl CandidateRelayConfig {
    /// Creates a new relay candidate.
    pub fn new_candidate_relay(self) -> Result<CandidateBase> {
        let mut candidate_id = self.base_config.candidate_id;
        if candidate_id.is_empty() {
            candidate_id = generate_cand_id();
        }

        let ip: IpAddr = match self.base_config.address.parse() {
            Ok(ip) => ip,
            Err(_) => return Err(Error::ErrAddressParseFailed),
        };
        let network_type = determine_network_type(&self.base_config.network, &ip)?;

        let c = CandidateBase {
            id: candidate_id,
            network_type: AtomicU8::new(network_type as u8),
            candidate_type: CandidateType::Relay,
            signaled: std::sync::RwLock::new(None),
            address: self.base_config.address,
            port: self.base_config.port,
            resolved_addr: SyncMutex::new(create_addr(network_type, ip, self.base_config.port)),
            component: AtomicU16::new(self.base_config.component),
            foundation_override: self.base_config.foundation,
            priority_override: self.base_config.priority,
            related_address: Some(CandidateRelatedAddress {
                address: self.rel_addr.clone(),
                port: self.rel_port,
            }),
            conn: self.base_config.conn,
            relay_client: self.relay_client.clone(),
            relay_protocol: self.base_config.relay_protocol,
            url: self.base_config.url,
            network_id: AtomicU16::new(self.base_config.network_id),
            network_cost: AtomicU16::new(self.base_config.network_cost.unwrap_or(0)),
            adapter_type: self.base_config.adapter_type,
            network_key: if self.base_config.network_key.is_empty() {
                self.rel_addr.clone()
            } else {
                self.base_config.network_key
            },
            generation: AtomicU32::new(self.base_config.generation),
            credentials: std::sync::RwLock::new(self.base_config.credentials),
            ..CandidateBase::default()
        };

        Ok(c)
    }
}
