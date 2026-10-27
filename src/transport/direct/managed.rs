//! Host-side records in service mode (`docs/protocol/v2-design.md` §12.10).
//! With the background service installed the host runs in the service's
//! resident process (SYSTEM). Its trusted devices and network switches stay
//! in that process's own profile, writable only by SYSTEM and
//! administrators. The installing user's GUI and CLI see them in the
//! resident snapshot and change them with [`Action`]s over the authorised
//! resident pipe; the resident validates every action. Without the service
//! the GUI or CLI process hosts and applies actions itself ([`perform`]).
use super::store::{self, TrustedDevice};
use crate::transport::nat::settings::Settings;
use crate::i18n::t;
use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use std::sync::Mutex;

/// What the hosting process uses for direct connections right now.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub(crate) struct HostDirect {
    #[serde(default)]
    pub settings: Settings,
    /// Devices this host trusts (`docs/access-model.md` §2.2).
    #[serde(default)]
    pub trusted: Vec<TrustedDevice>,
    /// TCP port of the direct endpoint, when it is listening.
    #[serde(default)]
    pub listening_port: Option<u16>,
}

impl HostDirect {
    /// This (hosting) process's state.
    pub(crate) fn current() -> Self {
        Self {
            settings: crate::transport::nat::settings::current(),
            trusted: store::trusted(),
            listening_port: super::host::listening_port(),
        }
    }
}

/// A change to the host side, made by the user's GUI or CLI.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) enum Action {
    /// Trust a device (or replace what it may do).
    Trust {
        device_id: String,
        public_key: String,
        name: String,
        grants: Vec<String>,
        /// Paired: it trusts this device too. Kept once set.
        #[serde(default)]
        paired: bool,
    },
    /// What a trusted device may do.
    SetGrants { device_id: String, grants: Vec<String> },
    /// Stop trusting a device and end its sessions.
    Remove { device_id: String },
    /// Every network switch (NAT traversal and direct connections).
    Network(Settings),
}

/// What the hosting process does after an action besides storing it.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct Applied {
    /// End this device's current sessions.
    pub end_sessions_of: Option<String>,
    /// "Direct connections only" changed: the host's server connection must
    /// be rebuilt.
    pub restart_presence: bool,
}

fn checked_caps(grants: &[String]) -> Result<Vec<String>> {
    ensure!(grants.len() <= 64, "{}", t().session.caps_too_many);
    if let Some(unknown) = grants
        .iter()
        .find(|cap| !farwing_protocol::caps::is_known(cap))
    {
        bail!("{}", (t().session.caps_unknown)(unknown));
    }
    Ok(farwing_protocol::caps::normalize(grants))
}

fn checked_device(device_id: &str) -> Result<()> {
    crate::account::api::validate_device_id(device_id).context(t().session.device_id_invalid)
}

/// Check an action before applying it (resident side).
pub(crate) fn validate(action: &Action) -> Result<()> {
    match action {
        Action::Trust {
            device_id,
            public_key,
            name,
            grants,
            ..
        } => {
            checked_device(device_id)?;
            ensure!(
                store::device_id_of(public_key).as_deref() == Some(device_id.as_str()),
                "{}",
                t().session.device_key_mismatch
            );
            ensure!(name.chars().count() <= 128, "{}", t().session.device_name_too_long);
            checked_caps(grants).map(drop)
        }
        Action::SetGrants { device_id, grants } => {
            checked_device(device_id)?;
            checked_caps(grants).map(drop)
        }
        Action::Remove { device_id } => checked_device(device_id),
        Action::Network(settings) => settings.validate(),
    }
}

/// Apply an action to this (hosting) process's records.
pub(crate) fn apply(action: Action) -> Result<Applied> {
    validate(&action)?;
    match action {
        Action::Trust {
            device_id,
            public_key,
            name,
            grants,
            paired,
        } => {
            let now = chrono::Utc::now().timestamp();
            let previous = store::find_trusted(&public_key);
            let new = previous.is_none();
            store::save_trusted(TrustedDevice {
                device_id: device_id.clone(),
                public_key,
                name: super::sanitize_name(&name),
                grants: store::with_view(&checked_caps(&grants)?),
                trusted_at: previous.as_ref().map_or(now, |p| p.trusted_at),
                last_seen: previous.as_ref().map_or(0, |p| p.last_seen),
                paired: paired || previous.is_some_and(|p| p.paired),
            })?;
            tracing::info!(device = %device_id, "device trusted");
            if new {
                crate::account::records::device_event(
                    farwing_protocol::api::device_event::TRUSTED_ADD,
                    &device_id,
                    &super::sanitize_name(&name),
                );
            }
            Ok(Applied::default())
        }
        Action::SetGrants { device_id, grants } => {
            ensure!(
                store::set_trusted_grants(&device_id, checked_caps(&grants)?)?,
                "{}",
                (t().session.device_not_trusted)(&device_id)
            );
            Ok(Applied::default())
        }
        Action::Remove { device_id } => {
            let name = store::trusted()
                .into_iter()
                .find(|d| d.device_id == device_id)
                .map(|d| d.name);
            if store::remove_trusted(&device_id)? > 0 {
                crate::account::records::device_event(
                    farwing_protocol::api::device_event::TRUSTED_REMOVE,
                    &device_id,
                    &name.unwrap_or_default(),
                );
            }
            Ok(Applied {
                end_sessions_of: Some(device_id),
                restart_presence: false,
            })
        }
        Action::Network(settings) => {
            let before = crate::transport::nat::settings::current();
            crate::transport::nat::settings::save(settings)?;
            Ok(Applied {
                end_sessions_of: None,
                restart_presence: before.serverless != settings.serverless,
            })
        }
    }
}

/// End the current sessions of `device_id` on `host`.
pub(crate) fn end_sessions(host: &crate::features::host::Handle, device_id: &str) {
    for connection in host.status().connections {
        if connection.device_id == device_id && !connection.session_id.is_empty() {
            host.disconnect_session(&connection.session_id);
        }
    }
}

/// Apply `action` in this process, which hosts (no background service).
pub(crate) fn perform(host: Option<&crate::features::host::Handle>, action: Action) -> Result<()> {
    let applied = apply(action)?;
    if let (Some(device), Some(host)) = (applied.end_sessions_of, host) {
        end_sessions(host, &device);
    }
    Ok(())
}

/// The resident's state as last reported to this (GUI) process.
static REMOTE: Mutex<Option<HostDirect>> = Mutex::new(None);

fn remote_slot() -> std::sync::MutexGuard<'static, Option<HostDirect>> {
    REMOTE.lock().unwrap_or_else(|e| e.into_inner())
}

/// Record the resident snapshot's state (GUI side).
pub(crate) fn remember(state: HostDirect) {
    *remote_slot() = Some(state);
}

/// The resident's state, once a snapshot has arrived.
pub(crate) fn remote() -> Option<HostDirect> {
    remote_slot().clone()
}

/// Show an accepted change before the next snapshot confirms it.
fn assume(action: &Action) {
    let mut slot = remote_slot();
    let Some(state) = slot.as_mut() else {
        return;
    };
    match action {
        Action::Trust {
            device_id,
            public_key,
            name,
            grants,
            paired,
        } => {
            let was_paired = state
                .trusted
                .iter()
                .any(|d| &d.device_id == device_id && d.paired);
            state.trusted.retain(|d| &d.device_id != device_id);
            state.trusted.push(TrustedDevice {
                device_id: device_id.clone(),
                public_key: public_key.clone(),
                name: name.clone(),
                grants: store::with_view(grants),
                trusted_at: chrono::Utc::now().timestamp(),
                last_seen: 0,
                paired: *paired || was_paired,
            });
        }
        Action::SetGrants { device_id, grants } => {
            let grants = store::with_view(grants);
            for device in state.trusted.iter_mut().filter(|d| &d.device_id == device_id) {
                device.grants = grants.clone();
            }
        }
        Action::Remove { device_id } => state.trusted.retain(|d| &d.device_id != device_id),
        Action::Network(settings) => state.settings = *settings,
    }
}

/// In service mode the network switches shown and used are the resident's;
/// keep this process's copy (used by its controller sessions) the same.
pub(crate) fn sync_local_settings(remote: &Settings) {
    static LAST: Mutex<Option<Settings>> = Mutex::new(None);
    let mut last = LAST.lock().unwrap_or_else(|e| e.into_inner());
    if last.as_ref() == Some(remote) {
        return;
    }
    *last = Some(*remote);
    if crate::transport::nat::settings::current() != *remote
        && let Err(error) = crate::transport::nat::settings::save(*remote)
    {
        tracing::warn!(error = %format!("{error:#}"), "service network settings not copied locally");
    }
}

// Installing the service hands the user's host-side records to the
// resident through the service vault (writable by the installing user and
// SYSTEM only); uninstalling it copies the resident's back. Trusted and
// paired devices and the direct switches thus survive both.
const HANDOVER: &str = "direct-handover.json";

#[derive(Serialize, Deserialize)]
struct Handover {
    settings: Settings,
    trusted: Vec<TrustedDevice>,
}

/// Installation: the user's records, for the resident to adopt.
pub(crate) fn hand_over() -> Result<()> {
    let state = Handover {
        settings: crate::transport::nat::settings::current(),
        trusted: store::trusted(),
    };
    let path = crate::platform::windows::host_service::vault::root()?.join(HANDOVER);
    std::fs::write(path, serde_json::to_vec(&state)?).context(t().session.handover_save_failed)
}

/// The resident adopts handed-over records once, replacing its own (which
/// may be left from an earlier installation). Returns whether there were any.
pub(crate) fn take_over() -> Result<bool> {
    let path = crate::platform::windows::host_service::vault::root()?.join(HANDOVER);
    let bytes = match std::fs::read(&path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error.into()),
    };
    let state: Handover = match serde_json::from_slice(&bytes) {
        Ok(state) => state,
        Err(error) => {
            // Never retried: kept beside for a look, out of the way.
            let _ = std::fs::rename(&path, path.with_extension("json.invalid"));
            return Err(error).context(t().session.handover_invalid);
        }
    };
    apply(Action::Network(state.settings))?;
    let mut trusted = Vec::new();
    for device in state.trusted {
        let checked = validate(&Action::Trust {
            device_id: device.device_id.clone(),
            public_key: device.public_key.clone(),
            name: device.name.clone(),
            grants: device.grants.clone(),
            paired: device.paired,
        })
        .and_then(|()| checked_caps(&device.grants));
        match checked {
            Ok(grants) => trusted.push(TrustedDevice {
                name: super::sanitize_name(&device.name),
                grants: store::with_view(&grants),
                ..device
            }),
            Err(error) => tracing::warn!(
                device = %device.device_id,
                error = %format!("{error:#}"),
                "handed-over trusted device skipped"
            ),
        }
    }
    let count = trusted.len();
    store::replace_trusted(trusted)?;
    // Removed once adopted: a failure above is retried next time.
    std::fs::remove_file(&path)?;
    tracing::info!(trusted = count, "adopted the installing user's direct records");
    Ok(true)
}

/// Uninstallation, while the resident still runs: its records become the
/// user's. Without them the user's previous records stay.
pub(crate) fn take_back() {
    let result = query().and_then(|(_, state)| {
        crate::transport::nat::settings::save(state.settings)?;
        store::replace_trusted(state.trusted)
    });
    if let Err(error) = result {
        tracing::warn!(error = %format!("{error:#}"), "background service direct records not copied back");
    }
}

/// Whether host-side direct state belongs to the background service.
pub(crate) fn managed() -> bool {
    crate::platform::windows::host_service::resident::managed()
}

/// The resident's account generation and state (CLI).
pub(crate) fn query() -> Result<(String, HostDirect)> {
    use crate::platform::windows::host_service::resident::{Reply, Request, call};
    match call(Request::DirectState)? {
        Reply::DirectState { account, state } => Ok((account, *state)),
        _ => bail!("{}", t().session.error_background_reply_invalid),
    }
}

/// Perform `action` in the resident (blocking; call off the UI thread).
pub(crate) fn request(account: String, action: Action) -> Result<()> {
    use crate::platform::windows::host_service::resident::{Reply, Request, call};
    validate(&action)?;
    match call(Request::Direct {
        account,
        action: action.clone(),
    })? {
        Reply::Done => {
            assume(&action);
            Ok(())
        }
        _ => bail!("{}", t().session.error_background_reply_invalid),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(n: u8) -> String {
        farwing_protocol::crypto::b64(&[n; 32])
    }

    fn trust(n: u8) -> Action {
        Action::Trust {
            device_id: store::device_id_of(&key(n)).unwrap(),
            public_key: key(n),
            name: format!("PC{n}"),
            grants: vec!["control".into()],
            paired: false,
        }
    }

    #[test]
    fn actions_are_validated() {
        assert!(validate(&trust(1)).is_ok());
        assert!(
            validate(&Action::Trust {
                device_id: store::device_id_of(&key(2)).unwrap(),
                public_key: key(1),
                name: "x".into(),
                grants: vec![],
                paired: false,
            })
            .is_err(),
            "the ID must be the key's"
        );
        assert!(validate(&Action::SetGrants { device_id: "../../x".into(), grants: vec![] }).is_err());
        assert!(
            validate(&Action::SetGrants {
                device_id: store::device_id_of(&key(1)).unwrap(),
                grants: vec!["root".into()]
            })
            .is_err()
        );
        assert!(validate(&Action::Remove { device_id: String::new() }).is_err());
        let mut settings = Settings::default();
        assert!(validate(&Action::Network(settings)).is_ok());
        settings.direct_port = 80;
        assert!(validate(&Action::Network(settings)).is_err());
    }

    #[test]
    fn pipe_messages_round_trip() {
        use crate::platform::windows::host_service::resident::{Reply, Request};
        let request = Request::Direct {
            account: "acct".into(),
            action: Action::SetGrants {
                device_id: "abcd".into(),
                grants: vec!["control".into()],
            },
        };
        let bytes = serde_json::to_vec(&request).unwrap();
        match serde_json::from_slice::<Request>(&bytes).unwrap() {
            Request::Direct { account, action: Action::SetGrants { device_id, grants } } => {
                assert_eq!((account.as_str(), device_id.as_str()), ("acct", "abcd"));
                assert_eq!(grants, vec!["control".to_owned()]);
            }
            _ => panic!("wrong request"),
        }
        let reply = Reply::DirectState {
            account: "acct".into(),
            state: Box::new(HostDirect::default()),
        };
        let bytes = serde_json::to_vec(&reply).unwrap();
        assert!(matches!(
            serde_json::from_slice::<Reply>(&bytes).unwrap(),
            Reply::DirectState { account, .. } if account == "acct"
        ));
    }

    #[test]
    fn accepted_changes_show_before_the_next_snapshot() {
        remember(HostDirect::default());
        assume(&trust(1));
        assume(&trust(2));
        let first = store::device_id_of(&key(1)).unwrap();
        assume(&Action::SetGrants {
            device_id: first.clone(),
            grants: vec!["files".into()],
        });
        assume(&Action::Remove {
            device_id: store::device_id_of(&key(2)).unwrap(),
        });
        let settings = Settings {
            lan_discovery: false,
            ..Default::default()
        };
        assume(&Action::Network(settings));
        let state = remote().unwrap();
        assert_eq!(state.trusted.len(), 1);
        assert_eq!(state.trusted[0].device_id, first);
        assert_eq!(state.trusted[0].grants, vec!["view".to_owned(), "files".to_owned()]);
        assert!(!state.settings.lan_discovery);
    }

    #[test]
    fn host_state_round_trips_and_defaults() {
        let state = HostDirect {
            listening_port: Some(28890),
            ..Default::default()
        };
        let bytes = serde_json::to_vec(&state).unwrap();
        assert_eq!(serde_json::from_slice::<HostDirect>(&bytes).unwrap(), state);
        assert_eq!(serde_json::from_str::<HostDirect>("{}").unwrap(), HostDirect::default());
    }
}
