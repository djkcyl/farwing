//! Notifications (`docs/notifications.md`): what happened that an account
//! should know, kept per account, pushed to its online devices
//! (`notice`) and read by the web console. Each account chooses the kinds
//! it receives; administrators also get the server's.

use crate::db::{self, R, now_s};
use crate::http::App;
use farwing_protocol::api::{Notice, NoticeSetting, notice};
use farwing_protocol::realtime::{Envelope, types};
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::time::{Duration, Instant};

/// A device offline this long is reported (`device.offline`).
pub const OFFLINE_AFTER: Duration = Duration::from_secs(10 * 60);
/// A relay node offline this long is reported to administrators.
pub const NODE_OFFLINE_AFTER: Duration = Duration::from_secs(5 * 60);
/// Failed sign-ins within [`FAILURES_WITHIN_S`] that are reported.
pub const FAILURES: i64 = 5;
pub const FAILURES_WITHIN_S: i64 = 600;

// ------------------------------------------------------------------ store

fn preferences(c: &Connection, user_id: &str) -> R<HashMap<String, bool>> {
    let text: Option<String> = c
        .query_row(
            "SELECT prefs FROM notice_prefs WHERE user_id = ?1",
            [user_id],
            |r| r.get(0),
        )
        .optional()?;
    Ok(text
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default())
}

/// Whether the account receives `kind`.
pub fn enabled(c: &Connection, user_id: &str, kind: &str) -> R<bool> {
    let Some(k) = notice::find(kind) else {
        return Ok(false);
    };
    Ok(preferences(c, user_id)?.get(kind).copied().unwrap_or(k.default))
}

/// Every kind the account may receive, as it is set.
pub fn settings(c: &Connection, user_id: &str, admin: bool) -> R<Vec<NoticeSetting>> {
    let prefs = preferences(c, user_id)?;
    Ok(notice::KINDS
        .iter()
        .filter(|k| admin || !k.admin)
        .map(|k| NoticeSetting {
            kind: k.kind.to_owned(),
            enabled: prefs.get(k.kind).copied().unwrap_or(k.default),
        })
        .collect())
}

/// Keep the choices that differ from the defaults.
pub fn save_settings(c: &Connection, user_id: &str, settings: &[NoticeSetting]) -> R<()> {
    let mut prefs = preferences(c, user_id)?;
    for s in settings {
        if let Some(k) = notice::find(&s.kind) {
            if s.enabled == k.default {
                prefs.remove(k.kind);
            } else {
                prefs.insert(k.kind.to_owned(), s.enabled);
            }
        }
    }
    c.execute(
        "INSERT INTO notice_prefs (user_id, prefs) VALUES (?1, ?2)
         ON CONFLICT(user_id) DO UPDATE SET prefs = excluded.prefs",
        params![user_id, serde_json::to_string(&prefs).unwrap_or_default()],
    )?;
    Ok(())
}

pub fn insert(c: &Connection, user_id: &str, kind: &str, data: &Value) -> R<Notice> {
    let at = now_s();
    c.execute(
        "INSERT INTO notices (user_id, kind, data, created_at) VALUES (?1, ?2, ?3, ?4)",
        params![user_id, kind, data.to_string(), at],
    )?;
    Ok(Notice {
        id: c.last_insert_rowid(),
        kind: kind.to_owned(),
        data: data.clone(),
        created_at: at,
        read: false,
    })
}

/// Newest first; `after` lists only newer ones.
pub fn list(
    c: &Connection,
    user_id: &str,
    before: Option<i64>,
    after: Option<i64>,
    limit: u32,
) -> R<Vec<Notice>> {
    if limit == 0 {
        return Ok(Vec::new());
    }
    let mut st = c.prepare(
        "SELECT id, kind, data, created_at, read_at FROM notices
         WHERE user_id = ?1 AND (?2 IS NULL OR id < ?2) AND (?3 IS NULL OR id > ?3)
         ORDER BY id DESC LIMIT ?4",
    )?;
    st.query_map(params![user_id, before, after, limit.min(200)], |r| {
        Ok(Notice {
            id: r.get(0)?,
            kind: r.get(1)?,
            data: serde_json::from_str(&r.get::<_, String>(2)?).unwrap_or_default(),
            created_at: r.get(3)?,
            read: r.get::<_, Option<i64>>(4)?.is_some(),
        })
    })?
    .collect()
}

pub fn unread(c: &Connection, user_id: &str) -> R<u32> {
    c.query_row(
        "SELECT COUNT(*) FROM notices WHERE user_id = ?1 AND read_at IS NULL",
        [user_id],
        |r| r.get(0),
    )
}

pub fn mark_read(c: &Connection, user_id: &str, ids: &[i64], all: bool) -> R<()> {
    let now = now_s();
    if all {
        c.execute(
            "UPDATE notices SET read_at = ?2 WHERE user_id = ?1 AND read_at IS NULL",
            params![user_id, now],
        )?;
    } else {
        let mut st = c.prepare(
            "UPDATE notices SET read_at = ?3 WHERE user_id = ?1 AND id = ?2 AND read_at IS NULL",
        )?;
        for id in ids {
            st.execute(params![user_id, id, now])?;
        }
    }
    Ok(())
}

pub fn prune(c: &Connection, keep_s: i64) -> R<usize> {
    c.execute(
        "DELETE FROM notices WHERE created_at < ?1",
        [now_s() - keep_s],
    )
}

/// The device's first sign-in to this account, and not the account's first.
pub fn new_sign_in_device(c: &Connection, user_id: &str, device_id: &str) -> R<bool> {
    let (all, this): (i64, i64) = c.query_row(
        "SELECT COUNT(*), COALESCE(SUM(device_id = ?2), 0) FROM access_tokens WHERE user_id = ?1",
        params![user_id, device_id],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    Ok(all > 0 && this == 0)
}

/// Failed sign-ins of `username` in the last [`FAILURES_WITHIN_S`].
pub fn recent_failures(c: &Connection, username: &str) -> R<i64> {
    c.query_row(
        "SELECT COUNT(*) FROM audit_log
         WHERE kind = 'login' AND ok = 0 AND target = ?1 COLLATE NOCASE AND at >= ?2",
        params![username, now_s() - FAILURES_WITHIN_S],
        |r| r.get(0),
    )
}

fn admins(c: &Connection) -> R<Vec<String>> {
    let mut st = c.prepare("SELECT id FROM users WHERE role = 'admin' AND disabled = 0")?;
    st.query_map([], |r| r.get(0))?.collect()
}

// --------------------------------------------------------------- delivery

/// Tell `user_id` (unless it chose not to hear of `kind`), on all its
/// online devices but those in `except` (the one that did it, or that it
/// is about).
pub fn send(app: &App, user_id: &str, kind: &'static str, data: Value, except: &[&str]) {
    if user_id.is_empty() {
        return;
    }
    let app = App::clone(app);
    let user = user_id.to_owned();
    let except: Vec<String> = except.iter().map(|d| (*d).to_owned()).collect();
    tokio::spawn(async move {
        let stored = app
            .db
            .call(move |c| {
                if !enabled(c, &user, kind)? {
                    return Ok(None);
                }
                let notice = insert(c, &user, kind, &data)?;
                Ok(Some((notice, db::list_user_devices(c, &user)?)))
            })
            .await;
        match stored {
            Ok(Some((notice, devices))) => {
                let env = Envelope::new(types::NOTICE, &notice);
                for d in devices.iter().filter(|d| !except.contains(&d.id)) {
                    app.hub.push(&d.id, &env);
                }
            }
            Ok(None) => {}
            Err(e) => tracing::warn!(error = %format!("{e:#}"), kind, "notice not stored"),
        }
    });
}

/// Tell the owner of `device_id`.
pub fn send_owner(app: &App, device_id: &str, kind: &'static str, data: Value, except: &[&str]) {
    let app = App::clone(app);
    let device = device_id.to_owned();
    let except: Vec<String> = except.iter().map(|d| (*d).to_owned()).collect();
    tokio::spawn(async move {
        if let Ok(Some(owner)) = app
            .db
            .call(move |c| Ok(db::get_device(c, &device)?.and_then(|d| d.user_id)))
            .await
        {
            let except: Vec<&str> = except.iter().map(String::as_str).collect();
            send(&app, &owner, kind, data, &except);
        }
    });
}

/// Tell every administrator.
pub fn send_admins(app: &App, kind: &'static str, data: Value) {
    let app = App::clone(app);
    tokio::spawn(async move {
        if let Ok(ids) = app.db.call(|c| admins(c)).await {
            for id in ids {
                send(&app, &id, kind, data.clone(), &[]);
            }
        }
    });
}

/// A new connection record (not a reconnection): its host's owner hears of
/// it on the account's other devices.
pub fn connection(app: &App, o: &crate::records::Opened) {
    let who = if o.controller.nickname.is_empty() { &o.controller.username } else { &o.controller.nickname };
    let data = json!({
        "host": o.host.name,
        "host_id": o.host.device_id,
        "who": who,
        "controller": o.controller.name,
        "relation": o.relation,
        "admission": o.admission,
        "purpose": o.purpose,
    });
    let (host, controller) = (o.host.device_id.as_str(), o.controller.device_id.as_str());
    if o.relation == farwing_protocol::api::Relation::Own {
        send(app, &o.host.user_id, notice::CONNECTION_OWN, data, &[host, controller]);
    } else {
        send(app, &o.host.user_id, notice::CONNECTION_INCOMING, data, &[host]);
    }
}

// ---------------------------------------------------------------- watches

/// What the server watches to tell about: devices and relay nodes gone
/// offline, the automatic certificate failing.
#[derive(Default)]
pub struct Watch {
    /// Devices seen online since the server started that went offline:
    /// since when, and whether their owners were told.
    offline: parking_lot::Mutex<HashMap<String, (Instant, bool)>>,
    nodes: parking_lot::Mutex<HashMap<String, bool>>,
    certificate_told: std::sync::atomic::AtomicBool,
}

/// A device's presence changed (`HubEvent::PresenceChanged`).
pub fn presence_changed(app: &App, device_id: &str) {
    let mut offline = app.watch.offline.lock();
    if app.hub.is_online(device_id) {
        offline.remove(device_id);
    } else {
        offline.entry(device_id.to_owned()).or_insert((Instant::now(), false));
    }
}

/// Once a minute: report what has been down long enough.
pub async fn sweep(app: &App) {
    let due: Vec<String> = {
        let mut offline = app.watch.offline.lock();
        offline
            .iter_mut()
            .filter(|(_, (since, told))| !*told && since.elapsed() >= OFFLINE_AFTER)
            .map(|(id, (_, told))| {
                *told = true;
                id.clone()
            })
            .collect()
    };
    for id in due {
        let d = id.clone();
        if let Ok(Some(device)) = app.db.call(move |c| db::get_device(c, &d)).await
            && let Some(owner) = &device.user_id
        {
            let minutes = OFFLINE_AFTER.as_secs() / 60;
            let data = json!({ "device": device.alias, "device_id": device.id, "minutes": minutes });
            send(app, owner, notice::DEVICE_OFFLINE, data, &[&device.id]);
        }
    }
    // Relay nodes: joined, enabled, and gone for a while.
    let now = now_s();
    for node in app.edges.status() {
        let gone = !node.online
            && node.row.enabled
            && !node.row.public_key.is_empty()
            && node
                .row
                .last_seen_at
                .is_some_and(|at| now - at >= NODE_OFFLINE_AFTER.as_secs() as i64);
        let mut told = app.watch.nodes.lock();
        let was = told.get(&node.row.id).copied().unwrap_or(false);
        if gone && !was {
            told.insert(node.row.id.clone(), true);
            drop(told);
            send_admins(app, notice::ADMIN_NODE_OFFLINE, json!({ "node": node.row.name }));
        } else if !gone && was {
            told.remove(&node.row.id);
        }
    }
    // The automatic certificate: once per run of failures.
    let error = match app.tls.read().as_ref() {
        Some(crate::tls::Presented::Acme(acme)) => acme.status().last_error,
        _ => None,
    };
    use std::sync::atomic::Ordering;
    match error {
        Some(error) if !app.watch.certificate_told.swap(true, Ordering::Relaxed) => {
            send_admins(app, notice::ADMIN_CERTIFICATE, json!({ "error": error }));
        }
        None => app.watch.certificate_told.store(false, Ordering::Relaxed),
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn db() -> Connection {
        let dir = std::env::temp_dir().join(format!("farwing-notify-{}", crate::crypto::new_uuid()));
        std::fs::create_dir_all(&dir).unwrap();
        crate::db::open_connection(&dir.join("farwing.db")).unwrap()
    }

    #[test]
    fn choices_override_the_defaults_and_reading_clears_the_count() {
        let c = db();
        assert!(enabled(&c, "u1", notice::CONNECTION_INCOMING).unwrap());
        assert!(!enabled(&c, "u1", notice::DEVICE_OFFLINE).unwrap());
        assert!(!enabled(&c, "u1", "no.such.kind").unwrap());
        save_settings(
            &c,
            "u1",
            &[
                NoticeSetting { kind: notice::CONNECTION_INCOMING.into(), enabled: false },
                NoticeSetting { kind: notice::DEVICE_OFFLINE.into(), enabled: true },
            ],
        )
        .unwrap();
        assert!(!enabled(&c, "u1", notice::CONNECTION_INCOMING).unwrap());
        assert!(enabled(&c, "u1", notice::DEVICE_OFFLINE).unwrap());
        let users = settings(&c, "u1", false).unwrap();
        assert!(users.iter().all(|s| !s.kind.starts_with("admin.")));
        assert_eq!(settings(&c, "u1", true).unwrap().len(), notice::KINDS.len());

        let first = insert(&c, "u1", notice::FRIEND_REQUEST, &json!({ "who": "bob" })).unwrap();
        let second = insert(&c, "u1", notice::FRIEND_ACCEPTED, &json!({ "who": "carol" })).unwrap();
        insert(&c, "u2", notice::FRIEND_REQUEST, &json!({})).unwrap();
        assert_eq!(unread(&c, "u1").unwrap(), 2);
        let newer = list(&c, "u1", None, Some(first.id), 50).unwrap();
        assert_eq!(newer.len(), 1);
        assert_eq!(newer[0].data["who"], "carol");
        mark_read(&c, "u1", &[second.id], false).unwrap();
        assert_eq!(unread(&c, "u1").unwrap(), 1);
        mark_read(&c, "u1", &[], true).unwrap();
        assert_eq!(unread(&c, "u1").unwrap(), 0);
        assert_eq!(unread(&c, "u2").unwrap(), 1);
    }
}
