//! Account metadata; stable hardware is invalidated by device version/platform.
use crate::account::api::{DeviceDetail, DeviceGroups};
use std::collections::HashMap;

#[derive(Clone)]
pub(super) struct CachedDetail {
    pub value: std::result::Result<DeviceDetail, String>,
    pub refresh_error: Option<String>,
    version: String,
    platform: farwing_protocol::Platform,
}
#[derive(Clone)]
pub(super) struct Catalog {
    pub groups: DeviceGroups,
    pub details: HashMap<String, CachedDetail>,
    pub suggested_name: String,
    pub features: crate::account::feature_ability::FeatureCatalog,
}
impl Catalog {
    pub fn from_groups(
        groups: DeviceGroups,
        previous: Option<Self>,
        mut suggested_name: String,
        features: crate::account::feature_ability::FeatureCatalog,
    ) -> Self {
        let details = previous.map(|c| c.details).unwrap_or_default();
        if groups
            .entries()
            .any(|(_, d)| d.device_id != groups.current_device_id && d.alias == suggested_name)
        {
            suggested_name.push('-');
            suggested_name.push_str(&groups.current_device_id[12..]);
        }
        let mut catalog = Self {
            groups,
            details,
            suggested_name,
            features,
        };
        catalog.prune_details();
        catalog
    }
    pub fn prune_details(&mut self) {
        self.details.retain(|id, cached| {
            self.groups.entries().any(|(_, d)| {
                &d.device_id == id
                    && cached.version == d.version_name
                    && cached.platform == d.platform
            })
        });
    }
    pub fn store_detail(&mut self, id: &str, value: std::result::Result<DeviceDetail, String>) {
        if let Err(error) = &value
            && let Some(cached) = self.details.get_mut(id)
            && cached.value.is_ok()
        {
            cached.refresh_error = Some(error.clone());
            return;
        }
        if let Some((_, d)) = self.groups.entries().find(|(_, d)| d.device_id == id) {
            self.details.insert(
                id.into(),
                CachedDetail {
                    value,
                    refresh_error: None,
                    version: d.version_name.clone(),
                    platform: d.platform,
                },
            );
        }
    }
}
