//! Device registration, device lists (own and shared) and device pushes.

use super::auth::{AccountAuth, AnyAuth, clock_skew, timestamp_fresh};
use super::{App, ClientIp, JsonBody};
use crate::crypto::sha256_hex;
use crate::db::{self, DeviceRow, UserRow};
use crate::error::{ApiError, ApiResult};
use axum::Json;
use axum::body::Body;
use axum::extract::{Path, Request, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use farwing_protocol::api::{
    Device, DeviceDetail, DeviceInitRequest, DeviceInitResponse, DeviceList, DevicePolicy, Empty,
    Relation, RenameRequest, RenameResponse, STATUS_OFFLINE, STATUS_ONLINE, UserSummary,
    WallpaperResponse,
};
use farwing_protocol::realtime::{DeviceChangeKind, DeviceChanged, Envelope, types};
use farwing_protocol::error::reason;
use farwing_protocol::{ErrorCode, caps, crypto, ids, limits};
use std::time::Duration;

/// Server-relative wallpaper URL with a cache-busting `v`.
pub fn wallpaper_url(row: &DeviceRow) -> String {
    match &row.wallpaper_etag {
        Some(etag) => format!(
            "{}/{}/wallpaper?v={}",
            farwing_protocol::api::path::DEVICES,
            row.id,
            &etag[..etag.len().min(16)]
        ),
        None => String::new(),
    }
}

pub fn user_summary(u: &UserRow) -> UserSummary {
    UserSummary {
        user_id: u.id.clone(),
        username: u.username.clone(),
        nickname: u.nickname.clone(),
    }
}

/// Whose view of a device is being built.
#[derive(Clone, Debug)]
pub enum Viewer {
    /// The owning account.
    Owner,
    /// A friend of the owner holding `granted` on the device.
    Friend {
        user_id: String,
        owner: UserSummary,
        granted: Vec<String>,
    },
}

pub fn build_device(app: &App, row: &DeviceRow, viewer: &Viewer) -> Device {
    let mut participants = app.hub.participants(&row.id);
    let (owner, granted) = match viewer {
        Viewer::Owner => (None, caps::all()),
        Viewer::Friend {
            user_id,
            owner,
            granted,
        } => {
            // Friends only see their own participation.
            participants.retain(|p| &p.user_id == user_id);
            (Some(owner.clone()), granted.clone())
        }
    };
    Device {
        device_id: row.id.clone(),
        alias: row.alias.clone(),
        status: if app.hub.is_online(&row.id) {
            STATUS_ONLINE
        } else {
            STATUS_OFFLINE
        }
        .to_owned(),
        platform: row.platform,
        version: row.version.clone(),
        capabilities: row.capabilities.clone(),
        policy: row.policy,
        wol_supported: row.wol_supported(),
        wallpaper_url: wallpaper_url(row),
        participants,
        last_seen_at: row.last_seen_at,
        owner,
        granted,
        public_key: row.public_key.clone(),
    }
}

/// Send `env` to the online devices of `user_id`, except `except`.
pub async fn send_to_user(app: &App, user_id: &str, except: Option<&str>, env: &Envelope) {
    let user = user_id.to_owned();
    let ids = match app.db.call(move |c| db::user_device_ids(c, &user)).await {
        Ok(ids) => ids,
        Err(e) => {
            tracing::warn!(error = %e, "listing account devices failed");
            return;
        }
    };
    for id in ids {
        if Some(id.as_str()) != except {
            app.hub.push(&id, env);
        }
    }
}

/// A bound device with everyone who sees it: the owner and the owner's
/// friends holding at least one capability on it.
pub struct Audience {
    pub row: DeviceRow,
    pub owner: UserRow,
    pub friends: Vec<(String, Vec<String>)>,
}

/// Load the audience of `device_id` (`None` when unknown or unbound).
pub async fn load_audience(app: &App, device_id: &str) -> Option<Audience> {
    let id = device_id.to_owned();
    let loaded = app
        .db
        .call(move |c| {
            let Some(row) = db::get_device(c, &id)? else {
                return Ok(None);
            };
            let Some(owner) = row.user_id.as_deref() else {
                return Ok(None);
            };
            let Some(owner) = db::get_user(c, owner)? else {
                return Ok(None);
            };
            let friends = db::device_audience(c, &row)?;
            Ok(Some(Audience {
                row,
                owner,
                friends,
            }))
        })
        .await;
    match loaded {
        Ok(a) => a,
        Err(e) => {
            tracing::warn!(error = %format!("{e:#}"), "loading device audience failed");
            None
        }
    }
}

fn changed_env(kind: DeviceChangeKind, device: Device, shared: bool) -> Envelope {
    Envelope::new(
        types::DEVICE_CHANGED,
        DeviceChanged {
            kind,
            device,
            shared,
        },
    )
}

/// Push `device.changed{kind}` for one friend's view of a device.
pub async fn push_shared(
    app: &App,
    friend: &str,
    row: &DeviceRow,
    owner: &UserSummary,
    granted: Vec<String>,
    kind: DeviceChangeKind,
) {
    let mut device = build_device(
        app,
        row,
        &Viewer::Friend {
            user_id: friend.to_owned(),
            owner: owner.clone(),
            granted,
        },
    );
    if kind == DeviceChangeKind::Removed {
        device.participants.clear();
    }
    send_to_user(app, friend, None, &changed_env(kind, device, true)).await;
}

/// Push `device.changed{kind}` to the owner's devices (except the device
/// itself unless `include_self`) and to the friends that see it.
pub async fn push_changed(app: &App, a: &Audience, kind: DeviceChangeKind, include_self: bool) {
    let mut device = build_device(app, &a.row, &Viewer::Owner);
    if kind == DeviceChangeKind::Removed {
        device.status = STATUS_OFFLINE.to_owned();
        device.participants.clear();
    }
    let except = (!include_self).then_some(a.row.id.as_str());
    send_to_user(app, &a.owner.id, except, &changed_env(kind, device, false)).await;
    let owner = user_summary(&a.owner);
    for (friend, granted) in &a.friends {
        push_shared(app, friend, &a.row, &owner, granted.clone(), kind).await;
    }
}

/// Push `device.changed{kind}` for `device_id` (see [`push_changed`]).
pub async fn notify_changed(
    app: &App,
    device_id: &str,
    kind: DeviceChangeKind,
    include_self: bool,
) {
    if let Some(a) = load_audience(app, device_id).await {
        push_changed(app, &a, kind, include_self).await;
    }
}

fn validate_init(req: &DeviceInitRequest) -> ApiResult<()> {
    if !crypto::valid_public_key(&req.public_key) {
        return Err(ApiError::bad_request("public_key must be a base64url Ed25519 key"));
    }
    if req.installation_id.len() > 64 {
        return Err(ApiError::bad_request("installation_id too long"));
    }
    if !timestamp_fresh(req.timestamp) {
        return Err(clock_skew());
    }
    let proof = crypto::init_proof_message(&req.public_key, req.timestamp);
    if req.proof.len() > 128 || crypto::verify(&req.public_key, &proof, &req.proof).is_err() {
        return Err(ApiError::device_unauthorized());
    }
    Ok(())
}

/// `POST /api/v2/devices/init`: register the installation's key (a new key
/// is a new device) or update the profile of the device holding it.
pub async fn init(
    State(app): State<App>,
    ClientIp(ip): ClientIp,
    JsonBody(req): JsonBody<DeviceInitRequest>,
) -> ApiResult<Json<DeviceInitResponse>> {
    validate_init(&req)?;
    if !app.replay.insert(&req.proof) {
        return Err(ApiError::device_unauthorized());
    }
    let key = req.public_key.trim().to_owned();
    let k2 = key.clone();
    let existing = app
        .db
        .call(move |c| db::get_device_by_public_key(c, &k2))
        .await?;
    if let Some(device) = existing {
        let (id, req2) = (device.id.clone(), req.clone());
        app.db
            .call(move |c| db::update_device_profile(c, &id, &req2))
            .await?;
        let changed = device.platform != req.platform
            || device.version != db::clip(&req.version, 64)
            || device.capabilities != db::normalize_capabilities(&req.capabilities)
            || device.policy != db::normalize_policy(req.policy);
        if changed {
            notify_changed(&app, &device.id, DeviceChangeKind::Updated, false).await;
        }
        return Ok(Json(DeviceInitResponse {
            device_id: device.id,
            alias: device.alias,
        }));
    }

    // New key: a new (unbound) device. Logging in binds it to an account.
    app.limiter
        .take(
            &format!("init:{ip}"),
            app.cfg.limits.device_init_per_ip_hour,
            Duration::from_secs(3600),
        )
        .map_err(ApiError::rate_limited)?;
    let ip_text = ip.to_string();
    let req2 = req.clone();
    // The device ID is derived from the key: the same on every server and
    // without one (validate_init checked the key).
    let id = db::derived_device_id(&key)
        .ok_or_else(|| ApiError::bad_request("public_key must be a base64url Ed25519 key"))?;
    let (id, alias) = app
        .db
        .call(move |c| {
            db::insert_device(c, &id, &key, &req2, &ip_text)?;
            let alias = db::get_device(c, &id)?.map(|d| d.alias).unwrap_or_default();
            Ok((id, alias))
        })
        .await?;
    tracing::info!(device_id = %id, platform = req.platform.as_str(), "device registered");
    Ok(Json(DeviceInitResponse {
        device_id: id,
        alias,
    }))
}

/// `GET /api/v2/devices`
pub async fn list(State(app): State<App>, auth: AccountAuth) -> ApiResult<Json<DeviceList>> {
    let uid = auth.user.id.clone();
    let (rows, shared) = app
        .db
        .call(move |c| Ok((db::list_user_devices(c, &uid)?, db::shared_devices(c, &uid)?)))
        .await?;
    let current = rows
        .iter()
        .find(|r| r.id == auth.device.id)
        .cloned()
        .unwrap_or_else(|| auth.device.clone());
    Ok(Json(DeviceList {
        current_device: build_device(&app, &current, &Viewer::Owner),
        own: rows
            .iter()
            .filter(|r| r.id != auth.device.id)
            .map(|r| build_device(&app, r, &Viewer::Owner))
            .collect(),
        shared: shared
            .iter()
            .map(|(row, owner, granted)| {
                build_device(
                    &app,
                    row,
                    &Viewer::Friend {
                        user_id: auth.user.id.clone(),
                        owner: user_summary(owner),
                        granted: granted.clone(),
                    },
                )
            })
            .collect(),
    }))
}

/// A device the caller can see: one of its own, or a friend's device it
/// holds at least one capability on.
pub struct Visible {
    pub row: DeviceRow,
    pub relation: Relation,
    pub granted: Vec<String>,
    /// The owner (a friend) of a shared device.
    pub owner: Option<UserRow>,
}

/// Resolve a device visible to the caller (`device_not_found` otherwise).
pub async fn visible_device(app: &App, auth: &AccountAuth, id: &str) -> ApiResult<Visible> {
    if !ids::is_device_id(id) {
        return Err(ApiError::device_not_found());
    }
    let id = id.to_owned();
    let uid = auth.user.id.clone();
    app.db
        .call(move |c| {
            let Some(row) = db::get_device(c, &id)? else {
                return Ok(None);
            };
            let Some((relation, granted)) = db::access(c, &uid, &row)? else {
                return Ok(None);
            };
            if granted.is_empty() {
                return Ok(None);
            }
            let owner = match relation {
                Relation::Friend => row
                    .user_id
                    .as_deref()
                    .map(|u| db::get_user(c, u))
                    .transpose()?
                    .flatten(),
                _ => None,
            };
            Ok(Some(Visible {
                row,
                relation,
                granted,
                owner,
            }))
        })
        .await?
        .ok_or_else(ApiError::device_not_found)
}

/// Load a device that belongs to the caller's account (`forbidden` for a
/// visible friend's device, `device_not_found` otherwise).
pub async fn owned_device(app: &App, auth: &AccountAuth, id: &str) -> ApiResult<DeviceRow> {
    let v = visible_device(app, auth, id).await?;
    if v.relation != Relation::Own {
        return Err(ApiError::reason(
            ErrorCode::Forbidden,
            reason::NOT_OWN_DEVICE,
            "only devices of this account can be managed",
        ));
    }
    Ok(v.row)
}

/// `GET /api/v2/devices/{id}/detail`
pub async fn detail(
    State(app): State<App>,
    auth: AccountAuth,
    Path(id): Path<String>,
) -> ApiResult<Json<DeviceDetail>> {
    let v = visible_device(&app, &auth, &id).await?;
    let own = v.relation == Relation::Own;
    let r = v.row;
    Ok(Json(DeviceDetail {
        online: app.hub.is_online(&r.id),
        device_id: r.id,
        alias: r.alias,
        platform: r.platform,
        os: r.os,
        version: r.version,
        registered_at: r.created_at,
        last_seen_at: r.last_seen_at,
        connect_id: if own { r.connect_id } else { None },
        hardware: if own { r.hardware } else { None },
        owner: v.owner.as_ref().map(user_summary),
    }))
}

/// `PATCH /api/v2/devices/{id}`
pub async fn rename(
    State(app): State<App>,
    auth: AccountAuth,
    Path(id): Path<String>,
    JsonBody(req): JsonBody<RenameRequest>,
) -> ApiResult<Json<RenameResponse>> {
    let row = owned_device(&app, &auth, &id).await?;
    let alias = req.alias.trim().to_owned();
    if alias.is_empty() || alias.chars().count() > 64 || alias.chars().any(char::is_control) {
        return Err(ApiError::reason(
            ErrorCode::BadRequest,
            reason::DEVICE_NAME_INVALID,
            "name must be 1-64 printable characters",
        )
        .with_detail("max", 64));
    }
    let (id2, a2) = (row.id.clone(), alias.clone());
    app.db
        .call(move |c| db::set_device_alias(c, &id2, &a2))
        .await?;
    notify_changed(&app, &row.id, DeviceChangeKind::Updated, true).await;
    Ok(Json(RenameResponse {
        device_id: row.id,
        alias,
    }))
}

/// Unbind a device from its account and revoke its access tokens; the owner's
/// devices and the friends that saw it are told it was removed.
pub async fn unbind_device(app: &App, row: &DeviceRow, reason: &'static str) -> ApiResult<()> {
    if row.user_id.is_none() {
        return Ok(());
    }
    let audience = load_audience(app, &row.id).await;
    let id = row.id.clone();
    app.db
        .call(move |c| {
            db::revoke_tokens_for_device(c, &id, reason)?;
            db::set_device_user(c, &id, None)?;
            Ok(())
        })
        .await?;
    app.hub.revoke_device(&row.id);
    if let Some(a) = audience {
        push_changed(app, &a, DeviceChangeKind::Removed, false).await;
    }
    tracing::info!(device_id = %row.id, reason, "device unbound from account");
    Ok(())
}

/// `DELETE /api/v2/devices/{id}`
pub async fn remove(
    State(app): State<App>,
    auth: AccountAuth,
    Path(id): Path<String>,
) -> ApiResult<Json<Empty>> {
    let row = owned_device(&app, &auth, &id).await?;
    unbind_device(&app, &row, "removed").await?;
    Ok(Json(Empty {}))
}

/// `GET /api/v2/devices/self/friend-access`: the friends' devices that may
/// control the calling device, so it can admit them on direct connections.
pub async fn friend_access(
    State(app): State<App>,
    auth: AccountAuth,
) -> ApiResult<Json<farwing_protocol::api::FriendAccessList>> {
    let device = auth.device.clone();
    let rows = app
        .db
        .call(move |c| db::friend_access(c, &device))
        .await?;
    Ok(Json(farwing_protocol::api::FriendAccessList {
        devices: rows
            .into_iter()
            .map(|(row, user, grants)| farwing_protocol::api::FriendAccess {
                device_id: row.id,
                public_key: row.public_key,
                alias: row.alias,
                user_id: user.id,
                nickname: if user.nickname.is_empty() {
                    user.username
                } else {
                    user.nickname
                },
                grants,
            })
            .collect(),
    }))
}

/// `PUT /api/v2/devices/self/policy`
pub async fn set_policy(
    State(app): State<App>,
    auth: AnyAuth,
    JsonBody(policy): JsonBody<DevicePolicy>,
) -> ApiResult<Json<DevicePolicy>> {
    let policy = db::normalize_policy(policy);
    let id = auth.device.id.clone();
    app.db
        .call(move |c| db::set_device_policy(c, &id, &policy))
        .await?;
    if auth.device.policy != policy {
        notify_changed(&app, &auth.device.id, DeviceChangeKind::Updated, false).await;
    }
    Ok(Json(policy))
}

fn sniff_image(data: &[u8]) -> Option<&'static str> {
    if data.starts_with(b"\x89PNG\r\n\x1a\n") {
        Some("image/png")
    } else if data.starts_with(&[0xFF, 0xD8, 0xFF]) {
        Some("image/jpeg")
    } else if data.len() >= 26 && data.starts_with(b"BM") {
        Some("image/bmp")
    } else {
        None
    }
}

fn wallpaper_path(app: &App, device_id: &str) -> std::path::PathBuf {
    app.cfg.wallpaper_dir().join(device_id)
}

/// `PUT /api/v2/devices/self/wallpaper` (raw PNG/JPEG/BMP body).
pub async fn put_wallpaper(
    State(app): State<App>,
    auth: AccountAuth,
    req: Request,
) -> ApiResult<Json<WallpaperResponse>> {
    let body = super::read_body(req, limits::MAX_WALLPAPER_BYTES).await?;
    let Some(content_type) = sniff_image(&body) else {
        return Err(ApiError::bad_request("wallpaper must be PNG, JPEG or BMP"));
    };
    let etag = sha256_hex(&body);
    let dir = app.cfg.wallpaper_dir();
    let path = wallpaper_path(&app, &auth.device.id);
    let tmp = dir.join(format!(
        "{}.tmp-{}",
        auth.device.id,
        crate::crypto::new_uuid()
    ));
    let write = async {
        tokio::fs::create_dir_all(&dir).await?;
        tokio::fs::write(&tmp, &body).await?;
        tokio::fs::rename(&tmp, &path).await
    };
    if let Err(e) = write.await {
        let _ = tokio::fs::remove_file(&tmp).await;
        return Err(anyhow::anyhow!("storing wallpaper: {e}").into());
    }
    let (id, e2) = (auth.device.id.clone(), etag.clone());
    app.db
        .call(move |c| db::set_wallpaper(c, &id, Some(&e2), Some(content_type)))
        .await?;
    let mut row = auth.device.clone();
    row.wallpaper_etag = Some(etag);
    notify_changed(&app, &row.id, DeviceChangeKind::Updated, false).await;
    Ok(Json(WallpaperResponse {
        wallpaper_url: wallpaper_url(&row),
    }))
}

/// `DELETE /api/v2/devices/self/wallpaper`
pub async fn delete_wallpaper(State(app): State<App>, auth: AccountAuth) -> ApiResult<Json<Empty>> {
    let id = auth.device.id.clone();
    app.db
        .call(move |c| db::set_wallpaper(c, &id, None, None))
        .await?;
    let _ = tokio::fs::remove_file(wallpaper_path(&app, &auth.device.id)).await;
    if auth.device.wallpaper_etag.is_some() {
        notify_changed(&app, &auth.device.id, DeviceChangeKind::Updated, false).await;
    }
    Ok(Json(Empty {}))
}

/// `GET /api/v2/devices/{id}/wallpaper` (own and shared devices).
pub async fn get_wallpaper(
    State(app): State<App>,
    auth: AccountAuth,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> ApiResult<Response> {
    let row = visible_device(&app, &auth, &id).await?.row;
    let (Some(etag), Some(ct)) = (row.wallpaper_etag.clone(), row.wallpaper_type.clone()) else {
        return Err(ApiError::not_found());
    };
    let quoted = format!("\"{etag}\"");
    let etag_header = HeaderValue::from_str(&quoted).map_err(|_| ApiError::internal())?;
    if headers
        .get(header::IF_NONE_MATCH)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.split(',').any(|t| t.trim() == quoted || t.trim() == "*"))
    {
        return Ok((StatusCode::NOT_MODIFIED, [(header::ETAG, etag_header)]).into_response());
    }
    let data = match tokio::fs::read(wallpaper_path(&app, &row.id)).await {
        Ok(d) => d,
        Err(_) => return Err(ApiError::not_found()),
    };
    Ok((
        StatusCode::OK,
        [
            (
                header::CONTENT_TYPE,
                HeaderValue::from_str(&ct).map_err(|_| ApiError::internal())?,
            ),
            (header::ETAG, etag_header),
            (
                header::CACHE_CONTROL,
                HeaderValue::from_static("private, no-cache"),
            ),
        ],
        Body::from(data),
    )
        .into_response())
}
