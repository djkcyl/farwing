//! Wired adapter wake inspection. Reading is unprivileged: the kernel's
//! wake permission comes from sysfs and the magic-packet mode from
//! `ethtool` when it is installed. Changing either needs root, which this
//! desktop client does not hold, so configuration is refused with the command
//! to run instead.
use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use std::path::Path;
use tokio_util::sync::CancellationToken;

const CONFIGURE: &str = "Linux 上配置网卡唤醒需要 root 权限，程序内不提供；\
     请执行 sudo ethtool -s <网卡> wol g，并在 BIOS 中开启网络唤醒后重新检查";

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct AdapterKey {
    pub guid: String,
    pub mac: String,
}
impl AdapterKey {
    pub fn validate(&self) -> Result<()> {
        uuid::Uuid::parse_str(&self.guid).context("网卡标识无效")?;
        ensure!(
            self.mac.len() == 12 && self.mac.bytes().all(|b| b.is_ascii_hexdigit()),
            "网卡MAC无效"
        );
        Ok(())
    }
}
#[derive(Clone, Default, Serialize, Deserialize)]
pub(crate) struct Property {
    pub key: String,
    pub value: String,
    pub writable: bool,
}
impl Property {
    pub fn title(&self) -> &str {
        match self.key.as_str() {
            "WakeOnMagicPacket" => "魔术包唤醒（系统）",
            "*WakeOnMagicPacket" => "魔术包唤醒（驱动）",
            "AllowComputerToTurnOffDevice" => "网卡电源管理",
            "S5WakeOnLan" => "关机后唤醒",
            "EnablePME" => "电源管理事件（PME）",
            "DeviceWake" => "允许网卡唤醒电脑",
            "MagicPacketOnly" => "仅允许魔术包唤醒",
            _ => &self.key,
        }
    }
    pub fn enabled(&self) -> Option<bool> {
        match self.value.as_str() {
            "Enabled" | "1" => Some(true),
            "Disabled" | "0" => Some(false),
            _ => None,
        }
    }
}
#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct Adapter {
    pub key: AdapterKey,
    pub name: String,
    pub description: String,
    pub index: u32,
    pub connected: bool,
    pub items: Vec<Property>,
    pub errors: Vec<String>,
}

fn read(path: &Path) -> Option<String> {
    std::fs::read_to_string(path)
        .ok()
        .map(|s| s.trim().to_owned())
}

fn property(key: &str, enabled: Option<bool>) -> Property {
    Property {
        key: key.into(),
        value: match enabled {
            Some(true) => "Enabled",
            Some(false) => "Disabled",
            None => "",
        }
        .into(),
        writable: false,
    }
}

/// The current `Wake-on` modes from `ethtool`, whose netlink interface
/// answers ordinary users; `None` when it is missing or refuses.
fn wake_modes(name: &str) -> Option<String> {
    let output = std::process::Command::new("ethtool")
        .arg(name)
        .env("LC_ALL", "C")
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .output()
        .ok()?;
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .find_map(|line| line.trim().strip_prefix("Wake-on:"))
        .map(|modes| modes.trim().to_owned())
}

/// A wired physical Ethernet adapter (not Wi-Fi, not a bridge or tunnel).
fn adapter(name: &str) -> Option<Adapter> {
    let directory = Path::new("/sys/class/net").join(name);
    let device = directory.join("device");
    if !device.exists()
        || read(&directory.join("type")).as_deref() != Some("1")
        || directory.join("wireless").exists()
        || directory.join("phy80211").exists()
    {
        return None;
    }
    let mac = read(&directory.join("address"))?
        .replace(':', "")
        .to_uppercase();
    if mac.len() != 12 || mac == "000000000000" {
        return None;
    }
    let index = read(&directory.join("ifindex"))?.parse().ok()?;
    // Stable per adapter: the Windows key is the interface GUID.
    let guid = uuid::Uuid::new_v5(
        &uuid::Uuid::NAMESPACE_OID,
        format!("openuuyc-nic:{name}:{mac}").as_bytes(),
    )
    .hyphenated()
    .to_string();
    let driver = std::fs::read_link(device.join("driver"))
        .ok()
        .and_then(|link| Some(link.file_name()?.to_string_lossy().into_owned()));
    let description = super::super::device_profile::pci_device_name(&device)
        .or(driver)
        .unwrap_or_else(|| name.to_owned());
    let mut errors = Vec::new();
    let magic = match wake_modes(name) {
        Some(modes) => Some(modes.contains('g')),
        None => {
            errors.push("未能读取魔术包唤醒状态；安装 ethtool 后可重新检查".to_owned());
            None
        }
    };
    let wake = read(&device.join("power/wakeup")).map(|v| v == "enabled");
    Some(Adapter {
        key: AdapterKey { guid, mac },
        name: name.to_owned(),
        description,
        index,
        connected: read(&directory.join("carrier")).as_deref() == Some("1"),
        items: vec![
            property("WakeOnMagicPacket", magic),
            property("DeviceWake", wake),
        ],
        errors,
    })
}

pub(crate) fn inspect(
    target: Option<&AdapterKey>,
    apply: bool,
    cancel: &CancellationToken,
) -> Result<Vec<Adapter>> {
    if let Some(key) = target {
        key.validate()?;
    }
    if apply {
        bail!(CONFIGURE);
    }
    ensure!(!cancel.is_cancelled(), "操作已取消");
    let mut names: Vec<String> = std::fs::read_dir("/sys/class/net")
        .context("无法读取网卡列表")?
        .flatten()
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    let adapters: Vec<Adapter> = names
        .iter()
        .filter_map(|name| adapter(name))
        .filter(|adapter| target.is_none_or(|key| adapter.key == *key))
        .collect();
    ensure!(target.is_none() || !adapters.is_empty(), "目标网卡已移除");
    Ok(adapters)
}

pub(crate) fn configure(key: &AdapterKey, _cancel: &CancellationToken) -> Result<Vec<Adapter>> {
    key.validate()?;
    bail!(CONFIGURE)
}

#[cfg(test)]
mod tests {
    #[test]
    fn wired_adapters_have_stable_keys() {
        let cancel = tokio_util::sync::CancellationToken::new();
        let first = super::inspect(None, false, &cancel).unwrap();
        let second = super::inspect(None, false, &cancel).unwrap();
        assert_eq!(first.len(), second.len());
        for (a, b) in first.iter().zip(&second) {
            a.key.validate().unwrap();
            assert_eq!(a.key, b.key);
            let only = super::inspect(Some(&a.key), false, &cancel).unwrap();
            assert_eq!(only.len(), 1);
        }
        if let Some(adapter) = first.first() {
            assert!(super::configure(&adapter.key, &cancel).is_err());
        }
    }
}
