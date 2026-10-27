//! Storage of the web console (`docs/web-console.md` §8): account roles and
//! TOTP, second-factor marks on logins, invitations, settings changed
//! online, client update announcements and the audit log.

use crate::db::{R, now_s};
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

pub const ROLE_ADMIN: &str = "admin";
pub const ROLE_USER: &str = "user";

// ===================================================================== roles

pub fn role(c: &Connection, user_id: &str) -> R<String> {
    Ok(c
        .query_row("SELECT role FROM users WHERE id = ?1", [user_id], |r| r.get(0))
        .optional()?
        .unwrap_or_else(|| ROLE_USER.to_owned()))
}

pub fn set_role(c: &Connection, user_id: &str, role: &str) -> R<usize> {
    c.execute(
        "UPDATE users SET role = ?2, updated_at = ?3 WHERE id = ?1",
        params![user_id, role, now_s()],
    )
}

/// Enabled administrators.
pub fn admin_count(c: &Connection) -> R<i64> {
    c.query_row(
        "SELECT COUNT(*) FROM users WHERE role = 'admin' AND disabled = 0",
        [],
        |r| r.get(0),
    )
}

pub fn set_nickname(c: &Connection, user_id: &str, nickname: &str) -> R<usize> {
    c.execute(
        "UPDATE users SET nickname = ?2, updated_at = ?3 WHERE id = ?1",
        params![user_id, nickname, now_s()],
    )
}

/// One account for the admin list.
#[derive(Clone, Debug, Serialize)]
pub struct AccountSummary {
    pub id: String,
    pub username: String,
    pub nickname: String,
    pub role: String,
    pub disabled: bool,
    pub totp: bool,
    pub created_at: i64,
    pub devices: i64,
    pub last_seen_at: i64,
}

pub fn list_accounts(c: &Connection) -> R<Vec<AccountSummary>> {
    let mut st = c.prepare(
        "SELECT u.id, u.username, u.nickname, u.role, u.disabled, u.totp_enabled_at IS NOT NULL,
                u.created_at,
                (SELECT COUNT(*) FROM devices d WHERE d.user_id = u.id),
                COALESCE((SELECT MAX(d.last_seen_at) FROM devices d WHERE d.user_id = u.id), 0)
         FROM users u ORDER BY u.username COLLATE NOCASE",
    )?;
    st.query_map([], |r| {
        Ok(AccountSummary {
            id: r.get(0)?,
            username: r.get(1)?,
            nickname: r.get(2)?,
            role: r.get(3)?,
            disabled: r.get::<_, i64>(4)? != 0,
            totp: r.get(5)?,
            created_at: r.get(6)?,
            devices: r.get(7)?,
            last_seen_at: r.get(8)?,
        })
    })?
    .collect()
}

pub fn account(c: &Connection, user_id: &str) -> R<Option<AccountSummary>> {
    Ok(list_accounts(c)?.into_iter().find(|a| a.id == user_id))
}

// ====================================================================== TOTP

/// `(secret, enabled_at, recovery code hashes)`.
pub fn totp(c: &Connection, user_id: &str) -> R<(Option<String>, Option<i64>, Vec<String>)> {
    c.query_row(
        "SELECT totp_secret, totp_enabled_at, totp_recovery FROM users WHERE id = ?1",
        [user_id],
        |r| {
            let recovery: String = r.get(2)?;
            Ok((
                r.get(0)?,
                r.get(1)?,
                serde_json::from_str(&recovery).unwrap_or_default(),
            ))
        },
    )
    .optional()
    .map(|v| v.unwrap_or((None, None, Vec::new())))
}

/// Store a secret being set up (not enabled until confirmed).
pub fn set_totp_pending(c: &Connection, user_id: &str, secret: &str) -> R<usize> {
    c.execute(
        "UPDATE users SET totp_secret = ?2, totp_enabled_at = NULL, totp_recovery = '[]' WHERE id = ?1",
        params![user_id, secret],
    )
}

pub fn enable_totp(c: &Connection, user_id: &str, recovery_hashes: &[String]) -> R<usize> {
    c.execute(
        "UPDATE users SET totp_enabled_at = ?2, totp_recovery = ?3 WHERE id = ?1 AND totp_secret IS NOT NULL",
        params![
            user_id,
            now_s(),
            serde_json::to_string(recovery_hashes).unwrap_or_else(|_| "[]".into())
        ],
    )
}

pub fn clear_totp(c: &Connection, user_id: &str) -> R<usize> {
    c.execute(
        "UPDATE users SET totp_secret = NULL, totp_enabled_at = NULL, totp_recovery = '[]' WHERE id = ?1",
        [user_id],
    )
}

pub fn set_totp_recovery(c: &Connection, user_id: &str, recovery_hashes: &[String]) -> R<usize> {
    c.execute(
        "UPDATE users SET totp_recovery = ?2 WHERE id = ?1",
        params![
            user_id,
            serde_json::to_string(recovery_hashes).unwrap_or_else(|_| "[]".into())
        ],
    )
}

/// When the login with this token passed the second factor.
pub fn token_mfa(c: &Connection, token_hash: &str) -> R<Option<i64>> {
    Ok(c
        .query_row(
            "SELECT mfa_at FROM access_tokens WHERE token_hash = ?1",
            [token_hash],
            |r| r.get::<_, Option<i64>>(0),
        )
        .optional()?
        .flatten())
}

pub fn set_token_mfa(c: &Connection, token_hash: &str) -> R<usize> {
    c.execute(
        "UPDATE access_tokens SET mfa_at = ?2 WHERE token_hash = ?1",
        params![token_hash, now_s()],
    )
}

/// When the token was issued.
pub fn token_created(c: &Connection, token_hash: &str) -> R<Option<i64>> {
    c.query_row(
        "SELECT created_at FROM access_tokens WHERE token_hash = ?1",
        [token_hash],
        |r| r.get(0),
    )
    .optional()
}

// =================================================================== invites

#[derive(Clone, Debug, Serialize)]
pub struct Invite {
    pub id: String,
    pub created_by: String,
    pub note: String,
    pub max_uses: Option<i64>,
    pub uses: i64,
    pub expires_at: Option<i64>,
    pub auto_friend: bool,
    pub revoked_at: Option<i64>,
    pub created_at: i64,
}

impl Invite {
    /// Usable right now.
    pub fn usable(&self, now: i64) -> bool {
        self.revoked_at.is_none()
            && self.expires_at.is_none_or(|t| t > now)
            && self.max_uses.is_none_or(|m| self.uses < m)
    }
}

const INVITE_COLS: &str =
    "id, created_by, note, max_uses, uses, expires_at, auto_friend, revoked_at, created_at";

fn invite_from_row(r: &rusqlite::Row<'_>) -> R<Invite> {
    Ok(Invite {
        id: r.get(0)?,
        created_by: r.get(1)?,
        note: r.get(2)?,
        max_uses: r.get(3)?,
        uses: r.get(4)?,
        expires_at: r.get(5)?,
        auto_friend: r.get::<_, i64>(6)? != 0,
        revoked_at: r.get(7)?,
        created_at: r.get(8)?,
    })
}

#[allow(clippy::too_many_arguments)]
pub fn create_invite(
    c: &Connection,
    id: &str,
    code_hash: &str,
    created_by: &str,
    note: &str,
    max_uses: Option<i64>,
    expires_at: Option<i64>,
    auto_friend: bool,
) -> R<()> {
    c.execute(
        "INSERT INTO invites (id, code_hash, created_by, note, max_uses, expires_at, auto_friend, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
        params![id, code_hash, created_by, note, max_uses, expires_at, auto_friend as i64, now_s()],
    )?;
    Ok(())
}

pub fn list_invites(c: &Connection) -> R<Vec<Invite>> {
    let mut st = c.prepare(&format!(
        "SELECT {INVITE_COLS} FROM invites ORDER BY created_at DESC"
    ))?;
    st.query_map([], invite_from_row)?.collect()
}

pub fn invite_by_hash(c: &Connection, code_hash: &str) -> R<Option<Invite>> {
    c.query_row(
        &format!("SELECT {INVITE_COLS} FROM invites WHERE code_hash = ?1"),
        [code_hash],
        invite_from_row,
    )
    .optional()
}

pub fn revoke_invite(c: &Connection, id: &str) -> R<usize> {
    c.execute(
        "UPDATE invites SET revoked_at = ?2 WHERE id = ?1 AND revoked_at IS NULL",
        params![id, now_s()],
    )
}

/// Count one use; false when the invitation is no longer usable.
pub fn use_invite(c: &Connection, id: &str, user_id: &str, ip: &str) -> R<bool> {
    let now = now_s();
    let changed = c.execute(
        "UPDATE invites SET uses = uses + 1
         WHERE id = ?1 AND revoked_at IS NULL
           AND (expires_at IS NULL OR expires_at > ?2)
           AND (max_uses IS NULL OR uses < max_uses)",
        params![id, now],
    )?;
    if changed == 0 {
        return Ok(false);
    }
    c.execute(
        "INSERT INTO invite_uses (invite_id, user_id, ip, used_at) VALUES (?1, ?2, ?3, ?4)",
        params![id, user_id, ip, now],
    )?;
    Ok(true)
}

#[derive(Clone, Debug, Serialize)]
pub struct InviteUse {
    pub user_id: String,
    pub username: String,
    pub ip: String,
    pub used_at: i64,
}

pub fn invite_uses(c: &Connection, id: &str) -> R<Vec<InviteUse>> {
    let mut st = c.prepare(
        "SELECT u.user_id, COALESCE(a.username, ''), u.ip, u.used_at
         FROM invite_uses u LEFT JOIN users a ON a.id = u.user_id
         WHERE u.invite_id = ?1 ORDER BY u.used_at DESC",
    )?;
    st.query_map([id], |r| {
        Ok(InviteUse {
            user_id: r.get(0)?,
            username: r.get(1)?,
            ip: r.get(2)?,
            used_at: r.get(3)?,
        })
    })?
    .collect()
}

// ================================================================== settings

pub fn settings(c: &Connection) -> R<HashMap<String, String>> {
    let mut st = c.prepare("SELECT key, value FROM settings")?;
    st.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect()
}

pub fn set_setting(c: &Connection, key: &str, value: &str) -> R<()> {
    c.execute(
        "INSERT INTO settings (key, value, updated_at) VALUES (?1, ?2, ?3)
         ON CONFLICT (key) DO UPDATE SET value = excluded.value, updated_at = excluded.updated_at",
        params![key, value, now_s()],
    )?;
    Ok(())
}

pub fn delete_setting(c: &Connection, key: &str) -> R<()> {
    c.execute("DELETE FROM settings WHERE key = ?1", [key])?;
    Ok(())
}

// ============================================================== admin tokens

/// An administration API token (`Authorization: Bearer fwa_…`).
#[derive(Clone, Debug, Serialize)]
pub struct AdminToken {
    pub id: String,
    pub name: String,
    /// `read` (GET only) or `admin`.
    pub scope: String,
    /// Networks the token may be used from (CIDRs); empty = any.
    pub allow: Vec<String>,
    pub created_by: String,
    pub created_by_name: String,
    pub created_at: i64,
    pub expires_at: Option<i64>,
    pub last_used_at: Option<i64>,
    pub last_used_ip: String,
    pub revoked_at: Option<i64>,
}

pub const SCOPE_READ: &str = "read";
pub const SCOPE_ADMIN: &str = "admin";

fn admin_token_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<AdminToken> {
    let allow: String = r.get(3)?;
    Ok(AdminToken {
        id: r.get(0)?,
        name: r.get(1)?,
        scope: r.get(2)?,
        allow: allow
            .split(',')
            .filter(|a| !a.is_empty())
            .map(str::to_owned)
            .collect(),
        created_by: r.get(4)?,
        created_by_name: r.get(5)?,
        created_at: r.get(6)?,
        expires_at: r.get(7)?,
        last_used_at: r.get(8)?,
        last_used_ip: r.get(9)?,
        revoked_at: r.get(10)?,
    })
}

const ADMIN_TOKEN_COLUMNS: &str = "t.id, t.name, t.scope, t.allow, t.created_by,     COALESCE(u.username, ''), t.created_at, t.expires_at, t.last_used_at, t.last_used_ip,     t.revoked_at";

#[allow(clippy::too_many_arguments)]
pub fn create_admin_token(
    c: &Connection,
    id: &str,
    name: &str,
    hash: &str,
    scope: &str,
    allow: &[String],
    created_by: &str,
    expires_at: Option<i64>,
) -> R<()> {
    c.execute(
        "INSERT INTO admin_tokens (id, name, token_hash, scope, allow, created_by, created_at, expires_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
        params![id, name, hash, scope, allow.join(","), created_by, now_s(), expires_at],
    )?;
    Ok(())
}

pub fn admin_tokens(c: &Connection) -> R<Vec<AdminToken>> {
    let mut st = c.prepare(&format!(
        "SELECT {ADMIN_TOKEN_COLUMNS} FROM admin_tokens t LEFT JOIN users u ON u.id = t.created_by
         ORDER BY t.created_at DESC"
    ))?;
    st.query_map([], admin_token_row)?.collect()
}

/// The token with this hash, with whether it may still be used.
pub fn admin_token_by_hash(c: &Connection, hash: &str) -> R<Option<AdminToken>> {
    c.query_row(
        &format!(
            "SELECT {ADMIN_TOKEN_COLUMNS} FROM admin_tokens t LEFT JOIN users u ON u.id = t.created_by
             WHERE t.token_hash = ?1"
        ),
        [hash],
        admin_token_row,
    )
    .optional()
}

pub fn touch_admin_token(c: &Connection, id: &str, ip: &str) -> R<()> {
    c.execute(
        "UPDATE admin_tokens SET last_used_at = ?2, last_used_ip = ?3 WHERE id = ?1",
        params![id, now_s(), ip],
    )?;
    Ok(())
}

pub fn revoke_admin_token(c: &Connection, id: &str) -> R<usize> {
    c.execute(
        "UPDATE admin_tokens SET revoked_at = ?2 WHERE id = ?1 AND revoked_at IS NULL",
        params![id, now_s()],
    )
}

// =================================================================== updates

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Announcement {
    #[serde(default)]
    pub id: String,
    pub platform: String,
    pub channel: String,
    pub version: String,
    #[serde(default)]
    pub notes: String,
    pub url: String,
    pub published_at: i64,
    #[serde(default)]
    pub prerelease: bool,
}

pub fn announcements(c: &Connection) -> R<Vec<Announcement>> {
    let mut st = c.prepare(
        "SELECT id, platform, channel, version, notes, url, published_at, prerelease
         FROM update_announcements ORDER BY published_at DESC, created_at DESC",
    )?;
    st.query_map([], |r| {
        Ok(Announcement {
            id: r.get(0)?,
            platform: r.get(1)?,
            channel: r.get(2)?,
            version: r.get(3)?,
            notes: r.get(4)?,
            url: r.get(5)?,
            published_at: r.get(6)?,
            prerelease: r.get::<_, i64>(7)? != 0,
        })
    })?
    .collect()
}

pub fn save_announcement(c: &Connection, a: &Announcement) -> R<()> {
    c.execute(
        "INSERT INTO update_announcements (id, platform, channel, version, notes, url, published_at, prerelease, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
         ON CONFLICT (id) DO UPDATE SET platform = excluded.platform, channel = excluded.channel,
             version = excluded.version, notes = excluded.notes, url = excluded.url,
             published_at = excluded.published_at, prerelease = excluded.prerelease",
        params![
            a.id,
            a.platform,
            a.channel,
            a.version,
            a.notes,
            a.url,
            a.published_at,
            a.prerelease as i64,
            now_s()
        ],
    )?;
    Ok(())
}

pub fn delete_announcement(c: &Connection, id: &str) -> R<usize> {
    c.execute("DELETE FROM update_announcements WHERE id = ?1", [id])
}

// ===================================================================== audit

#[derive(Clone, Debug, Default, Serialize)]
pub struct AuditEntry {
    pub id: i64,
    pub at: i64,
    pub actor_user: Option<String>,
    pub actor_name: String,
    pub kind: String,
    pub target: String,
    pub detail: String,
    pub ok: bool,
    pub ip: String,
}

pub fn audit(c: &Connection, e: &AuditEntry) -> R<()> {
    c.execute(
        "INSERT INTO audit_log (at, actor_user, actor_name, kind, target, detail, ok, ip)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
        params![
            if e.at == 0 { now_s() } else { e.at },
            e.actor_user,
            e.actor_name,
            e.kind,
            e.target,
            e.detail,
            e.ok as i64,
            e.ip
        ],
    )?;
    Ok(())
}

/// Most recent entries first; `before` pages backwards by ID.
pub fn audit_entries(
    c: &Connection,
    before: Option<i64>,
    kind: Option<&str>,
    actor: Option<&str>,
    limit: i64,
) -> R<Vec<AuditEntry>> {
    let mut st = c.prepare(
        "SELECT id, at, actor_user, actor_name, kind, target, detail, ok, ip FROM audit_log
         WHERE (?1 IS NULL OR id < ?1)
           AND (?2 IS NULL OR kind = ?2)
           AND (?3 IS NULL OR actor_name = ?3 OR actor_user = ?3)
         ORDER BY id DESC LIMIT ?4",
    )?;
    st.query_map(params![before, kind, actor, limit.clamp(1, 500)], |r| {
        Ok(AuditEntry {
            id: r.get(0)?,
            at: r.get(1)?,
            actor_user: r.get(2)?,
            actor_name: r.get(3)?,
            kind: r.get(4)?,
            target: r.get(5)?,
            detail: r.get(6)?,
            ok: r.get::<_, i64>(7)? != 0,
            ip: r.get(8)?,
        })
    })?
    .collect()
}

/// Audit kinds an account sees about itself (`GET /me/events`).
pub const ACCOUNT_EVENT_KINDS: &[&str] = &[
    "login",
    "login.totp",
    "register",
    "logout",
    "auth.revoke",
    "password.change",
    "totp.enable",
    "totp.disable",
    "user.update",
    "user.password",
    "user.totp_reset",
    "device.rename",
    "device.remove",
    "device.unbind",
    "device.disconnect",
    "device.power",
    "friend.add",
    "friend.remove",
    "grants.update",
    "access_code.on",
    "access_code.off",
    "trusted.add",
    "trusted.remove",
];

/// The events of an account: what it did, and what was done to it (the
/// entry's target is its username), of [`ACCOUNT_EVENT_KINDS`]; newest
/// first.
pub fn account_events(
    c: &Connection,
    user_id: &str,
    username: &str,
    before: Option<i64>,
    limit: u32,
) -> R<Vec<AuditEntry>> {
    let kinds = ACCOUNT_EVENT_KINDS
        .iter()
        .map(|k| format!("'{k}'"))
        .collect::<Vec<_>>()
        .join(",");
    let mut st = c.prepare(&format!(
        "SELECT id, at, actor_user, actor_name, kind, target, detail, ok, ip FROM audit_log
         WHERE (actor_user = ?1 OR target = ?2) AND kind IN ({kinds})
           AND (?3 IS NULL OR id < ?3)
         ORDER BY id DESC LIMIT ?4"
    ))?;
    st.query_map(params![user_id, username, before, limit.clamp(1, 200)], |r| {
        Ok(AuditEntry {
            id: r.get(0)?,
            at: r.get(1)?,
            actor_user: r.get(2)?,
            actor_name: r.get(3)?,
            kind: r.get(4)?,
            target: r.get(5)?,
            detail: r.get(6)?,
            ok: r.get::<_, i64>(7)? != 0,
            ip: r.get(8)?,
        })
    })?
    .collect()
}

/// Drop entries older than `keep_s`.
pub fn prune_audit(c: &Connection, keep_s: i64) -> R<usize> {
    c.execute("DELETE FROM audit_log WHERE at < ?1", [now_s() - keep_s])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::{create_user, open_connection};

    #[test]
    fn roles_invites_and_audit() {
        let dir = tempfile::tempdir().unwrap();
        let c = open_connection(&dir.path().join("t.db")).unwrap();
        create_user(&c, "u1", "alice", "", "x").unwrap();
        assert_eq!(role(&c, "u1").unwrap(), ROLE_USER);
        assert_eq!(admin_count(&c).unwrap(), 0);
        set_role(&c, "u1", ROLE_ADMIN).unwrap();
        assert_eq!(admin_count(&c).unwrap(), 1);

        create_invite(&c, "i1", "h1", "u1", "team", Some(1), None, true).unwrap();
        let invite = invite_by_hash(&c, "h1").unwrap().unwrap();
        assert!(invite.usable(now_s()));
        assert!(use_invite(&c, "i1", "u2", "127.0.0.1").unwrap());
        assert!(!use_invite(&c, "i1", "u3", "127.0.0.1").unwrap(), "only one use");
        assert_eq!(invite_uses(&c, "i1").unwrap().len(), 1);
        create_invite(&c, "i2", "h2", "u1", "", None, Some(now_s() - 1), false).unwrap();
        assert!(!use_invite(&c, "i2", "u4", "").unwrap(), "expired");

        audit(
            &c,
            &AuditEntry {
                actor_name: "alice".into(),
                kind: "user.create".into(),
                target: "bob".into(),
                ok: true,
                ..Default::default()
            },
        )
        .unwrap();
        let entries = audit_entries(&c, None, Some("user.create"), None, 10).unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].target, "bob");

        let a = Announcement {
            id: "a1".into(),
            platform: "windows".into(),
            channel: "stable".into(),
            version: "0.1.1".into(),
            notes: String::new(),
            url: "https://example.com/f.exe".into(),
            published_at: 1,
            prerelease: false,
        };
        save_announcement(&c, &a).unwrap();
        assert_eq!(announcements(&c).unwrap().len(), 1);
    }
}

// ------------------------------------------------------------- edge nodes

const NODE_COLUMNS: &str = "id, name, region, public_key, enabled, settings, created_at, joined_at, last_seen_at, last_ip";

fn node_row(r: &rusqlite::Row<'_>) -> R<crate::edge::NodeRow> {
    let settings: String = r.get(5)?;
    Ok(crate::edge::NodeRow {
        id: r.get(0)?,
        name: r.get(1)?,
        region: r.get(2)?,
        public_key: r.get(3)?,
        enabled: r.get::<_, i64>(4)? != 0,
        settings: serde_json::from_str(&settings).unwrap_or_default(),
        created_at: r.get(6)?,
        joined_at: r.get(7)?,
        last_seen_at: r.get(8)?,
        last_ip: r.get(9)?,
    })
}

pub fn edge_nodes(c: &Connection) -> R<Vec<crate::edge::NodeRow>> {
    let mut st = c.prepare(&format!("SELECT {NODE_COLUMNS} FROM edge_nodes ORDER BY name, id"))?;
    st.query_map([], node_row)?.collect()
}

pub fn edge_node(c: &Connection, id: &str) -> R<Option<crate::edge::NodeRow>> {
    c.query_row(
        &format!("SELECT {NODE_COLUMNS} FROM edge_nodes WHERE id = ?1"),
        [id],
        node_row,
    )
    .optional()
}

/// A node waiting to join with the token hashing to `join_hash`.
pub fn create_edge_node(
    c: &Connection,
    row: &crate::edge::NodeRow,
    join_hash: &str,
    join_expires: i64,
) -> R<()> {
    c.execute(
        "INSERT INTO edge_nodes (id, name, region, join_hash, join_expires, settings, created_at) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        rusqlite::params![
            row.id,
            row.name,
            row.region,
            join_hash,
            join_expires,
            serde_json::to_string(&row.settings).expect("serializable"),
            row.created_at
        ],
    )?;
    Ok(())
}

/// Join the node whose unexpired token hashes to `join_hash` with
/// `public_key`; its ID.
pub fn join_edge_node(c: &Connection, join_hash: &str, public_key: &str) -> R<Option<String>> {
    let now = crate::db::now_s();
    let id: Option<String> = c
        .query_row(
            "SELECT id FROM edge_nodes WHERE join_hash = ?1 AND join_expires > ?2",
            rusqlite::params![join_hash, now],
            |r| r.get(0),
        )
        .optional()?;
    if let Some(id) = &id {
        c.execute(
            "UPDATE edge_nodes SET public_key = ?2, join_hash = NULL, join_expires = NULL, joined_at = ?3 WHERE id = ?1",
            rusqlite::params![id, public_key, now],
        )?;
    }
    Ok(id)
}

/// A new join token for a node (rejoining with a new identity).
pub fn rejoin_edge_node(c: &Connection, id: &str, join_hash: &str, join_expires: i64) -> R<usize> {
    c.execute(
        "UPDATE edge_nodes SET join_hash = ?2, join_expires = ?3 WHERE id = ?1",
        rusqlite::params![id, join_hash, join_expires],
    )
}

pub fn update_edge_node(c: &Connection, row: &crate::edge::NodeRow) -> R<usize> {
    c.execute(
        "UPDATE edge_nodes SET name = ?2, region = ?3, enabled = ?4, settings = ?5 WHERE id = ?1",
        rusqlite::params![
            row.id,
            row.name,
            row.region,
            row.enabled as i64,
            serde_json::to_string(&row.settings).expect("serializable")
        ],
    )
}

pub fn delete_edge_node(c: &Connection, id: &str) -> R<usize> {
    c.execute("DELETE FROM edge_nodes WHERE id = ?1", [id])
}

pub fn touch_edge_node(c: &Connection, id: &str, ip: &str) -> R<()> {
    c.execute(
        "UPDATE edge_nodes SET last_seen_at = ?2, last_ip = ?3 WHERE id = ?1",
        rusqlite::params![id, crate::db::now_s(), ip],
    )?;
    Ok(())
}
