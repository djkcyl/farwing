//! Settings an administrator can change online (`docs/web-console.md` §6):
//! stored in the database, they override the configuration file's values
//! until the next change. Everything else stays in the configuration file.

use crate::config::Config;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RuntimeSettings {
    /// Name shown in `GET /api/v2/server` and the web console.
    pub instance_name: String,
    /// Devices without an account may host and connect (guests, visitors).
    pub guests: bool,
    /// Accounts may be registered with an invitation code.
    pub registration: bool,
    /// Session bitrate cap handed to clients.
    pub bitrate_limit_mbps: Option<u32>,
    /// What browser controllers may do at most (a subset of `caps::WEB`
    /// with `view`); `["view", "audio"]` makes the web view-only.
    pub web_caps: Vec<String>,
}

fn web_caps_default() -> Vec<String> {
    farwing_protocol::caps::WEB.iter().map(|c| (*c).to_owned()).collect()
}

impl RuntimeSettings {
    pub fn from_config(cfg: &Config) -> Self {
        Self {
            instance_name: cfg.instance_name.clone(),
            guests: cfg.guest.enabled,
            registration: false,
            bitrate_limit_mbps: cfg.bitrate_limit_mbps,
            web_caps: web_caps_default(),
        }
    }

    /// The configuration's values with the stored overrides applied.
    pub fn load(cfg: &Config, stored: &HashMap<String, String>) -> Self {
        let mut s = Self::from_config(cfg);
        if let Some(v) = stored.get("instance_name") {
            s.instance_name = v.clone();
        }
        if let Some(v) = stored.get("guests") {
            s.guests = v == "true";
        }
        if let Some(v) = stored.get("registration") {
            s.registration = v == "true";
        }
        if let Some(v) = stored.get("bitrate_limit_mbps") {
            s.bitrate_limit_mbps = v.parse().ok().filter(|n| *n > 0);
        }
        if let Some(v) = stored.get("web_caps") {
            let caps: Vec<String> = v.split(',').filter(|c| !c.is_empty()).map(str::to_owned).collect();
            let candidate = Self { web_caps: caps, ..s.clone() };
            if candidate.validate().is_ok() {
                s = candidate;
            }
        }
        s
    }

    /// Key-value pairs to store.
    pub fn pairs(&self) -> Vec<(&'static str, String)> {
        vec![
            ("instance_name", self.instance_name.clone()),
            ("guests", self.guests.to_string()),
            ("registration", self.registration.to_string()),
            (
                "bitrate_limit_mbps",
                self.bitrate_limit_mbps.map(|n| n.to_string()).unwrap_or_default(),
            ),
            ("web_caps", self.web_caps.join(",")),
        ]
    }

    pub fn validate(&self) -> Result<(), &'static str> {
        let name = self.instance_name.trim();
        if name.is_empty() || name.chars().count() > 64 {
            return Err("实例名称应为 1–64 个字符");
        }
        if self.bitrate_limit_mbps.is_some_and(|n| n == 0 || n > 10_000) {
            return Err("码率上限应为 1–10000 Mbit/s");
        }
        if !self.web_caps.iter().any(|c| c == farwing_protocol::caps::VIEW)
            || self.web_caps.iter().any(|c| !farwing_protocol::caps::WEB.contains(&c.as_str()))
        {
            return Err("网页端能力必须包含观看，且只能从网页端支持的能力中选择");
        }
        Ok(())
    }
}
