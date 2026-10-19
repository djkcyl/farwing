//! Account-device power commands.
use crate::account::api::DeviceInfo;
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
            Self::Wake => "开机",
            Self::Shutdown => "关机",
            Self::Reboot => "重启",
        }
    }

    pub fn check(
        self,
        device: &DeviceInfo,
        features: &crate::account::feature_ability::FeatureCatalog,
    ) -> Result<()> {
        device.validated_device_id()?;
        if !device.platform.is_desktop() || !device.controlled_support {
            bail!("该设备不支持电脑电源操作");
        }
        if !device.controllable {
            bail!("该设备未开放远程控制");
        }
        match self {
            Self::Wake => {
                if !device.is_offline() {
                    bail!("只有离线设备可以发送开机请求");
                }
                if !device.support_wol {
                    bail!("该设备未提供远程开机能力");
                }
            }
            Self::Shutdown | Self::Reboot => {
                let feature = if self == Self::Shutdown {
                    crate::account::feature_ability::Feature::Shutdown
                } else {
                    crate::account::feature_ability::Feature::Reboot
                };
                if !features.policy_for(device).supports(feature) {
                    bail!("该设备未声明支持此电源操作");
                }
                if !device.is_connected() {
                    bail!("只有在线设备可以关机或重启");
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
            return "请求已受理，等待设备状态变化".into();
        }
        let mut routes = Vec::new();
        match self.local_packets {
            Some(0) => routes.push("当前设备同网（本机发送失败）".to_owned()),
            Some(sent) => routes.push(format!("本机已发送 {sent} 个唤醒报文")),
            None if self.current_device_in_same_network == Some(true) => {
                routes.push("当前设备同网".to_owned())
            }
            None => {}
        }
        if let Some(count) = self.assist_count {
            routes.push(format!("{count} 台协助设备"));
        }
        if routes.is_empty() {
            "开机请求已受理，等待上线".into()
        } else {
            format!("开机请求已受理，等待上线 · {}", routes.join(" · "))
        }
    }
}
