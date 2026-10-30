//! First start and recovery (`docs/protocol/deployment-design.md` §2.2):
//! unauthenticated, but every change needs the one-time setup code from the
//! server's console.

use super::settings_api::{self, Certificate, SettingsView};
use super::tokens::NewToken;
use super::{App, ClientIp, JsonBody};
use crate::error::{ApiError, ApiResult};
use crate::webdb;
use axum::Json;
use axum::extract::State;
use farwing_protocol::ErrorCode;
use farwing_protocol::error::reason;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::time::Duration;

/// Routes under this prefix answer before the setup.
pub const PATH: &str = "/api/v2/setup";

#[derive(Serialize)]
pub struct SetupInfo {
    /// `setup` (no administrator yet), `recovery` (a recovery code was
    /// issued) or `done`.
    pub state: &'static str,
    /// The settings to start from (only while not set up).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub current: Option<SettingsView>,
}

/// `GET /api/v2/setup`
pub async fn info(State(app): State<App>) -> ApiResult<Json<SetupInfo>> {
    if app.setup_pending() {
        return Ok(Json(SetupInfo {
            state: "setup",
            current: Some(settings_api::view(&app)),
        }));
    }
    let pending = app.db.call(|c| crate::setup::code_pending(c)).await?;
    Ok(Json(SetupInfo {
        state: if pending { "recovery" } else { "done" },
        current: None,
    }))
}

/// At most this many attempts with a wrong code per address and 15 minutes.
const ATTEMPTS: u32 = 10;

async fn check_code(app: &App, ip: ClientIp, code: &str) -> ApiResult<()> {
    let key = format!("setup:{}", ip.0);
    if app
        .limiter
        .check(&key, ATTEMPTS, Duration::from_secs(15 * 60))
        .is_err()
    {
        return Err(ApiError::new(ErrorCode::RateLimited, "too many attempts"));
    }
    let code = code.to_owned();
    if app
        .db
        .call(move |c| crate::setup::code_valid(c, &code))
        .await?
    {
        return Ok(());
    }
    let _ = app
        .limiter
        .take(&key, ATTEMPTS, Duration::from_secs(15 * 60));
    Err(ApiError::reason(
        ErrorCode::Forbidden,
        reason::SETUP_CODE_INVALID,
        "the setup code is wrong or has expired",
    ))
}

#[derive(Deserialize)]
pub struct Admin {
    pub username: String,
    pub password: String,
    #[serde(default)]
    pub nickname: Option<String>,
}

#[derive(Deserialize)]
pub struct SetupRequest {
    pub code: String,
    /// Settings over the current ones (as in `PUT /admin/settings`).
    #[serde(default)]
    pub settings: Value,
    /// An uploaded certificate (with `tls.mode = "provided"`).
    #[serde(default)]
    pub certificate: Option<Certificate>,
    pub admin: Admin,
    /// Also issue an administration API token of the new administrator
    /// (for setting up without a browser).
    #[serde(default)]
    pub token: Option<NewToken>,
}

#[derive(Serialize)]
pub struct SetupDone {
    pub outcome: crate::runtime::Outcome,
    #[serde(flatten)]
    pub view: SettingsView,
    /// The requested API token, shown once.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub token: Option<String>,
}

/// `POST /api/v2/setup`: settings and the first administrator.
pub async fn complete(
    State(app): State<App>,
    ip: ClientIp,
    JsonBody(req): JsonBody<SetupRequest>,
) -> ApiResult<Json<SetupDone>> {
    if !app.setup_pending() {
        return Err(ApiError::reason(
            ErrorCode::Conflict,
            reason::SETUP_DONE,
            "this server is already set up",
        ));
    }
    check_code(&app, ip, &req.code).await?;
    super::me::username_rule(&req.admin.username)?;
    super::me::password_rule(&req.admin.password)?;
    if let Some(token) = &req.token {
        super::tokens::check(token)?;
    }
    if let Some(c) = &req.certificate {
        settings_api::store_certificate(&app, c)?;
    }
    let mut cfg = settings_api::patched(&app.cfg(), req.settings)?;
    // Behind a reverse proxy every request comes from the proxy: trust the
    // usual one on this machine to name the client, unless set otherwise.
    if cfg.tls.mode == crate::config::TlsMode::Proxy && cfg.trusted_proxies.is_empty() {
        cfg.trusted_proxies = ["127.0.0.1/32", "::1/128"]
            .iter()
            .filter_map(|n| n.parse().ok())
            .collect();
    }
    let outcome = app
        .control()
        .apply(cfg)
        .await
        .map_err(settings_api::apply_error)?;
    // Nobody to lock out yet (the setup code can be issued again): a moved
    // listener or the reverse proxy mode is kept without confirmation.
    if outcome.listen_url.is_some() {
        app.control().confirm_listen().await;
    }
    let Admin {
        username,
        password,
        nickname,
    } = req.admin;
    let name = username.clone();
    let _permit = app.password_slots.acquire().await;
    let user_id = app
        .db
        .call(move |c| {
            let id = crate::accounts::create_user(c, &username, nickname.as_deref(), &password)
                .map_err(|e| rusqlite::Error::ToSqlConversionFailure(e.into()))?;
            webdb::set_role(c, &id, webdb::ROLE_ADMIN)?;
            crate::setup::clear_code(c)?;
            Ok(id)
        })
        .await?;
    app.setup_pending
        .store(false, std::sync::atomic::Ordering::Release);
    app.egress.reload_user(&app.db, &user_id).await;
    super::me::audit(
        &app,
        Some((&user_id, &name)),
        "server.setup",
        &name,
        Value::Null,
        true,
        ip.0,
    )
    .await;
    tracing::info!(admin = %name, "server set up");
    let token = match &req.token {
        Some(token) => Some(super::tokens::issue(&app, &user_id, token).await?.token),
        None => None,
    };
    Ok(Json(SetupDone {
        outcome,
        view: settings_api::view(&app),
        token,
    }))
}

#[derive(Deserialize)]
pub struct RecoverRequest {
    pub code: String,
    pub username: String,
    pub password: String,
    /// Also issue an administration API token of this administrator.
    #[serde(default)]
    pub token: Option<NewToken>,
}

/// `POST /api/v2/setup/recover`: make `username` an administrator with
/// this password (created if missing; two-step verification turned off,
/// logins revoked).
pub async fn recover(
    State(app): State<App>,
    ip: ClientIp,
    JsonBody(req): JsonBody<RecoverRequest>,
) -> ApiResult<Json<Value>> {
    if app.setup_pending() {
        return Err(ApiError::new(
            ErrorCode::SetupRequired,
            "this server has not been set up",
        ));
    }
    check_code(&app, ip, &req.code).await?;
    super::me::username_rule(&req.username)?;
    super::me::password_rule(&req.password)?;
    if let Some(token) = &req.token {
        super::tokens::check(token)?;
    }
    let RecoverRequest {
        username,
        password,
        token,
        ..
    } = req;
    let name = username.clone();
    let _permit = app.password_slots.acquire().await;
    let user_id = app
        .db
        .call(move |c| {
            let fail = |e: anyhow::Error| rusqlite::Error::ToSqlConversionFailure(e.into());
            let id = match crate::db::get_user_by_name(c, &username)? {
                Some(user) => {
                    let hash = crate::crypto::hash_password(&password).map_err(fail)?;
                    crate::db::set_user_password(c, &user.id, &hash)?;
                    crate::db::set_user_disabled(c, &user.id, false)?;
                    webdb::clear_totp(c, &user.id)?;
                    crate::db::revoke_tokens_for_user(c, &user.id, "admin_recovered")?;
                    user.id
                }
                None => crate::accounts::create_user(c, &username, None, &password)
                    .map_err(fail)?,
            };
            webdb::set_role(c, &id, webdb::ROLE_ADMIN)?;
            crate::setup::clear_code(c)?;
            Ok(id)
        })
        .await?;
    app.hub.revoke_user(&user_id);
    app.egress.reload_user(&app.db, &user_id).await;
    super::me::audit(
        &app,
        Some((&user_id, &name)),
        "server.recover_admin",
        &name,
        Value::Null,
        true,
        ip.0,
    )
    .await;
    tracing::warn!(admin = %name, "administrator recovered with a setup code");
    Ok(Json(match token {
        Some(token) => json!({ "token": super::tokens::issue(&app, &user_id, &token).await?.token }),
        None => json!({}),
    }))
}
