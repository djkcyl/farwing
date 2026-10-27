//! Connection records and account events (`docs/connection-records.md`):
//! each account sees the connections its devices took part in and the
//! events of the account; administrators see all records.

use super::auth::{AccountAuth, AdminAuth, AnyAuth};
use super::{App, ClientIp, JsonBody};
use crate::error::{ApiError, ApiResult};
use crate::records::{self, Filter, ReportError, ReportOrDb};
use crate::webdb;
use axum::Json;
use axum::extract::{Query, State};
use farwing_protocol::ErrorCode;
use farwing_protocol::api::{
    AccountEvent, ConnectionRecord, DeviceEvent, Empty, RecordParty, RecordReport, device_event,
};
use serde::Deserialize;
use serde_json::json;

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct RecordsQuery {
    /// Admins: one account's records.
    pub user: Option<String>,
    pub device: Option<String>,
    pub before: Option<i64>,
    pub limit: Option<u32>,
}

const PAGE: u32 = 50;

/// `GET /api/v2/records`: the account's connections, newest first.
pub async fn list(
    State(app): State<App>,
    auth: AccountAuth,
    Query(q): Query<RecordsQuery>,
) -> ApiResult<Json<Vec<ConnectionRecord>>> {
    let filter = Filter {
        user: Some(auth.user.id.clone()),
        device: q.device.filter(|d| !d.is_empty()),
        before: q.before,
        limit: q.limit.unwrap_or(PAGE),
    };
    Ok(Json(app.db.call(move |c| records::list(c, &filter)).await?))
}

/// `GET /api/v2/admin/records`: every connection, or one account's or
/// device's.
pub async fn admin_list(
    State(app): State<App>,
    _admin: AdminAuth,
    Query(q): Query<RecordsQuery>,
) -> ApiResult<Json<Vec<ConnectionRecord>>> {
    let filter = Filter {
        user: q.user.filter(|u| !u.is_empty()),
        device: q.device.filter(|d| !d.is_empty()),
        before: q.before,
        limit: q.limit.unwrap_or(PAGE),
    };
    Ok(Json(app.db.call(move |c| records::list(c, &filter)).await?))
}

/// `POST /api/v2/records/reports`: a side's summary of a visit (visitors
/// without an account too).
pub async fn report(
    State(app): State<App>,
    auth: AnyAuth,
    JsonBody(r): JsonBody<RecordReport>,
) -> ApiResult<Json<Empty>> {
    if r.visit.is_empty() || r.visit.len() > 64 {
        return Err(ApiError::bad_request("invalid visit"));
    }
    let too_long = r.host.as_ref().is_some_and(|h| {
        h.files.len() > farwing_protocol::api::HostReport::MAX_FILES
            || h.ports.len() > farwing_protocol::api::HostReport::MAX_PORTS
            || h.files.iter().any(|f| f.name.len() > 512)
            || h.ports.iter().any(|p| p.len() > 300)
    }) || r
        .direct
        .as_ref()
        .is_some_and(|d| d.host_name.len() > 128 || d.controller_name.len() > 128);
    if too_long {
        return Err(ApiError::bad_request("report too large"));
    }
    let reporter = RecordParty {
        device_id: auth.device.id.clone(),
        name: auth.device.alias.clone(),
        user_id: auth.user.as_ref().map(|u| u.id.clone()).unwrap_or_default(),
        username: auth.user.as_ref().map(|u| u.username.clone()).unwrap_or_default(),
        nickname: auth.user.as_ref().map(|u| u.nickname.clone()).unwrap_or_default(),
        platform: auth.device.platform,
    };
    let outcome = app
        .db
        .call(move |c| Ok(records::report(c, &reporter, &r)))
        .await?;
    match outcome {
        Ok(()) => Ok(Json(Empty {})),
        Err(ReportOrDb::Refused(ReportError::NotFound)) => Err(ApiError::not_found()),
        Err(ReportOrDb::Refused(ReportError::NotAParty)) => {
            Err(ApiError::new(ErrorCode::Forbidden, "not a party of this connection"))
        }
        Err(ReportOrDb::Db(e)) => Err(anyhow::Error::from(e).into()),
    }
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct EventsQuery {
    pub before: Option<i64>,
    pub limit: Option<u32>,
}

/// `GET /api/v2/me/events`: the account's sign-ins, security, device,
/// friend and access-setting events, newest first.
pub async fn account_events(
    State(app): State<App>,
    auth: AccountAuth,
    Query(q): Query<EventsQuery>,
) -> ApiResult<Json<Vec<AccountEvent>>> {
    let (uid, username) = (auth.user.id.clone(), auth.user.username.clone());
    let limit = q.limit.unwrap_or(PAGE);
    let entries = app
        .db
        .call(move |c| webdb::account_events(c, &uid, &username, q.before, limit))
        .await?;
    Ok(Json(
        entries
            .into_iter()
            .map(|e| AccountEvent {
                id: e.id,
                at: e.at,
                kind: e.kind,
                // Someone else: an administrator, a friend.
                actor: if e.actor_user.as_deref() == Some(auth.user.id.as_str()) {
                    String::new()
                } else {
                    e.actor_name
                },
                target: e.target,
                detail: serde_json::from_str(&e.detail).unwrap_or_default(),
                ok: e.ok,
                ip: e.ip,
            })
            .collect(),
    ))
}

/// `POST /api/v2/devices/self/events`: the calling device's access code
/// was turned on or off, or it trusts a device or no longer does.
pub async fn device_event(
    State(app): State<App>,
    auth: AccountAuth,
    ClientIp(ip): ClientIp,
    JsonBody(e): JsonBody<DeviceEvent>,
) -> ApiResult<Json<Empty>> {
    if !device_event::ALL.contains(&e.kind.as_str())
        || e.device_id.len() > 64
        || e.device_name.len() > 128
    {
        return Err(ApiError::bad_request("invalid event"));
    }
    let mut detail = json!({ "device": auth.device.alias, "device_id": auth.device.id });
    if !e.device_id.is_empty() {
        detail["other"] = json!(e.device_name);
        detail["other_id"] = json!(e.device_id);
    }
    super::me::audit(
        &app,
        Some((&auth.user.id, &auth.user.username)),
        &e.kind,
        &auth.user.username,
        detail,
        true,
        ip,
    )
    .await;
    Ok(Json(Empty {}))
}
