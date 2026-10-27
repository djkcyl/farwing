//! Account-device power commands.
use crate::account::api::DeviceInfo;
use crate::i18n::t;
use anyhow::{Result, bail};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PowerAction {
    Wake,
    Shutdown,
    Reboot,
}

impl PowerAction {
    pub fn label(self) -> &'static str {
        match self {
            Self::Wake => t().session.power_wake,
            Self::Shutdown => t().session.power_shutdown,
            Self::Reboot => t().session.power_restart,
        }
    }

    pub fn check(
        self,
        device: &DeviceInfo,
        features: &crate::account::feature_ability::FeatureCatalog,
    ) -> Result<()> {
        device.validated_device_id()?;
        if !device.platform.is_desktop() || !device.controlled_support {
            bail!("{}", t().session.power_unsupported_device);
        }
        if !device.controllable {
            bail!("{}", t().session.power_control_not_allowed);
        }
        match self {
            Self::Wake => {
                if !device.is_offline() {
                    bail!("{}", t().session.power_wake_offline_only);
                }
                if !device.support_wol {
                    bail!("{}", t().session.power_wake_unsupported);
                }
            }
            Self::Shutdown | Self::Reboot => {
                let feature = if self == Self::Shutdown {
                    crate::account::feature_ability::Feature::Power
                } else {
                    crate::account::feature_ability::Feature::Power
                };
                if !features.policy_for(device).supports(feature) {
                    bail!("{}", t().session.power_action_unsupported);
                }
                if !device.is_connected() {
                    bail!("{}", t().session.power_online_only);
                }
            }
        }
        Ok(())
    }
}

#[derive(Default)]
pub(crate) struct PowerReceipt {
    pub current_device_in_same_network: Option<bool>,
    pub assist_count: Option<u32>,
    /// Wake-on-LAN target when this device is on the target's LAN.
    pub target: Option<farwing_protocol::api::WolTarget>,
    /// Magic packets this device broadcast itself.
    pub local_packets: Option<u32>,
}

impl PowerReceipt {
    pub fn summary(&self, action: PowerAction) -> String {
        if action != PowerAction::Wake {
            return t().session.power_accepted.into();
        }
        let mut routes = Vec::new();
        match self.local_packets {
            Some(0) => routes.push(t().session.wake_same_network_send_failed.to_owned()),
            Some(sent) => routes.push((t().session.wake_packets_sent)(sent)),
            None if self.current_device_in_same_network == Some(true) => {
                routes.push(t().session.wake_same_network.to_owned())
            }
            None => {}
        }
        if let Some(count) = self.assist_count {
            routes.push((t().session.wake_helper_devices)(count));
        }
        if routes.is_empty() {
            t().session.wake_accepted.into()
        } else {
            (t().session.wake_accepted_via)(&routes.join(" · "))
        }
    }
}
