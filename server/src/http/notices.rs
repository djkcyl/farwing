//! The account's notifications and which kinds it receives
//! (`docs/notifications.md`).

use super::auth::AccountAuth;
use super::{App, JsonBody};
use crate::error::{ApiError, ApiResult};
use crate::notify;
use axum::Json;
use axum::extract::{Query, State};
use farwing_protocol::api::{Empty, NoticeList, NoticeRead, NoticeSettings};
use serde::Deserialize;

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct NoticesQuery {
    pub before: Option<i64>,
    pub after: Option<i64>,
    pub limit: Option<u32>,
}

/// `GET /api/v2/me/notices`: newest first, with the unread count.
pub async fn list(
    State(app): State<App>,
    auth: AccountAuth,
    Query(q): Query<NoticesQuery>,
) -> ApiResult<Json<NoticeList>> {
    let uid = auth.user.id.clone();
    let limit = q.limit.unwrap_or(50);
    let list = app
        .db
        .call(move |c| {
            Ok(NoticeList {
                notices: notify::list(c, &uid, q.before, q.after, limit)?,
                unread: notify::unread(c, &uid)?,
            })
        })
        .await?;
    Ok(Json(list))
}

/// `POST /api/v2/me/notices/read`: these notifications, or all, are read.
pub async fn read(
    State(app): State<App>,
    auth: AccountAuth,
    JsonBody(req): JsonBody<NoticeRead>,
) -> ApiResult<Json<Empty>> {
    if req.ids.len() > 200 {
        return Err(ApiError::bad_request("too many notices"));
    }
    let uid = auth.user.id.clone();
    app.db
        .call(move |c| notify::mark_read(c, &uid, &req.ids, req.all))
        .await?;
    Ok(Json(Empty {}))
}

async fn current(app: &App, user_id: String) -> ApiResult<NoticeSettings> {
    Ok(app
        .db
        .call(move |c| {
            let admin = crate::webdb::role(c, &user_id)? == "admin";
            Ok(NoticeSettings {
                settings: notify::settings(c, &user_id, admin)?,
            })
        })
        .await?)
}

/// `GET /api/v2/me/notice-settings`
pub async fn settings(State(app): State<App>, auth: AccountAuth) -> ApiResult<Json<NoticeSettings>> {
    Ok(Json(current(&app, auth.user.id.clone()).await?))
}

/// `PUT /api/v2/me/notice-settings`: the kinds named are set.
pub async fn save_settings(
    State(app): State<App>,
    auth: AccountAuth,
    JsonBody(req): JsonBody<NoticeSettings>,
) -> ApiResult<Json<NoticeSettings>> {
    if req.settings.len() > 64 {
        return Err(ApiError::bad_request("too many settings"));
    }
    let uid = auth.user.id.clone();
    app.db
        .call(move |c| notify::save_settings(c, &uid, &req.settings))
        .await?;
    Ok(Json(current(&app, auth.user.id.clone()).await?))
}
