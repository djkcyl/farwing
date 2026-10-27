//! Friends, friend requests and grants (`docs/protocol/v2-design.md` §3).

use super::auth::AccountAuth;
use super::devices::{push_shared, user_summary};
use super::{App, ClientIp, JsonBody};
use crate::crypto::new_uuid;
use crate::db::{self, DeviceRow, UserRow};
use crate::error::{ApiError, ApiResult};
use axum::Json;
use axum::extract::{Path, State};
use farwing_protocol::api::{
    Empty, Friend, FriendAccept, FriendList, FriendRequest, FriendRequestCreate,
    FriendRequestOutcome, FriendRequests, Grants,
};
use farwing_protocol::realtime::{
    DeviceChangeKind, Envelope, FriendChangeKind, FriendChanged, types,
};
use farwing_protocol::error::reason;
use farwing_protocol::{ErrorCode, caps, ids};
use std::collections::HashMap;
use std::time::Duration;

/// Friend requests a user may send per hour.
const FRIEND_REQUESTS_PER_HOUR: u32 = 20;
/// Per-device entries accepted in one `PUT /grants/{user_id}`.
const MAX_GRANT_DEVICES: usize = 256;

fn user_not_found() -> ApiError {
    ApiError::new(ErrorCode::UserNotFound, "user not found")
}

fn not_friends() -> ApiError {
    ApiError::reason(ErrorCode::NotFound, reason::NOT_FRIENDS, "not friends")
}

/// Push `friend.changed{kind, user: about}` to the online devices of `to`.
async fn push_friend_changed(app: &App, to: &str, kind: FriendChangeKind, about: &UserRow) {
    let env = Envelope::new(
        types::FRIEND_CHANGED,
        FriendChanged {
            kind,
            user: user_summary(about),
        },
    );
    super::devices::send_to_user(app, to, None, &env).await;
}

/// Devices of `owner` with the capabilities `viewer` holds on each.
async fn devices_seen_by(
    app: &App,
    owner: &str,
    viewer: &str,
) -> ApiResult<Vec<(DeviceRow, Vec<String>)>> {
    let (owner, viewer) = (owner.to_owned(), viewer.to_owned());
    Ok(app
        .db
        .call(move |c| {
            let caps = db::caps_on_devices(c, &owner, &viewer)?;
            Ok(db::list_user_devices(c, &owner)?
                .into_iter()
                .map(|row| {
                    let granted = caps.get(&row.id).cloned().unwrap_or_default();
                    (row, granted)
                })
                .collect())
        })
        .await?)
}

/// Tell `viewer` about the devices of `owner` that became visible or
/// invisible (`before` → `after`), or whose capabilities changed.
async fn push_visibility(
    app: &App,
    owner: &UserRow,
    viewer: &str,
    before: &HashMap<String, Vec<String>>,
    after: &[(DeviceRow, Vec<String>)],
) {
    let summary = user_summary(owner);
    for (row, granted) in after {
        let old = before.get(&row.id).map(Vec::as_slice).unwrap_or(&[]);
        let kind = match (old.is_empty(), granted.is_empty()) {
            (true, false) => DeviceChangeKind::Added,
            (false, true) => DeviceChangeKind::Removed,
            (false, false) if old != granted.as_slice() => DeviceChangeKind::Updated,
            _ => continue,
        };
        push_shared(app, viewer, row, &summary, granted.clone(), kind).await;
    }
}

fn as_map(list: &[(DeviceRow, Vec<String>)]) -> HashMap<String, Vec<String>> {
    list.iter()
        .map(|(row, caps)| (row.id.clone(), caps.clone()))
        .collect()
}

/// What changed on each device between `before` and `after`, for a notice:
/// `[{device, added, removed}]` (device names, capability names).
fn grant_changes(
    before: &HashMap<String, Vec<String>>,
    after: &[(DeviceRow, Vec<String>)],
) -> serde_json::Value {
    let changes: Vec<serde_json::Value> = after
        .iter()
        .filter_map(|(row, now)| {
            let old = before.get(&row.id).map(Vec::as_slice).unwrap_or(&[]);
            let added: Vec<&String> = now.iter().filter(|c| !old.contains(c)).collect();
            let removed: Vec<&String> = old.iter().filter(|c| !now.contains(c)).collect();
            if added.is_empty() && removed.is_empty() {
                return None;
            }
            let name = if row.alias.is_empty() { &row.name } else { &row.alias };
            Some(serde_json::json!({ "device": name, "added": added, "removed": removed }))
        })
        .collect();
    serde_json::Value::Array(changes)
}

/// After `a` and `b` became friends: announce the friendship to `b` and the
/// newly shared devices to both.
async fn announce_friendship(app: &App, a: &UserRow, b: &UserRow) -> ApiResult<()> {
    push_friend_changed(app, &b.id, FriendChangeKind::RequestAccepted, a).await;
    for (owner, viewer) in [(a, b), (b, a)] {
        let seen = devices_seen_by(app, &owner.id, &viewer.id).await?;
        push_visibility(app, owner, &viewer.id, &HashMap::new(), &seen).await;
    }
    Ok(())
}

async fn load_user(app: &App, id: &str) -> ApiResult<Option<UserRow>> {
    if !ids::is_uuid(id) {
        return Ok(None);
    }
    let id = id.to_owned();
    Ok(app.db.call(move |c| db::get_user(c, &id)).await?)
}

/// `GET /api/v2/friends`
pub async fn list(State(app): State<App>, auth: AccountAuth) -> ApiResult<Json<FriendList>> {
    let uid = auth.user.id.clone();
    let rows = app.db.call(move |c| db::list_friends(c, &uid)).await?;
    Ok(Json(FriendList {
        friends: rows
            .iter()
            .map(|(u, since)| Friend {
                user: user_summary(u),
                since: *since,
            })
            .collect(),
    }))
}

/// `GET /api/v2/friends/requests`
pub async fn requests(
    State(app): State<App>,
    auth: AccountAuth,
) -> ApiResult<Json<FriendRequests>> {
    let uid = auth.user.id.clone();
    let (incoming, outgoing) = app
        .db
        .call(move |c| {
            Ok((
                db::list_friend_requests(c, &uid, true)?,
                db::list_friend_requests(c, &uid, false)?,
            ))
        })
        .await?;
    let convert = |rows: Vec<(db::FriendRequestRow, UserRow)>| {
        rows.into_iter()
            .map(|(r, u)| FriendRequest {
                id: r.id,
                user: user_summary(&u),
                created_at: r.created_at,
            })
            .collect()
    };
    Ok(Json(FriendRequests {
        incoming: convert(incoming),
        outgoing: convert(outgoing),
    }))
}

enum SendOutcome {
    Accepted(i64),
    Existing(db::FriendRequestRow),
    Created(db::FriendRequestRow),
}

/// `POST /api/v2/friends/requests`: send a request, or become friends right
/// away when the other user already asked.
pub async fn send_request(
    State(app): State<App>,
    auth: AccountAuth,
    JsonBody(req): JsonBody<FriendRequestCreate>,
) -> ApiResult<Json<FriendRequestOutcome>> {
    app.limiter
        .take(
            &format!("friend-request:{}", auth.user.id),
            FRIEND_REQUESTS_PER_HOUR,
            Duration::from_secs(3600),
        )
        .map_err(ApiError::rate_limited)?;
    let username = req.username.trim().to_owned();
    if username.is_empty() || username.len() > 128 {
        return Err(user_not_found());
    }
    let other = app
        .db
        .call(move |c| db::get_user_by_name(c, &username))
        .await?
        .filter(|u| !u.disabled)
        .ok_or_else(user_not_found)?;
    if other.id == auth.user.id {
        return Err(ApiError::reason(
            ErrorCode::BadRequest,
            reason::SELF_FRIEND,
            "cannot befriend yourself",
        ));
    }
    let (me, them) = (auth.user.id.clone(), other.id.clone());
    let outcome = app
        .db
        .call(move |c| {
            if db::are_friends(c, &me, &them)? {
                return Ok(None);
            }
            if db::find_friend_request(c, &them, &me)?.is_some() {
                let shares = caps::normalize(caps::DEFAULT_FRIEND);
                return Ok(Some(SendOutcome::Accepted(db::make_friends(c, &me, &them, &shares)?)));
            }
            if let Some(r) = db::find_friend_request(c, &me, &them)? {
                return Ok(Some(SendOutcome::Existing(r)));
            }
            Ok(Some(SendOutcome::Created(db::insert_friend_request(
                c,
                &new_uuid(),
                &me,
                &them,
            )?)))
        })
        .await?
        .ok_or_else(|| ApiError::new(ErrorCode::AlreadyFriends, "already friends"))?;
    let request = |r: db::FriendRequestRow| FriendRequest {
        id: r.id,
        user: user_summary(&other),
        created_at: r.created_at,
    };
    Ok(Json(match outcome {
        SendOutcome::Accepted(since) => {
            tracing::info!(user_id = %auth.user.id, friend = %other.id, "friends (mutual request)");
            announce_friendship(&app, &auth.user, &other).await?;
            let data = serde_json::json!({
                "who": display(&auth.user),
                "caps": caps::normalize(caps::DEFAULT_FRIEND),
            });
            crate::notify::send(&app, &other.id, farwing_protocol::api::notice::FRIEND_ACCEPTED, data, &[]);
            FriendRequestOutcome::Accepted {
                friend: Friend {
                    user: user_summary(&other),
                    since,
                },
            }
        }
        SendOutcome::Existing(r) => FriendRequestOutcome::Sent {
            request: request(r),
        },
        SendOutcome::Created(r) => {
            push_friend_changed(&app, &other.id, FriendChangeKind::RequestReceived, &auth.user)
                .await;
            let data = serde_json::json!({ "who": display(&auth.user) });
            crate::notify::send(&app, &other.id, farwing_protocol::api::notice::FRIEND_REQUEST, data, &[]);
            FriendRequestOutcome::Sent {
                request: request(r),
            }
        }
    }))
}

/// Load a pending request; `incoming` requires it to be addressed to the
/// caller, otherwise sent by the caller. Returns the request and the other
/// party.
async fn own_request(
    app: &App,
    auth: &AccountAuth,
    id: &str,
    incoming: bool,
) -> ApiResult<(db::FriendRequestRow, UserRow)> {
    if !ids::is_uuid(id) {
        return Err(ApiError::not_found());
    }
    let (id, me) = (id.to_owned(), auth.user.id.clone());
    app.db
        .call(move |c| {
            let Some(r) = db::get_friend_request(c, &id)? else {
                return Ok(None);
            };
            let (mine, other) = if incoming {
                (&r.to_user, &r.from_user)
            } else {
                (&r.from_user, &r.to_user)
            };
            if *mine != me {
                return Ok(None);
            }
            let other = db::get_user(c, other)?;
            Ok(other.map(|u| (r, u)))
        })
        .await?
        .ok_or_else(ApiError::not_found)
}

/// `POST /api/v2/friends/requests/{id}/accept` with what the caller shares
/// ([`FriendAccept`]) → the new [`Friend`].
pub async fn accept_request(
    State(app): State<App>,
    auth: AccountAuth,
    ClientIp(ip): ClientIp,
    Path(id): Path<String>,
    JsonBody(req): JsonBody<FriendAccept>,
) -> ApiResult<Json<Friend>> {
    let (_, other) = own_request(&app, &auth, &id, true).await?;
    let (me, them) = (auth.user.id.clone(), other.id.clone());
    let shares = caps::normalize(&req.caps);
    let shared = shares.clone();
    let since = app
        .db
        .call(move |c| db::make_friends(c, &me, &them, &shared))
        .await?;
    tracing::info!(user_id = %auth.user.id, friend = %other.id, ?shares, "friend request accepted");
    friend_event(&app, &auth, &other, "friend.add", ip).await;
    let data = serde_json::json!({ "who": display(&auth.user), "caps": shares });
    crate::notify::send(&app, &other.id, farwing_protocol::api::notice::FRIEND_ACCEPTED, data, &[]);
    announce_friendship(&app, &auth.user, &other).await?;
    Ok(Json(Friend {
        user: user_summary(&other),
        since,
    }))
}

/// `POST /api/v2/friends/requests/{id}/reject`
pub async fn reject_request(
    State(app): State<App>,
    auth: AccountAuth,
    Path(id): Path<String>,
) -> ApiResult<Json<Empty>> {
    let (r, other) = own_request(&app, &auth, &id, true).await?;
    app.db
        .call(move |c| db::delete_friend_request(c, &r.id))
        .await?;
    push_friend_changed(&app, &other.id, FriendChangeKind::RequestRejected, &auth.user).await;
    Ok(Json(Empty {}))
}

/// `DELETE /api/v2/friends/requests/{id}`: withdraw an outgoing request.
pub async fn cancel_request(
    State(app): State<App>,
    auth: AccountAuth,
    Path(id): Path<String>,
) -> ApiResult<Json<Empty>> {
    let (r, other) = own_request(&app, &auth, &id, false).await?;
    app.db
        .call(move |c| db::delete_friend_request(c, &r.id))
        .await?;
    push_friend_changed(&app, &other.id, FriendChangeKind::RequestCancelled, &auth.user).await;
    Ok(Json(Empty {}))
}

/// `DELETE /api/v2/friends/{user_id}`: end the friendship, drop the grants in
/// both directions and end the sessions they allowed (`permission_revoked`).
pub async fn remove(
    State(app): State<App>,
    auth: AccountAuth,
    ClientIp(ip): ClientIp,
    Path(user_id): Path<String>,
) -> ApiResult<Json<Empty>> {
    let other = load_user(&app, &user_id).await?.ok_or_else(not_friends)?;
    let mine_seen = devices_seen_by(&app, &auth.user.id, &other.id).await?;
    let theirs_seen = devices_seen_by(&app, &other.id, &auth.user.id).await?;
    let (me, them) = (auth.user.id.clone(), other.id.clone());
    let removed = app
        .db
        .call(move |c| db::remove_friendship(c, &me, &them))
        .await?;
    if !removed {
        return Err(not_friends());
    }
    let revoke_all = |seen: &[(DeviceRow, Vec<String>)]| -> HashMap<String, Vec<String>> {
        seen.iter()
            .map(|(row, _)| (row.id.clone(), Vec::new()))
            .collect()
    };
    app.hub.regrant(&other.id, &revoke_all(&mine_seen));
    app.hub.regrant(&auth.user.id, &revoke_all(&theirs_seen));
    tracing::info!(user_id = %auth.user.id, friend = %other.id, "friendship removed");
    friend_event(&app, &auth, &other, "friend.remove", ip).await;
    let data = serde_json::json!({ "who": display(&auth.user) });
    crate::notify::send(&app, &other.id, farwing_protocol::api::notice::FRIEND_REMOVED, data, &[]);
    push_friend_changed(&app, &other.id, FriendChangeKind::Removed, &auth.user).await;
    let gone = |seen: &[(DeviceRow, Vec<String>)]| -> Vec<(DeviceRow, Vec<String>)> {
        seen.iter()
            .map(|(row, _)| (row.clone(), Vec::new()))
            .collect()
    };
    push_visibility(&app, &auth.user, &other.id, &as_map(&mine_seen), &gone(&mine_seen)).await;
    push_visibility(&app, &other, &auth.user.id, &as_map(&theirs_seen), &gone(&theirs_seen)).await;
    Ok(Json(Empty {}))
}

/// `GET /api/v2/grants/{user_id}`: what the caller grants this friend.
pub async fn get_grants(
    State(app): State<App>,
    auth: AccountAuth,
    Path(user_id): Path<String>,
) -> ApiResult<Json<Grants>> {
    let other = load_user(&app, &user_id).await?.ok_or_else(not_friends)?;
    let (me, them) = (auth.user.id.clone(), other.id.clone());
    app.db
        .call(move |c| {
            if !db::are_friends(c, &me, &them)? {
                return Ok(None);
            }
            db::get_grants(c, &me, &them).map(Some)
        })
        .await?
        .map(Json)
        .ok_or_else(not_friends)
}

/// `PUT /api/v2/grants/{user_id}`: replace what the caller grants this
/// friend. Live sessions follow immediately (`session.permissions`, or
/// `permission_revoked` when the purpose is no longer allowed).
/// An account event of both friends: the entry's target is the other one.
async fn friend_event(app: &App, auth: &AccountAuth, other: &UserRow, kind: &str, ip: std::net::IpAddr) {
    let detail = serde_json::json!({ "friend": display(other), "by": display(&auth.user) });
    let actor = Some((auth.user.id.as_str(), auth.user.username.as_str()));
    super::me::audit(app, actor, kind, &other.username, detail, true, ip).await;
}

fn display(u: &UserRow) -> String {
    if u.nickname.is_empty() { u.username.clone() } else { u.nickname.clone() }
}

pub async fn put_grants(
    State(app): State<App>,
    auth: AccountAuth,
    ClientIp(ip): ClientIp,
    Path(user_id): Path<String>,
    JsonBody(req): JsonBody<Grants>,
) -> ApiResult<Json<Grants>> {
    if req.devices.len() > MAX_GRANT_DEVICES {
        return Err(ApiError::bad_request("too many device entries"));
    }
    let other = load_user(&app, &user_id).await?.ok_or_else(not_friends)?;
    let grants = Grants {
        default: caps::normalize(&req.default),
        devices: req
            .devices
            .iter()
            .map(|(id, list)| (id.clone(), caps::normalize(list)))
            .collect(),
    };
    let before = devices_seen_by(&app, &auth.user.id, &other.id).await?;
    let (me, them) = (auth.user.id.clone(), other.id.clone());
    let stored = app
        .db
        .call(move |c| {
            if !db::are_friends(c, &me, &them)? {
                return Ok(Err(not_friends()));
            }
            let own = db::user_device_ids(c, &me)?;
            if grants.devices.keys().any(|d| !own.contains(d)) {
                return Ok(Err(ApiError::bad_request(
                    "grants may only name devices of this account",
                )));
            }
            db::replace_grants(c, &me, &them, &grants)?;
            Ok(Ok(db::get_grants(c, &me, &them)?))
        })
        .await??;
    let after = devices_seen_by(&app, &auth.user.id, &other.id).await?;
    tracing::info!(user_id = %auth.user.id, friend = %other.id, "grants changed");
    if as_map(&before) != as_map(&after) {
        friend_event(&app, &auth, &other, "grants.update", ip).await;
        let data = serde_json::json!({
            "who": display(&auth.user),
            "changes": grant_changes(&as_map(&before), &after),
        });
        crate::notify::send(&app, &other.id, farwing_protocol::api::notice::FRIEND_GRANTS, data, &[]);
    }
    app.hub.regrant(&other.id, &as_map(&after));
    push_friend_changed(&app, &other.id, FriendChangeKind::GrantsChanged, &auth.user).await;
    push_visibility(&app, &auth.user, &other.id, &as_map(&before), &after).await;
    Ok(Json(stored))
}
