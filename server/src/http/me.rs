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
use farwing_protocol::{ErrorCode, ids};
use serde::{Deserialize, Serialize};
use std::net::IpAddr;
use std::time::Duration;

/// Record an audit entry; failures are logged, never returned.
pub async fn audit(
    app: &App,
    actor: Option<(&str, &str)>,
    kind: &str,
    target: &str,
    detail: &str,
    ok: bool,
    ip: IpAddr,
) {
    let entry = AuditEntry {
        actor_user: actor.map(|(id, _)| id.to_owned()),
        actor_name: actor.map(|(_, name)| name.to_owned()).unwrap_or_default(),
        kind: kind.to_owned(),
        target: target.to_owned(),
        detail: detail.to_owned(),
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

/// The command line's username rule, with the web console's wording.
pub fn username_rule(name: &str) -> ApiResult<()> {
    crate::admin::validate_username(name)
        .map_err(|_| ApiError::bad_request("用户名须为 1–64 位字母、数字或 . _ - @"))
}

/// The command line's password rule, with the web console's wording.
pub fn password_rule(password: &str) -> ApiResult<()> {
    crate::admin::check_password(password).map_err(|_| {
        if password.chars().count() > 1024 {
            ApiError::bad_request("密码太长")
        } else {
            ApiError::bad_request(format!("密码至少 {} 位", crate::admin::MIN_PASSWORD_LEN))
        }
    })
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
        return Err(ApiError::bad_request("昵称不能包含控制字符"));
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
        audit(&app, actor, "password.change", &auth.user.username, "当前密码不正确", false, ip).await;
        return Err(ApiError::new(ErrorCode::Forbidden, "当前密码不正确"));
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
    audit(&app, actor, "password.change", &auth.user.username, "", true, ip).await;
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
        return Err(ApiError::new(ErrorCode::Forbidden, "密码不正确"));
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
        return Err(ApiError::new(ErrorCode::Conflict, "已经开启了二次验证"));
    }
    if !admin {
        return Err(ApiError::forbidden("二次验证只用于管理员账号"));
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
        return Err(ApiError::new(ErrorCode::Conflict, "请先开始设置二次验证"));
    };
    if !crate::totp::verify(&secret, &req.code, db::now_s()) {
        return Err(ApiError::new(ErrorCode::Forbidden, "验证码不正确"));
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
        "",
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
        return Err(ApiError::new(ErrorCode::Forbidden, "密码不正确"));
    }
    if !second_factor(&app, &auth.user.id, &req.code).await? {
        return Err(ApiError::new(ErrorCode::Forbidden, "验证码不正确"));
    }
    let uid = auth.user.id.clone();
    app.db.call(move |c| webdb::clear_totp(c, &uid)).await?;
    audit(
        &app,
        Some((&auth.user.id, &auth.user.username)),
        "totp.disable",
        &auth.user.username,
        "",
        true,
        ip,
    )
    .await;
    Ok(Json(Empty {}))
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
        audit(&app, actor, "login.totp", &auth.user.username, "验证码不正确", false, ip).await;
        return Err(ApiError::new(ErrorCode::Forbidden, "验证码不正确"));
    }
    app.limiter.clear(&key);
    let hash = auth.token_hash.clone();
    app.db.call(move |c| webdb::set_token_mfa(c, &hash)).await?;
    audit(&app, actor, "login.totp", &auth.user.username, "", true, ip).await;
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
        return Err(ApiError::forbidden("本服务器没有开放注册"));
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
                return Ok(Err("邀请码无效"));
            };
            if !invite.usable(db::now_s()) {
                return Ok(Err("邀请码已失效"));
            }
            if db::get_user_by_name(&tx, &name)?.is_some() {
                return Ok(Err("用户名已被使用"));
            }
            db::create_user(&tx, &uid, &name, &nickname, &hash)?;
            if !webdb::use_invite(&tx, &invite.id, &uid, &ip_text)? {
                return Ok(Err("邀请码已失效"));
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
        Err(message) => {
            audit(&app, None, "register", &username, message, false, ip).await;
            return Err(ApiError::bad_request(message));
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
        &format!("邀请码 {}", invite.note),
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
