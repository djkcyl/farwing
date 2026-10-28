//! Per-profile network switches (`network.json`): NAT traversal (§11) and
//! direct connections (§12). Sessions read the file when they start; the
//! settings page uses the in-process copy, and the direct endpoint follows
//! [`subscribe`]. With the background service installed, the host-side
//! switches of the hosting (resident) process are set through the resident
//! pipe (`crate::transport::direct::managed`).
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::sync::Mutex;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub(crate) struct Settings {
    /// Predict a symmetric peer's ports (and let it predict ours).
    pub aggressive_punching: bool,
    /// Request PCP / NAT-PMP / UPnP-IGD mappings for the ICE UDP port.
    pub port_mapping: bool,
    /// Accept direct connections on [`Settings::direct_port`] (§12.1).
    pub direct: bool,
    /// TCP port of the direct endpoint; direct sessions use the UDP ports
    /// from the same number.
    pub direct_port: u16,
    /// Announce this host on the LAN and answer discovery queries (§12.4).
    pub lan_discovery: bool,
    /// Host without the server: direct connections only (§12.7).
    pub serverless: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            aggressive_punching: true,
            port_mapping: true,
            direct: true,
            direct_port: farwing_protocol::direct::DEFAULT_PORT,
            lan_discovery: true,
            serverless: false,
        }
    }
}

/// Lowest and highest direct endpoint port the settings accept.
pub(crate) const DIRECT_PORTS: std::ops::RangeInclusive<u16> = 1024..=65_000;

impl Settings {
    pub(crate) fn validate(&self) -> Result<()> {
        anyhow::ensure!(
            DIRECT_PORTS.contains(&self.direct_port),
            "{}",
            (crate::i18n::t().session.direct_port_range)(*DIRECT_PORTS.start(), *DIRECT_PORTS.end())
        );
        Ok(())
    }
}

static CACHE: Mutex<Option<Settings>> = Mutex::new(None);

/// The settings as this process last loaded or saved them.
static CHANGES: std::sync::LazyLock<tokio::sync::watch::Sender<Settings>> =
    std::sync::LazyLock::new(|| tokio::sync::watch::channel(Settings::default()).0);

fn publish(settings: Settings) {
    *CACHE.lock().unwrap_or_else(|e| e.into_inner()) = Some(settings);
    CHANGES.send_if_modified(|current| {
        let changed = *current != settings;
        *current = settings;
        changed
    });
}

/// Changes made in this process (or seen when reloading the file).
pub(crate) fn subscribe() -> tokio::sync::watch::Receiver<Settings> {
    current();
    CHANGES.subscribe()
}

fn path() -> Result<std::path::PathBuf> {
    Ok(crate::product::local_data_dir()?.join("network.json"))
}

fn read(path: &std::path::Path) -> Result<Settings> {
    Ok(crate::storage::read(path)
        .context(crate::i18n::t().session.network_settings_read_failed)?
        .unwrap_or_default())
}

/// The stored settings; defaults when missing or unreadable.
pub(crate) fn load() -> Settings {
    let settings = path().and_then(|path| read(&path)).unwrap_or_else(|error| {
        tracing::warn!(error = %format!("{error:#}"), "network settings unavailable; using defaults");
        Settings::default()
    });
    publish(settings);
    settings
}

/// The settings as last loaded or saved in this process.
pub(crate) fn current() -> Settings {
    let cached = *CACHE.lock().unwrap_or_else(|e| e.into_inner());
    cached.unwrap_or_else(load)
}

pub(crate) fn save(settings: Settings) -> Result<()> {
    settings.validate()?;
    let result = crate::storage::write(&path()?, &settings);
    result.context(crate::i18n::t().session.network_settings_save_failed)?;
    publish(settings);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_and_partial_files() {
        assert_eq!(
            serde_json::from_str::<Settings>("{}").unwrap(),
            Settings::default()
        );
        let partial: Settings = serde_json::from_str(r#"{"port_mapping":false}"#).unwrap();
        assert!(partial.aggressive_punching && !partial.port_mapping);
        assert!(partial.direct && partial.lan_discovery && !partial.serverless);
        assert_eq!(partial.direct_port, farwing_protocol::direct::DEFAULT_PORT);
    }

    #[test]
    fn direct_port_is_validated() {
        assert!(Settings::default().validate().is_ok());
        for port in [0, 80, 1023, 65_001, u16::MAX] {
            let settings = Settings {
                direct_port: port,
                ..Default::default()
            };
            assert!(settings.validate().is_err(), "{port}");
        }
    }
}
