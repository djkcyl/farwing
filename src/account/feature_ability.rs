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

use farwing_protocol::{Platform, capability as name};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Feature {
    Microphone,
    PortMapping,
    Annotation,
    CustomBitrate,
    ScreenChroma,
    SmartMouse,
    ManualTransfer,
    MultiScreen,
    ManualSuperScreen,
    FpsSuperScreen,
    Shutdown,
    Reboot,
}

impl Feature {
    fn capability(self) -> &'static str {
        match self {
            Self::PortMapping => name::PORTS,
            Self::ManualTransfer => name::FILES,
            Self::Shutdown | Self::Reboot => name::POWER,
            Self::Microphone
            | Self::Annotation
            | Self::CustomBitrate
            | Self::ScreenChroma
            | Self::SmartMouse
            | Self::MultiScreen
            | Self::ManualSuperScreen
            | Self::FpsSuperScreen => name::HOST,
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
    devices: Arc<Mutex<HashMap<String, (Platform, Vec<String>)>>>,
}

impl FeatureCatalog {
    /// Record the capabilities a device list reported.
    pub(crate) fn remember(&self, device: &crate::account::api::DeviceInfo) {
        if device.device_id.is_empty() {
            return;
        }
        self.devices
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(
                device.device_id.clone(),
                (device.platform, device.capabilities.clone()),
            );
    }

    pub(crate) fn remember_target(
        &self,
        device_id: &str,
        platform: Platform,
        capabilities: &[String],
    ) {
        self.devices
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(device_id.to_owned(), (platform, capabilities.to_vec()));
    }

    /// Policy for a device whose capabilities were seen in a device list.
    pub(crate) fn policy(&self, device_id: &str, platform: Platform) -> FeaturePolicy {
        let capabilities = self
            .devices
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(device_id)
            .map(|(_, capabilities)| capabilities.clone())
            .unwrap_or_default();
        FeaturePolicy {
            platform,
            capabilities,
        }
    }

    pub(crate) fn policy_for(&self, device: &crate::account::api::DeviceInfo) -> FeaturePolicy {
        self.remember(device);
        FeaturePolicy {
            platform: device.platform,
            capabilities: device.capabilities.clone(),
        }
    }

    pub(crate) async fn close(&self) {}
}

#[derive(Clone, Debug)]
pub(crate) struct FeaturePolicy {
    platform: Platform,
    capabilities: Vec<String>,
}

impl FeaturePolicy {
    /// A host reached directly: a Farwing host of this build's platform
    /// (`docs/protocol/v2-design.md` §12).
    pub(crate) fn direct(platform: Platform) -> Self {
        Self {
            platform,
            capabilities: local_capabilities(),
        }
    }

    pub(crate) fn is_windows(&self) -> bool {
        self.platform == Platform::Windows
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
        catalog.remember_target("a1b2c3d4e5f6g7h8", Platform::Windows, &local_capabilities());
        let policy = catalog.policy("a1b2c3d4e5f6g7h8", Platform::Windows);
        assert!(policy.is_windows() && policy.supports(Feature::PortMapping));
        assert!(policy.supports(Feature::Annotation));
        assert!(!catalog.policy("unknown", Platform::Windows).supports(Feature::Reboot));
    }
}
