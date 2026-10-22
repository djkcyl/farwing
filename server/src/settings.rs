//! The settings in force: the current snapshot every request reads, its
//! storage in the database and the notification components subscribe to
//! (`docs/protocol/deployment-design.md` §2.3, §3).

use crate::config::Config;
use anyhow::{Context, Result};
use parking_lot::RwLock;
use rusqlite::Connection;
use std::net::SocketAddr;
use std::path::Path;
use std::sync::Arc;
use tokio::sync::watch;

/// The settings document in the `settings` table.
const KEY: &str = "server_config";

pub struct Settings {
    current: RwLock<Arc<Config>>,
    changed: watch::Sender<Arc<Config>>,
}

impl Settings {
    pub fn new(cfg: Config) -> Self {
        let cfg = Arc::new(cfg);
        let (changed, _) = watch::channel(Arc::clone(&cfg));
        Self {
            current: RwLock::new(cfg),
            changed,
        }
    }

    /// The settings in force.
    pub fn get(&self) -> Arc<Config> {
        Arc::clone(&self.current.read())
    }

    pub fn subscribe(&self) -> watch::Receiver<Arc<Config>> {
        self.changed.subscribe()
    }

    /// Put `cfg` in force (already validated and applied by the components).
    pub fn replace(&self, cfg: Config) -> Arc<Config> {
        let cfg = Arc::new(cfg);
        *self.current.write() = Arc::clone(&cfg);
        self.changed.send_replace(Arc::clone(&cfg));
        cfg
    }
}

/// The stored settings, or the initial ones (then stored) on first start.
/// `listen` (`--listen`) fixes the listen address.
pub fn load(c: &Connection, data_dir: &Path, listen: Option<SocketAddr>) -> Result<Config> {
    let stored = crate::webdb::settings(c)?.remove(KEY);
    let mut cfg = match stored {
        Some(json) => serde_json::from_str::<Config>(&json)
            .context("the stored settings cannot be read")?,
        None => {
            let cfg = Config::initial(
                data_dir.to_path_buf(),
                listen.unwrap_or_else(|| Config::default().listen),
            );
            save(c, &cfg)?;
            cfg
        }
    };
    cfg.data_dir = data_dir.to_path_buf();
    if let Some(listen) = listen {
        cfg.listen = listen;
        cfg.listen_locked = true;
    }
    Ok(cfg)
}

pub fn save(c: &Connection, cfg: &Config) -> crate::db::R<()> {
    crate::webdb::set_setting(c, KEY, &serde_json::to_string(cfg).expect("serializable"))
}
