//! Device feature policy, separate from peer-protocol feature levels.
//!
//! Every Farwing device reports what it can do before any session exists
//! (`host`, `files`, `ports`, `power`, ...) when it registers; device lists
//! return those capabilities. A controller gates account-level actions on
//! them. In-session features are negotiated by the peer HELLO; every v2 host
//! implements them, so they only require the `host` capability here.
use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
};

use farwing_protocol::capability as name;

/// What a device offers, as its registered capabilities say.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Feature {
    /// Remote desktop and everything in its sessions.
    Host,
    Files,
    Ports,
    Power,
}

impl Feature {
    fn capability(self) -> &'static str {
        match self {
            Self::Host => name::HOST,
            Self::Files => name::FILES,
            Self::Ports => name::PORTS,
            Self::Power => name::POWER,
        }
    }
}

/// Capabilities this Windows build reports at device registration.
pub(crate) fn local_capabilities() -> Vec<String> {
    [
        name::HOST,
        name::FILES,
        name::PORTS,
        name::POWER,
        name::WAKE_ON_LAN,
        name::ASSIST,
    ]
    .into_iter()
    .map(str::to_owned)
    .collect()
}

#[derive(Clone, Default)]
pub(crate) struct FeatureCatalog {
    devices: Arc<Mutex<HashMap<String, Vec<String>>>>,
}

impl FeatureCatalog {
    /// Record the capabilities a device list reported.
    pub(crate) fn remember(&self, device: &crate::account::api::DeviceInfo) {
        if device.device_id.is_empty() {
            return;
        }
        self.remember_target(&device.device_id, &device.capabilities);
    }

    pub(crate) fn remember_target(&self, device_id: &str, capabilities: &[String]) {
        self.devices
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(device_id.to_owned(), capabilities.to_vec());
    }

    /// Policy for a device whose capabilities were seen in a device list.
    pub(crate) fn policy(&self, device_id: &str) -> FeaturePolicy {
        let capabilities = self
            .devices
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(device_id)
            .cloned()
            .unwrap_or_default();
        FeaturePolicy { capabilities }
    }

    pub(crate) fn policy_for(&self, device: &crate::account::api::DeviceInfo) -> FeaturePolicy {
        self.remember(device);
        FeaturePolicy {
            capabilities: device.capabilities.clone(),
        }
    }
}

#[derive(Clone, Debug)]
pub(crate) struct FeaturePolicy {
    capabilities: Vec<String>,
}

impl FeaturePolicy {
    /// A host reached directly: a Farwing host of this build
    /// (`docs/protocol/v2-design.md` §12).
    pub(crate) fn direct() -> Self {
        Self {
            capabilities: local_capabilities(),
        }
    }

    pub(crate) fn supports(&self, feature: Feature) -> bool {
        self.capabilities
            .iter()
            .any(|capability| capability == feature.capability())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capability_policy() {
        let catalog = FeatureCatalog::default();
        catalog.remember_target("a1b2c3d4e5f6g7h8", &local_capabilities());
        let policy = catalog.policy("a1b2c3d4e5f6g7h8");
        assert!(policy.supports(Feature::Ports));
        assert!(policy.supports(Feature::Host));
        assert!(!catalog.policy("unknown").supports(Feature::Power));
    }
}
