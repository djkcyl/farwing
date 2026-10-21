//! Administration API of the web console (`docs/web-console.md` §6). Every
//! endpoint needs [`AdminAuth`]; every change is written to the audit log.

use super::auth::AdminAuth;
use super::me::{audit, new_hash};
use super::{App, JsonBody};
use crate::crypto::{new_uuid, sha256_hex};
use crate::db::{self, DeviceRow};
use crate::error::{ApiError, ApiResult};
use crate::settings::RuntimeSettings;
use crate::webdb::{self, AccountSummary, Announcement, AuditEntry, Invite, InviteUse};
use axum::Json;
use axum::extract::{Path, Query, State};
use farwing_protocol::ErrorCode;
use farwing_protocol::api::Empty;
use farwing_protocol::error::reason;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use std::collections::{HashMap, HashSet};

/// Record an administrator's change; `detail` as in [`audit`].
async fn log(app: &App, admin: &AdminAuth, kind: &str, target: &str, detail: Value) {
    audit(
        app,
        Some((&admin.user().id, &admin.user().username)),
        kind,
        target,
        detail,
        true,
        admin.ip,
    )
    .await;
}

/// A random password: 12 letters and digits, with both.
fn random_password() -> String {
    const LETTERS: &[u8] = b"abcdefghjkmnpqrstuvwxyzABCDEFGHJKLMNPQRSTUVWXYZ";
    const DIGITS: &[u8] = b"23456789";
    loop {
        let bytes: [u8; 12] = crate::crypto::random_array();
        let text: String = bytes
            .iter()
            .enumerate()
            .map(|(i, b)| {
                if i % 4 == 3 {
                    DIGITS[*b as usize % DIGITS.len()] as char
                } else {
                    LETTERS[*b as usize % LETTERS.len()] as char
                }
            })
            .collect();
        if crate::admin::check_password(&text).is_ok() {
            return text;
        }
    }
}

// ================================================================ overview

#[derive(Serialize)]
pub struct Overview {
    pub version: String,
    pub instance_name: String,
    pub public_url: String,
    pub uptime_s: u64,
    pub tls_fingerprint: Option<String>,
    /// Automatic certificate (`[tls] mode = "acme"`).
    pub acme: Option<crate::acme::Status>,
    pub turn_mode: String,
    pub nat_discovery: Vec<String>,
    pub db_bytes: u64,
    pub accounts: i64,
    pub admins: i64,
    pub devices: i64,
    pub devices_online: usize,
    pub sessions: usize,
    pub sessions_connected: usize,
    pub guests: bool,
    pub registration: bool,
    pub recent: Vec<AuditEntry>,
}

/// `GET /api/v2/admin/overview`
pub async fn overview(State(app): State<App>, _admin: AdminAuth) -> ApiResult<Json<Overview>> {
    let (accounts, admins, devices, recent) = app
        .db
        .call(|c| {
            let accounts: i64 = c.query_row("SELECT COUNT(*) FROM users", [], |r| r.get(0))?;
            let devices: i64 = c.query_row("SELECT COUNT(*) FROM devices", [], |r| r.get(0))?;
            Ok((
                accounts,
                webdb::admin_count(c)?,
                devices,
                webdb::audit_entries(c, None, None, None, 8)?,
            ))
        })
        .await?;
    let db_path = app.cfg.db_path();
    let db_bytes = ["", "-wal"]
        .iter()
        .filter_map(|suffix| {
            let mut p = db_path.clone().into_os_string();
            p.push(suffix);
            std::fs::metadata(p).ok().map(|m| m.len())
        })
        .sum();
    let sessions = app.hub.live_sessions();
    let settings = app.settings.read().clone();
    Ok(Json(Overview {
        version: env!("CARGO_PKG_VERSION").to_owned(),
        instance_name: settings.instance_name,
        public_url: app.cfg.public_url.clone(),
        uptime_s: app.started_at.elapsed().as_secs(),
        tls_fingerprint: app.tls.as_ref().map(|t| t.fingerprint()),
        acme: match &app.tls {
            Some(crate::tls::Presented::Acme(acme)) => Some(acme.status()),
            _ => None,
        },
        turn_mode: format!("{:?}", app.cfg.turn.mode).to_lowercase(),
        nat_discovery: app.nat_discovery.clone(),
        db_bytes,
        accounts,
        admins,
        devices,
        devices_online: app.hub.online_devices().len(),
        sessions_connected: sessions.iter().filter(|s| s.connected).count(),
        sessions: sessions.len(),
        guests: settings.guests,
        registration: settings.registration,
        recent,
    }))
}

// ================================================================ accounts

/// `GET /api/v2/admin/users`
pub async fn users(State(app): State<App>, _admin: AdminAuth) -> ApiResult<Json<Vec<AccountSummary>>> {
    Ok(Json(app.db.call(|c| webdb::list_accounts(c)).await?))
}

#[derive(Deserialize)]
pub struct CreateUser {
    pub username: String,
    #[serde(default)]
    pub nickname: String,
    /// Empty: generate one and return it.
    #[serde(default)]
    pub password: String,
    #[serde(default)]
    pub admin: bool,
}

#[derive(Serialize)]
pub struct CreatedUser {
    pub id: String,
    /// The generated password, shown once.
    pub password: Option<String>,
}

/// `POST /api/v2/admin/users`
pub async fn create_user(
    State(app): State<App>,
    admin: AdminAuth,
    JsonBody(req): JsonBody<CreateUser>,
) -> ApiResult<Json<CreatedUser>> {
    let username = req.username.trim().to_owned();
    super::me::username_rule(&username)?;
    let generated = req.password.is_empty().then(random_password);
    let password = generated.clone().unwrap_or(req.password);
    super::me::password_rule(&password)?;
    let hash = new_hash(password).await?;
    let id = new_uuid();
    let (uid, name, nickname, is_admin) = (
        id.clone(),
        username.clone(),
        db::clip(req.nickname.trim(), 64),
        req.admin,
    );
    let created = app
        .db
        .call(move |c| {
            if db::get_user_by_name(c, &name)?.is_some() {
                return Ok(false);
            }
            db::create_user(c, &uid, &name, &nickname, &hash)?;
            if is_admin {
                webdb::set_role(c, &uid, webdb::ROLE_ADMIN)?;
            }
            Ok(true)
        })
        .await?;
    if !created {
        return Err(super::me::username_taken());
    }
    let detail = if req.admin { json!({ "admin": true }) } else { Value::Null };
    log(&app, &admin, "user.create", &username, detail).await;
    Ok(Json(CreatedUser {
        id,
        password: generated,
    }))
}

#[derive(Serialize)]
pub struct DeviceEntry {
    pub device_id: String,
    pub alias: String,
    pub name: String,
    pub platform: String,
    pub os: String,
    pub version: String,
    pub owner_id: Option<String>,
    pub owner: Option<String>,
    pub online: bool,
    pub host: bool,
    pub connect_id: Option<String>,
    pub created_ip: String,
    pub created_at: i64,
    pub last_seen_at: i64,
}

fn device_entry(
    d: &DeviceRow,
    owners: &HashMap<String, String>,
    online: &HashSet<String>,
    ips: &HashMap<String, String>,
) -> DeviceEntry {
    DeviceEntry {
        device_id: d.id.clone(),
        alias: d.alias.clone(),
        name: d.name.clone(),
        platform: d.platform.as_str().to_owned(),
        os: d.os.clone(),
        version: d.version.clone(),
        owner_id: d.user_id.clone(),
        owner: d.user_id.as_ref().and_then(|u| owners.get(u).cloned()),
        online: online.contains(&d.id),
        host: d.host_supported(),
        connect_id: d.connect_id.clone(),
        created_ip: ips.get(&d.id).cloned().unwrap_or_default(),
        created_at: d.created_at,
        last_seen_at: d.last_seen_at,
    }
}

#[derive(Serialize)]
pub struct Login {
    pub device_id: String,
    pub alias: String,
    pub platform: String,
    pub created_at: i64,
    pub last_used_at: i64,
}

#[derive(Serialize)]
pub struct Friend {
    pub user_id: String,
    pub username: String,
    pub nickname: String,
    pub since: i64,
    /// What this account grants the friend.
    pub grants_given: farwing_protocol::api::Grants,
    /// What the friend grants this account.
    pub grants_received: farwing_protocol::api::Grants,
}

#[derive(Serialize)]
pub struct UserDetail {
    pub account: AccountSummary,
    pub devices: Vec<DeviceEntry>,
    pub logins: Vec<Login>,
    pub friends: Vec<Friend>,
}

/// `GET /api/v2/admin/users/{id}`
pub async fn user(
    State(app): State<App>,
    _admin: AdminAuth,
    Path(id): Path<String>,
) -> ApiResult<Json<UserDetail>> {
    let online: HashSet<String> = app.hub.online_devices().into_iter().collect();
    let detail = app
        .db
        .call(move |c| {
            let Some(account) = webdb::account(c, &id)? else {
                return Ok(None);
            };
            let owners = HashMap::from([(account.id.clone(), account.username.clone())]);
            let ips = created_ips(c)?;
            let devices: Vec<DeviceRow> = db::list_user_devices(c, &id)?;
            let aliases: HashMap<String, (String, String)> = devices
                .iter()
                .map(|d| (d.id.clone(), (d.alias.clone(), d.platform.as_str().to_owned())))
                .collect();
            let logins = db::list_logins(c, &id)?
                .into_iter()
                .map(|l| {
                    let (alias, platform) = aliases.get(&l.device_id).cloned().unwrap_or_default();
                    Login {
                        device_id: l.device_id,
                        alias,
                        platform,
                        created_at: l.created_at,
                        last_used_at: l.last_used_at,
                    }
                })
                .collect();
            let mut friends = Vec::new();
            for (user, since) in db::list_friends(c, &id)? {
                friends.push(Friend {
                    grants_given: db::get_grants(c, &id, &user.id)?,
                    grants_received: db::get_grants(c, &user.id, &id)?,
                    user_id: user.id,
                    username: user.username,
                    nickname: user.nickname,
                    since,
                });
            }
            Ok(Some(UserDetail {
                devices: devices
                    .iter()
                    .map(|d| device_entry(d, &owners, &HashSet::new(), &ips))
                    .collect(),
                account,
                logins,
                friends,
            }))
        })
        .await?;
    let mut detail = detail.ok_or_else(ApiError::not_found)?;
    for d in &mut detail.devices {
        d.online = online.contains(&d.device_id);
    }
    Ok(Json(detail))
}

#[derive(Deserialize)]
pub struct UpdateUser {
    pub nickname: Option<String>,
    pub admin: Option<bool>,
    pub disabled: Option<bool>,
}

/// `PATCH /api/v2/admin/users/{id}`: nickname, role, disabled.
pub async fn update_user(
    State(app): State<App>,
    admin: AdminAuth,
    Path(id): Path<String>,
    JsonBody(req): JsonBody<UpdateUser>,
) -> ApiResult<Json<Empty>> {
    let me = admin.user().id.clone();
    let uid = id.clone();
    let outcome = app
        .db
        .call(move |c| {
            let Some(account) = webdb::account(c, &uid)? else {
                return Ok(Err(ApiError::not_found()));
            };
            let is_admin = account.role == webdb::ROLE_ADMIN && !account.disabled;
            let losing_admin = is_admin
                && (req.admin == Some(false) || req.disabled == Some(true));
            if losing_admin && webdb::admin_count(c)? <= 1 {
                return Ok(Err(last_admin()));
            }
            if uid == me && (req.admin == Some(false) || req.disabled == Some(true)) {
                return Ok(Err(ApiError::reason(
                    ErrorCode::BadRequest,
                    reason::SELF_DEMOTE,
                    "cannot demote or disable your own account",
                )));
            }
            if let Some(nickname) = &req.nickname {
                webdb::set_nickname(c, &uid, &db::clip(nickname.trim(), 64))?;
            }
            if let Some(make_admin) = req.admin {
                webdb::set_role(
                    c,
                    &uid,
                    if make_admin { webdb::ROLE_ADMIN } else { webdb::ROLE_USER },
                )?;
            }
            if let Some(disabled) = req.disabled {
                db::set_user_disabled(c, &uid, disabled)?;
                if disabled {
                    db::revoke_tokens_for_user(c, &uid, "user_disabled")?;
                }
            }
            Ok(Ok((account.username, req.admin, req.disabled)))
        })
        .await?;
    let (username, role, disabled) = outcome?;
    if disabled == Some(true) {
        app.hub.revoke_user(&id);
    }
    let mut changes = Map::new();
    if let Some(r) = role {
        changes.insert("admin".into(), r.into());
    }
    if let Some(d) = disabled {
        changes.insert("disabled".into(), d.into());
    }
    log(&app, &admin, "user.update", &username, changes_detail(changes)).await;
    Ok(Json(Empty {}))
}

#[derive(Deserialize)]
pub struct ResetPassword {
    #[serde(default)]
    pub password: String,
}

#[derive(Serialize)]
pub struct NewPassword {
    pub password: Option<String>,
}

/// `POST /api/v2/admin/users/{id}/password`: every login of the account
/// ends.
pub async fn reset_password(
    State(app): State<App>,
    admin: AdminAuth,
    Path(id): Path<String>,
    JsonBody(req): JsonBody<ResetPassword>,
) -> ApiResult<Json<NewPassword>> {
    let generated = req.password.is_empty().then(random_password);
    let password = generated.clone().unwrap_or(req.password);
    super::me::password_rule(&password)?;
    let hash = new_hash(password).await?;
    let uid = id.clone();
    let username = app
        .db
        .call(move |c| {
            let Some(user) = db::get_user(c, &uid)? else {
                return Ok(None);
            };
            db::set_user_password(c, &uid, &hash)?;
            db::revoke_tokens_for_user(c, &uid, "password_reset")?;
            Ok(Some(user.username))
        })
        .await?
        .ok_or_else(ApiError::not_found)?;
    app.hub.revoke_user(&id);
    log(&app, &admin, "user.password", &username, Value::Null).await;
    Ok(Json(NewPassword {
        password: generated,
    }))
}

/// `POST /api/v2/admin/users/{id}/totp/reset`
pub async fn reset_totp(
    State(app): State<App>,
    admin: AdminAuth,
    Path(id): Path<String>,
) -> ApiResult<Json<Empty>> {
    let uid = id.clone();
    let username = app
        .db
        .call(move |c| {
            let Some(user) = db::get_user(c, &uid)? else {
                return Ok(None);
            };
            webdb::clear_totp(c, &uid)?;
            Ok(Some(user.username))
        })
        .await?
        .ok_or_else(ApiError::not_found)?;
    log(&app, &admin, "user.totp_reset", &username, Value::Null).await;
    Ok(Json(Empty {}))
}

/// `DELETE /api/v2/admin/users/{id}`
pub async fn delete_user(
    State(app): State<App>,
    admin: AdminAuth,
    Path(id): Path<String>,
) -> ApiResult<Json<Empty>> {
    if id == admin.user().id {
        return Err(ApiError::reason(
            ErrorCode::BadRequest,
            reason::SELF_DELETE,
            "cannot delete your own account",
        ));
    }
    let uid = id.clone();
    let outcome = app
        .db
        .call(move |c| {
            let Some(account) = webdb::account(c, &uid)? else {
                return Ok(Err(ApiError::not_found()));
            };
            if account.role == webdb::ROLE_ADMIN
                && !account.disabled
                && webdb::admin_count(c)? <= 1
            {
                return Ok(Err(last_admin()));
            }
            db::delete_user(c, &uid)?;
            Ok(Ok(account.username))
        })
        .await?;
    let username = outcome?;
    app.hub.revoke_user(&id);
    log(&app, &admin, "user.delete", &username, Value::Null).await;
    Ok(Json(Empty {}))
}

// ================================================================= invites

#[derive(Deserialize)]
pub struct CreateInvite {
    #[serde(default)]
    pub note: String,
    pub max_uses: Option<i64>,
    /// Hours until it expires; `None` = never.
    pub valid_hours: Option<i64>,
    #[serde(default)]
    pub auto_friend: bool,
}

#[derive(Serialize)]
pub struct CreatedInvite {
    pub id: String,
    /// The code, shown once.
    pub code: String,
}

/// `GET /api/v2/admin/invites`
pub async fn invites(State(app): State<App>, _admin: AdminAuth) -> ApiResult<Json<Vec<Invite>>> {
    Ok(Json(app.db.call(|c| webdb::list_invites(c)).await?))
}

/// `POST /api/v2/admin/invites`
pub async fn create_invite(
    State(app): State<App>,
    admin: AdminAuth,
    JsonBody(req): JsonBody<CreateInvite>,
) -> ApiResult<Json<CreatedInvite>> {
    const USES: std::ops::RangeInclusive<i64> = 1..=1000;
    if req.max_uses.is_some_and(|n| !USES.contains(&n)) {
        return Err(ApiError::reason(
            ErrorCode::BadRequest,
            reason::INVITE_USES_INVALID,
            "max_uses must be 1-1000",
        )
        .with_detail("min", *USES.start())
        .with_detail("max", *USES.end()));
    }
    if req.valid_hours.is_some_and(|h| !(1..=24 * 365).contains(&h)) {
        return Err(ApiError::reason(
            ErrorCode::BadRequest,
            reason::INVITE_VALIDITY_INVALID,
            "valid_hours must be 1 hour to 1 year",
        ));
    }
    let code = {
        const ALPHABET: &[u8] = b"ABCDEFGHJKLMNPQRSTUVWXYZ23456789";
        let bytes: [u8; 12] = crate::crypto::random_array();
        let chars: String = bytes
            .iter()
            .map(|b| ALPHABET[*b as usize % ALPHABET.len()] as char)
            .collect();
        format!("{}-{}-{}", &chars[..4], &chars[4..8], &chars[8..])
    };
    let id = new_uuid();
    let (iid, hash, by, note) = (
        id.clone(),
        sha256_hex(code.as_bytes()),
        admin.user().id.clone(),
        db::clip(req.note.trim(), 128),
    );
    let expires = req.valid_hours.map(|h| db::now_s() + h * 3600);
    let (max, auto) = (req.max_uses, req.auto_friend);
    let note2 = note.clone();
    app.db
        .call(move |c| webdb::create_invite(c, &iid, &hash, &by, &note2, max, expires, auto))
        .await?;
    log(&app, &admin, "invite.create", &note, Value::Null).await;
    Ok(Json(CreatedInvite { id, code }))
}

/// `DELETE /api/v2/admin/invites/{id}`
pub async fn revoke_invite(
    State(app): State<App>,
    admin: AdminAuth,
    Path(id): Path<String>,
) -> ApiResult<Json<Empty>> {
    let iid = id.clone();
    app.db.call(move |c| webdb::revoke_invite(c, &iid)).await?;
    log(&app, &admin, "invite.revoke", &id, Value::Null).await;
    Ok(Json(Empty {}))
}

/// `GET /api/v2/admin/invites/{id}/uses`
pub async fn invite_uses(
    State(app): State<App>,
    _admin: AdminAuth,
    Path(id): Path<String>,
) -> ApiResult<Json<Vec<InviteUse>>> {
    Ok(Json(app.db.call(move |c| webdb::invite_uses(c, &id)).await?))
}

// ================================================================= devices

fn created_ips(c: &rusqlite::Connection) -> db::R<HashMap<String, String>> {
    let mut st = c.prepare("SELECT id, created_ip FROM devices")?;
    st.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect()
}

/// `GET /api/v2/admin/devices`
pub async fn devices(State(app): State<App>, _admin: AdminAuth) -> ApiResult<Json<Vec<DeviceEntry>>> {
    let online: HashSet<String> = app.hub.online_devices().into_iter().collect();
    let entries = app
        .db
        .call(move |c| {
            let owners: HashMap<String, String> = db::list_users(c)?
                .into_iter()
                .map(|u| (u.id, u.username))
                .collect();
            let ips = created_ips(c)?;
            let mut entries: Vec<DeviceEntry> = db::list_all_devices(c)?
                .iter()
                .map(|d| device_entry(d, &owners, &online, &ips))
                .collect();
            entries.sort_by(|a, b| {
                b.online
                    .cmp(&a.online)
                    .then(b.last_seen_at.cmp(&a.last_seen_at))
            });
            Ok(entries)
        })
        .await?;
    Ok(Json(entries))
}

/// `DELETE /api/v2/admin/devices/{id}`: unbind from its account and sign
/// it out.
pub async fn unbind_device(
    State(app): State<App>,
    admin: AdminAuth,
    Path(id): Path<String>,
) -> ApiResult<Json<Empty>> {
    let did = id.clone();
    let alias = app
        .db
        .call(move |c| {
            let Some(d) = db::get_device(c, &did)? else {
                return Ok(None);
            };
            db::revoke_tokens_for_device(c, &d.id, "removed_by_admin")?;
            db::set_device_user(c, &d.id, None)?;
            Ok(Some(d.alias))
        })
        .await?
        .ok_or_else(ApiError::not_found)?;
    app.hub.revoke_device(&id);
    log(&app, &admin, "device.unbind", &format!("{alias} [{id}]"), Value::Null).await;
    Ok(Json(Empty {}))
}

/// `POST /api/v2/admin/devices/{id}/disconnect`: close its connections
/// (it reconnects when its credentials are still valid).
pub async fn disconnect_device(
    State(app): State<App>,
    admin: AdminAuth,
    Path(id): Path<String>,
) -> ApiResult<Json<Empty>> {
    app.hub.disconnect_device(&id);
    log(&app, &admin, "device.disconnect", &id, Value::Null).await;
    Ok(Json(Empty {}))
}

// ================================================================ sessions

#[derive(Serialize)]
pub struct SessionEntry {
    #[serde(flatten)]
    pub session: crate::hub::LiveSession,
    pub host_alias: String,
    pub host_owner: Option<String>,
    pub controller_alias: String,
}

/// `GET /api/v2/admin/sessions`
pub async fn sessions(State(app): State<App>, _admin: AdminAuth) -> ApiResult<Json<Vec<SessionEntry>>> {
    let live = app.hub.live_sessions();
    let entries = app
        .db
        .call(move |c| {
            let owners: HashMap<String, String> = db::list_users(c)?
                .into_iter()
                .map(|u| (u.id, u.username))
                .collect();
            let mut out = Vec::new();
            for session in live {
                let host = db::get_device(c, &session.host_device_id)?;
                let controller = db::get_device(c, &session.controller_device_id)?;
                out.push(SessionEntry {
                    host_alias: host.as_ref().map(|d| d.alias.clone()).unwrap_or_default(),
                    host_owner: host
                        .as_ref()
                        .and_then(|d| d.user_id.as_ref())
                        .and_then(|u| owners.get(u).cloned()),
                    controller_alias: controller.map(|d| d.alias).unwrap_or_default(),
                    session,
                });
            }
            Ok(out)
        })
        .await?;
    Ok(Json(entries))
}

/// `DELETE /api/v2/admin/sessions/{id}`
pub async fn end_session(
    State(app): State<App>,
    admin: AdminAuth,
    Path(id): Path<String>,
) -> ApiResult<Json<Empty>> {
    if !app.hub.end_session(&id) {
        return Err(ApiError::not_found());
    }
    log(&app, &admin, "session.end", &id, Value::Null).await;
    Ok(Json(Empty {}))
}

// ================================================================= updates

/// `GET /api/v2/admin/updates`
pub async fn updates(State(app): State<App>, _admin: AdminAuth) -> ApiResult<Json<Vec<Announcement>>> {
    Ok(Json(app.db.call(|c| webdb::announcements(c)).await?))
}

fn check_announcement(a: &Announcement) -> ApiResult<()> {
    let invalid = |why, message| Err(ApiError::reason(ErrorCode::BadRequest, why, message));
    if crate::config::normalize_platform(&a.platform).is_none() {
        return invalid(reason::UPDATE_PLATFORM_INVALID, "unknown platform");
    }
    if a.channel != "stable" && a.channel != "beta" {
        return invalid(reason::UPDATE_CHANNEL_INVALID, "channel must be stable or beta");
    }
    if a.version.trim().is_empty() || a.version.len() > 64 {
        return invalid(reason::UPDATE_VERSION_INVALID, "invalid version");
    }
    if !(a.url.starts_with("https://") || a.url.starts_with("http://")) || a.url.len() > 2048 {
        return invalid(reason::UPDATE_URL_INVALID, "url must be an http(s) link");
    }
    if a.notes.len() > 16 * 1024 {
        return invalid(reason::UPDATE_NOTES_TOO_LONG, "notes too long");
    }
    Ok(())
}

/// `POST /api/v2/admin/updates` (new) and `PUT /api/v2/admin/updates/{id}`.
pub async fn save_update(
    State(app): State<App>,
    admin: AdminAuth,
    id: Option<Path<String>>,
    JsonBody(mut req): JsonBody<Announcement>,
) -> ApiResult<Json<Announcement>> {
    req.id = id.map(|Path(id)| id).unwrap_or_else(new_uuid);
    req.platform = crate::config::normalize_platform(&req.platform)
        .unwrap_or("")
        .to_owned();
    if req.published_at <= 0 {
        req.published_at = db::now_s();
    }
    check_announcement(&req)?;
    let saved = req.clone();
    app.db
        .call(move |c| webdb::save_announcement(c, &saved))
        .await?;
    log(
        &app,
        &admin,
        "update.save",
        &format!("{} {} {}", req.platform, req.channel, req.version),
        Value::Null,
    )
    .await;
    Ok(Json(req))
}

/// `DELETE /api/v2/admin/updates/{id}`
pub async fn delete_update(
    State(app): State<App>,
    admin: AdminAuth,
    Path(id): Path<String>,
) -> ApiResult<Json<Empty>> {
    let uid = id.clone();
    app.db
        .call(move |c| webdb::delete_announcement(c, &uid))
        .await?;
    log(&app, &admin, "update.delete", &id, Value::Null).await;
    Ok(Json(Empty {}))
}

// ================================================================ settings

#[derive(Serialize)]
pub struct SettingsView {
    /// Changeable online.
    pub runtime: RuntimeSettings,
    /// The configuration file (secrets hidden), read-only.
    pub config: serde_json::Value,
}

fn config_view(app: &App) -> serde_json::Value {
    let c = &app.cfg;
    serde_json::json!({
        "listen": c.listen.to_string(),
        "public_url": c.public_url,
        "data_dir": c.data_dir.display().to_string(),
        "trusted_proxies": c.trusted_proxies.iter().map(|n| n.to_string()).collect::<Vec<_>>(),
        "tls": {
            "mode": format!("{:?}", c.tls.mode).to_lowercase(),
            "cert": c.tls.cert.as_ref().map(|p| p.display().to_string()),
            "self_signed": c.tls.self_signed,
            "hostnames": c.tls.hostnames,
            "acme_domains": (c.tls.mode == crate::config::TlsMode::Acme).then(|| c.acme_domains()),
            "acme_directory": (c.tls.mode == crate::config::TlsMode::Acme).then(|| c.tls.acme.directory_url()),
            "acme_http_listen": c.tls.acme.http_listen.map(|a| a.to_string()),
        },
        "turn": {
            "mode": format!("{:?}", c.turn.mode).to_lowercase(),
            "urls": c.turn.urls,
            // Whether one is set; the secret itself is never shown.
            "shared_secret": !c.turn.shared_secret.is_empty(),
            "credential_ttl_s": c.turn.credential_ttl.as_secs(),
            "force_relay": c.turn.force_relay,
            "listen_udp": c.turn.listen_udp.map(|a| a.to_string()),
            "listen_tcp": c.turn.listen_tcp.map(|a| a.to_string()),
            "public_ip": c.turn.public_ip.map(|a| a.to_string()),
            "relay_ports": [c.turn.relay_port_min, c.turn.relay_port_max],
            "stun": c.turn.stun.iter().map(|s| s.listen.to_string()).collect::<Vec<_>>(),
        },
        "web": {
            "enabled": c.web.enabled,
            "admin_allow": c.web.admin_allow.iter().map(|n| n.to_string()).collect::<Vec<_>>(),
            "session_idle_minutes": c.web.session_idle_minutes,
            "session_max_hours": c.web.session_max_hours,
            "audit_days": c.web.audit_days,
        },
        "limits": {
            "login_failures_per_ip": c.limits.login_failures_per_ip,
            "login_failures_per_user": c.limits.login_failures_per_user,
            "device_init_per_ip_hour": c.limits.device_init_per_ip_hour,
            "assist_failures_per_minute": c.limits.assist_failures_per_minute,
            "guest_token_ttl_s": c.limits.guest_token_ttl_s,
        },
    })
}

/// `GET /api/v2/admin/settings`
pub async fn settings(State(app): State<App>, _admin: AdminAuth) -> ApiResult<Json<SettingsView>> {
    Ok(Json(SettingsView {
        runtime: app.settings.read().clone(),
        config: config_view(&app),
    }))
}

/// `PUT /api/v2/admin/settings`
pub async fn set_settings(
    State(app): State<App>,
    admin: AdminAuth,
    JsonBody(mut req): JsonBody<RuntimeSettings>,
) -> ApiResult<Json<RuntimeSettings>> {
    req.instance_name = req.instance_name.trim().to_owned();
    // Store the web limit in the canonical order, without duplicates.
    req.web_caps = farwing_protocol::caps::WEB
        .iter()
        .filter(|c| req.web_caps.iter().any(|w| w == *c))
        .map(|c| (*c).to_owned())
        .chain(req.web_caps.iter().filter(|w| !farwing_protocol::caps::WEB.contains(&w.as_str())).cloned())
        .collect();
    req.validate()?;
    let pairs = req.pairs();
    app.db
        .call(move |c| {
            for (k, v) in &pairs {
                webdb::set_setting(c, k, v)?;
            }
            Ok(())
        })
        .await?;
    let before = std::mem::replace(&mut *app.settings.write(), req.clone());
    // The changed settings with their new values (`null` bitrate: no limit).
    let mut changes = Map::new();
    if before.instance_name != req.instance_name {
        changes.insert("instance_name".into(), req.instance_name.clone().into());
    }
    if before.guests != req.guests {
        changes.insert("guests".into(), req.guests.into());
    }
    if before.registration != req.registration {
        changes.insert("registration".into(), req.registration.into());
    }
    if before.bitrate_limit_mbps != req.bitrate_limit_mbps {
        changes.insert("bitrate_limit_mbps".into(), json!(req.bitrate_limit_mbps));
    }
    if before.web_caps != req.web_caps {
        changes.insert("web_caps".into(), json!(req.web_caps));
    }
    log(&app, &admin, "settings.save", "", changes_detail(changes)).await;
    Ok(Json(req))
}

fn last_admin() -> ApiError {
    ApiError::reason(
        ErrorCode::BadRequest,
        reason::LAST_ADMIN,
        "the last administrator must stay",
    )
}

/// An audit detail of changed fields; none: no detail.
fn changes_detail(changes: Map<String, Value>) -> Value {
    if changes.is_empty() { Value::Null } else { Value::Object(changes) }
}

// =================================================================== audit

#[derive(Deserialize)]
pub struct AuditQuery {
    pub before: Option<i64>,
    pub kind: Option<String>,
    pub actor: Option<String>,
    pub limit: Option<i64>,
}

/// `GET /api/v2/admin/audit`
pub async fn audit_log(
    State(app): State<App>,
    _admin: AdminAuth,
    Query(q): Query<AuditQuery>,
) -> ApiResult<Json<Vec<AuditEntry>>> {
    Ok(Json(
        app.db
            .call(move |c| {
                webdb::audit_entries(
                    c,
                    q.before,
                    q.kind.as_deref().filter(|s| !s.is_empty()),
                    q.actor.as_deref().filter(|s| !s.is_empty()),
                    q.limit.unwrap_or(100),
                )
            })
            .await?,
    ))
}
