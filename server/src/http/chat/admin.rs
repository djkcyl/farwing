//! The console's groups page (`docs/protocol/chat-design.md` §13): every
//! group with its numbers; freeze, dissolve and restore, hand to another
//! owner, set its attachment space, read its audit log. Never the messages.

use super::{id, not_found};
use crate::chat::store;
use crate::chat::{push, GroupCtx};
use crate::error::{ApiError, ApiResult};
use crate::http::admin_api::log;
use crate::http::auth::AdminAuth;
use crate::http::{App, JsonBody};
use axum::Json;
use axum::extract::{Path, Query, State};
use farwing_protocol::api::Empty;
use farwing_protocol::chat::{AuditList, ChatUser, Id};
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::json;

#[derive(Serialize)]
pub struct AdminGroup {
    pub id: Id,
    pub name: String,
    pub icon: String,
    pub owner: ChatUser,
    pub public: bool,
    pub members: u32,
    pub channels: u32,
    /// Bytes of attachments.
    pub storage: i64,
    /// Its own attachment space (bytes); none: the server's default.
    pub quota_bytes: Option<i64>,
    pub created_at: i64,
    pub frozen: bool,
    pub dissolved_at: Option<i64>,
}

/// `GET /admin/chat/groups`
pub async fn groups(State(app): State<App>, _admin: AdminAuth) -> ApiResult<Json<Vec<AdminGroup>>> {
    let list = app
        .db
        .call(|c| {
            let mut out = Vec::new();
            for g in store::all_groups(c)? {
                out.push(AdminGroup {
                    id: Id(g.id),
                    name: g.name.clone(),
                    icon: g.icon.clone(),
                    owner: store::user(c, &g.owner_id)?,
                    public: g.public,
                    members: store::member_count(c, g.id)?,
                    channels: store::channel_count(c, g.id)?,
                    storage: store::group_usage(c, g.id)?,
                    quota_bytes: g.quota_bytes,
                    created_at: g.created_at,
                    frozen: g.frozen_at.is_some(),
                    dissolved_at: g.dissolved_at,
                });
            }
            Ok(out)
        })
        .await?;
    Ok(Json(list))
}

fn double_option<'de, T: Deserialize<'de>, D: Deserializer<'de>>(d: D) -> Result<Option<Option<T>>, D::Error> {
    Option::<T>::deserialize(d).map(Some)
}

#[derive(Deserialize, Default)]
#[serde(default)]
pub struct AdminGroupUpdate {
    frozen: Option<bool>,
    /// A member to own it.
    owner: Option<String>,
    /// Bytes; `null`: the server's default.
    #[serde(deserialize_with = "double_option")]
    quota_bytes: Option<Option<i64>>,
}

/// `PATCH /admin/chat/groups/{id}`
pub async fn update(
    State(app): State<App>,
    admin: AdminAuth,
    Path(gid): Path<String>,
    JsonBody(req): JsonBody<AdminGroupUpdate>,
) -> ApiResult<Json<Empty>> {
    let group = id(&gid)?;
    if req.quota_bytes.flatten().is_some_and(|q| q < 0) {
        return Err(ApiError::bad_request("quota_bytes is positive"));
    }
    let detail = json!({ "frozen": req.frozen, "owner": req.owner, "quota_bytes": req.quota_bytes.flatten() });
    app.db
        .call(move |c| {
            let Some(mut g) = store::group(c, group)? else {
                return Ok(Err(not_found()));
            };
            if let Some(f) = req.frozen {
                g.frozen_at = if f { g.frozen_at.or(Some(crate::db::now_s())) } else { None };
            }
            if let Some(o) = &req.owner {
                if store::member(c, group, o)?.is_none() {
                    return Ok(Err(ApiError::bad_request("the new owner is not a member")));
                }
                g.owner_id = o.clone();
            }
            if let Some(q) = req.quota_bytes {
                g.quota_bytes = q;
            }
            store::update_group(c, &g)?;
            Ok(Ok(()))
        })
        .await??;
    log(&app, &admin, "chat_group_update", &group.to_string(), detail).await;
    // Permissions follow the owner and the frozen state.
    push::group_state(&app, group, None).await;
    Ok(Json(Empty {}))
}

/// `DELETE /admin/chat/groups/{id}`: dissolve (restorable for 30 days).
pub async fn dissolve(State(app): State<App>, admin: AdminAuth, Path(gid): Path<String>) -> ApiResult<Json<Empty>> {
    let group = id(&gid)?;
    let members = app
        .db
        .call(move |c| {
            let Some(mut g) = store::group(c, group)? else {
                return Ok(Err(not_found()));
            };
            if g.dissolved_at.is_none() {
                g.dissolved_at = Some(crate::db::now_s());
                store::update_group(c, &g)?;
            }
            Ok(Ok(store::member_ids(c, group)?))
        })
        .await??;
    log(&app, &admin, "chat_group_dissolve", &group.to_string(), json!({})).await;
    push::group_removed(&app, group, &members, "dissolved");
    Ok(Json(Empty {}))
}

/// `POST /admin/chat/groups/{id}/restore`
pub async fn restore(State(app): State<App>, admin: AdminAuth, Path(gid): Path<String>) -> ApiResult<Json<Empty>> {
    let group = id(&gid)?;
    app.db
        .call(move |c| {
            let Some(mut g) = store::group(c, group)? else {
                return Ok(Err(not_found()));
            };
            g.dissolved_at = None;
            store::update_group(c, &g)?;
            Ok(Ok(()))
        })
        .await??;
    log(&app, &admin, "chat_group_restore", &group.to_string(), json!({})).await;
    push::group_state(&app, group, None).await;
    Ok(Json(Empty {}))
}

#[derive(Deserialize)]
pub struct AuditQuery {
    before: Option<i64>,
}

/// `GET /admin/chat/groups/{id}/audit?before=`
pub async fn audit(
    State(app): State<App>,
    _admin: AdminAuth,
    Path(gid): Path<String>,
    Query(q): Query<AuditQuery>,
) -> ApiResult<Json<AuditList>> {
    let group = id(&gid)?;
    if app
        .db
        .call(move |c| super::groups::group_ctx_any(c, group))
        .await?
        .map(|ctx: GroupCtx| ctx.group.id)
        .is_none()
    {
        return Err(not_found());
    }
    Ok(Json(super::groups::audit_log_of(&app, group, q.before).await?))
}
