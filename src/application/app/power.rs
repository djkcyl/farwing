use super::*;
use crate::account::power::{PowerAction, PowerReceipt};
use crate::i18n::t;

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
                PowerAction::Wake => t().app.power_online,
                PowerAction::Shutdown => t().app.power_offline,
                PowerAction::Reboot => t().app.power_back_online,
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
            anyhow::bail!(t().app.power_busy);
        }
        if self
            .power_progress
            .get(&device.device_id)
            .is_some_and(|p| p.waiting)
        {
            anyhow::bail!(t().app.power_waiting);
        }
        let catalog = self.catalog.as_ref().context(t().app.devices_not_ready)?;
        if device.device_id == catalog.groups.current_device_id
            || !catalog
                .groups
                .desktop_devices
                .iter()
                .any(|d| d.device_id == device.device_id)
        {
            anyhow::bail!(t().app.power_own_computers_only);
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
                    PowerAction::Wake => t().app.power_timeout_online,
                    PowerAction::Shutdown => t().app.power_timeout_offline,
                    PowerAction::Reboot => t().app.power_timeout_restart,
                }
                .into();
            }
        }
    }
}
