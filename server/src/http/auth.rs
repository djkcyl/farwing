//! Request signatures (device authentication), account and guest
//! authentication extractors.
//!
//! Every device-authenticated request carries `X-Farwing-Device-Id`,
//! `X-Farwing-Timestamp` and `X-Farwing-Signature`, an Ed25519 signature by
//! the device key over [`farwing_protocol::crypto::request_message`] (method,
//! path and query, timestamp and the SHA-256 of the body exactly as received).
//! Account requests add `Authorization: Bearer <access_token>`, which must be
//! bound to the signing device.

use super::App;
use crate::crypto::token_hash;
use crate::db::{self, DeviceRow, UserRow};
use crate::error::{ApiError, ApiResult};
use crate::hub::LinkAuth;
use axum::extract::FromRequestParts;
use axum::http::HeaderMap;
use axum::http::request::Parts;
use farwing_protocol::error::reason;
use farwing_protocol::{ErrorCode, crypto, headers, ids, limits};
use parking_lot::Mutex;
use std::collections::HashMap;
use std::time::{Duration, Instant};

/// The request body exactly as received, buffered for signed requests by the
/// common middleware before any extractor runs.
#[derive(Clone, Debug, Default)]
pub struct RawBody(pub bytes::Bytes);

/// A device whose request signature was verified (cached per request so a
/// signature is checked, and consumed, only once).
#[derive(Clone, Debug)]
struct VerifiedDevice(DeviceRow);

/// Signatures seen within [`limits::SIGNATURE_REPLAY_WINDOW_S`], keyed by a
/// 128-bit digest of the signature text.
#[derive(Default)]
pub struct ReplayCache {
    seen: Mutex<HashMap<[u8; 16], Instant>>,
}

impl ReplayCache {
    fn window() -> Duration {
        Duration::from_secs(limits::SIGNATURE_REPLAY_WINDOW_S)
    }

    fn key(signature: &str) -> [u8; 16] {
        use sha2::{Digest, Sha256};
        let digest = Sha256::digest(signature.as_bytes());
        let mut key = [0u8; 16];
        key.copy_from_slice(&digest[..16]);
        key
    }

    /// Record `signature`; `false` when it was already used within the window.
    pub fn insert(&self, signature: &str) -> bool {
        let now = Instant::now();
        let key = Self::key(signature);
        let mut seen = self.seen.lock();
        match seen.get(&key) {
            Some(t) if now.duration_since(*t) < Self::window() => false,
            _ => {
                seen.insert(key, now);
                true
            }
        }
    }

    pub fn prune(&self) {
        let now = Instant::now();
        self.seen
            .lock()
            .retain(|_, t| now.duration_since(*t) < Self::window());
    }
}

fn header<'a>(h: &'a HeaderMap, name: &str) -> Option<&'a str> {
    h.get(name)
        .and_then(|v| v.to_str().ok())
        .map(str::trim)
        .filter(|v| !v.is_empty())
}

pub fn bearer(h: &HeaderMap) -> Option<&str> {
    let v = header(h, "authorization")?;
    let (scheme, token) = v.split_once(' ')?;
    scheme
        .eq_ignore_ascii_case("bearer")
        .then(|| token.trim())
        .filter(|t| !t.is_empty())
}

/// Whether the request carries a signature (its body must be buffered).
pub fn is_signed(h: &HeaderMap) -> bool {
    h.contains_key(headers::SIGNATURE)
}

/// Whether `timestamp_ms` is within the accepted clock skew.
pub fn timestamp_fresh(timestamp_ms: i64) -> bool {
    (db::now_ms() - timestamp_ms).abs() <= limits::REQUEST_SKEW_MS
}

fn unauthorized(reason: &'static str) -> ApiError {
    tracing::debug!(reason, "device authentication failed");
    ApiError::device_unauthorized()
}

/// The request time is too far from the server's: the device's clock is off.
pub fn clock_skew() -> ApiError {
    ApiError::reason(
        ErrorCode::DeviceUnauthorized,
        reason::CLOCK_SKEW,
        "request time too far from the server's clock",
    )
}

/// Verify the request signature and return the signing device.
pub async fn device_from_parts(app: &App, parts: &mut Parts) -> ApiResult<DeviceRow> {
    if let Some(v) = parts.extensions.get::<VerifiedDevice>() {
        return Ok(v.0.clone());
    }
    let h = &parts.headers;
    let (Some(id), Some(ts), Some(signature)) = (
        header(h, headers::DEVICE_ID),
        header(h, headers::TIMESTAMP),
        header(h, headers::SIGNATURE),
    ) else {
        return Err(unauthorized("missing signature headers"));
    };
    if !ids::is_device_id(id) || signature.len() > 128 {
        return Err(unauthorized("malformed signature headers"));
    }
    let Ok(timestamp) = ts.parse::<i64>() else {
        return Err(unauthorized("malformed timestamp"));
    };
    if !timestamp_fresh(timestamp) {
        return Err(clock_skew());
    }
    let body = parts
        .extensions
        .get::<RawBody>()
        .map(|b| b.0.clone())
        .unwrap_or_default();
    let id = id.to_owned();
    let signature = signature.to_owned();
    let row = app
        .db
        .call(move |c| db::get_device(c, &id))
        .await?
        .ok_or_else(|| unauthorized("unknown device"))?;
    let method = parts.method.as_str();
    let path_and_query = parts
        .uri
        .path_and_query()
        .map(|p| p.as_str())
        .unwrap_or("/");
    // Behind a reverse proxy that strips the public URL's path prefix the
    // client signed the prefixed path.
    let path_prefix = app.path_prefix();
    let prefixed = (!path_prefix.is_empty()).then(|| format!("{path_prefix}{path_and_query}"));
    let valid = std::iter::once(path_and_query)
        .chain(prefixed.as_deref())
        .any(|path| {
            crypto::verify(
                &row.public_key,
                &crypto::request_message(method, path, timestamp, &body),
                &signature,
            )
            .is_ok()
        });
    if !valid {
        return Err(unauthorized("bad signature"));
    }
    if !app.replay.insert(&signature) {
        return Err(unauthorized("replayed signature"));
    }
    parts.extensions.insert(VerifiedDevice(row.clone()));
    Ok(row)
}

enum TokenCheck {
    Ok(UserRow),
    Unknown,
    Revoked,
    Mismatch,
}

/// Resolve an access token for `device`: `unauthorized`, `session_revoked`
/// (revoked, expired, account disabled or device unbound) or
/// `device_mismatch` (bound to another device) on failure. Slides the
/// token's expiry.
pub async fn account_for_device(
    app: &App,
    token: &str,
    device: &DeviceRow,
) -> ApiResult<(UserRow, String)> {
    if !ids::is_token(token, ids::ACCESS_TOKEN_PREFIX) {
        return Err(ApiError::unauthorized());
    }
    let hash = token_hash(token);
    let h2 = hash.clone();
    let device_id = device.id.clone();
    let device_user = device.user_id.clone();
    let browser = web_lifetime(app, device);
    let check = app
        .db
        .call(move |c| {
            let Some(tok) = db::get_access_token(c, &h2)? else {
                return Ok(TokenCheck::Unknown);
            };
            let now = db::now_s();
            if !tok.live(now) || browser.is_some_and(|(idle, max)| !web_live(&tok, now, idle, max)) {
                return Ok(TokenCheck::Revoked);
            }
            if tok.device_id != device_id {
                return Ok(TokenCheck::Mismatch);
            }
            let Some(user) = db::get_user(c, &tok.user_id)?.filter(|u| !u.disabled) else {
                return Ok(TokenCheck::Revoked);
            };
            if device_user.as_deref() != Some(user.id.as_str()) {
                return Ok(TokenCheck::Revoked);
            }
            if now - tok.last_used_at >= db::ACCESS_TOKEN_TOUCH_S {
                db::touch_access_token(c, &h2, now)?;
            }
            Ok(TokenCheck::Ok(user))
        })
        .await?;
    match check {
        TokenCheck::Ok(user) => Ok((user, hash)),
        TokenCheck::Unknown => Err(ApiError::unauthorized()),
        TokenCheck::Revoked => Err(revoked()),
        TokenCheck::Mismatch => Err(ApiError::new(
            ErrorCode::DeviceMismatch,
            "token belongs to another device",
        )),
    }
}

fn revoked() -> ApiError {
    ApiError::new(ErrorCode::SessionRevoked, "login revoked or expired")
}

/// A browser's login expires sooner: `(idle seconds, maximum seconds)`
/// (`[web]` in the configuration).
fn web_lifetime(app: &App, device: &DeviceRow) -> Option<(i64, i64)> {
    (device.platform == farwing_protocol::Platform::Web).then(|| {
        let cfg = app.cfg();
        (
            i64::from(cfg.web.session_idle_minutes.max(1)) * 60,
            i64::from(cfg.web.session_max_hours.max(1)) * 3600,
        )
    })
}

fn web_live(tok: &db::AccessTokenRow, now: i64, idle: i64, max: i64) -> bool {
    tok.last_used_at > now - idle && tok.created_at > now - max
}

/// An administrator (`docs/web-console.md` §4.2): an account with the
/// `admin` role, from an allowed network, whose login passed the second
/// factor when TOTP is enabled; or an administration API token
/// (`Authorization: Bearer fwa_…`, no signature) created by one.
pub struct AdminAuth {
    pub user: UserRow,
    pub ip: std::net::IpAddr,
    /// The API token used (ID, name), when not a signed-in login.
    pub token: Option<(String, String)>,
    /// A read-only API token.
    pub read_only: bool,
}

impl AdminAuth {
    pub fn user(&self) -> &UserRow {
        &self.user
    }
}

fn admin_required() -> ApiError {
    ApiError::reason(
        ErrorCode::Forbidden,
        reason::ADMIN_REQUIRED,
        "administrator role required",
    )
}

/// The administration API token in `Authorization`, checked.
async fn admin_token(
    app: &App,
    token: &str,
    method: &axum::http::Method,
    ip: std::net::IpAddr,
) -> ApiResult<AdminAuth> {
    if !ids::is_token(token, super::tokens::PREFIX) {
        return Err(ApiError::unauthorized());
    }
    let hash = token_hash(token);
    let row = app
        .db
        .call(move |c| crate::webdb::admin_token_by_hash(c, &hash))
        .await?
        .ok_or_else(ApiError::unauthorized)?;
    let now = db::now_s();
    if row.revoked_at.is_some() || row.expires_at.is_some_and(|e| e <= now) {
        return Err(revoked());
    }
    let networks: Vec<ipnet::IpNet> = row.allow.iter().filter_map(|a| a.parse().ok()).collect();
    if !networks.is_empty() && !networks.iter().any(|n| n.contains(&ip)) {
        return Err(ApiError::reason(
            ErrorCode::Forbidden,
            reason::ADMIN_NETWORK_DENIED,
            "this token is not allowed from this address",
        ));
    }
    let row_scope_read_only = row.scope != crate::webdb::SCOPE_ADMIN;
    if row_scope_read_only
        && !matches!(*method, axum::http::Method::GET | axum::http::Method::HEAD)
    {
        return Err(ApiError::forbidden("this token can only read"));
    }
    let (creator, id) = (row.created_by.clone(), row.id.clone());
    let ip_text = ip.to_string();
    let user = app
        .db
        .call(move |c| {
            crate::webdb::touch_admin_token(c, &id, &ip_text)?;
            let user = db::get_user(c, &creator)?;
            let role = crate::webdb::role(c, &creator)?;
            Ok(user.filter(|u| !u.disabled && role == crate::webdb::ROLE_ADMIN))
        })
        .await?
        .ok_or_else(revoked)?;
    Ok(AdminAuth {
        user,
        ip,
        read_only: row_scope_read_only,
        token: Some((row.id, row.name)),
    })
}

impl FromRequestParts<App> for AdminAuth {
    type Rejection = ApiError;
    async fn from_request_parts(parts: &mut Parts, app: &App) -> Result<Self, Self::Rejection> {
        let ip = parts
            .extensions
            .get::<super::ClientIp>()
            .map(|c| c.0)
            .ok_or_else(ApiError::internal)?;
        let allow = app.cfg().web.admin_allow.clone();
        if !allow.is_empty() && !allow.iter().any(|n| n.contains(&ip)) {
            return Err(ApiError::reason(
                ErrorCode::Forbidden,
                reason::ADMIN_NETWORK_DENIED,
                "admin console not allowed from this address",
            ));
        }
        if !is_signed(&parts.headers)
            && let Some(token) = bearer(&parts.headers)
            && token.starts_with(super::tokens::PREFIX)
        {
            return admin_token(app, &token.to_owned(), &parts.method, ip).await;
        }
        let account = AccountAuth::from_request_parts(parts, app).await?;
        let (uid, hash) = (account.user.id.clone(), account.token_hash.clone());
        let (role, totp, mfa) = app
            .db
            .call(move |c| {
                Ok((
                    crate::webdb::role(c, &uid)?,
                    crate::webdb::totp(c, &uid)?.1,
                    crate::webdb::token_mfa(c, &hash)?,
                ))
            })
            .await?;
        if role != crate::webdb::ROLE_ADMIN {
            return Err(admin_required());
        }
        if totp.is_some() && mfa.is_none() {
            return Err(ApiError::reason(
                ErrorCode::Forbidden,
                reason::TOTP_REQUIRED,
                "two-step verification required",
            ));
        }
        Ok(AdminAuth {
            user: account.user,
            ip,
            token: None,
            read_only: false,
        })
    }
}

/// Device signature only.
pub struct DeviceAuth {
    pub device: DeviceRow,
}

impl FromRequestParts<App> for DeviceAuth {
    type Rejection = ApiError;
    async fn from_request_parts(parts: &mut Parts, app: &App) -> Result<Self, Self::Rejection> {
        Ok(DeviceAuth {
            device: device_from_parts(app, parts).await?,
        })
    }
}

/// Device signature + an access token bound to that device.
pub struct AccountAuth {
    pub device: DeviceRow,
    pub user: UserRow,
    pub token_hash: String,
}

impl AccountAuth {
    pub fn link_auth(&self) -> LinkAuth {
        LinkAuth::Account {
            user_id: self.user.id.clone(),
            token_hash: self.token_hash.clone(),
        }
    }
}

impl FromRequestParts<App> for AccountAuth {
    type Rejection = ApiError;
    async fn from_request_parts(parts: &mut Parts, app: &App) -> Result<Self, Self::Rejection> {
        let device = device_from_parts(app, parts).await?;
        let token = bearer(&parts.headers).ok_or_else(ApiError::unauthorized)?;
        let (user, token_hash) = account_for_device(app, token, &device).await?;
        Ok(AccountAuth {
            device,
            user,
            token_hash,
        })
    }
}

/// Device signature + an access token or a guest token.
pub struct AnyAuth {
    pub device: DeviceRow,
    pub user: Option<UserRow>,
    pub auth: LinkAuth,
}

impl FromRequestParts<App> for AnyAuth {
    type Rejection = ApiError;
    async fn from_request_parts(parts: &mut Parts, app: &App) -> Result<Self, Self::Rejection> {
        let device = device_from_parts(app, parts).await?;
        let token = bearer(&parts.headers).ok_or_else(ApiError::unauthorized)?;
        if token.starts_with(ids::GUEST_TOKEN_PREFIX) {
            let token_hash = guest_for_device(app, token, &device).await?;
            return Ok(AnyAuth {
                device,
                user: None,
                auth: LinkAuth::Guest { token_hash },
            });
        }
        let (user, token_hash) = account_for_device(app, token, &device).await?;
        let auth = LinkAuth::Account {
            user_id: user.id.clone(),
            token_hash,
        };
        Ok(AnyAuth {
            device,
            user: Some(user),
            auth,
        })
    }
}

pub async fn guest_for_device(app: &App, token: &str, device: &DeviceRow) -> ApiResult<String> {
    if !app.cfg().guest.enabled || !ids::is_token(token, ids::GUEST_TOKEN_PREFIX) {
        return Err(ApiError::unauthorized());
    }
    let hash = token_hash(token);
    let h2 = hash.clone();
    let (dev, expires) = app
        .db
        .call(move |c| db::get_guest_token(c, &h2))
        .await?
        .ok_or_else(ApiError::unauthorized)?;
    if expires <= db::now_s() {
        return Err(ApiError::unauthorized());
    }
    if dev != device.id {
        return Err(ApiError::new(
            ErrorCode::DeviceMismatch,
            "guest token belongs to another device",
        ));
    }
    Ok(hash)
}

/// Re-check a realtime admission credential (used when a ticket is redeemed).
pub async fn link_auth_valid(app: &App, device_id: &str, auth: &LinkAuth) -> bool {
    let device_id = device_id.to_owned();
    let auth = auth.clone();
    let cfg = app.cfg();
    let guest_enabled = cfg.guest.enabled;
    let web_idle = i64::from(cfg.web.session_idle_minutes.max(1)) * 60;
    let web_max = i64::from(cfg.web.session_max_hours.max(1)) * 3600;
    app.db
        .call(move |c| {
            Ok(match &auth {
                LinkAuth::Account {
                    user_id,
                    token_hash,
                } => {
                    let tok = db::get_access_token(c, token_hash)?;
                    let user = db::get_user(c, user_id)?;
                    let dev = db::get_device(c, &device_id)?;
                    let now = db::now_s();
                    let browser = dev
                        .as_ref()
                        .is_some_and(|d| d.platform == farwing_protocol::Platform::Web);
                    tok.is_some_and(|t| {
                        t.live(now)
                            && t.device_id == device_id
                            && (!browser || web_live(&t, now, web_idle, web_max))
                    }) && user.is_some_and(|u| !u.disabled)
                        && dev.is_some_and(|d| d.user_id.as_deref() == Some(user_id.as_str()))
                }
                LinkAuth::Guest { token_hash } => {
                    guest_enabled
                        && db::get_guest_token(c, token_hash)?
                            .is_some_and(|(d, exp)| d == device_id && exp > db::now_s())
                }
            })
        })
        .await
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn replay_cache_rejects_reuse() {
        let cache = ReplayCache::default();
        assert!(cache.insert("sig-a"));
        assert!(!cache.insert("sig-a"));
        assert!(cache.insert("sig-b"));
        cache.prune();
        assert!(!cache.insert("sig-b"));
    }
}
