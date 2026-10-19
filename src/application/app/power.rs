use super::*;
use crate::account::power::{PowerAction, PowerReceipt};

pub(super) struct AcceptedPower {
    pub device: DeviceInfo,
    pub action: PowerAction,
    pub receipt: PowerReceipt,
}

pub(super) struct PowerProgress {
    pub device: DeviceInfo,
    pub action: PowerAction,
    pub message: String,
    pub waiting: bool,
    started: Instant,
    saw_offline: bool,
}

pub(super) struct PendingPower {
    pub id: String,
    pub action: PowerAction,
    pub saw_offline: bool,
}

impl PowerProgress {
    fn observe(&mut self, devices: &DeviceList) {
        if !self.waiting {
            return;
        }
        let Some(device) = devices
            .devices
            .iter()
            .find(|d| d.device_id == self.device.device_id)
        else {
            return;
        };
        self.saw_offline |= device.is_offline();
        let observed = match self.action {
            PowerAction::Wake => device.is_connected(),
            PowerAction::Shutdown => device.is_offline(),
            PowerAction::Reboot => self.saw_offline && device.is_connected(),
        };
        if observed {
            self.waiting = false;
            self.message = match self.action {
                PowerAction::Wake => "设备已上线",
                PowerAction::Shutdown => "设备已离线",
                PowerAction::Reboot => "已观察到设备离线后重新上线",
            }
            .into();
        }
    }

    pub fn elapsed(&self) -> u64 {
        self.started.elapsed().as_secs()
    }
}

impl DeviceCenterApp {
    pub(super) fn power_available(&self, device: &DeviceInfo, action: PowerAction) -> Result<()> {
        if self.mutation_pending || self.logout_pending || self.login_running {
            anyhow::bail!("正在处理账号或设备操作");
        }
        if self
            .power_progress
            .get(&device.device_id)
            .is_some_and(|p| p.waiting)
        {
            anyhow::bail!("正在等待该设备的电源操作结果");
        }
        let catalog = self.catalog.as_ref().context("等待完整设备清单")?;
        if device.device_id == catalog.groups.current_device_id
            || !catalog
                .groups
                .desktop_devices
                .iter()
                .any(|d| d.device_id == device.device_id)
        {
            anyhow::bail!("仅支持本账号绑定的远端电脑");
        }
        action.check(device, &catalog.features)
    }

    pub(super) fn accept_power(&mut self, accepted: AcceptedPower) {
        let saw_offline = self.pending_power.take().is_some_and(|p| {
            p.id == accepted.device.device_id && p.action == accepted.action && p.saw_offline
        });
        let message = accepted.receipt.summary(accepted.action);
        self.power_progress.insert(
            accepted.device.device_id.clone(),
            PowerProgress {
                device: accepted.device,
                action: accepted.action,
                message,
                waiting: true,
                started: Instant::now(),
                saw_offline,
            },
        );
        // A device push may arrive before the HTTP acknowledgement.
        if let Some(devices) = &self.devices {
            for progress in self.power_progress.values_mut() {
                progress.observe(devices);
            }
        }
    }

    pub(super) fn observe_power(&mut self, devices: &DeviceList) {
        if let Some(pending) = &mut self.pending_power {
            pending.saw_offline |= devices
                .devices
                .iter()
                .any(|d| d.device_id == pending.id && d.is_offline());
        }
        for progress in self.power_progress.values_mut() {
            progress.observe(devices);
        }
    }

    pub(super) fn tick_power(&mut self) {
        // Ticking only advances local waiting/timeout state. Device-list
        // updates drive observe_power; waiting must not add HTTP polling.
        for progress in self.power_progress.values_mut() {
            if progress.waiting && progress.elapsed() >= 120 {
                progress.waiting = false;
                progress.message = match progress.action {
                    PowerAction::Wake => "等待超时，尚未确认上线；请检查远端电源及唤醒条件",
                    PowerAction::Shutdown => "等待超时，尚未确认设备离线；请检查远端状态",
                    PowerAction::Reboot => "尚未确认离线再上线，不能据此判定重启成功或失败",
                }
                .into();
            }
        }
    }
}
