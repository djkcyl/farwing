//! Login, logout, login sessions, `/me` and guest sessions.

use super::auth::{AccountAuth, DeviceAuth};
use super::devices::{load_audience, notify_changed, push_changed, unbind_device};
use super::{App, ClientIp, JsonBody};
use crate::crypto::{dummy_password_hash, new_token, token_hash, verify_password};
use crate::db;
use crate::error::{ApiError, ApiResult};
use axum::Json;
use axum::extract::{Path, State};
use farwing_protocol::api::{
    AuthSession, AuthSessions, Empty, GuestSession, LoginRequest, LoginResponse, Me,
};
use farwing_protocol::ids;
use farwing_protocol::realtime::DeviceChangeKind;
use std::time::Duration;

const LOGIN_WINDOW: Duration = Duration::from_secs(15 * 60);

/// `POST /api/v2/auth/login`: the access token is bound to the signing
/// device. A login replaces every earlier login of the device.
pub async fn login(
    State(app): State<App>,
    ClientIp(ip): ClientIp,
    dev: DeviceAuth,
    JsonBody(req): JsonBody<LoginRequest>,
) -> ApiResult<Json<LoginResponse>> {
    let username = req.username.trim().to_owned();
    let ip_key = format!("login-ip:{ip}");
    let user_key = format!("login-user:{}", username.to_lowercase());
    let limits = &app.cfg.limits;
    // Count the attempt before verifying, so parallel guesses cannot all
    // pass the limit; a successful login returns the IP slot.
    app.limiter
        .take(&ip_key, limits.login_failures_per_ip, LOGIN_WINDOW)
        .map_err(ApiError::rate_limited)?;
    if let Err(wait) = app
        .limiter
        .take(&user_key, limits.login_failures_per_user, LOGIN_WINDOW)
    {
        app.limiter.refund(&ip_key);
        return Err(ApiError::rate_limited(wait));
    }
    let Ok(_slot) = tokio::time::timeout(Duration::from_secs(10), app.password_slots.acquire()).await
    else {
        app.limiter.refund(&ip_key);
        app.limiter.refund(&user_key);
        return Err(ApiError::rate_limited(1000));
    };

    let lookup = username.clone();
    let user = if username.is_empty() || username.len() > 128 {
        None
    } else {
        app.db
            .call(move |c| db::get_user_by_name(c, &lookup))
            .await?
    };
    let password = req.password;
    let candidate = user.clone();
    let ok = tokio::task::spawn_blocking(move || match candidate {
        Some(u) if !u.disabled => verify_password(&password, &u.password_hash),
        _ => {
            let _ = verify_password(&password, dummy_password_hash());
            false
        }
    })
    .await
    .unwrap_or(false);
    let Some(user) = user.filter(|_| ok) else {
        tracing::info!(%ip, "login failed");
        super::me::audit(&app, None, "login", &username, "用户名或密码不正确", false, ip).await;
        return Err(ApiError::unauthorized());
    };
    app.limiter.refund(&ip_key);
    app.limiter.clear(&user_key);

    let token = new_token(ids::ACCESS_TOKEN_PREFIX);
    let hash = token_hash(&token);
    let device = dev.device;
    let previous_owner = device.user_id.clone();
    let rebind = previous_owner.as_deref() != Some(user.id.as_str());
    // Everyone who saw the device under its previous owner.
    let old_audience = if rebind && previous_owner.is_some() {
        load_audience(&app, &device.id).await
    } else {
        None
    };
    let (did, uid) = (device.id.clone(), user.id.clone());
    app.db
        .call(move |c| {
            let tx = c.transaction()?;
            db::revoke_device_logins(&tx, &did, &uid, "replaced_by_login")?;
            db::insert_access_token(&tx, &hash, &uid, &did)?;
            db::set_device_user(&tx, &did, Some(&uid))?;
            tx.commit()
        })
        .await?;
    app.hub.revoke_device_other_users(&device.id, &user.id);
    if rebind {
        if let Some(a) = old_audience {
            push_changed(&app, &a, DeviceChangeKind::Removed, false).await;
        }
        notify_changed(&app, &device.id, DeviceChangeKind::Added, false).await;
    }
    tracing::info!(user_id = %user.id, device_id = %device.id, "login");
    super::me::audit(
        &app,
        Some((&user.id, &user.username)),
        "login",
        &user.username,
        &format!("{} [{}] {}", device.alias, device.id, device.platform.as_str()),
        true,
        ip,
    )
    .await;
    Ok(Json(LoginResponse {
        access_token: token,
        user_id: user.id,
        username: user.username,
        nickname: user.nickname,
    }))
}

/// `POST /api/v2/auth/logout`: revoke the token and unbind the device.
pub async fn logout(State(app): State<App>, auth: AccountAuth) -> ApiResult<Json<Empty>> {
    let hash = auth.token_hash.clone();
    app.db
        .call(move |c| db::revoke_token(c, &hash, "logout"))
        .await?;
    unbind_device(&app, &auth.device, "logout").await?;
    tracing::info!(user_id = %auth.user.id, device_id = %auth.device.id, "logout");
    Ok(Json(Empty {}))
}

/// `GET /api/v2/auth/sessions`: live logins of this account, one per device.
pub async fn auth_sessions(
    State(app): State<App>,
    auth: AccountAuth,
) -> ApiResult<Json<AuthSessions>> {
    let uid = auth.user.id.clone();
    let rows = app
        .db
        .call(move |c| {
            let mut out = Vec::new();
            for login in db::list_logins(c, &uid)? {
                let device = db::get_device(c, &login.device_id)?;
                out.push((login, device));
            }
            Ok(out)
        })
        .await?;
    Ok(Json(AuthSessions {
        sessions: rows
            .into_iter()
            .map(|(login, device)| AuthSession {
                current: login.device_id == auth.device.id,
                device_alias: device.as_ref().map(|d| d.alias.clone()).unwrap_or_default(),
                platform: device.map(|d| d.platform).unwrap_or_default(),
                device_id: login.device_id,
                created_at: login.created_at,
                last_used_at: login.last_used_at,
            })
            .collect(),
    }))
}

/// `DELETE /api/v2/auth/sessions/{device_id}`: sign that device out (like its
/// own logout: tokens revoked, device unbound, realtime connections closed).
pub async fn revoke_auth_session(
    State(app): State<App>,
    auth: AccountAuth,
    Path(device_id): Path<String>,
) -> ApiResult<Json<Empty>> {
    if !ids::is_device_id(&device_id) {
        return Err(ApiError::not_found());
    }
    let (uid, did) = (auth.user.id.clone(), device_id.clone());
    let (revoked, device) = app
        .db
        .call(move |c| {
            let n = db::revoke_user_device_tokens(c, &uid, &did, "revoked_by_user")?;
            Ok((n, db::get_device(c, &did)?))
        })
        .await?;
    let bound = device
        .as_ref()
        .is_some_and(|d| d.user_id.as_deref() == Some(auth.user.id.as_str()));
    if revoked == 0 && !bound {
        return Err(ApiError::not_found());
    }
    match device.filter(|_| bound) {
        Some(row) => unbind_device(&app, &row, "revoked_by_user").await?,
        None => app.hub.revoke_device(&device_id),
    }
    tracing::info!(user_id = %auth.user.id, device_id = %device_id, "login revoked");
    Ok(Json(Empty {}))
}

/// `GET /api/v2/me`
pub async fn me(auth: AccountAuth) -> Json<Me> {
    Json(Me {
        user_id: auth.user.id,
        username: auth.user.username,
        nickname: auth.user.nickname,
    })
}

/// `POST /api/v2/guest/session`
pub async fn guest_session(
    State(app): State<App>,
    dev: DeviceAuth,
) -> ApiResult<Json<GuestSession>> {
    if !app.settings.read().guests {
        return Err(ApiError::forbidden("服务器未开启游客协助"));
    }
    let token = new_token(ids::GUEST_TOKEN_PREFIX);
    let hash = token_hash(&token);
    let expires_at = db::now_s() + app.cfg.limits.guest_token_ttl_s as i64;
    let id = dev.device.id.clone();
    app.db
        .call(move |c| db::insert_guest_token(c, &hash, &id, expires_at))
        .await?;
    Ok(Json(GuestSession {
        guest_token: token,
        expires_at,
    }))
}
