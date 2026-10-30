//! Who may use the relays and forwarding, and how much
//! (`docs/protocol/egress-policy.md` §3).

use super::Payer;
use crate::config::{EgressConfig, EgressPolicy};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// An account's own policy; `None` takes the server's default for
/// accounts. `rate_mbps` and `quota_gb` 0: unlimited.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct UserPolicy {
    pub relay: Option<bool>,
    pub forward: Option<bool>,
    pub rate_mbps: Option<u32>,
    pub quota_gb: Option<u32>,
}

impl UserPolicy {
    /// Within the settings' ranges (0 meaning unlimited).
    pub fn valid(&self) -> bool {
        self.rate_mbps
            .is_none_or(|n| n == 0 || crate::config::EGRESS_RATE_MBPS.contains(&n))
            && self
                .quota_gb
                .is_none_or(|n| n == 0 || crate::config::EGRESS_QUOTA_GB.contains(&n))
    }
}

/// A payer's policy in force.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub struct Effective {
    pub relay: bool,
    pub forward: bool,
    /// Bits per second; none: unlimited.
    pub rate_bps: Option<u64>,
    /// Bytes per month; none: unlimited.
    pub quota_bytes: Option<u64>,
}

const MBIT: u64 = 1_000_000;
const GB: u64 = 1_000_000_000;

impl Effective {
    const UNLIMITED: Self = Self {
        relay: true,
        forward: true,
        rate_bps: None,
        quota_bytes: None,
    };

    fn of(defaults: &EgressPolicy, own: Option<&UserPolicy>) -> Self {
        let own = own.copied().unwrap_or_default();
        let limit = |own: Option<u32>, default: Option<u32>, unit: u64| match own {
            Some(0) => None,
            Some(n) => Some(u64::from(n) * unit),
            None => default.map(|n| u64::from(n) * unit),
        };
        Self {
            relay: own.relay.unwrap_or(defaults.relay),
            forward: own.forward.unwrap_or(defaults.forward),
            rate_bps: limit(own.rate_mbps, defaults.rate_mbps, MBIT),
            quota_bytes: limit(own.quota_gb, defaults.quota_gb, GB),
        }
    }
}

/// The accounts' own policies and whether they administer the server.
#[derive(Default)]
pub struct Policies {
    config: EgressConfig,
    users: HashMap<String, (bool, UserPolicy)>,
}

impl Policies {
    pub fn new(config: EgressConfig, users: Vec<(String, bool, UserPolicy)>) -> Self {
        Self {
            config,
            users: users
                .into_iter()
                .map(|(id, admin, policy)| (id, (admin, policy)))
                .collect(),
        }
    }

    pub fn configure(&mut self, config: EgressConfig) {
        self.config = config;
    }

    pub fn config(&self) -> &EgressConfig {
        &self.config
    }

    /// Account `id` changed (created, its role or policy); `None`: deleted.
    pub fn set_user(&mut self, id: &str, user: Option<(bool, UserPolicy)>) {
        match user {
            Some(user) => {
                self.users.insert(id.to_owned(), user);
            }
            None => {
                self.users.remove(id);
            }
        }
    }

    /// `payer`'s policy in force. Administrators are not limited; an
    /// account the server does not know gets the accounts' default.
    pub fn resolve(&self, payer: &Payer) -> Effective {
        match payer {
            Payer::Guests => Effective::of(&self.config.guests, None),
            Payer::User(id) => match self.users.get(id) {
                Some((true, _)) => Effective::UNLIMITED,
                Some((false, own)) => Effective::of(&self.config.users, Some(own)),
                None => Effective::of(&self.config.users, None),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> EgressConfig {
        EgressConfig {
            limit_mbps: None,
            users: EgressPolicy {
                relay: true,
                forward: false,
                rate_mbps: Some(20),
                quota_gb: Some(100),
            },
            guests: EgressPolicy {
                relay: false,
                ..EgressPolicy::default()
            },
        }
    }

    #[test]
    fn an_account_takes_its_own_settings_over_the_defaults() {
        let policies = Policies::new(
            config(),
            vec![
                ("plain".into(), false, UserPolicy::default()),
                (
                    "own".into(),
                    false,
                    UserPolicy {
                        relay: Some(false),
                        forward: Some(true),
                        rate_mbps: Some(0),
                        quota_gb: Some(5),
                    },
                ),
                ("admin".into(), true, UserPolicy { relay: Some(false), ..UserPolicy::default() }),
            ],
        );
        let plain = policies.resolve(&Payer::User("plain".into()));
        assert_eq!(
            plain,
            Effective {
                relay: true,
                forward: false,
                rate_bps: Some(20 * MBIT),
                quota_bytes: Some(100 * GB)
            }
        );
        let own = policies.resolve(&Payer::User("own".into()));
        assert_eq!(
            own,
            Effective {
                relay: false,
                forward: true,
                rate_bps: None,
                quota_bytes: Some(5 * GB)
            }
        );
        assert_eq!(policies.resolve(&Payer::User("admin".into())), Effective::UNLIMITED);
        assert!(!policies.resolve(&Payer::Guests).relay);
        assert_eq!(policies.resolve(&Payer::User("unknown".into())), plain);
    }

    #[test]
    fn own_limits_stay_in_range() {
        assert!(UserPolicy { rate_mbps: Some(0), quota_gb: Some(0), ..UserPolicy::default() }.valid());
        assert!(!UserPolicy { rate_mbps: Some(20_000), ..UserPolicy::default() }.valid());
    }
}
