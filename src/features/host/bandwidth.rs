//! The bandwidth each controller device reached in its last session, so the
//! next session starts near it instead of at a fixed fraction of the tier
//! (`docs/protocol/v2-design.md` §6, start bitrate).

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

const MAX_AGE_SECONDS: i64 = 7 * 24 * 3600;
const ENTRIES: usize = 64;
/// Sessions shorter than this did not converge.
pub(crate) const MIN_SESSION: std::time::Duration = std::time::Duration::from_secs(15);

#[derive(Default, Serialize, Deserialize)]
struct Store {
    #[serde(default)]
    devices: BTreeMap<String, Entry>,
}

#[derive(Clone, Copy, Serialize, Deserialize)]
struct Entry {
    bps: u32,
    saved_at: i64,
}

static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn path() -> Option<std::path::PathBuf> {
    Some(
        crate::product::local_data_dir()
            .ok()?
            .join("cache")
            .join("bandwidth.json"),
    )
}

fn read() -> Store {
    path()
        .and_then(|path| std::fs::read(path).ok())
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_default()
}

/// The last estimate for `device`, if recent.
pub(crate) fn remembered(device: &str) -> Option<u32> {
    if device.is_empty() {
        return None;
    }
    let _guard = LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let now = chrono::Utc::now().timestamp();
    read()
        .devices
        .get(device)
        .filter(|entry| now - entry.saved_at < MAX_AGE_SECONDS && entry.bps > 0)
        .map(|entry| entry.bps)
}

/// Record the estimate a session with `device` converged to.
pub(crate) fn remember(device: &str, bps: u32) {
    if device.is_empty() || bps == 0 {
        return;
    }
    let Some(path) = path() else {
        return;
    };
    let _guard = LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let mut store = read();
    let now = chrono::Utc::now().timestamp();
    store
        .devices
        .retain(|_, entry| now - entry.saved_at < MAX_AGE_SECONDS);
    store.devices.insert(
        device.to_owned(),
        Entry {
            bps,
            saved_at: now,
        },
    );
    while store.devices.len() > ENTRIES {
        let oldest = store
            .devices
            .iter()
            .min_by_key(|(_, entry)| entry.saved_at)
            .map(|(device, _)| device.clone());
        match oldest {
            Some(device) => store.devices.remove(&device),
            None => break,
        };
    }
    let result = (|| -> anyhow::Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&path, serde_json::to_vec(&store)?)?;
        Ok(())
    })();
    if let Err(error) = result {
        tracing::debug!(%error, "bandwidth memory could not be saved");
    }
}

/// The start bitrate for a session: near the remembered estimate (with a
/// margin), within the tier's bounds.
pub(crate) fn start_rate(remembered: Option<u32>, floor: u32, initial: u32, maximum: u32) -> u32 {
    match remembered {
        Some(bps) => ((f64::from(bps) * 0.8) as u32).clamp(floor.min(maximum), maximum),
        None => initial,
    }
}

#[cfg(test)]
mod tests {
    use super::start_rate;

    #[test]
    fn start_rate_uses_the_memory_within_bounds() {
        assert_eq!(start_rate(None, 300_000, 3_000_000, 20_000_000), 3_000_000);
        assert_eq!(start_rate(Some(10_000_000), 300_000, 3_000_000, 20_000_000), 8_000_000);
        assert_eq!(start_rate(Some(100_000_000), 300_000, 3_000_000, 20_000_000), 20_000_000);
        assert_eq!(start_rate(Some(10_000), 300_000, 3_000_000, 20_000_000), 300_000);
    }
}
