//! SQLite persistence (WAL mode). All functions are synchronous and run on a
//! blocking thread via [`Db::call`]; the admin CLI uses them directly.

use anyhow::Context;
use farwing_protocol::api::{DeviceInitRequest, DevicePolicy, Grants, HardwareInfo, Relation};
use farwing_protocol::{Platform, caps, limits};
use rusqlite::{Connection, OptionalExtension, Row, params};
use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

pub type R<T> = rusqlite::Result<T>;

#[derive(Clone)]
pub struct Db {
    conn: Arc<Mutex<Connection>>,
}

pub fn now_s() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

pub fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Schema version of [`SCHEMA`]. A database of any other version is refused;
/// there is no migration (see [`open_connection`]).
const SCHEMA_VERSION: i64 = 8;

const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS meta (
    key   TEXT PRIMARY KEY,
    value TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS users (
    id            TEXT PRIMARY KEY,
    username      TEXT NOT NULL UNIQUE COLLATE NOCASE,
    nickname      TEXT NOT NULL DEFAULT '',
    password_hash TEXT NOT NULL,
    disabled      INTEGER NOT NULL DEFAULT 0,
    created_at    INTEGER NOT NULL,
    updated_at    INTEGER NOT NULL,
    role          TEXT NOT NULL DEFAULT 'user',
    totp_secret   TEXT,
    totp_enabled_at INTEGER,
    totp_recovery TEXT NOT NULL DEFAULT '[]',
    -- Egress policy (`docs/protocol/egress-policy.md` §3.1); NULL: the
    -- server's default. `rate_mbps` and `quota_gb` 0: unlimited.
    relay         INTEGER,
    forward       INTEGER,
    rate_mbps     INTEGER,
    quota_gb      INTEGER,
    -- Chat (`docs/protocol/chat-design.md` §2.3): the avatar's version
    -- (empty: none) and the status the account chose.
    avatar        TEXT NOT NULL DEFAULT '',
    status        TEXT NOT NULL DEFAULT 'online'
);
CREATE TABLE IF NOT EXISTS devices (
    id              TEXT PRIMARY KEY,
    public_key      TEXT NOT NULL UNIQUE,
    installation_id TEXT NOT NULL DEFAULT '',
    user_id         TEXT REFERENCES users(id) ON DELETE SET NULL,
    name            TEXT NOT NULL DEFAULT '',
    alias           TEXT NOT NULL DEFAULT '',
    platform        TEXT NOT NULL DEFAULT 'unknown',
    os              TEXT NOT NULL DEFAULT '',
    version         TEXT NOT NULL DEFAULT '',
    capabilities    TEXT NOT NULL DEFAULT '[]',
    policy          TEXT NOT NULL DEFAULT '{}',
    hardware        TEXT,
    epoch           INTEGER NOT NULL DEFAULT 0,
    connect_id      TEXT UNIQUE,
    wol_mac         TEXT NOT NULL DEFAULT '',
    wol_inner_ip    TEXT NOT NULL DEFAULT '',
    wol_subnet_mask TEXT NOT NULL DEFAULT '',
    wol_public_ip   TEXT NOT NULL DEFAULT '',
    wallpaper_etag  TEXT,
    wallpaper_type  TEXT,
    created_ip      TEXT NOT NULL DEFAULT '',
    created_at      INTEGER NOT NULL,
    updated_at      INTEGER NOT NULL,
    last_seen_at    INTEGER NOT NULL DEFAULT 0,
    -- The device's X25519 key for call frame keys (chat-design §10.1).
    x25519          TEXT NOT NULL DEFAULT ''
);
CREATE INDEX IF NOT EXISTS devices_user ON devices(user_id);
CREATE TABLE IF NOT EXISTS access_tokens (
    token_hash    TEXT PRIMARY KEY,
    user_id       TEXT NOT NULL,
    device_id     TEXT NOT NULL,
    created_at    INTEGER NOT NULL,
    last_used_at  INTEGER NOT NULL,
    revoked_at    INTEGER,
    revoke_reason TEXT,
    mfa_at        INTEGER
);
CREATE INDEX IF NOT EXISTS access_tokens_device ON access_tokens(device_id);
CREATE INDEX IF NOT EXISTS access_tokens_user ON access_tokens(user_id);
CREATE TABLE IF NOT EXISTS guest_tokens (
    token_hash TEXT PRIMARY KEY,
    device_id  TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    expires_at INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS sessions (
    id                   TEXT PRIMARY KEY,
    host_device_id       TEXT NOT NULL,
    controller_device_id TEXT NOT NULL,
    controller_user_id   TEXT NOT NULL,
    purpose              TEXT NOT NULL,
    connect_type         TEXT NOT NULL,
    relation             TEXT NOT NULL DEFAULT 'own',
    state                TEXT NOT NULL,
    created_at           INTEGER NOT NULL,
    started_at           INTEGER,
    ended_at             INTEGER,
    end_reason           TEXT
);
CREATE INDEX IF NOT EXISTS sessions_open ON sessions(ended_at);
CREATE TABLE IF NOT EXISTS assist_saved (
    user_id             TEXT NOT NULL,
    connect_id          TEXT NOT NULL,
    publisher_device_id TEXT NOT NULL DEFAULT '',
    remark              TEXT NOT NULL DEFAULT '',
    is_favorite         INTEGER NOT NULL DEFAULT 0,
    favorited_at        INTEGER,
    last_connected_at   INTEGER,
    PRIMARY KEY (user_id, connect_id)
);
CREATE TABLE IF NOT EXISTS power_requests (
    id                  TEXT PRIMARY KEY,
    device_id           TEXT NOT NULL,
    user_id             TEXT NOT NULL,
    requester_device_id TEXT NOT NULL,
    action              TEXT NOT NULL,
    created_at          INTEGER NOT NULL,
    result_code         INTEGER
);
CREATE TABLE IF NOT EXISTS friendships (
    user_a     TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    user_b     TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    created_at INTEGER NOT NULL,
    PRIMARY KEY (user_a, user_b),
    CHECK (user_a < user_b)
);
CREATE INDEX IF NOT EXISTS friendships_b ON friendships(user_b);
CREATE TABLE IF NOT EXISTS friend_requests (
    id         TEXT PRIMARY KEY,
    from_user  TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    to_user    TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    created_at INTEGER NOT NULL,
    UNIQUE (from_user, to_user)
);
CREATE INDEX IF NOT EXISTS friend_requests_to ON friend_requests(to_user);
CREATE TABLE IF NOT EXISTS grants (
    owner_user   TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    grantee_user TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    device_id    TEXT,
    caps         TEXT NOT NULL DEFAULT '[]',
    updated_at   INTEGER NOT NULL
);
CREATE UNIQUE INDEX IF NOT EXISTS grants_key ON grants(owner_user, grantee_user, IFNULL(device_id, ''));
CREATE INDEX IF NOT EXISTS grants_grantee ON grants(grantee_user);
CREATE TABLE IF NOT EXISTS edge_nodes (
    id            TEXT PRIMARY KEY,
    name          TEXT NOT NULL,
    region        TEXT NOT NULL DEFAULT '',
    public_key    TEXT NOT NULL DEFAULT '',
    join_hash     TEXT UNIQUE,
    join_expires  INTEGER,
    enabled       INTEGER NOT NULL DEFAULT 1,
    settings      TEXT NOT NULL,
    created_at    INTEGER NOT NULL,
    joined_at     INTEGER,
    last_seen_at  INTEGER,
    last_ip       TEXT NOT NULL DEFAULT ''
);
CREATE TABLE IF NOT EXISTS admin_tokens (
    id           TEXT PRIMARY KEY,
    name         TEXT NOT NULL,
    token_hash   TEXT NOT NULL UNIQUE,
    scope        TEXT NOT NULL,
    allow        TEXT NOT NULL DEFAULT '',
    created_by   TEXT NOT NULL,
    created_at   INTEGER NOT NULL,
    expires_at   INTEGER,
    last_used_at INTEGER,
    last_used_ip TEXT NOT NULL DEFAULT '',
    revoked_at   INTEGER
);
CREATE TABLE IF NOT EXISTS invites (
    id          TEXT PRIMARY KEY,
    code_hash   TEXT NOT NULL UNIQUE,
    created_by  TEXT NOT NULL,
    note        TEXT NOT NULL DEFAULT '',
    max_uses    INTEGER,
    uses        INTEGER NOT NULL DEFAULT 0,
    expires_at  INTEGER,
    auto_friend INTEGER NOT NULL DEFAULT 0,
    revoked_at  INTEGER,
    created_at  INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS invite_uses (
    invite_id TEXT NOT NULL,
    user_id   TEXT NOT NULL,
    ip        TEXT NOT NULL DEFAULT '',
    used_at   INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS settings (
    key        TEXT PRIMARY KEY,
    value      TEXT NOT NULL,
    updated_at INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS update_announcements (
    id           TEXT PRIMARY KEY,
    platform     TEXT NOT NULL,
    channel      TEXT NOT NULL,
    version      TEXT NOT NULL,
    notes        TEXT NOT NULL DEFAULT '',
    url          TEXT NOT NULL,
    published_at INTEGER NOT NULL,
    prerelease   INTEGER NOT NULL DEFAULT 0,
    created_at   INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS audit_log (
    id         INTEGER PRIMARY KEY AUTOINCREMENT,
    at         INTEGER NOT NULL,
    actor_user TEXT,
    actor_name TEXT NOT NULL DEFAULT '',
    kind       TEXT NOT NULL,
    target     TEXT NOT NULL DEFAULT '',
    detail     TEXT NOT NULL DEFAULT '',
    ok         INTEGER NOT NULL DEFAULT 1,
    ip         TEXT NOT NULL DEFAULT ''
);
CREATE INDEX IF NOT EXISTS audit_at ON audit_log(at);
CREATE TABLE IF NOT EXISTS records (
    seq                  INTEGER PRIMARY KEY AUTOINCREMENT,
    visit                TEXT NOT NULL,
    session_id           TEXT NOT NULL DEFAULT '',
    host_device_id       TEXT NOT NULL,
    host_user_id         TEXT NOT NULL DEFAULT '',
    host_name            TEXT NOT NULL DEFAULT '',
    host_platform        TEXT NOT NULL DEFAULT '',
    controller_device_id TEXT NOT NULL,
    controller_user_id   TEXT NOT NULL DEFAULT '',
    controller_name      TEXT NOT NULL DEFAULT '',
    controller_username  TEXT NOT NULL DEFAULT '',
    controller_nickname  TEXT NOT NULL DEFAULT '',
    controller_platform  TEXT NOT NULL DEFAULT '',
    relation             TEXT NOT NULL,
    admission            TEXT NOT NULL DEFAULT '',
    purpose              TEXT NOT NULL,
    direct               INTEGER NOT NULL DEFAULT 0,
    ip                   TEXT NOT NULL DEFAULT '',
    started_at           INTEGER NOT NULL,
    ended_at             INTEGER,
    outcome              TEXT NOT NULL,
    end_reason           TEXT NOT NULL DEFAULT '',
    sessions             INTEGER NOT NULL DEFAULT 1,
    viewer               TEXT,
    host                 TEXT
);
CREATE INDEX IF NOT EXISTS records_visit ON records(controller_device_id, visit);
CREATE INDEX IF NOT EXISTS records_session ON records(session_id);
CREATE INDEX IF NOT EXISTS records_host_user ON records(host_user_id);
CREATE INDEX IF NOT EXISTS records_controller_user ON records(controller_user_id);
CREATE TABLE IF NOT EXISTS notices (
    id         INTEGER PRIMARY KEY AUTOINCREMENT,
    user_id    TEXT NOT NULL,
    kind       TEXT NOT NULL,
    data       TEXT NOT NULL DEFAULT '{}',
    created_at INTEGER NOT NULL,
    read_at    INTEGER
);
CREATE INDEX IF NOT EXISTS notices_user ON notices(user_id, id);
CREATE TABLE IF NOT EXISTS notice_prefs (
    user_id TEXT PRIMARY KEY,
    prefs   TEXT NOT NULL DEFAULT '{}'
);
CREATE TABLE IF NOT EXISTS egress_usage (
    payer TEXT NOT NULL,
    month TEXT NOT NULL,
    bytes INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (payer, month)
);
"#;

/// Chat tables (`docs/protocol/chat-design.md` §2.3). IDs are Snowflakes
/// (`crate::chat::ids`); the `@everyone` role's ID is its group's.
const CHAT_SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS chat_groups (
    id            INTEGER PRIMARY KEY,
    name          TEXT NOT NULL,
    icon          TEXT NOT NULL DEFAULT '',
    owner_id      TEXT NOT NULL,
    public        INTEGER NOT NULL DEFAULT 0,
    join_approval INTEGER NOT NULL DEFAULT 0,
    description   TEXT NOT NULL DEFAULT '',
    -- Attachment space in bytes; NULL: the server's default.
    quota_bytes   INTEGER,
    created_at    INTEGER NOT NULL,
    frozen_at     INTEGER,
    dissolved_at  INTEGER
);
CREATE INDEX IF NOT EXISTS chat_groups_owner ON chat_groups(owner_id);
CREATE TABLE IF NOT EXISTS chat_members (
    group_id    INTEGER NOT NULL REFERENCES chat_groups(id) ON DELETE CASCADE,
    user_id     TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    nickname    TEXT NOT NULL DEFAULT '',
    joined_at   INTEGER NOT NULL,
    muted_until INTEGER,
    PRIMARY KEY (group_id, user_id)
);
CREATE INDEX IF NOT EXISTS chat_members_user ON chat_members(user_id);
CREATE TABLE IF NOT EXISTS chat_roles (
    id          INTEGER PRIMARY KEY,
    group_id    INTEGER NOT NULL REFERENCES chat_groups(id) ON DELETE CASCADE,
    name        TEXT NOT NULL,
    color       INTEGER NOT NULL DEFAULT 0,
    permissions INTEGER NOT NULL,
    position    INTEGER NOT NULL,
    mentionable INTEGER NOT NULL DEFAULT 0
);
CREATE INDEX IF NOT EXISTS chat_roles_group ON chat_roles(group_id);
CREATE TABLE IF NOT EXISTS chat_member_roles (
    group_id INTEGER NOT NULL,
    user_id  TEXT NOT NULL,
    role_id  INTEGER NOT NULL REFERENCES chat_roles(id) ON DELETE CASCADE,
    PRIMARY KEY (group_id, user_id, role_id),
    FOREIGN KEY (group_id, user_id) REFERENCES chat_members(group_id, user_id) ON DELETE CASCADE
);
CREATE TABLE IF NOT EXISTS chat_channels (
    id            INTEGER PRIMARY KEY,
    group_id      INTEGER REFERENCES chat_groups(id) ON DELETE CASCADE,
    kind          TEXT NOT NULL,
    name          TEXT NOT NULL DEFAULT '',
    topic         TEXT NOT NULL DEFAULT '',
    parent_id     INTEGER,
    position      INTEGER NOT NULL DEFAULT 0,
    slow_mode_s   INTEGER NOT NULL DEFAULT 0,
    voice_bitrate INTEGER NOT NULL DEFAULT 64,
    voice_limit   INTEGER NOT NULL DEFAULT 0,
    created_at    INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS chat_channels_group ON chat_channels(group_id);
CREATE TABLE IF NOT EXISTS chat_overwrites (
    channel_id INTEGER NOT NULL REFERENCES chat_channels(id) ON DELETE CASCADE,
    role_id    INTEGER NOT NULL,
    allow      INTEGER NOT NULL DEFAULT 0,
    deny       INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (channel_id, role_id)
);
CREATE TABLE IF NOT EXISTS chat_dm (
    channel_id INTEGER PRIMARY KEY REFERENCES chat_channels(id) ON DELETE CASCADE,
    user_a     TEXT NOT NULL,
    user_b     TEXT NOT NULL,
    UNIQUE (user_a, user_b),
    CHECK (user_a < user_b)
);
CREATE INDEX IF NOT EXISTS chat_dm_b ON chat_dm(user_b);
CREATE TABLE IF NOT EXISTS chat_messages (
    id          INTEGER PRIMARY KEY,
    channel_id  INTEGER NOT NULL REFERENCES chat_channels(id) ON DELETE CASCADE,
    author_id   TEXT NOT NULL,
    content     TEXT NOT NULL DEFAULT '',
    reply_to    INTEGER,
    mentions    TEXT NOT NULL DEFAULT '{}',
    attachments TEXT NOT NULL DEFAULT '[]',
    pinned_at   INTEGER,
    edited_at   INTEGER,
    deleted_at  INTEGER
);
CREATE INDEX IF NOT EXISTS chat_messages_channel ON chat_messages(channel_id, id);
CREATE INDEX IF NOT EXISTS chat_messages_author ON chat_messages(author_id, id);
CREATE INDEX IF NOT EXISTS chat_messages_pinned ON chat_messages(channel_id, pinned_at)
    WHERE pinned_at IS NOT NULL;
CREATE VIRTUAL TABLE IF NOT EXISTS chat_messages_fts USING fts5(
    content, tokenize = 'trigram', content = '', contentless_delete = 1
);
CREATE TABLE IF NOT EXISTS chat_attachments (
    id          INTEGER PRIMARY KEY,
    owner_id    TEXT NOT NULL,
    channel_id  INTEGER NOT NULL REFERENCES chat_channels(id) ON DELETE CASCADE,
    group_id    INTEGER,
    name        TEXT NOT NULL,
    mime        TEXT NOT NULL,
    size        INTEGER NOT NULL,
    width       INTEGER NOT NULL DEFAULT 0,
    height      INTEGER NOT NULL DEFAULT 0,
    thumb       INTEGER NOT NULL DEFAULT 0,
    sha256      TEXT NOT NULL DEFAULT '',
    -- The bytes arrived; until then the hash of the upload token.
    uploaded    INTEGER NOT NULL DEFAULT 0,
    upload_hash TEXT,
    created_at  INTEGER NOT NULL,
    message_id  INTEGER
);
CREATE INDEX IF NOT EXISTS chat_attachments_message ON chat_attachments(message_id);
CREATE INDEX IF NOT EXISTS chat_attachments_group ON chat_attachments(group_id);
CREATE INDEX IF NOT EXISTS chat_attachments_owner ON chat_attachments(owner_id);
CREATE INDEX IF NOT EXISTS chat_attachments_pending ON chat_attachments(created_at)
    WHERE message_id IS NULL;
CREATE TABLE IF NOT EXISTS chat_read_states (
    user_id      TEXT NOT NULL,
    channel_id   INTEGER NOT NULL REFERENCES chat_channels(id) ON DELETE CASCADE,
    last_read_id INTEGER NOT NULL DEFAULT 0,
    mentions     INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (user_id, channel_id)
);
CREATE TABLE IF NOT EXISTS chat_bans (
    group_id INTEGER NOT NULL REFERENCES chat_groups(id) ON DELETE CASCADE,
    user_id  TEXT NOT NULL,
    by_user  TEXT NOT NULL,
    reason   TEXT NOT NULL DEFAULT '',
    at       INTEGER NOT NULL,
    PRIMARY KEY (group_id, user_id)
);
CREATE TABLE IF NOT EXISTS chat_invites (
    code       TEXT PRIMARY KEY,
    group_id   INTEGER NOT NULL REFERENCES chat_groups(id) ON DELETE CASCADE,
    by_user    TEXT NOT NULL,
    max_uses   INTEGER,
    uses       INTEGER NOT NULL DEFAULT 0,
    expires_at INTEGER,
    revoked_at INTEGER,
    created_at INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS chat_invites_group ON chat_invites(group_id);
CREATE TABLE IF NOT EXISTS chat_group_invites (
    id       INTEGER PRIMARY KEY,
    group_id INTEGER NOT NULL REFERENCES chat_groups(id) ON DELETE CASCADE,
    user_id  TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    by_user  TEXT NOT NULL,
    at       INTEGER NOT NULL,
    UNIQUE (group_id, user_id)
);
CREATE INDEX IF NOT EXISTS chat_group_invites_user ON chat_group_invites(user_id);
CREATE TABLE IF NOT EXISTS chat_join_requests (
    group_id INTEGER NOT NULL REFERENCES chat_groups(id) ON DELETE CASCADE,
    user_id  TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    via      TEXT NOT NULL,
    at       INTEGER NOT NULL,
    PRIMARY KEY (group_id, user_id)
);
CREATE TABLE IF NOT EXISTS chat_audit (
    id       INTEGER PRIMARY KEY AUTOINCREMENT,
    group_id INTEGER NOT NULL,
    actor_id TEXT NOT NULL,
    action   TEXT NOT NULL,
    target   TEXT NOT NULL DEFAULT '',
    details  TEXT NOT NULL DEFAULT '{}',
    at       INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS chat_audit_group ON chat_audit(group_id, id);
CREATE TABLE IF NOT EXISTS chat_prefs (
    user_id TEXT PRIMARY KEY REFERENCES users(id) ON DELETE CASCADE,
    prefs   TEXT NOT NULL DEFAULT '{}'
);
"#;

impl Db {
    pub fn open(path: &Path) -> anyhow::Result<Self> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        }
        let conn = open_connection(path)?;
        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
        })
    }

    /// Run `f` on a blocking thread with exclusive access to the connection.
    pub async fn call<T, F>(&self, f: F) -> anyhow::Result<T>
    where
        T: Send + 'static,
        F: FnOnce(&mut Connection) -> R<T> + Send + 'static,
    {
        let conn = Arc::clone(&self.conn);
        tokio::task::spawn_blocking(move || {
            let mut guard = conn.lock().unwrap_or_else(|p| p.into_inner());
            f(&mut guard)
        })
        .await
        .context("database task panicked")?
        .context("database error")
    }

    /// Synchronous access (startup).
    pub fn with<T>(&self, f: impl FnOnce(&mut Connection) -> R<T>) -> anyhow::Result<T> {
        let mut guard = self.conn.lock().unwrap_or_else(|p| p.into_inner());
        f(&mut guard).context("database error")
    }
}

/// Open the database, creating the schema in a new one. A database of another
/// schema version is refused: there is no migration.
pub fn open_connection(path: &Path) -> anyhow::Result<Connection> {
    let conn = Connection::open(path).with_context(|| format!("opening {}", path.display()))?;
    conn.busy_timeout(Duration::from_secs(10))?;
    conn.pragma_update(None, "journal_mode", "WAL")?;
    conn.pragma_update(None, "synchronous", "NORMAL")?;
    conn.pragma_update(None, "foreign_keys", "ON")?;
    // Check the version before touching the schema.
    let has_meta: i64 = conn.query_row(
        "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = 'meta'",
        [],
        |r| r.get(0),
    )?;
    let version: Option<String> = if has_meta > 0 {
        conn.query_row(
            "SELECT value FROM meta WHERE key = 'schema_version'",
            [],
            |r| r.get(0),
        )
        .optional()?
    } else {
        None
    };
    if let Some(v) = version.as_deref().map(|v| v.parse::<i64>().unwrap_or(0))
        && v != SCHEMA_VERSION
    {
        anyhow::bail!(
            "database {} uses schema version {v}; this server only opens version {SCHEMA_VERSION} \
             and has no migration: move the file aside to start with an empty database",
            path.display()
        );
    }
    conn.execute_batch(SCHEMA)?;
    conn.execute_batch(CHAT_SCHEMA)?;
    if version.is_none() {
        conn.execute(
            "INSERT OR REPLACE INTO meta (key, value) VALUES ('schema_version', ?1)",
            [SCHEMA_VERSION.to_string()],
        )?;
    }
    Ok(conn)
}

/// The device ID of `public_key`: derived from the key, the same on every
/// server and without one.
pub fn derived_device_id(public_key: &str) -> Option<String> {
    farwing_protocol::crypto::local_device_id(public_key)
}

// ===================================================================== users

#[derive(Clone, Debug)]
pub struct UserRow {
    pub id: String,
    pub username: String,
    pub nickname: String,
    pub password_hash: String,
    pub disabled: bool,
    pub created_at: i64,
}

impl UserRow {
    pub fn display_name(&self) -> &str {
        if self.nickname.is_empty() {
            &self.username
        } else {
            &self.nickname
        }
    }
}

const USER_COLS: &str = "id, username, nickname, password_hash, disabled, created_at";

fn user_from_row(r: &Row<'_>) -> R<UserRow> {
    Ok(UserRow {
        id: r.get(0)?,
        username: r.get(1)?,
        nickname: r.get(2)?,
        password_hash: r.get(3)?,
        disabled: r.get::<_, i64>(4)? != 0,
        created_at: r.get(5)?,
    })
}

pub fn create_user(
    c: &Connection,
    id: &str,
    username: &str,
    nickname: &str,
    password_hash: &str,
) -> R<()> {
    let now = now_s();
    c.execute(
        "INSERT INTO users (id, username, nickname, password_hash, disabled, created_at, updated_at)
         VALUES (?1, ?2, ?3, ?4, 0, ?5, ?5)",
        params![id, username, nickname, password_hash, now],
    )?;
    Ok(())
}

pub fn get_user(c: &Connection, id: &str) -> R<Option<UserRow>> {
    c.query_row(
        &format!("SELECT {USER_COLS} FROM users WHERE id = ?1"),
        [id],
        user_from_row,
    )
    .optional()
}

pub fn get_user_by_name(c: &Connection, username: &str) -> R<Option<UserRow>> {
    c.query_row(
        &format!("SELECT {USER_COLS} FROM users WHERE username = ?1"),
        [username],
        user_from_row,
    )
    .optional()
}

pub fn list_users(c: &Connection) -> R<Vec<UserRow>> {
    let mut st = c.prepare(&format!("SELECT {USER_COLS} FROM users ORDER BY username"))?;
    st.query_map([], user_from_row)?.collect()
}

pub fn set_user_password(c: &Connection, id: &str, password_hash: &str) -> R<usize> {
    c.execute(
        "UPDATE users SET password_hash = ?2, updated_at = ?3 WHERE id = ?1",
        params![id, password_hash, now_s()],
    )
}

pub fn set_user_disabled(c: &Connection, id: &str, disabled: bool) -> R<usize> {
    c.execute(
        "UPDATE users SET disabled = ?2, updated_at = ?3 WHERE id = ?1",
        params![id, disabled as i64, now_s()],
    )
}

/// Delete a user: unbind its devices, revoke its tokens, drop saved lists.
/// Friendships, friend requests and grants go with the user row (cascade).
pub fn delete_user(c: &mut Connection, id: &str) -> R<Vec<String>> {
    let tx = c.transaction()?;
    let devices: Vec<String> = {
        let mut st = tx.prepare("SELECT id FROM devices WHERE user_id = ?1")?;
        st.query_map([id], |r| r.get(0))?.collect::<R<_>>()?
    };
    revoke_tokens_for_user(&tx, id, "user_deleted")?;
    tx.execute("UPDATE devices SET user_id = NULL WHERE user_id = ?1", [id])?;
    tx.execute("DELETE FROM assist_saved WHERE user_id = ?1", [id])?;
    crate::chat::store::dissolve_owned(&tx, id)?;
    tx.execute("DELETE FROM users WHERE id = ?1", [id])?;
    tx.commit()?;
    Ok(devices)
}

// =================================================================== devices

#[derive(Clone, Debug, Default)]
pub struct DeviceRow {
    pub id: String,
    pub public_key: String,
    pub installation_id: String,
    pub user_id: Option<String>,
    pub name: String,
    pub alias: String,
    pub platform: Platform,
    pub os: String,
    pub version: String,
    pub capabilities: Vec<String>,
    pub policy: DevicePolicy,
    pub hardware: Option<HardwareInfo>,
    pub epoch: i64,
    pub connect_id: Option<String>,
    pub wol_mac: String,
    pub wol_inner_ip: String,
    pub wol_subnet_mask: String,
    pub wol_public_ip: String,
    pub wallpaper_etag: Option<String>,
    pub wallpaper_type: Option<String>,
    pub created_at: i64,
    pub updated_at: i64,
    pub last_seen_at: i64,
}

impl DeviceRow {
    pub fn has_capability(&self, name: &str) -> bool {
        self.capabilities.iter().any(|c| c == name)
    }

    /// The device has a host implementation (can be controlled).
    pub fn host_supported(&self) -> bool {
        self.has_capability(farwing_protocol::capability::HOST)
    }

    /// Wake-on-LAN information registered and allowed by the owner.
    pub fn wol_supported(&self) -> bool {
        self.policy.wol && !self.wol_mac.is_empty()
    }

    /// Concurrent participants this host accepts.
    pub fn max_participants(&self) -> u32 {
        self.policy.max_participants.clamp(1, limits::MAX_PARTICIPANTS)
    }
}

const DEVICE_COLS: &str = "id, public_key, installation_id, user_id, name, alias, platform, os, version, \
     capabilities, policy, hardware, epoch, connect_id, wol_mac, wol_inner_ip, wol_subnet_mask, \
     wol_public_ip, wallpaper_etag, wallpaper_type, created_at, updated_at, last_seen_at";

fn json_list(text: String) -> Vec<String> {
    serde_json::from_str(&text).unwrap_or_default()
}

fn device_from_row(r: &Row<'_>) -> R<DeviceRow> {
    let platform: String = r.get(6)?;
    let policy: String = r.get(10)?;
    let hardware: Option<String> = r.get(11)?;
    Ok(DeviceRow {
        id: r.get(0)?,
        public_key: r.get(1)?,
        installation_id: r.get(2)?,
        user_id: r.get(3)?,
        name: r.get(4)?,
        alias: r.get(5)?,
        platform: Platform::parse(&platform).unwrap_or(Platform::Unknown),
        os: r.get(7)?,
        version: r.get(8)?,
        capabilities: json_list(r.get(9)?),
        policy: serde_json::from_str(&policy).unwrap_or_default(),
        hardware: hardware.and_then(|h| serde_json::from_str(&h).ok()),
        epoch: r.get(12)?,
        connect_id: r.get(13)?,
        wol_mac: r.get(14)?,
        wol_inner_ip: r.get(15)?,
        wol_subnet_mask: r.get(16)?,
        wol_public_ip: r.get(17)?,
        wallpaper_etag: r.get(18)?,
        wallpaper_type: r.get(19)?,
        created_at: r.get(20)?,
        updated_at: r.get(21)?,
        last_seen_at: r.get(22)?,
    })
}

/// Truncate to at most `max` characters.
pub fn clip(s: &str, max: usize) -> String {
    s.trim().chars().take(max).collect()
}

/// Clamp `max_participants` into `1..=MAX_PARTICIPANTS`.
pub fn normalize_policy(mut policy: DevicePolicy) -> DevicePolicy {
    policy.max_participants = policy.max_participants.clamp(1, limits::MAX_PARTICIPANTS);
    policy
}

/// Stored form of the capability list reported at device init.
pub fn normalize_capabilities(list: &[String]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for c in list.iter().map(|v| clip(v, 64)).filter(|v| !v.is_empty()) {
        if !out.contains(&c) && out.len() < 64 {
            out.push(c);
        }
    }
    out
}

fn normalize_hardware(h: &HardwareInfo) -> HardwareInfo {
    HardwareInfo {
        cpu: clip(&h.cpu, 256),
        base_board: clip(&h.base_board, 256),
        video: h.video.iter().map(|v| clip(v, 256)).take(16).collect(),
        memory: h.memory.max(0),
        screen: clip(&h.screen, 64),
        mac: clip(&h.mac, 64),
    }
}

/// Column values of the profile reported by `POST /devices/init`.
struct Profile {
    installation_id: String,
    name: String,
    platform: &'static str,
    os: String,
    version: String,
    capabilities: String,
    policy: String,
    hardware: Option<String>,
}

fn profile_values(p: &DeviceInitRequest) -> Profile {
    Profile {
        installation_id: clip(&p.installation_id, 64),
        name: clip(&p.name, 128),
        platform: p.platform.as_str(),
        os: clip(&p.os, 256),
        version: clip(&p.version, 64),
        capabilities: serde_json::to_string(&normalize_capabilities(&p.capabilities))
            .unwrap_or_else(|_| "[]".into()),
        policy: serde_json::to_string(&normalize_policy(p.policy)).unwrap_or_else(|_| "{}".into()),
        hardware: p
            .hardware
            .as_ref()
            .and_then(|h| serde_json::to_string(&normalize_hardware(h)).ok()),
    }
}

/// Register a new (unbound) device for `public_key`.
pub fn insert_device(
    c: &Connection,
    id: &str,
    public_key: &str,
    p: &DeviceInitRequest,
    created_ip: &str,
) -> R<()> {
    let now = now_s();
    let v = profile_values(p);
    c.execute(
        "INSERT INTO devices (id, public_key, installation_id, user_id, name, alias, platform, os,
             version, capabilities, policy, hardware, created_ip, created_at, updated_at)
         VALUES (?1, ?2, ?3, NULL, ?4, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?12)",
        params![
            id,
            public_key,
            v.installation_id,
            v.name,
            v.platform,
            v.os,
            v.version,
            v.capabilities,
            v.policy,
            v.hardware,
            created_ip,
            now,
        ],
    )?;
    Ok(())
}

/// Update the reported profile of an existing device (re-initialization).
pub fn update_device_profile(c: &Connection, id: &str, p: &DeviceInitRequest) -> R<()> {
    let v = profile_values(p);
    c.execute(
        "UPDATE devices SET installation_id = ?2, name = ?3, platform = ?4, os = ?5, version = ?6,
             capabilities = ?7, policy = ?8, hardware = ?9, updated_at = ?10
         WHERE id = ?1",
        params![
            id,
            v.installation_id,
            v.name,
            v.platform,
            v.os,
            v.version,
            v.capabilities,
            v.policy,
            v.hardware,
            now_s(),
        ],
    )?;
    Ok(())
}

pub fn get_device(c: &Connection, id: &str) -> R<Option<DeviceRow>> {
    c.query_row(
        &format!("SELECT {DEVICE_COLS} FROM devices WHERE id = ?1"),
        [id],
        device_from_row,
    )
    .optional()
}

pub fn get_device_by_public_key(c: &Connection, public_key: &str) -> R<Option<DeviceRow>> {
    c.query_row(
        &format!("SELECT {DEVICE_COLS} FROM devices WHERE public_key = ?1"),
        [public_key],
        device_from_row,
    )
    .optional()
}

pub fn get_device_by_connect_id(c: &Connection, connect_id: &str) -> R<Option<DeviceRow>> {
    c.query_row(
        &format!("SELECT {DEVICE_COLS} FROM devices WHERE connect_id = ?1"),
        [connect_id],
        device_from_row,
    )
    .optional()
}

pub fn list_user_devices(c: &Connection, user_id: &str) -> R<Vec<DeviceRow>> {
    let mut st = c.prepare(&format!(
        "SELECT {DEVICE_COLS} FROM devices WHERE user_id = ?1 ORDER BY created_at, id"
    ))?;
    st.query_map([user_id], device_from_row)?.collect()
}

pub fn list_all_devices(c: &Connection) -> R<Vec<DeviceRow>> {
    let mut st = c.prepare(&format!(
        "SELECT {DEVICE_COLS} FROM devices ORDER BY created_at, id"
    ))?;
    st.query_map([], device_from_row)?.collect()
}

pub fn user_device_ids(c: &Connection, user_id: &str) -> R<Vec<String>> {
    let mut st = c.prepare("SELECT id FROM devices WHERE user_id = ?1")?;
    st.query_map([user_id], |r| r.get(0))?.collect()
}

/// Bind the device to `user_id` (or unbind it). Device-specific grants of a
/// previous owner are dropped when the owner changes.
pub fn set_device_user(c: &Connection, id: &str, user_id: Option<&str>) -> R<usize> {
    c.execute(
        "DELETE FROM grants WHERE device_id = ?1 AND owner_user IS NOT ?2",
        params![id, user_id],
    )?;
    c.execute(
        "UPDATE devices SET user_id = ?2, updated_at = ?3 WHERE id = ?1",
        params![id, user_id, now_s()],
    )
}

pub fn set_device_alias(c: &Connection, id: &str, alias: &str) -> R<usize> {
    c.execute(
        "UPDATE devices SET alias = ?2, updated_at = ?3 WHERE id = ?1",
        params![id, alias, now_s()],
    )
}

pub fn set_device_policy(c: &Connection, id: &str, policy: &DevicePolicy) -> R<usize> {
    let text =
        serde_json::to_string(&normalize_policy(*policy)).unwrap_or_else(|_| "{}".to_owned());
    c.execute(
        "UPDATE devices SET policy = ?2, updated_at = ?3 WHERE id = ?1",
        params![id, text, now_s()],
    )
}

/// Increment and return the device online epoch.
pub fn next_epoch(c: &Connection, id: &str) -> R<i64> {
    c.query_row(
        "UPDATE devices SET epoch = epoch + 1, last_seen_at = ?2 WHERE id = ?1 RETURNING epoch",
        params![id, now_s()],
        |r| r.get(0),
    )
}

pub fn touch_last_seen(c: &Connection, id: &str) -> R<usize> {
    c.execute(
        "UPDATE devices SET last_seen_at = ?2 WHERE id = ?1",
        params![id, now_s()],
    )
}

/// Assign a connect ID on first use; returns the stable value.
pub fn ensure_connect_id(c: &Connection, id: &str) -> R<String> {
    if let Some(existing) = c
        .query_row("SELECT connect_id FROM devices WHERE id = ?1", [id], |r| {
            r.get::<_, Option<String>>(0)
        })
        .optional()?
        .flatten()
    {
        return Ok(existing);
    }
    for _ in 0..64 {
        let candidate = crate::crypto::new_connect_id();
        match c.execute(
            "UPDATE devices SET connect_id = ?2 WHERE id = ?1 AND connect_id IS NULL",
            params![id, candidate],
        ) {
            Ok(_) => {
                let v: Option<String> =
                    c.query_row("SELECT connect_id FROM devices WHERE id = ?1", [id], |r| {
                        r.get(0)
                    })?;
                if let Some(v) = v {
                    return Ok(v);
                }
            }
            Err(rusqlite::Error::SqliteFailure(e, _))
                if e.code == rusqlite::ErrorCode::ConstraintViolation =>
            {
                continue;
            }
            Err(e) => return Err(e),
        }
    }
    Err(rusqlite::Error::QueryReturnedNoRows)
}

pub fn set_wol_info(
    c: &Connection,
    id: &str,
    mac: &str,
    inner_ip: &str,
    mask: &str,
    public_ip: &str,
) -> R<usize> {
    c.execute(
        "UPDATE devices SET wol_mac = ?2, wol_inner_ip = ?3, wol_subnet_mask = ?4, wol_public_ip = ?5,
             updated_at = ?6 WHERE id = ?1",
        params![id, mac, inner_ip, mask, public_ip, now_s()],
    )
}

pub fn set_wallpaper(
    c: &Connection,
    id: &str,
    etag: Option<&str>,
    content_type: Option<&str>,
) -> R<usize> {
    c.execute(
        "UPDATE devices SET wallpaper_etag = ?2, wallpaper_type = ?3, updated_at = ?4 WHERE id = ?1",
        params![id, etag, content_type, now_s()],
    )
}

// ==================================================================== tokens

/// Access tokens expire after this many seconds without use (sliding).
pub const ACCESS_TOKEN_IDLE_S: i64 = limits::ACCESS_TOKEN_IDLE_DAYS * 86_400;
/// `last_used_at` is written at most this often per token.
pub const ACCESS_TOKEN_TOUCH_S: i64 = 60;

#[derive(Clone, Debug)]
pub struct AccessTokenRow {
    pub user_id: String,
    pub device_id: String,
    pub revoked: bool,
    pub created_at: i64,
    pub last_used_at: i64,
}

impl AccessTokenRow {
    /// Not revoked and used within the idle window.
    pub fn live(&self, now: i64) -> bool {
        !self.revoked && self.last_used_at > now - ACCESS_TOKEN_IDLE_S
    }
}

pub fn insert_access_token(c: &Connection, hash: &str, user_id: &str, device_id: &str) -> R<()> {
    let now = now_s();
    c.execute(
        "INSERT INTO access_tokens (token_hash, user_id, device_id, created_at, last_used_at)
         VALUES (?1, ?2, ?3, ?4, ?4)",
        params![hash, user_id, device_id, now],
    )?;
    Ok(())
}

pub fn get_access_token(c: &Connection, hash: &str) -> R<Option<AccessTokenRow>> {
    c.query_row(
        "SELECT user_id, device_id, revoked_at, created_at, last_used_at
         FROM access_tokens WHERE token_hash = ?1",
        [hash],
        |r| {
            Ok(AccessTokenRow {
                user_id: r.get(0)?,
                device_id: r.get(1)?,
                revoked: r.get::<_, Option<i64>>(2)?.is_some(),
                created_at: r.get(3)?,
                last_used_at: r.get(4)?,
            })
        },
    )
    .optional()
}

/// Slide the expiry of a token that was just used.
pub fn touch_access_token(c: &Connection, hash: &str, now: i64) -> R<usize> {
    c.execute(
        "UPDATE access_tokens SET last_used_at = ?2 WHERE token_hash = ?1 AND revoked_at IS NULL",
        params![hash, now],
    )
}

pub fn revoke_token(c: &Connection, hash: &str, reason: &str) -> R<usize> {
    c.execute(
        "UPDATE access_tokens SET revoked_at = ?2, revoke_reason = ?3
         WHERE token_hash = ?1 AND revoked_at IS NULL",
        params![hash, now_s(), reason],
    )
}

pub fn revoke_tokens_for_device(c: &Connection, device_id: &str, reason: &str) -> R<usize> {
    c.execute(
        "UPDATE access_tokens SET revoked_at = ?2, revoke_reason = ?3
         WHERE device_id = ?1 AND revoked_at IS NULL",
        params![device_id, now_s(), reason],
    )
}

/// Revoke every token of `device_id` (a new login replaces the previous one);
/// returns the other accounts that lost a login on the device.
pub fn revoke_device_logins(
    c: &Connection,
    device_id: &str,
    new_user: &str,
    reason: &str,
) -> R<Vec<String>> {
    let users: Vec<String> = {
        let mut st = c.prepare(
            "SELECT DISTINCT user_id FROM access_tokens
             WHERE device_id = ?1 AND user_id <> ?2 AND revoked_at IS NULL",
        )?;
        st.query_map(params![device_id, new_user], |r| r.get(0))?
            .collect::<R<_>>()?
    };
    revoke_tokens_for_device(c, device_id, reason)?;
    Ok(users)
}

pub fn revoke_tokens_for_user(c: &Connection, user_id: &str, reason: &str) -> R<usize> {
    c.execute(
        "UPDATE access_tokens SET revoked_at = ?2, revoke_reason = ?3
         WHERE user_id = ?1 AND revoked_at IS NULL",
        params![user_id, now_s(), reason],
    )
}

/// Revoke the logins of `user_id` on `device_id`.
pub fn revoke_user_device_tokens(
    c: &Connection,
    user_id: &str,
    device_id: &str,
    reason: &str,
) -> R<usize> {
    c.execute(
        "UPDATE access_tokens SET revoked_at = ?3, revoke_reason = ?4
         WHERE user_id = ?1 AND device_id = ?2 AND revoked_at IS NULL",
        params![user_id, device_id, now_s(), reason],
    )
}

/// One live login of an account: `(device_id, created_at, last_used_at)`.
#[derive(Clone, Debug)]
pub struct LoginRow {
    pub device_id: String,
    pub created_at: i64,
    pub last_used_at: i64,
}

/// Live logins of `user_id`, one per device, most recently used first.
pub fn list_logins(c: &Connection, user_id: &str) -> R<Vec<LoginRow>> {
    let mut st = c.prepare(
        "SELECT device_id, MIN(created_at), MAX(last_used_at) FROM access_tokens
         WHERE user_id = ?1 AND revoked_at IS NULL AND last_used_at > ?2
         GROUP BY device_id ORDER BY MAX(last_used_at) DESC, device_id",
    )?;
    st.query_map(params![user_id, now_s() - ACCESS_TOKEN_IDLE_S], |r| {
        Ok(LoginRow {
            device_id: r.get(0)?,
            created_at: r.get(1)?,
            last_used_at: r.get(2)?,
        })
    })?
    .collect()
}

pub fn insert_guest_token(c: &Connection, hash: &str, device_id: &str, expires_at: i64) -> R<()> {
    c.execute("DELETE FROM guest_tokens WHERE expires_at < ?1", [now_s()])?;
    c.execute(
        "INSERT INTO guest_tokens (token_hash, device_id, created_at, expires_at) VALUES (?1, ?2, ?3, ?4)",
        params![hash, device_id, now_s(), expires_at],
    )?;
    Ok(())
}

/// `(device_id, expires_at)`.
pub fn get_guest_token(c: &Connection, hash: &str) -> R<Option<(String, i64)>> {
    c.query_row(
        "SELECT device_id, expires_at FROM guest_tokens WHERE token_hash = ?1",
        [hash],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )
    .optional()
}

// =================================================================== friends

/// Canonical order of a friendship pair (`user_a < user_b`).
fn pair<'a>(a: &'a str, b: &'a str) -> (&'a str, &'a str) {
    if a < b { (a, b) } else { (b, a) }
}

/// `created_at` of the friendship between `a` and `b`.
pub fn friendship_since(c: &Connection, a: &str, b: &str) -> R<Option<i64>> {
    let (a, b) = pair(a, b);
    c.query_row(
        "SELECT created_at FROM friendships WHERE user_a = ?1 AND user_b = ?2",
        params![a, b],
        |r| r.get(0),
    )
    .optional()
}

pub fn are_friends(c: &Connection, a: &str, b: &str) -> R<bool> {
    Ok(friendship_since(c, a, b)?.is_some())
}

/// Friends of `user_id` with the friendship start, ordered by name.
pub fn list_friends(c: &Connection, user_id: &str) -> R<Vec<(UserRow, i64)>> {
    let mut st = c.prepare(
        "SELECT u.id, u.username, u.nickname, u.password_hash, u.disabled, u.created_at, f.created_at
         FROM friendships f
         JOIN users u ON u.id = CASE WHEN f.user_a = ?1 THEN f.user_b ELSE f.user_a END
         WHERE f.user_a = ?1 OR f.user_b = ?1
         ORDER BY u.username COLLATE NOCASE",
    )?;
    st.query_map([user_id], |r| Ok((user_from_row(r)?, r.get(6)?)))?
        .collect()
}

pub fn friend_ids(c: &Connection, user_id: &str) -> R<Vec<String>> {
    let mut st = c.prepare(
        "SELECT CASE WHEN user_a = ?1 THEN user_b ELSE user_a END
         FROM friendships WHERE user_a = ?1 OR user_b = ?1",
    )?;
    st.query_map([user_id], |r| r.get(0))?.collect()
}

/// Make `a` and `b` friends: drop pending requests between them; `a` shares
/// `shares` on all its devices, `b` the default [`caps::DEFAULT_FRIEND`].
/// Returns the friendship start.
pub fn make_friends(c: &mut Connection, a: &str, b: &str, shares: &[String]) -> R<i64> {
    let now = now_s();
    let tx = c.transaction()?;
    let (x, y) = pair(a, b);
    tx.execute(
        "INSERT OR IGNORE INTO friendships (user_a, user_b, created_at) VALUES (?1, ?2, ?3)",
        params![x, y, now],
    )?;
    delete_requests_between(&tx, a, b)?;
    let text = |list: Vec<String>| serde_json::to_string(&list).unwrap_or_else(|_| "[]".into());
    let shared = text(caps::normalize(shares));
    let default = text(caps::normalize(caps::DEFAULT_FRIEND));
    for (owner, grantee, caps) in [(a, b, &shared), (b, a, &default)] {
        tx.execute(
            "DELETE FROM grants WHERE owner_user = ?1 AND grantee_user = ?2",
            params![owner, grantee],
        )?;
        tx.execute(
            "INSERT INTO grants (owner_user, grantee_user, device_id, caps, updated_at)
             VALUES (?1, ?2, NULL, ?3, ?4)",
            params![owner, grantee, caps, now],
        )?;
    }
    let since = tx.query_row(
        "SELECT created_at FROM friendships WHERE user_a = ?1 AND user_b = ?2",
        params![x, y],
        |r| r.get(0),
    )?;
    tx.commit()?;
    Ok(since)
}

/// End the friendship of `a` and `b`, with the grants in both directions and
/// any pending request. Returns whether they were friends.
pub fn remove_friendship(c: &mut Connection, a: &str, b: &str) -> R<bool> {
    let tx = c.transaction()?;
    let (x, y) = pair(a, b);
    let n = tx.execute(
        "DELETE FROM friendships WHERE user_a = ?1 AND user_b = ?2",
        params![x, y],
    )?;
    tx.execute(
        "DELETE FROM grants WHERE (owner_user = ?1 AND grantee_user = ?2)
             OR (owner_user = ?2 AND grantee_user = ?1)",
        params![a, b],
    )?;
    delete_requests_between(&tx, a, b)?;
    tx.commit()?;
    Ok(n > 0)
}

#[derive(Clone, Debug)]
pub struct FriendRequestRow {
    pub id: String,
    pub from_user: String,
    pub to_user: String,
    pub created_at: i64,
}

fn request_from_row(r: &Row<'_>) -> R<FriendRequestRow> {
    Ok(FriendRequestRow {
        id: r.get(0)?,
        from_user: r.get(1)?,
        to_user: r.get(2)?,
        created_at: r.get(3)?,
    })
}

pub fn get_friend_request(c: &Connection, id: &str) -> R<Option<FriendRequestRow>> {
    c.query_row(
        "SELECT id, from_user, to_user, created_at FROM friend_requests WHERE id = ?1",
        [id],
        request_from_row,
    )
    .optional()
}

pub fn find_friend_request(c: &Connection, from: &str, to: &str) -> R<Option<FriendRequestRow>> {
    c.query_row(
        "SELECT id, from_user, to_user, created_at FROM friend_requests
         WHERE from_user = ?1 AND to_user = ?2",
        params![from, to],
        request_from_row,
    )
    .optional()
}

pub fn insert_friend_request(
    c: &Connection,
    id: &str,
    from: &str,
    to: &str,
) -> R<FriendRequestRow> {
    let now = now_s();
    c.execute(
        "INSERT INTO friend_requests (id, from_user, to_user, created_at) VALUES (?1, ?2, ?3, ?4)",
        params![id, from, to, now],
    )?;
    Ok(FriendRequestRow {
        id: id.to_owned(),
        from_user: from.to_owned(),
        to_user: to.to_owned(),
        created_at: now,
    })
}

pub fn delete_friend_request(c: &Connection, id: &str) -> R<usize> {
    c.execute("DELETE FROM friend_requests WHERE id = ?1", [id])
}

fn delete_requests_between(c: &Connection, a: &str, b: &str) -> R<usize> {
    c.execute(
        "DELETE FROM friend_requests WHERE (from_user = ?1 AND to_user = ?2)
             OR (from_user = ?2 AND to_user = ?1)",
        params![a, b],
    )
}

/// Pending requests involving `user_id`: `(request, other party)`;
/// `incoming` selects requests sent to the user.
pub fn list_friend_requests(
    c: &Connection,
    user_id: &str,
    incoming: bool,
) -> R<Vec<(FriendRequestRow, UserRow)>> {
    let (mine, other) = if incoming {
        ("to_user", "from_user")
    } else {
        ("from_user", "to_user")
    };
    let mut st = c.prepare(&format!(
        "SELECT r.id, r.from_user, r.to_user, r.created_at,
                u.id, u.username, u.nickname, u.password_hash, u.disabled, u.created_at
         FROM friend_requests r JOIN users u ON u.id = r.{other}
         WHERE r.{mine} = ?1 ORDER BY r.created_at DESC, r.id"
    ))?;
    st.query_map([user_id], |r| {
        Ok((
            request_from_row(r)?,
            UserRow {
                id: r.get(4)?,
                username: r.get(5)?,
                nickname: r.get(6)?,
                password_hash: r.get(7)?,
                disabled: r.get::<_, i64>(8)? != 0,
                created_at: r.get(9)?,
            },
        ))
    })?
    .collect()
}

// ==================================================================== grants

fn caps_from_text(text: &str) -> Vec<String> {
    caps::normalize(&json_list(text.to_owned()))
}

/// What `owner` grants `grantee`.
pub fn get_grants(c: &Connection, owner: &str, grantee: &str) -> R<Grants> {
    let mut st = c.prepare(
        "SELECT device_id, caps FROM grants WHERE owner_user = ?1 AND grantee_user = ?2",
    )?;
    let rows = st
        .query_map(params![owner, grantee], |r| {
            Ok((r.get::<_, Option<String>>(0)?, r.get::<_, String>(1)?))
        })?
        .collect::<R<Vec<_>>>()?;
    let mut out = Grants::default();
    for (device, text) in rows {
        match device {
            None => out.default = caps_from_text(&text),
            Some(d) => {
                out.devices.insert(d, caps_from_text(&text));
            }
        }
    }
    Ok(out)
}

/// Replace what `owner` grants `grantee` (caps already normalized).
pub fn replace_grants(c: &mut Connection, owner: &str, grantee: &str, grants: &Grants) -> R<()> {
    let now = now_s();
    let tx = c.transaction()?;
    tx.execute(
        "DELETE FROM grants WHERE owner_user = ?1 AND grantee_user = ?2",
        params![owner, grantee],
    )?;
    let text = |list: &Vec<String>| serde_json::to_string(list).unwrap_or_else(|_| "[]".into());
    tx.execute(
        "INSERT INTO grants (owner_user, grantee_user, device_id, caps, updated_at)
         VALUES (?1, ?2, NULL, ?3, ?4)",
        params![owner, grantee, text(&grants.default), now],
    )?;
    for (device, list) in &grants.devices {
        tx.execute(
            "INSERT INTO grants (owner_user, grantee_user, device_id, caps, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![owner, grantee, device, text(list), now],
        )?;
    }
    tx.commit()
}

/// Capabilities a friend holds on one device, from the owner's grants:
/// the device entry when present, else the default entry.
fn friend_caps(grants: &Grants, device_id: &str) -> Vec<String> {
    grants
        .devices
        .get(device_id)
        .cloned()
        .unwrap_or_else(|| grants.default.clone())
}

/// How `user_id` relates to `device` and the capabilities it holds there:
/// own devices grant everything, friends' devices their owner's grants
/// (possibly none). `None` when the device is neither.
pub fn access(c: &Connection, user_id: &str, device: &DeviceRow) -> R<Option<(Relation, Vec<String>)>> {
    let Some(owner) = device.user_id.as_deref() else {
        return Ok(None);
    };
    if owner == user_id {
        return Ok(Some((Relation::Own, caps::all())));
    }
    if device.platform == farwing_protocol::Platform::Web || !are_friends(c, owner, user_id)? {
        return Ok(None);
    }
    let grants = get_grants(c, owner, user_id)?;
    Ok(Some((Relation::Friend, friend_caps(&grants, &device.id))))
}

/// Capabilities `grantee` holds on each device of `owner` (empty when they
/// are not friends).
pub fn caps_on_devices(
    c: &Connection,
    owner: &str,
    grantee: &str,
) -> R<HashMap<String, Vec<String>>> {
    let friends = are_friends(c, owner, grantee)?;
    let grants = if friends {
        get_grants(c, owner, grantee)?
    } else {
        Grants::default()
    };
    Ok(user_device_ids(c, owner)?
        .into_iter()
        .map(|id| {
            let caps = if friends {
                friend_caps(&grants, &id)
            } else {
                Vec::new()
            };
            (id, caps)
        })
        .collect())
}

/// Friends' devices `user_id` holds at least one capability on:
/// `(device, owner, capabilities)`.
pub fn shared_devices(c: &Connection, user_id: &str) -> R<Vec<(DeviceRow, UserRow, Vec<String>)>> {
    let mut out = Vec::new();
    for (owner, _) in list_friends(c, user_id)? {
        let grants = get_grants(c, &owner.id, user_id)?;
        for device in list_user_devices(c, &owner.id)? {
            // Browsers are never controlled; friends do not see them.
            if device.platform == farwing_protocol::Platform::Web {
                continue;
            }
            let caps = friend_caps(&grants, &device.id);
            if !caps.is_empty() {
                out.push((device, owner.clone(), caps));
            }
        }
    }
    Ok(out)
}

/// Friends of the device's owner that hold at least one capability on it:
/// `(user_id, capabilities)`.
pub fn device_audience(c: &Connection, device: &DeviceRow) -> R<Vec<(String, Vec<String>)>> {
    let Some(owner) = device.user_id.as_deref() else {
        return Ok(Vec::new());
    };
    if device.platform == farwing_protocol::Platform::Web {
        return Ok(Vec::new());
    }
    let mut out = Vec::new();
    for friend in friend_ids(c, owner)? {
        let caps = friend_caps(&get_grants(c, owner, &friend)?, &device.id);
        if !caps.is_empty() {
            out.push((friend, caps));
        }
    }
    Ok(out)
}

/// Devices of the owner's friends that may control `device`, with what
/// they may do there: `(device, friend, capabilities)`.
pub fn friend_access(c: &Connection, device: &DeviceRow) -> R<Vec<(DeviceRow, UserRow, Vec<String>)>> {
    let mut out = Vec::new();
    for (friend, caps) in device_audience(c, device)? {
        let Some(user) = get_user(c, &friend)? else {
            continue;
        };
        if user.disabled {
            continue;
        }
        for row in list_user_devices(c, &friend)? {
            out.push((row, user.clone(), caps.clone()));
        }
    }
    Ok(out)
}

// ================================================================== sessions

#[derive(Clone, Debug)]
pub struct SessionRecord {
    pub id: String,
    pub host_device_id: String,
    pub controller_device_id: String,
    pub controller_user_id: String,
    pub purpose: String,
    pub connect_type: String,
    pub relation: String,
    pub state: String,
    pub created_at: i64,
    pub started_at: Option<i64>,
}

pub fn insert_session(c: &Connection, s: &SessionRecord) -> R<()> {
    c.execute(
        "INSERT OR REPLACE INTO sessions (id, host_device_id, controller_device_id, controller_user_id,
             purpose, connect_type, relation, state, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
        params![
            s.id,
            s.host_device_id,
            s.controller_device_id,
            s.controller_user_id,
            s.purpose,
            s.connect_type,
            s.relation,
            s.state,
            s.created_at
        ],
    )?;
    Ok(())
}

pub fn set_session_state(c: &Connection, id: &str, state: &str, started_at: Option<i64>) -> R<()> {
    c.execute(
        "UPDATE sessions SET state = ?2, started_at = COALESCE(started_at, ?3)
         WHERE id = ?1 AND ended_at IS NULL",
        params![id, state, started_at],
    )?;
    Ok(())
}

pub fn end_session(c: &Connection, id: &str, reason: &str) -> R<()> {
    c.execute(
        "UPDATE sessions SET state = 'ended', ended_at = ?2, end_reason = ?3
         WHERE id = ?1 AND ended_at IS NULL",
        params![id, now_s(), reason],
    )?;
    Ok(())
}

/// Each payer's bytes in `month` (`YYYY-MM`); the guests' payer is empty.
pub fn egress_month(c: &Connection, month: &str) -> R<Vec<(String, u64)>> {
    let mut st = c.prepare("SELECT payer, bytes FROM egress_usage WHERE month = ?1")?;
    st.query_map([month], |r| Ok((r.get(0)?, r.get::<_, i64>(1)?.max(0) as u64)))?
        .collect()
}

/// Adds `(month, payer, bytes)` to the stored totals.
pub fn egress_add(c: &mut Connection, rows: &[(String, String, u64)]) -> R<()> {
    let tx = c.transaction()?;
    {
        let mut st = tx.prepare(
            "INSERT INTO egress_usage (payer, month, bytes) VALUES (?1, ?2, ?3)
             ON CONFLICT (payer, month) DO UPDATE SET bytes = bytes + excluded.bytes",
        )?;
        for (month, payer, bytes) in rows {
            st.execute(params![payer, month, i64::try_from(*bytes).unwrap_or(i64::MAX)])?;
        }
    }
    tx.commit()
}

pub fn end_open_sessions(c: &Connection, reason: &str) -> R<usize> {
    c.execute(
        "UPDATE sessions SET state = 'ended', ended_at = ?1, end_reason = ?2 WHERE ended_at IS NULL",
        params![now_s(), reason],
    )
}

pub fn list_open_sessions(c: &Connection) -> R<Vec<SessionRecord>> {
    let mut st = c.prepare(
        "SELECT id, host_device_id, controller_device_id, controller_user_id, purpose, connect_type,
             relation, state, created_at, started_at
         FROM sessions WHERE ended_at IS NULL ORDER BY created_at",
    )?;
    st.query_map([], |r| {
        Ok(SessionRecord {
            id: r.get(0)?,
            host_device_id: r.get(1)?,
            controller_device_id: r.get(2)?,
            controller_user_id: r.get(3)?,
            purpose: r.get(4)?,
            connect_type: r.get(5)?,
            relation: r.get(6)?,
            state: r.get(7)?,
            created_at: r.get(8)?,
            started_at: r.get(9)?,
        })
    })?
    .collect()
}

// ============================================================= assist saved

#[derive(Clone, Debug)]
pub struct SavedRow {
    pub connect_id: String,
    pub publisher_device_id: String,
    pub remark: String,
    pub is_favorite: bool,
    pub favorited_at: Option<i64>,
    pub last_connected_at: Option<i64>,
    /// The assisted device's key (empty when the device is gone).
    pub public_key: String,
}

const SAVED_COLS: &str = "connect_id, publisher_device_id, remark, is_favorite, favorited_at, \
     last_connected_at, COALESCE((SELECT public_key FROM devices \
     WHERE devices.id = assist_saved.publisher_device_id), '')";

fn saved_from_row(r: &Row<'_>) -> R<SavedRow> {
    Ok(SavedRow {
        connect_id: r.get(0)?,
        publisher_device_id: r.get(1)?,
        remark: r.get(2)?,
        is_favorite: r.get::<_, i64>(3)? != 0,
        favorited_at: r.get(4)?,
        last_connected_at: r.get(5)?,
        public_key: r.get(6)?,
    })
}

pub fn record_recent(c: &Connection, user_id: &str, connect_id: &str, device_id: &str) -> R<()> {
    c.execute(
        "INSERT INTO assist_saved (user_id, connect_id, publisher_device_id, last_connected_at)
         VALUES (?1, ?2, ?3, ?4)
         ON CONFLICT (user_id, connect_id) DO UPDATE SET
             publisher_device_id = excluded.publisher_device_id,
             last_connected_at = excluded.last_connected_at",
        params![user_id, connect_id, device_id, now_s()],
    )?;
    Ok(())
}

pub fn list_recent(c: &Connection, user_id: &str) -> R<Vec<SavedRow>> {
    let mut st = c.prepare(&format!(
        "SELECT {SAVED_COLS} FROM assist_saved
         WHERE user_id = ?1 AND last_connected_at IS NOT NULL
         ORDER BY last_connected_at DESC LIMIT 200"
    ))?;
    st.query_map([user_id], saved_from_row)?.collect()
}

pub fn list_favorites(c: &Connection, user_id: &str) -> R<Vec<SavedRow>> {
    let mut st = c.prepare(&format!(
        "SELECT {SAVED_COLS} FROM assist_saved
         WHERE user_id = ?1 AND is_favorite = 1
         ORDER BY favorited_at DESC, connect_id LIMIT 500"
    ))?;
    st.query_map([user_id], saved_from_row)?.collect()
}

/// Replace the favorites set. `items` are `(connect_id, publisher_device_id, remark, favorited_at)`.
pub fn replace_favorites(
    c: &mut Connection,
    user_id: &str,
    items: &[(String, String, String, i64)],
) -> R<()> {
    let tx = c.transaction()?;
    tx.execute(
        "UPDATE assist_saved SET is_favorite = 0, favorited_at = NULL WHERE user_id = ?1",
        [user_id],
    )?;
    for (connect_id, device_id, remark, at) in items {
        tx.execute(
            "INSERT INTO assist_saved (user_id, connect_id, publisher_device_id, remark, is_favorite, favorited_at)
             VALUES (?1, ?2, ?3, ?4, 1, ?5)
             ON CONFLICT (user_id, connect_id) DO UPDATE SET
                 publisher_device_id = CASE WHEN excluded.publisher_device_id <> ''
                     THEN excluded.publisher_device_id ELSE assist_saved.publisher_device_id END,
                 remark = excluded.remark, is_favorite = 1, favorited_at = excluded.favorited_at",
            params![user_id, connect_id, device_id, remark, at],
        )?;
    }
    tx.execute(
        "DELETE FROM assist_saved WHERE user_id = ?1 AND is_favorite = 0 AND last_connected_at IS NULL",
        [user_id],
    )?;
    tx.commit()
}

/// Remove `key` (publisher device ID or connect ID) from the `recent` or `favorites` list.
pub fn delete_saved(c: &Connection, user_id: &str, kind: &str, key: &str) -> R<usize> {
    let n = if kind == "recent" {
        c.execute(
            "UPDATE assist_saved SET last_connected_at = NULL
             WHERE user_id = ?1 AND (publisher_device_id = ?2 OR connect_id = ?2)",
            params![user_id, key],
        )?
    } else {
        c.execute(
            "UPDATE assist_saved SET is_favorite = 0, favorited_at = NULL
             WHERE user_id = ?1 AND (publisher_device_id = ?2 OR connect_id = ?2)",
            params![user_id, key],
        )?
    };
    c.execute(
        "DELETE FROM assist_saved WHERE user_id = ?1 AND is_favorite = 0 AND last_connected_at IS NULL",
        [user_id],
    )?;
    Ok(n)
}

pub fn clear_recent(c: &Connection, user_id: &str) -> R<()> {
    c.execute(
        "UPDATE assist_saved SET last_connected_at = NULL WHERE user_id = ?1",
        [user_id],
    )?;
    c.execute(
        "DELETE FROM assist_saved WHERE user_id = ?1 AND is_favorite = 0",
        [user_id],
    )?;
    Ok(())
}

// ===================================================================== power

#[derive(Clone, Debug)]
pub struct PowerRequestRow {
    pub id: String,
    pub device_id: String,
    pub user_id: String,
    pub requester_device_id: String,
    pub action: String,
    pub result_code: Option<i64>,
}

pub fn insert_power_request(
    c: &Connection,
    id: &str,
    device_id: &str,
    user_id: &str,
    requester: &str,
    action: &str,
) -> R<()> {
    c.execute(
        "INSERT INTO power_requests (id, device_id, user_id, requester_device_id, action, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        params![id, device_id, user_id, requester, action, now_s()],
    )?;
    Ok(())
}

pub fn get_power_request(c: &Connection, id: &str) -> R<Option<PowerRequestRow>> {
    c.query_row(
        "SELECT id, device_id, user_id, requester_device_id, action, result_code
         FROM power_requests WHERE id = ?1",
        [id],
        |r| {
            Ok(PowerRequestRow {
                id: r.get(0)?,
                device_id: r.get(1)?,
                user_id: r.get(2)?,
                requester_device_id: r.get(3)?,
                action: r.get(4)?,
                result_code: r.get(5)?,
            })
        },
    )
    .optional()
}

pub fn set_power_result(c: &Connection, id: &str, code: i32) -> R<()> {
    c.execute(
        "UPDATE power_requests SET result_code = ?2 WHERE id = ?1",
        params![id, code],
    )?;
    Ok(())
}

// ============================================================ admin events

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn grants_resolution_and_friendship_cleanup() {
        let dir = tempfile::tempdir().unwrap();
        let mut c = open_connection(&dir.path().join("t.db")).unwrap();
        create_user(&c, "u1", "alice", "", "x").unwrap();
        create_user(&c, "u2", "bob", "", "x").unwrap();
        let req = DeviceInitRequest {
            public_key: "k1".into(),
            ..Default::default()
        };
        insert_device(&c, "d1", "k1", &req, "").unwrap();
        set_device_user(&c, "d1", Some("u1")).unwrap();
        let d1 = get_device(&c, "d1").unwrap().unwrap();
        assert_eq!(
            access(&c, "u1", &d1).unwrap(),
            Some((Relation::Own, caps::all()))
        );
        assert_eq!(access(&c, "u2", &d1).unwrap(), None);
        make_friends(&mut c, "u2", "u1", &["view".into()]).unwrap();
        assert_eq!(
            access(&c, "u2", &d1).unwrap(),
            Some((Relation::Friend, vec!["view".into(), "audio".into(), "annotation".into()]))
        );
        let mut g = Grants::default();
        g.devices.insert("d1".into(), vec!["files".into()]);
        replace_grants(&mut c, "u1", "u2", &g).unwrap();
        assert_eq!(get_grants(&c, "u1", "u2").unwrap(), g);
        assert_eq!(
            access(&c, "u2", &d1).unwrap().unwrap().1,
            vec!["files".to_owned()]
        );
        assert_eq!(device_audience(&c, &d1).unwrap().len(), 1);
        // A new owner drops the device-specific grants of the old one.
        set_device_user(&c, "d1", Some("u2")).unwrap();
        assert!(get_grants(&c, "u1", "u2").unwrap().devices.is_empty());
        assert!(remove_friendship(&mut c, "u1", "u2").unwrap());
        assert_eq!(get_grants(&c, "u2", "u1").unwrap(), Grants::default());
        assert!(!are_friends(&c, "u1", "u2").unwrap());
    }
}
