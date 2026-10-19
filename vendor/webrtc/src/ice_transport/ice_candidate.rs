use std::fmt;
use std::sync::Arc;

use ice::candidate::candidate_base::{adapter_type_from_network_cost, CandidateBaseConfig};
use ice::candidate::candidate_host::CandidateHostConfig;
use ice::candidate::candidate_peer_reflexive::CandidatePeerReflexiveConfig;
use ice::candidate::candidate_relay::CandidateRelayConfig;
use ice::candidate::candidate_server_reflexive::CandidateServerReflexiveConfig;
use ice::candidate::Candidate;
use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};
use crate::ice_transport::ice_candidate_type::RTCIceCandidateType;
use crate::ice_transport::ice_protocol::RTCIceProtocol;

/// ICECandidate represents a ice candidate
///
/// ## Specifications
///
/// * [MDN]
/// * [W3C]
///
/// [MDN]: https://developer.mozilla.org/en-US/docs/Web/API/RTCIceCandidate
/// [W3C]: https://w3c.github.io/webrtc-pc/#rtcicecandidate-interface
#[derive(Default, Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RTCIceCandidate {
    pub stats_id: String,
    pub foundation: String,
    pub priority: u32,
    pub address: String,
    pub protocol: RTCIceProtocol,
    pub port: u16,
    pub typ: RTCIceCandidateType,
    pub component: u16,
    pub related_address: String,
    pub related_port: u16,
    pub tcp_type: String,
    pub relay_protocol: String,
    pub url: String,
    pub network_id: u16,
    pub network_cost: u16,
    pub network_key: String,
    pub generation: u32,
    pub username_fragment: Option<String>,
}

/// Conversion for ice_candidates
pub(crate) fn rtc_ice_candidates_from_ice_candidates(
    ice_candidates: &[Arc<dyn Candidate + Send + Sync>],
) -> Vec<RTCIceCandidate> {
    ice_candidates.iter().map(|c| c.into()).collect()
}

impl From<&Arc<dyn Candidate + Send + Sync>> for RTCIceCandidate {
    fn from(c: &Arc<dyn Candidate + Send + Sync>) -> Self {
        let typ: RTCIceCandidateType = c.candidate_type().into();
        let protocol = RTCIceProtocol::from(c.network_type().network_short().as_str());
        let (related_address, related_port) = if let Some(ra) = c.related_address() {
            (ra.address, ra.port)
        } else {
            (String::new(), 0)
        };

        RTCIceCandidate {
            stats_id: c.id(),
            foundation: c.foundation(),
            priority: c.priority(),
            address: c.address(),
            protocol,
            port: c.port(),
            component: c.component(),
            typ,
            tcp_type: c.tcp_type().to_string(),
            related_address,
            related_port,
            relay_protocol: c.relay_protocol(),
            url: c.url(),
            network_id: c.network_id(),
            network_cost: c.network_cost(),
            network_key: c.network_key(),
            generation: c.generation(),
            username_fragment: {
                let username = c.credentials().username;
                (!username.is_empty()).then_some(username)
            },
        }
    }
}

impl RTCIceCandidate {
    pub(crate) fn to_ice(&self) -> Result<impl Candidate> {
        let candidate_id = self.stats_id.clone();
        let base_config = CandidateBaseConfig {
            candidate_id,
            network: self.protocol.to_string(),
            address: self.address.clone(),
            port: self.port,
            component: self.component,
            //tcp_type: ice.NewTCPType(c.TCPType),
            foundation: self.foundation.clone(),
            priority: self.priority,
            relay_protocol: self.relay_protocol.clone(),
            url: self.url.clone(),
            network_id: self.network_id,
            network_cost: Some(self.network_cost),
            adapter_type: adapter_type_from_network_cost(self.network_cost),
            network_key: self.network_key.clone(),
            generation: self.generation,
            credentials: ice::candidate::IceCredentials {
                username: self.username_fragment.clone().unwrap_or_default(),
                password: String::new(),
            },
            ..Default::default()
        };

        let c = match self.typ {
            RTCIceCandidateType::Host => {
                let config = CandidateHostConfig {
                    base_config,
                    tcp_type: ice::tcp_type::TcpType::from(self.tcp_type.as_str()),
                    ..Default::default()
                };
                config.new_candidate_host()?
            }
            RTCIceCandidateType::Srflx => {
                let config = CandidateServerReflexiveConfig {
                    base_config,
                    rel_addr: self.related_address.clone(),
                    rel_port: self.related_port,
                };
                config.new_candidate_server_reflexive()?
            }
            RTCIceCandidateType::Prflx => {
                let config = CandidatePeerReflexiveConfig {
                    base_config,
                    rel_addr: self.related_address.clone(),
                    rel_port: self.related_port,
                };
                config.new_candidate_peer_reflexive()?
            }
            RTCIceCandidateType::Relay => {
                let config = CandidateRelayConfig {
                    base_config,
                    rel_addr: self.related_address.clone(),
                    rel_port: self.related_port,
                    relay_client: None, //TODO?
                };
                config.new_candidate_relay()?
            }
            _ => return Err(Error::ErrICECandidateTypeUnknown),
        };

        Ok(c)
    }

    /// to_json returns an ICECandidateInit
    /// as indicated by the spec <https://w3c.github.io/webrtc-pc/#dom-rtcicecandidate-tojson>
    pub fn to_json(&self) -> Result<RTCIceCandidateInit> {
        let candidate = self.to_ice()?;

        Ok(RTCIceCandidateInit {
            candidate: format!("candidate:{}", candidate.marshal()),
            sdp_mid: Some("".to_owned()),
            sdp_mline_index: Some(0u16),
            username_fragment: self.username_fragment.clone(),
        })
    }
}

impl fmt::Display for RTCIceCandidate {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{} {} {}:{}{}",
            self.protocol, self.typ, self.address, self.port, self.related_address,
        )
    }
}

/// ICECandidateInit is used to serialize ice candidates
#[derive(Default, Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RTCIceCandidateInit {
    pub candidate: String,
    pub sdp_mid: Option<String>,
    #[serde(rename = "sdpMLineIndex")]
    pub sdp_mline_index: Option<u16>,
    pub username_fragment: Option<String>,
}

impl RTCIceCandidateInit {
    /// Trickle candidates can carry their generation credential in the
    /// candidate string even when the JSON usernameFragment member is absent.
    pub fn effective_username_fragment(&self) -> Option<String> {
        if let Some(fragment) = self.username_fragment.as_ref().filter(|f| !f.is_empty()) {
            return Some(fragment.clone());
        }
        let mut extensions = self.candidate.split_whitespace().skip(8);
        while let (Some(key), Some(value)) = (extensions.next(), extensions.next()) {
            if key == "ufrag" {
                return Some(value.to_owned());
            }
        }
        None
    }
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn test_ice_candidate_serialization() {
        let tests = vec![
            (
                RTCIceCandidateInit {
                    candidate: "candidate:abc123".to_string(),
                    sdp_mid: Some("0".to_string()),
                    sdp_mline_index: Some(0),
                    username_fragment: Some("def".to_string()),
                },
                r#"{"candidate":"candidate:abc123","sdpMid":"0","sdpMLineIndex":0,"usernameFragment":"def"}"#,
            ),
            (
                RTCIceCandidateInit {
                    candidate: "candidate:abc123".to_string(),
                    sdp_mid: None,
                    sdp_mline_index: None,
                    username_fragment: None,
                },
                r#"{"candidate":"candidate:abc123","sdpMid":null,"sdpMLineIndex":null,"usernameFragment":null}"#,
            ),
        ];

        for (candidate_init, expected_string) in tests {
            let result = serde_json::to_string(&candidate_init);
            assert!(result.is_ok(), "testCase: marshal err: {result:?}");
            let candidate_data = result.unwrap();
            assert_eq!(candidate_data, expected_string, "string is not expected");

            let result = serde_json::from_str::<RTCIceCandidateInit>(&candidate_data);
            assert!(result.is_ok(), "testCase: unmarshal err: {result:?}");
            if let Ok(actual_candidate_init) = result {
                assert_eq!(actual_candidate_init, candidate_init);
            }
        }
    }
}
