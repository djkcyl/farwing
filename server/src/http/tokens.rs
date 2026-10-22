//! Administration API tokens (`docs/server-admin-api.md`): created and
//! revoked by a signed-in administrator in the web console, used with
//! `Authorization: Bearer fwa_…` from scripts and curl.

use super::auth::AdminAuth;
use super::{App, JsonBody};
use crate::error::{ApiError, ApiResult};
use crate::webdb::{self, AdminToken};
use axum::Json;
use axum::extract::{Path, State};
use farwing_protocol::ErrorCode;
use farwing_protocol::error::reason;
use ipnet::IpNet;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

pub const PREFIX: &str = "fwa_";
const NAME_MAX: usize = 64;
const DAYS_MAX: u32 = 3650;

fn login_only(admin: &AdminAuth) -> ApiResult<()> {
    match admin.token {
        Some(_) => Err(ApiError::reason(
            ErrorCode::Forbidden,
            reason::ADMIN_LOGIN_REQUIRED,
            "tokens are managed by a signed-in administrator",
        )),
        None => Ok(()),
    }
}

/// `GET /api/v2/admin/tokens`
pub async fn list(State(app): State<App>, _admin: AdminAuth) -> ApiResult<Json<Vec<AdminToken>>> {
    Ok(Json(app.db.call(|c| webdb::admin_tokens(c)).await?))
}

#[derive(Deserialize)]
pub struct NewToken {
    pub name: String,
    /// `read` or `admin`.
    pub scope: String,
    /// Days until it expires; none = never.
    #[serde(default)]
    pub expires_days: Option<u32>,
    /// Networks (CIDRs) it may be used from; empty = any.
    #[serde(default)]
    pub allow: Vec<String>,
}

#[derive(Serialize)]
pub struct Created {
    /// Shown once.
    pub token: String,
    #[serde(flatten)]
    pub info: AdminToken,
}

fn invalid(field: &str, message: &str) -> ApiError {
    super::settings_api::setting_error(crate::config::SettingError::new(
        field, "invalid", message,
    ))
}

/// Refuse an invalid token request.
pub fn check(req: &NewToken) -> ApiResult<()> {
    checked(req).map(|_| ())
}

/// A checked token request: its name and networks.
fn checked(req: &NewToken) -> ApiResult<(String, Vec<String>)> {
    let name = req.name.trim().to_owned();
    if name.is_empty() || name.chars().count() > NAME_MAX {
        return Err(invalid("name", "the name must be 1-64 characters"));
    }
    if req.scope != webdb::SCOPE_READ && req.scope != webdb::SCOPE_ADMIN {
        return Err(invalid("scope", "the scope must be read or admin"));
    }
    if req.expires_days.is_some_and(|d| d == 0 || d > DAYS_MAX) {
        return Err(invalid("expires_days", "the validity must be 1-3650 days"));
    }
    let allow: Vec<String> = req
        .allow
        .iter()
        .map(|a| a.trim())
        .filter(|a| !a.is_empty())
        .map(|a| {
            a.parse::<IpNet>()
                .or_else(|_| a.parse::<std::net::IpAddr>().map(IpNet::from))
                .map(|n| n.to_string())
                .map_err(|_| invalid("allow", "not a network address"))
        })
        .collect::<ApiResult<_>>()?;
    Ok((name, allow))
}

/// Store a new token of `user_id` for `req` (checked); the token, shown
/// once, and its row.
pub async fn issue(app: &App, user_id: &str, req: &NewToken) -> ApiResult<Created> {
    let (name, allow) = checked(req)?;
    let token = crate::crypto::new_token(PREFIX);
    let hash = crate::crypto::token_hash(&token);
    let id = crate::crypto::new_uuid();
    let expires_at = req
        .expires_days
        .map(|d| crate::db::now_s() + i64::from(d) * 86_400);
    let (tid, uid, scope) = (id.clone(), user_id.to_owned(), req.scope.clone());
    let info = app
        .db
        .call(move |c| {
            webdb::create_admin_token(c, &tid, &name, &hash, &scope, &allow, &uid, expires_at)?;
            webdb::admin_tokens(c)
        })
        .await?
        .into_iter()
        .find(|t| t.id == id)
        .ok_or_else(ApiError::internal)?;
    Ok(Created { token, info })
}

/// `POST /api/v2/admin/tokens`
pub async fn create(
    State(app): State<App>,
    admin: AdminAuth,
    JsonBody(req): JsonBody<NewToken>,
) -> ApiResult<Json<Created>> {
    login_only(&admin)?;
    let created = issue(&app, &admin.user().id, &req).await?;
    let name = created.info.name.clone();
    super::admin_api::log(
        &app,
        &admin,
        "token.create",
        &name,
        json!({ "scope": req.scope, "expires_days": req.expires_days }),
    )
    .await;
    Ok(Json(created))
}

/// `DELETE /api/v2/admin/tokens/{id}`
pub async fn revoke(
    State(app): State<App>,
    admin: AdminAuth,
    Path(id): Path<String>,
) -> ApiResult<Json<Value>> {
    login_only(&admin)?;
    let tid = id.clone();
    let n = app
        .db
        .call(move |c| webdb::revoke_admin_token(c, &tid))
        .await?;
    if n == 0 {
        return Err(ApiError::not_found());
    }
    super::admin_api::log(&app, &admin, "token.revoke", &id, Value::Null).await;
    Ok(Json(json!({})))
}
