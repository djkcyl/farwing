//! Account self-service for the web console and clients
//! (`docs/web-console.md` §4–§5): nickname, password, TOTP, the second
//! factor of a login, registration with an invitation, and public
//! information about this server.

use super::auth::{AccountAuth, DeviceAuth};
use super::{App, ClientIp, JsonBody};
use crate::crypto::{hash_password, new_token, new_uuid, sha256_hex, token_hash, verify_password};
use crate::db;
use crate::error::{ApiError, ApiResult};
use crate::webdb::{self, AuditEntry};
use axum::Json;
use axum::extract::State;
use farwing_protocol::api::{Empty, LoginResponse};
use farwing_protocol::error::reason;
use farwing_protocol::{ErrorCode, ids};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::net::IpAddr;
use std::time::Duration;

/// Record an audit entry; failures are logged, never returned.
///
/// `detail` is language-neutral: a JSON object with stable keys that the
/// web console shows in its language (`{"reason": "login_failed"}`,
/// `{"admin": true}`, `{"guests": false}`), or `Null` for none.
pub async fn audit(
    app: &App,
    actor: Option<(&str, &str)>,
    kind: &str,
    target: &str,
    detail: Value,
    ok: bool,
    ip: IpAddr,
) {
    let entry = AuditEntry {
        actor_user: actor.map(|(id, _)| id.to_owned()),
        actor_name: actor.map(|(_, name)| name.to_owned()).unwrap_or_default(),
        kind: kind.to_owned(),
        target: target.to_owned(),
        detail: if detail.is_null() { String::new() } else { detail.to_string() },
        ok,
        ip: ip.to_string(),
        ..Default::default()
    };
    if let Err(e) = app.db.call(move |c| webdb::audit(c, &entry)).await {
        tracing::warn!(error = %format!("{e:#}"), "audit entry not written");
    }
}

/// Verify a password on a blocking thread, bounded like logins.
pub async fn check_password(app: &App, password: String, hash: String) -> ApiResult<bool> {
    let Ok(_slot) = tokio::time::timeout(Duration::from_secs(10), app.password_slots.acquire()).await
    else {
        return Err(ApiError::rate_limited(1000));
    };
    Ok(tokio::task::spawn_blocking(move || verify_password(&password, &hash))
        .await
        .unwrap_or(false))
}

pub async fn new_hash(password: String) -> ApiResult<String> {
    tokio::task::spawn_blocking(move || hash_password(&password))
        .await
        .map_err(|_| ApiError::internal())?
        .map_err(|_| ApiError::internal())
}

/// The detail of a failed attempt in the audit log.
pub fn failure(reason: &'static str) -> Value {
    json!({ "reason": reason })
}

/// The command line's username rule.
pub fn username_rule(name: &str) -> ApiResult<()> {
    crate::admin::validate_username(name).map_err(|e| {
        ApiError::reason(ErrorCode::BadRequest, reason::USERNAME_INVALID, e.to_string())
            .with_detail("max", crate::admin::MAX_USERNAME_LEN)
    })
}

/// The command line's password rule.
pub fn password_rule(password: &str) -> ApiResult<()> {
    crate::admin::check_password(password).map_err(|e| {
        if password.chars().count() > crate::admin::MAX_PASSWORD_LEN {
            ApiError::reason(ErrorCode::BadRequest, reason::PASSWORD_TOO_LONG, e.to_string())
                .with_detail("max", crate::admin::MAX_PASSWORD_LEN)
        } else {
            ApiError::reason(ErrorCode::BadRequest, reason::PASSWORD_TOO_SHORT, e.to_string())
                .with_detail("min", crate::admin::MIN_PASSWORD_LEN)
        }
    })
}

/// The username belongs to another account.
pub fn username_taken() -> ApiError {
    ApiError::reason(ErrorCode::Conflict, reason::USERNAME_TAKEN, "username is already taken")
}

// ===================================================================== info

#[derive(Serialize)]
pub struct WebInfo {
    pub instance_name: String,
    pub version: String,
    pub public_url: String,
    pub tls_fingerprint: Option<String>,
    pub registration: bool,
    pub guests: bool,
    /// The newest stable client per platform.
    pub downloads: Vec<webdb::Announcement>,
}

/// `GET /api/v2/web/info` (no authentication): what the login and
/// "connect to this server" pages show.
pub async fn info(State(app): State<App>) -> ApiResult<Json<WebInfo>> {
    let all = app.db.call(|c| webdb::announcements(c)).await?;
    let mut downloads: Vec<webdb::Announcement> = Vec::new();
    for a in all.into_iter().filter(|a| a.channel == "stable" && !a.prerelease) {
        if !downloads.iter().any(|d| d.platform == a.platform) {
            downloads.push(a);
        }
    }
    let settings = app.settings.read().clone();
    Ok(Json(WebInfo {
        instance_name: settings.instance_name,
        version: env!("CARGO_PKG_VERSION").to_owned(),
        public_url: app.cfg.public_url.clone(),
        tls_fingerprint: app.tls.as_ref().map(|t| t.fingerprint()),
        registration: settings.registration,
        guests: settings.guests,
        downloads,
    }))
}

// ================================================================= profile

#[derive(Deserialize)]
pub struct ProfileRequest {
    pub nickname: String,
}

/// `PATCH /api/v2/me`
pub async fn set_profile(
    State(app): State<App>,
    auth: AccountAuth,
    JsonBody(req): JsonBody<ProfileRequest>,
) -> ApiResult<Json<Empty>> {
    let nickname = db::clip(req.nickname.trim(), 64);
    if nickname.chars().any(char::is_control) {
        return Err(ApiError::reason(
            ErrorCode::BadRequest,
            reason::NICKNAME_INVALID,
            "nickname contains control characters",
        ));
    }
    let uid = auth.user.id.clone();
    app.db
        .call(move |c| webdb::set_nickname(c, &uid, &nickname))
        .await?;
    Ok(Json(Empty {}))
}

#[derive(Serialize)]
pub struct Security {
    pub role: String,
    pub totp: bool,
    pub recovery_codes_left: usize,
    /// This login passed the second factor.
    pub verified: bool,
}

/// `GET /api/v2/me/security`
pub async fn security(State(app): State<App>, auth: AccountAuth) -> ApiResult<Json<Security>> {
    let (uid, hash) = (auth.user.id.clone(), auth.token_hash.clone());
    let (role, (_, enabled, recovery), mfa) = app
        .db
        .call(move |c| {
            Ok((
                webdb::role(c, &uid)?,
                webdb::totp(c, &uid)?,
                webdb::token_mfa(c, &hash)?,
            ))
        })
        .await?;
    Ok(Json(Security {
        role,
        totp: enabled.is_some(),
        recovery_codes_left: recovery.len(),
        verified: mfa.is_some(),
    }))
}

#[derive(Deserialize)]
pub struct PasswordRequest {
    pub old_password: String,
    pub new_password: String,
}

/// `POST /api/v2/me/password`: other logins of the account are signed out.
pub async fn set_password(
    State(app): State<App>,
    ClientIp(ip): ClientIp,
    auth: AccountAuth,
    JsonBody(req): JsonBody<PasswordRequest>,
) -> ApiResult<Json<Empty>> {
    password_rule(&req.new_password)?;
    let actor = Some((auth.user.id.as_str(), auth.user.username.as_str()));
    if !check_password(&app, req.old_password, auth.user.password_hash.clone()).await? {
        let why = reason::CURRENT_PASSWORD_INCORRECT;
        audit(&app, actor, "password.change", &auth.user.username, failure(why), false, ip).await;
        return Err(ApiError::reason(ErrorCode::Forbidden, why, "current password is incorrect"));
    }
    let hash = new_hash(req.new_password).await?;
    let (uid, keep) = (auth.user.id.clone(), auth.token_hash.clone());
    app.db
        .call(move |c| {
            db::set_user_password(c, &uid, &hash)?;
            c.execute(
                "UPDATE access_tokens SET revoked_at = ?3, revoke_reason = 'password_changed'
                 WHERE user_id = ?1 AND token_hash <> ?2 AND revoked_at IS NULL",
                rusqlite::params![uid, keep, db::now_s()],
            )
        })
        .await?;
    // Other connections of the account end; this device stays signed in.
    for device in app
        .db
        .call({
            let uid = auth.user.id.clone();
            move |c| db::user_device_ids(c, &uid)
        })
        .await?
    {
        if device != auth.device.id {
            app.hub.revoke_device(&device);
        }
    }
    audit(&app, actor, "password.change", &auth.user.username, Value::Null, true, ip).await;
    Ok(Json(Empty {}))
}

// ===================================================================== TOTP

#[derive(Deserialize)]
pub struct PasswordOnly {
    pub password: String,
}

#[derive(Serialize)]
pub struct TotpSetup {
    pub secret: String,
    pub uri: String,
}

/// `POST /api/v2/me/totp/setup`: a new secret to confirm with
/// `/me/totp/enable`.
pub async fn totp_setup(
    State(app): State<App>,
    auth: AccountAuth,
    JsonBody(req): JsonBody<PasswordOnly>,
) -> ApiResult<Json<TotpSetup>> {
    if !check_password(&app, req.password, auth.user.password_hash.clone()).await? {
        return Err(password_incorrect());
    }
    let secret = crate::totp::new_secret();
    let (uid, s2) = (auth.user.id.clone(), secret.clone());
    // Two-step verification guards the web console's administration; client
    // logins have no second step (`docs/web-console.md` §4.2).
    let (admin, enabled) = app
        .db
        .call(move |c| {
            let admin = webdb::role(c, &uid)? == webdb::ROLE_ADMIN;
            let enabled = webdb::totp(c, &uid)?.1.is_some();
            if admin && !enabled {
                webdb::set_totp_pending(c, &uid, &s2)?;
            }
            Ok((admin, enabled))
        })
        .await?;
    if enabled {
        return Err(ApiError::reason(
            ErrorCode::Conflict,
            reason::TOTP_ALREADY_ENABLED,
            "two-step verification is already enabled",
        ));
    }
    if !admin {
        return Err(ApiError::reason(
            ErrorCode::Forbidden,
            reason::TOTP_ADMIN_ONLY,
            "two-step verification is for administrators",
        ));
    }
    let issuer = format!("Farwing {}", app.settings.read().instance_name);
    Ok(Json(TotpSetup {
        uri: crate::totp::uri(&secret, &issuer, &auth.user.username),
        secret,
    }))
}

#[derive(Deserialize)]
pub struct CodeRequest {
    pub code: String,
}

#[derive(Serialize)]
pub struct RecoveryCodes {
    pub recovery_codes: Vec<String>,
}

/// `POST /api/v2/me/totp/enable`: confirm the secret with a code; returns
/// the one-time recovery codes. This login counts as verified.
pub async fn totp_enable(
    State(app): State<App>,
    ClientIp(ip): ClientIp,
    auth: AccountAuth,
    JsonBody(req): JsonBody<CodeRequest>,
) -> ApiResult<Json<RecoveryCodes>> {
    let uid = auth.user.id.clone();
    let (secret, enabled, _) = app.db.call(move |c| webdb::totp(c, &uid)).await?;
    let Some(secret) = secret.filter(|_| enabled.is_none()) else {
        return Err(ApiError::reason(
            ErrorCode::Conflict,
            reason::TOTP_SETUP_REQUIRED,
            "start the two-step verification setup first",
        ));
    };
    if !crate::totp::verify(&secret, &req.code, db::now_s()) {
        return Err(code_incorrect());
    }
    let codes = crate::totp::new_recovery_codes();
    let hashes: Vec<String> = codes.iter().map(|c| sha256_hex(c.as_bytes())).collect();
    let (uid, hash) = (auth.user.id.clone(), auth.token_hash.clone());
    app.db
        .call(move |c| {
            webdb::enable_totp(c, &uid, &hashes)?;
            webdb::set_token_mfa(c, &hash)
        })
        .await?;
    audit(
        &app,
        Some((&auth.user.id, &auth.user.username)),
        "totp.enable",
        &auth.user.username,
        Value::Null,
        true,
        ip,
    )
    .await;
    Ok(Json(RecoveryCodes {
        recovery_codes: codes,
    }))
}

#[derive(Deserialize)]
pub struct DisableRequest {
    pub password: String,
    pub code: String,
}

/// `POST /api/v2/me/totp/disable`
pub async fn totp_disable(
    State(app): State<App>,
    ClientIp(ip): ClientIp,
    auth: AccountAuth,
    JsonBody(req): JsonBody<DisableRequest>,
) -> ApiResult<Json<Empty>> {
    if !check_password(&app, req.password, auth.user.password_hash.clone()).await? {
        return Err(password_incorrect());
    }
    if !second_factor(&app, &auth.user.id, &req.code).await? {
        return Err(code_incorrect());
    }
    let uid = auth.user.id.clone();
    app.db.call(move |c| webdb::clear_totp(c, &uid)).await?;
    audit(
        &app,
        Some((&auth.user.id, &auth.user.username)),
        "totp.disable",
        &auth.user.username,
        Value::Null,
        true,
        ip,
    )
    .await;
    Ok(Json(Empty {}))
}

fn password_incorrect() -> ApiError {
    ApiError::reason(ErrorCode::Forbidden, reason::PASSWORD_INCORRECT, "password is incorrect")
}

fn code_incorrect() -> ApiError {
    ApiError::reason(
        ErrorCode::Forbidden,
        reason::TOTP_CODE_INCORRECT,
        "verification code is incorrect",
    )
}

/// Check a TOTP code or consume a recovery code.
async fn second_factor(app: &App, user_id: &str, code: &str) -> ApiResult<bool> {
    let uid = user_id.to_owned();
    let code = code.trim().to_ascii_lowercase();
    Ok(app
        .db
        .call(move |c| {
            let (secret, enabled, mut recovery) = webdb::totp(c, &uid)?;
            let (Some(secret), Some(_)) = (secret, enabled) else {
                return Ok(false);
            };
            if crate::totp::verify(&secret, &code, db::now_s()) {
                return Ok(true);
            }
            let hash = sha256_hex(code.as_bytes());
            if let Some(i) = recovery.iter().position(|h| *h == hash) {
                recovery.remove(i);
                webdb::set_totp_recovery(c, &uid, &recovery)?;
                return Ok(true);
            }
            Ok(false)
        })
        .await?)
}

/// `POST /api/v2/auth/totp`: the second factor of this login (TOTP or a
/// recovery code). Limited like password attempts.
pub async fn verify_login(
    State(app): State<App>,
    ClientIp(ip): ClientIp,
    auth: AccountAuth,
    JsonBody(req): JsonBody<CodeRequest>,
) -> ApiResult<Json<Empty>> {
    let key = format!("totp-user:{}", auth.user.id);
    app.limiter
        .take(&key, app.cfg.limits.login_failures_per_user, Duration::from_secs(15 * 60))
        .map_err(ApiError::rate_limited)?;
    let actor = Some((auth.user.id.as_str(), auth.user.username.as_str()));
    if !second_factor(&app, &auth.user.id, &req.code).await? {
        let failed = failure(reason::TOTP_CODE_INCORRECT);
        audit(&app, actor, "login.totp", &auth.user.username, failed, false, ip).await;
        return Err(code_incorrect());
    }
    app.limiter.clear(&key);
    let hash = auth.token_hash.clone();
    app.db.call(move |c| webdb::set_token_mfa(c, &hash)).await?;
    audit(&app, actor, "login.totp", &auth.user.username, Value::Null, true, ip).await;
    Ok(Json(Empty {}))
}

// ============================================================ registration

#[derive(Deserialize)]
pub struct RegisterRequest {
    pub invite: String,
    pub username: String,
    pub password: String,
    #[serde(default)]
    pub nickname: String,
}

/// `POST /api/v2/auth/register` (signed by the registering device): create
/// an account with an invitation and sign this device in.
pub async fn register(
    State(app): State<App>,
    ClientIp(ip): ClientIp,
    dev: DeviceAuth,
    JsonBody(req): JsonBody<RegisterRequest>,
) -> ApiResult<Json<LoginResponse>> {
    if !app.settings.read().registration {
        return Err(ApiError::new(
            ErrorCode::RegistrationDisabled,
            "registration is closed on this server",
        ));
    }
    app.limiter
        .take(&format!("register-ip:{ip}"), 10, Duration::from_secs(3600))
        .map_err(ApiError::rate_limited)?;
    let username = req.username.trim().to_owned();
    username_rule(&username)?;
    password_rule(&req.password)?;
    let nickname = db::clip(req.nickname.trim(), 64);
    let code_hash = sha256_hex(req.invite.trim().as_bytes());
    let hash = new_hash(req.password).await?;
    let token = new_token(ids::ACCESS_TOKEN_PREFIX);
    let tok_hash = token_hash(&token);
    let user_id = new_uuid();
    let device = dev.device;
    let (uid, did, name, ip_text) = (user_id.clone(), device.id.clone(), username.clone(), ip.to_string());
    let outcome = app
        .db
        .call(move |c| {
            let tx = c.transaction()?;
            let Some(invite) = webdb::invite_by_hash(&tx, &code_hash)? else {
                return Ok(Err(reason::INVITE_INVALID));
            };
            if !invite.usable(db::now_s()) {
                return Ok(Err(reason::INVITE_EXPIRED));
            }
            if db::get_user_by_name(&tx, &name)?.is_some() {
                return Ok(Err(reason::USERNAME_TAKEN));
            }
            db::create_user(&tx, &uid, &name, &nickname, &hash)?;
            if !webdb::use_invite(&tx, &invite.id, &uid, &ip_text)? {
                return Ok(Err(reason::INVITE_EXPIRED));
            }
            db::revoke_device_logins(&tx, &did, &uid, "replaced_by_login")?;
            db::insert_access_token(&tx, &tok_hash, &uid, &did)?;
            db::set_device_user(&tx, &did, Some(&uid))?;
            tx.commit()?;
            Ok(Ok(invite))
        })
        .await?;
    let invite = match outcome {
        Ok(invite) => invite,
        Err(why) => {
            audit(&app, None, "register", &username, failure(why), false, ip).await;
            return Err(match why {
                reason::USERNAME_TAKEN => username_taken(),
                reason::INVITE_INVALID => {
                    ApiError::reason(ErrorCode::BadRequest, why, "unknown invitation code")
                }
                _ => ApiError::reason(ErrorCode::BadRequest, why, "invitation code is no longer valid"),
            });
        }
    };
    if invite.auto_friend {
        let (a, b) = (invite.created_by.clone(), user_id.clone());
        if let Err(e) = app.db.call(move |c| db::make_friends(c, &a, &b).map(drop)).await {
            tracing::warn!(error = %format!("{e:#}"), "invited account not made a friend");
        }
    }
    app.hub.revoke_device_other_users(&device.id, &user_id);
    audit(
        &app,
        Some((&user_id, &username)),
        "register",
        &username,
        json!({ "invite": invite.note }),
        true,
        ip,
    )
    .await;
    tracing::info!(user_id = %user_id, "account registered with an invitation");
    Ok(Json(LoginResponse {
        access_token: token,
        user_id,
        username,
        nickname: String::new(),
    }))
}
