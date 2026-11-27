//! Wired adapter wake inspection, viewer-only: macOS does not expose the
//! adapter wake properties this UI reads, so inspection reports no adapters.
use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;

const UNSUPPORTED: &str = "macOS 暂不支持网卡唤醒设置";

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

pub(crate) fn inspect(
    target: Option<&AdapterKey>,
    _apply: bool,
    cancel: &CancellationToken,
) -> Result<Vec<Adapter>> {
    if let Some(key) = target {
        key.validate()?;
    }
    ensure!(!cancel.is_cancelled(), "操作已取消");
    ensure!(target.is_none(), "目标网卡已移除");
    Ok(Vec::new())
}

pub(crate) fn configure(key: &AdapterKey, _cancel: &CancellationToken) -> Result<Vec<Adapter>> {
    key.validate()?;
    bail!(UNSUPPORTED)
}
