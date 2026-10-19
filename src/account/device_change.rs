//! Device deltas from the authenticated presence connection. `device.changed`
//! pushes carry the full device: one of this account's devices, or a friend's
//! device this account holds grants on (`shared`).
use crate::account::api::{DeviceGroups, DeviceInfo, DeviceList};
use anyhow::{Result, bail};

pub(crate) mod relay;

#[derive(Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) enum ChangeKind {
    Bound,
    Changed,
    Removed,
}

#[derive(Clone, serde::Serialize, serde::Deserialize)]
pub(crate) struct DeviceChange {
    pub id: String,
    pub kind: ChangeKind,
    /// A friend's device rather than one of this account's.
    #[serde(default)]
    pub shared: bool,
    /// The full device for `Bound` and `Changed` pushes.
    #[serde(default)]
    device: Option<DeviceInfo>,
    /// A local rename confirmed by the server.
    #[serde(default)]
    alias: Option<String>,
}

impl DeviceChange {
    pub fn parse(push: &farwing_protocol::realtime::Envelope) -> Result<Option<Self>> {
        use farwing_protocol::realtime::{DeviceChangeKind, DeviceChanged, types};
        if push.kind != types::DEVICE_CHANGED {
            return Ok(None);
        }
        let Ok(changed) = serde_json::from_value::<DeviceChanged>(push.data.clone()) else {
            bail!("device push is malformed");
        };
        crate::account::api::validate_device_id(&changed.device.device_id)?;
        let id = changed.device.device_id.clone();
        let (kind, device) = match changed.kind {
            DeviceChangeKind::Removed => (ChangeKind::Removed, None),
            DeviceChangeKind::Added => (ChangeKind::Bound, Some(changed.device.into())),
            DeviceChangeKind::Updated => (ChangeKind::Changed, Some(changed.device.into())),
        };
        Ok(Some(Self {
            id,
            kind,
            shared: changed.shared,
            device,
            alias: None,
        }))
    }

    pub fn renamed(id: String, alias: String) -> Self {
        Self {
            id,
            kind: ChangeKind::Changed,
            shared: false,
            device: None,
            alias: Some(alias),
        }
    }

    pub fn removed(id: String) -> Self {
        Self {
            id,
            kind: ChangeKind::Removed,
            shared: false,
            device: None,
            alias: None,
        }
    }

    pub fn bound_device(&self) -> Option<DeviceInfo> {
        (self.kind == ChangeKind::Bound)
            .then(|| self.device.clone())
            .flatten()
    }

    pub fn merge(&mut self, newer: &Self) {
        if self.kind == ChangeKind::Removed && newer.kind == ChangeKind::Changed {
            return;
        }
        if newer.device.is_some() || newer.kind != ChangeKind::Changed {
            *self = newer.clone();
            return;
        }
        if let Some(alias) = &newer.alias {
            self.alias = Some(alias.clone());
            if let Some(device) = &mut self.device {
                device.alias = alias.clone();
            }
        }
    }

    fn apply_device(&self, device: &mut DeviceInfo) {
        if let Some(latest) = &self.device {
            *device = latest.clone();
        }
        if let Some(alias) = &self.alias {
            device.alias = alias.clone();
        }
    }

    pub fn apply_list(&self, list: &mut DeviceList) {
        if self.kind == ChangeKind::Removed {
            list.devices.retain(|d| d.device_id != self.id);
            list.shared.retain(|d| d.device_id != self.id);
            return;
        }
        let mut known = false;
        for device in std::iter::once(&mut list.current_device)
            .chain(list.devices.iter_mut())
            .chain(list.shared.iter_mut())
            .filter(|d| d.device_id == self.id)
        {
            known = true;
            self.apply_device(device);
        }
        if !known && let Some(device) = self.bound_device() {
            if self.shared {
                list.shared.push(device);
            } else {
                list.devices.push(device);
            }
        }
    }

    pub fn apply_groups(&self, groups: &mut DeviceGroups) {
        if self.shared {
            return;
        }
        let mut found = None;
        for devices in [&mut groups.desktop_devices, &mut groups.mobile_devices] {
            if let Some(index) = devices.iter().position(|d| d.device_id == self.id) {
                found = Some(devices.remove(index));
                break;
            }
        }
        if self.kind == ChangeKind::Removed {
            return;
        }
        let device = match found {
            Some(mut device) => {
                self.apply_device(&mut device);
                Some(device)
            }
            None => self.bound_device(),
        };
        if let Some(device) = device {
            groups.insert(device);
        }
    }
}
