//! Devices connected to before, as one list with one row per device: this
//! device's own record of the hosts it reached (`known_hosts.json`: direct
//! and server connections, signed in or not) and, signed in, the account's
//! recent short-number visits, which the server keeps so they follow the
//! account to its other devices.
use crate::account::assist::SavedDevice;
use crate::transport::direct::store::{self, KnownHost};

#[derive(Clone)]
pub(super) struct Recent {
    pub title: String,
    pub device_id: Option<String>,
    pub short_number: Option<String>,
    pub address: Option<String>,
    /// It admitted this device without a code last time.
    pub trusted: bool,
    /// Unix seconds of the last connection.
    pub last: i64,
    /// The account's record of it, if any.
    pub saved: Option<SavedDevice>,
}

/// `known` and the account's `saved` visits merged by device, in list order.
pub(super) fn merge(known: Vec<KnownHost>, saved: &[SavedDevice]) -> Vec<Recent> {
    let mut rows: Vec<Recent> = known
        .into_iter()
        .map(|host| Recent {
            title: host.name,
            device_id: Some(host.device_id),
            short_number: (!host.connect_id.is_empty()).then_some(host.connect_id),
            address: (!host.address.is_empty()).then_some(host.address),
            trusted: host.trusted,
            last: host.last_seen,
            saved: None,
        })
        .collect();
    for item in saved {
        let id = store::device_id_of(&item.public_key);
        if let Some(row) = rows.iter_mut().find(|row| id.is_some() && row.device_id == id) {
            row.short_number = Some(item.connect_id.clone());
            row.saved = Some(item.clone());
            row.last = row.last.max(item.last_connected_at.unwrap_or(0));
            continue;
        }
        rows.push(Recent {
            title: item.title().to_owned(),
            device_id: id,
            short_number: Some(item.connect_id.clone()),
            address: None,
            trusted: false,
            last: item.last_connected_at.unwrap_or(0),
            saved: Some(item.clone()),
        });
    }
    rows.sort_by(|a, b| {
        crate::account::api::list_order(
            &a.title,
            a.device_id.as_deref().unwrap_or_default(),
            &b.title,
            b.device_id.as_deref().unwrap_or_default(),
        )
        .then_with(|| a.address.cmp(&b.address))
    });
    rows
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn one_row_per_device_whatever_recorded_it() {
        let key = "wsTGg62JWf4fq1kmkJ1nEoLcx2KHJZ3SmcLJfT37KOc".to_owned();
        let id = store::device_id_of(&key).expect("a device key");
        let known = vec![KnownHost {
            device_id: id.clone(),
            public_key: key.clone(),
            name: "Desk".into(),
            address: "192.168.1.5:28891".into(),
            last_seen: 10,
            ..Default::default()
        }];
        let visit = |key: &str, connect_id: &str, at: i64| SavedDevice {
            publisher_device_id: String::new(),
            connect_id: connect_id.into(),
            remark: String::new(),
            is_favorite: false,
            last_connected_at: Some(at),
            favorited_at: None,
            saved_code: String::new(),
            public_key: key.into(),
        };
        let rows = merge(known, &[visit(&key, "123456789", 20), visit("", "987654321", 5)]);
        assert_eq!(rows.len(), 2);
        let desk = rows.iter().find(|r| r.device_id.as_deref() == Some(id.as_str())).unwrap();
        assert_eq!(desk.short_number.as_deref(), Some("123456789"));
        assert_eq!((desk.last, desk.address.is_some(), desk.saved.is_some()), (20, true, true));
        assert!(rows.iter().any(|r| r.device_id.is_none() && r.title == "987654321"));
    }
}
