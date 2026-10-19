//! Account snapshots, push overlays, and user-requested device details.
use super::*;
use crate::account::api::{DeviceDetail, DeviceGroups};
use crate::account::device_change::{ChangeKind, DeviceChange};
use std::collections::{HashMap, HashSet, VecDeque};

struct Read<T> {
    task: JoinHandle<Result<T>>,
    changes: HashMap<String, DeviceChange>,
}

impl<T> Read<T> {
    fn remember(&mut self, change: &DeviceChange) {
        self.changes
            .entry(change.id.clone())
            .and_modify(|old| old.merge(change))
            .or_insert_with(|| change.clone());
    }
}
impl<T> Drop for Read<T> {
    fn drop(&mut self) {
        self.task.abort();
    }
}
struct DetailRead {
    id: String,
    version: Option<(farwing_protocol::Platform, String)>,
    task: JoinHandle<Result<DeviceDetail>>,
}
impl Drop for DetailRead {
    fn drop(&mut self) {
        self.task.abort();
    }
}

pub(super) struct DeviceSync {
    list: Option<DeviceList>,
    catalog: Option<catalog::Catalog>,
    list_read: Option<Read<DeviceList>>,
    group_read: Option<Read<DeviceGroups>>,
    detail_read: Option<DetailRead>,
    list_requested: bool,
    groups_requested: bool,
    details: VecDeque<String>,
    unresolved: HashSet<String>,
}
impl Default for DeviceSync {
    fn default() -> Self {
        Self {
            list: None,
            catalog: None,
            list_read: None,
            group_read: None,
            detail_read: None,
            list_requested: true,
            groups_requested: true,
            details: VecDeque::new(),
            unresolved: HashSet::new(),
        }
    }
}
impl DeviceSync {
    pub async fn reset(&mut self) {
        if let Some(mut read) = self.list_read.take() {
            read.task.abort();
            let _ = (&mut read.task).await;
        }
        if let Some(mut read) = self.group_read.take() {
            read.task.abort();
            let _ = (&mut read.task).await;
        }
        if let Some(mut read) = self.detail_read.take() {
            read.task.abort();
            let _ = (&mut read.task).await;
        }
        *self = Self::default();
    }
    pub fn refresh(&mut self, reconnect: bool) {
        self.list_requested |= reconnect || self.list_read.is_none();
        self.groups_requested |= reconnect || self.group_read.is_none();
    }
    pub fn resynchronize(&mut self) {
        // An HTTP read from before an event gap must not overwrite the new
        // baseline. Dropping Read cancels its task and discards its overlays.
        self.list_read.take();
        self.group_read.take();
        self.refresh(true);
    }
    pub fn refresh_status(&mut self) {
        self.list_requested = true;
    }
    pub fn detail(&mut self, id: String) {
        if self.detail_read.as_ref().is_some_and(|r| r.id == id) {
            return;
        }
        self.details.retain(|d| d != &id);
        self.details.push_front(id);
    }
    fn version(&self, id: &str) -> Option<(farwing_protocol::Platform, String)> {
        self.catalog
            .as_ref()?
            .groups
            .entries()
            .find(|(_, d)| d.device_id == id)
            .map(|(_, d)| (d.platform, d.version_name.clone()))
    }
    fn emit(&self, events: &Sender<GuiEvent>, generation: u64, name: &str) {
        if let Some(list) = &self.list {
            let _ = events.send(GuiEvent::Devices(generation, list.clone()));
        }
        if let Some(catalog) = &self.catalog {
            let _ = events.send(GuiEvent::Catalog(
                generation,
                Ok(catalog.clone()),
                name.into(),
            ));
        }
    }
    pub fn change(
        &mut self,
        change: DeviceChange,
        events: &Sender<GuiEvent>,
        generation: u64,
        name: &str,
    ) {
        let known = self.list.as_ref().is_some_and(|l| {
            l.current_device.device_id == change.id
                || l.devices.iter().any(|d| d.device_id == change.id)
                || l.shared.iter().any(|d| d.device_id == change.id)
        }) || self
            .catalog
            .as_ref()
            .is_some_and(|c| c.groups.entries().any(|(_, d)| d.device_id == change.id));
        if let Some(read) = &mut self.list_read {
            read.remember(&change);
        }
        if let Some(read) = &mut self.group_read {
            read.remember(&change);
        }
        if let Some(list) = &mut self.list {
            change.apply_list(list);
        }
        if let Some(catalog) = &mut self.catalog {
            change.apply_groups(&mut catalog.groups);
            catalog.prune_details();
        }
        if change.kind == ChangeKind::Removed {
            self.unresolved.remove(&change.id);
            self.details.retain(|id| id != &change.id);
            if self.detail_read.as_ref().is_some_and(|r| r.id == change.id) {
                self.detail_read.take();
            }
        } else if ((!known && change.bound_device().is_none())
            || (change.kind == ChangeKind::Bound && change.bound_device().is_none()))
            && self.unresolved.insert(change.id.clone())
        {
            // Unknown membership is resolved once, not on every repeated push.
            self.refresh(true);
        }
        self.emit(events, generation, name);
    }
    async fn receive(
        &mut self,
        events: &Sender<GuiEvent>,
        generation: u64,
        name: &str,
        suggested: &str,
        features: crate::account::feature_ability::FeatureCatalog,
    ) -> Option<anyhow::Error> {
        let mut failure = None;
        let mut list_changed = false;
        let mut catalog_changed = false;
        if self
            .list_read
            .as_ref()
            .is_some_and(|r| r.task.is_finished())
        {
            let mut read = self.list_read.take().unwrap();
            match (&mut read.task).await.unwrap_or_else(|e| Err(e.into())) {
                Ok(mut list) => {
                    for change in read.changes.values() {
                        change.apply_list(&mut list);
                    }
                    self.list = Some(list);
                    list_changed = true;
                }
                Err(error) => failure = Some(error),
            }
        }
        if self
            .group_read
            .as_ref()
            .is_some_and(|r| r.task.is_finished())
        {
            let mut read = self.group_read.take().unwrap();
            match (&mut read.task).await.unwrap_or_else(|e| Err(e.into())) {
                Ok(mut groups) => {
                    if let Err(error) =
                        crate::account::api::validate_device_id(&groups.current_device_id)
                    {
                        let _ = events.send(GuiEvent::Catalog(
                            generation,
                            Err(error.to_string()),
                            name.to_owned(),
                        ));
                    } else {
                        for change in read.changes.values() {
                            change.apply_groups(&mut groups);
                        }
                        self.catalog = Some(catalog::Catalog::from_groups(
                            groups,
                            self.catalog.take(),
                            suggested.to_owned(),
                            features.clone(),
                        ));
                        catalog_changed = true;
                    }
                }
                Err(error) => {
                    let _ = events.send(GuiEvent::Catalog(
                        generation,
                        Err(format!("{error:#}")),
                        name.to_owned(),
                    ));
                }
            }
        }
        if self
            .detail_read
            .as_ref()
            .is_some_and(|r| r.task.is_finished())
        {
            let mut read = self.detail_read.take().unwrap();
            let result = (&mut read.task)
                .await
                .unwrap_or_else(|e| Err(e.into()))
                .map_err(|e| format!("{e:#}"));
            if read.version != self.version(&read.id) {
                // An update during this read made the hardware result stale.
                if self.version(&read.id).is_some() {
                    self.details.push_front(read.id.clone());
                }
            } else {
                if let Some(catalog) = &mut self.catalog {
                    catalog.store_detail(&read.id, result.clone());
                }
                let _ = events.send(GuiEvent::Detail(generation, read.id.clone(), result));
                catalog_changed = true;
            }
        }
        if list_changed && let Some(list) = &self.list {
            let _ = events.send(GuiEvent::Devices(generation, list.clone()));
        }
        if catalog_changed && let Some(catalog) = &self.catalog {
            let _ = events.send(GuiEvent::Catalog(
                generation,
                Ok(catalog.clone()),
                name.into(),
            ));
        }
        failure
    }

    pub async fn poll(
        &mut self,
        client: &Arc<AuthenticatedClient>,
        events: &Sender<GuiEvent>,
        generation: u64,
    ) -> Option<anyhow::Error> {
        let failure = self
            .receive(
                events,
                generation,
                &client.account_name(),
                &client.suggested_device_name(),
                client.feature_catalog(),
            )
            .await;
        // Pending reads belong to account synchronization, not window focus.
        // Startup and push reconciliation must finish
        // while the control center is covered, minimized or in the tray.
        self.start_reads(client, events);
        failure
    }

    fn start_reads(&mut self, client: &Arc<AuthenticatedClient>, events: &Sender<GuiEvent>) {
        if self.list_requested && self.list_read.is_none() {
            self.list_requested = false;
            let client = Arc::clone(client);
            let _ = events.send(GuiEvent::Working("正在同步设备状态".into()));
            self.list_read = Some(Read {
                task: tokio::spawn(async move { client.list_devices().await }),
                changes: HashMap::new(),
            });
        }
        if self.groups_requested && self.group_read.is_none() {
            self.groups_requested = false;
            let client = Arc::clone(client);
            self.group_read = Some(Read {
                task: tokio::spawn(async move { client.device_groups().await }),
                changes: HashMap::new(),
            });
        }
        if self.detail_read.is_none() {
            while let Some(id) = self.details.pop_front() {
                if crate::account::api::validate_device_id(&id).is_err() {
                    continue;
                }
                let version = self.version(&id);
                let target = id.clone();
                let client = Arc::clone(client);
                self.detail_read = Some(DetailRead {
                    id,
                    version,
                    task: tokio::spawn(async move { client.device_detail(&target).await }),
                });
                break;
            }
        }
    }
}
