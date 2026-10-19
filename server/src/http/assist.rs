//! Admission through the server (`docs/protocol/v2-design.md` §2,
//! `docs/access-model.md` §2.4): a controller device, with or without an
//! account, asks a host by short number or device ID. The host decides: a
//! device it trusts is admitted by its key; a visitor proves the access code
//! with SPAKE2 relayed by the server (which never learns the code), or the
//! host's user confirms.

use super::auth::{AccountAuth, AnyAuth};
use super::sessions::{device_controller_info, session_ticket};
use super::{App, JsonBody};
use crate::assist::{DecisionOutcome, Parties, ReplyError};
use crate::crypto::new_uuid;
use crate::db::{self, DeviceRow};
use crate::error::{ApiError, ApiResult};
use crate::hub::SessionSpec;
use axum::Json;
use axum::extract::{Path, State};
use farwing_protocol::api::{
    AssistAcceptRequest, AssistDecision, AssistHost, AssistIdentity, AssistJoin,
    AssistJoinRequest, AssistMode, AssistPakeReply, AssistPending, AssistRefusal, Empty,
    FavoritesRequest, Purpose, Relation, SavedDevice, SavedList,
};
use farwing_protocol::crypto::ASSIST_PAKE_SLOTS;
use farwing_protocol::realtime::{
    AssistAcceptPush, AssistCancelled, AssistJoinPush, Envelope, types,
};
use farwing_protocol::session::ConnectType;
use farwing_protocol::{ErrorCode, ids};
use serde_json::json;
use std::time::Duration;

/// Longest PAKE message or key confirmation accepted (base64url).
const MAX_PAKE_TEXT: usize = 128;
/// Failed attempts tolerated per connect ID per hour, across all accounts.
const TARGET_FAILURES_PER_HOUR: u32 = 50;

fn pake_list_ok(list: &[String], max: usize) -> bool {
    list.len() <= max && list.iter().all(|m| m.len() <= MAX_PAKE_TEXT)
}

/// The host a join asks for: its short number, or its device ID.
fn join_target(req: &AssistJoinRequest) -> ApiResult<String> {
    match (req.connect_id.is_empty(), req.device_id.is_empty()) {
        (false, true) if ids::is_connect_id(&req.connect_id) => Ok(req.connect_id.clone()),
        (true, false) if ids::is_device_id(&req.device_id) => Ok(req.device_id.clone()),
        _ => Err(ApiError::bad_request("give connect_id or device_id")),
    }
}

/// Resolve an online host by short number or device ID.
async fn assist_host(app: &App, auth: &AnyAuth, target: &str) -> ApiResult<DeviceRow> {
    let key = target.to_owned();
    let host = app
        .db
        .call(move |c| {
            if ids::is_connect_id(&key) {
                db::get_device_by_connect_id(c, &key)
            } else {
                db::get_device(c, &key)
            }
        })
        .await?
        .ok_or_else(ApiError::device_not_found)?;
    if host.id == auth.device.id {
        return Err(ApiError::bad_request("a device cannot assist itself"));
    }
    if !app.hub.is_online(&host.id) {
        return Err(ApiError::device_offline());
    }
    Ok(host)
}

fn host_info(host: &DeviceRow) -> AssistHost {
    AssistHost {
        device_id: host.id.clone(),
        alias: host.alias.clone(),
        platform: host.platform,
        version: host.version.clone(),
        public_key: host.public_key.clone(),
    }
}

fn host_wait(app: &App) -> Duration {
    Duration::from_millis(app.cfg.limits.assist_host_reply_ms)
}

/// The controller, for rate limits and request ownership: its account, or
/// its device without one.
fn controller_key(auth: &AnyAuth) -> String {
    match &auth.user {
        Some(user) => user.id.clone(),
        None => format!("device:{}", auth.device.id),
    }
}

fn failure_key(auth: &AnyAuth, target: &str) -> String {
    format!("assist:{}:{target}", controller_key(auth))
}

fn target_key(connect_id: &str) -> String {
    format!("assist-target:{connect_id}")
}

fn unavailable() -> ApiError {
    ApiError::new(ErrorCode::AssistUnavailable, "对方没有开启访问码，也没有信任这台设备")
}

fn code_required(mode: Option<AssistMode>) -> ApiError {
    let e = ApiError::new(ErrorCode::AssistCodeRequired, "需要访问码");
    match mode {
        Some(m) => e.with_details(json!({ "mode": m.as_str() })),
        None => e,
    }
}

fn refusal_error(refusal: Option<AssistRefusal>, mode: Option<AssistMode>) -> ApiError {
    match refusal {
        Some(AssistRefusal::CodeInvalid) => {
            ApiError::new(ErrorCode::AssistCodeInvalid, "访问码不正确")
        }
        Some(AssistRefusal::CodeRequired) => code_required(mode),
        Some(AssistRefusal::Unavailable) => unavailable(),
        Some(AssistRefusal::Rejected) | None => {
            ApiError::new(ErrorCode::AssistRejected, "对方拒绝了本次连接")
        }
    }
}

/// Tell the host to forget a request it answered.
fn push_cancelled(app: &App, host_device_id: &str, request_id: &str) {
    app.hub.push(
        host_device_id,
        &Envelope::new(
            types::ASSIST_CANCELLED,
            AssistCancelled {
                request_id: request_id.to_owned(),
            },
        ),
    );
}

/// `POST /api/v2/assist/join`: push the controller's PAKE messages to the
/// host and return its answer.
///
/// Every attempt that reaches the host counts against the rate limits until
/// it ends in a successful decision: a wrong code is only detected by the
/// controller (key confirmation), so the server cannot tell failures apart.
pub async fn join(
    State(app): State<App>,
    auth: AnyAuth,
    JsonBody(req): JsonBody<AssistJoinRequest>,
) -> ApiResult<Json<AssistPending>> {
    if !pake_list_ok(&req.pake, ASSIST_PAKE_SLOTS) {
        return Err(ApiError::bad_request("invalid pake"));
    }
    let wanted = join_target(&req)?;
    // Attempts are counted before the host is asked, so concurrent guesses
    // cannot all pass.
    let key = failure_key(&auth, &wanted);
    let target = target_key(&wanted);
    app.limiter
        .take(
            &key,
            app.cfg.limits.assist_failures_per_minute,
            Duration::from_secs(60),
        )
        .map_err(ApiError::rate_limited)?;
    if let Err(wait) = app
        .limiter
        .take(&target, TARGET_FAILURES_PER_HOUR, Duration::from_secs(3600))
    {
        app.limiter.refund(&key);
        return Err(ApiError::rate_limited(wait));
    }
    // Outcomes before the host is reached reveal nothing about the code.
    let refund = || {
        app.limiter.refund(&key);
        app.limiter.refund(&target);
    };
    let host = match assist_host(&app, &auth, &wanted).await {
        Ok(host) => host,
        Err(error) => {
            refund();
            return Err(error);
        }
    };
    if !host.policy.assist {
        refund();
        return Err(unavailable());
    }
    let rid = new_uuid();
    let wait = host_wait(&app);
    let rx = app.assist.begin(
        &rid,
        Parties {
            host_device_id: host.id.clone(),
            connect_id: host.connect_id.clone().unwrap_or_default(),
            target: wanted.clone(),
            controller_user_id: controller_key(&auth),
            controller_device_id: auth.device.id.clone(),
        },
        wait,
    );
    let pushed = app.hub.push(
        &host.id,
        &Envelope::new(
            types::ASSIST_JOIN,
            AssistJoinPush {
                request_id: rid.clone(),
                controller: device_controller_info(&app, &auth.device, auth.user.as_ref()),
                pake: req.pake.clone(),
            },
        ),
    );
    if !pushed {
        app.assist.remove(&rid);
        refund();
        return Err(ApiError::device_offline());
    }
    let reply: AssistPakeReply = match tokio::time::timeout(wait, rx).await {
        Ok(Ok(r)) => r,
        _ => {
            app.assist.remove(&rid);
            return Err(ApiError::timeout());
        }
    };
    if !reply.allowed {
        app.assist.remove(&rid);
        return Err(unavailable());
    }
    if !reply.trusted && reply.mode.needs_password() && req.pake.is_empty() {
        app.assist.remove(&rid);
        push_cancelled(&app, &host.id, &rid);
        return Err(code_required(Some(reply.mode)));
    }
    app.assist.await_accept(&rid, reply.mode);
    tracing::info!(request_id = %rid, target = %wanted, controller = %controller_key(&auth), mode = reply.mode.as_str(), trusted = reply.trusted, "assist request answered by host");
    Ok(Json(AssistPending {
        request_id: rid,
        mode: reply.mode,
        trusted: reply.trusted,
        pake: reply.pake,
        key_confirm: reply.key_confirm,
        host: host_info(&host),
    }))
}

/// `POST /api/v2/assist/requests/{request_id}/accept` (long poll): forward
/// the controller's key confirmation and wait for the host's decision.
pub async fn accept(
    State(app): State<App>,
    auth: AnyAuth,
    Path(rid): Path<String>,
    JsonBody(req): JsonBody<AssistAcceptRequest>,
) -> ApiResult<Json<AssistJoin>> {
    if req.key_confirm.len() > MAX_PAKE_TEXT {
        return Err(ApiError::bad_request("invalid key_confirm"));
    }
    let wait = Duration::from_millis(app.cfg.limits.assist_decision_wait_ms);
    let mode = app.assist.mode(&rid);
    let Some((parties, rx)) = app
        .assist
        .begin_decision(&rid, &controller_key(&auth), &auth.device.id, wait)
    else {
        return Err(ApiError::not_found());
    };
    let pushed = app.hub.push(
        &parties.host_device_id,
        &Envelope::new(
            types::ASSIST_ACCEPT,
            AssistAcceptPush {
                request_id: rid.clone(),
                key_confirm: req.key_confirm,
            },
        ),
    );
    if !pushed {
        app.assist.remove(&rid);
        return Err(ApiError::device_offline());
    }
    let outcome = tokio::time::timeout(wait, rx).await;
    app.assist.remove(&rid);
    match outcome {
        Ok(Ok(DecisionOutcome::Decision(AssistDecision {
            allowed: true,
            trusted,
            ..
        }))) => {
            match start_assist_session(&app, &auth, &parties, trusted).await {
                Ok(join) => {
                    // A successful decision: the attempt was not a failure.
                    app.limiter.refund(&target_key(&parties.target));
                    app.limiter.clear(&failure_key(&auth, &parties.target));
                    Ok(Json(join))
                }
                Err(e) => {
                    push_cancelled(&app, &parties.host_device_id, &rid);
                    Err(e)
                }
            }
        }
        Ok(Ok(DecisionOutcome::Decision(d))) => {
            tracing::info!(request_id = %rid, target = %parties.target, controller = %controller_key(&auth), refusal = ?d.refusal, "assist request refused");
            Err(refusal_error(d.refusal, mode))
        }
        Ok(Ok(DecisionOutcome::Cancelled)) => {
            Err(ApiError::new(ErrorCode::Conflict, "请求已取消"))
        }
        _ => {
            push_cancelled(&app, &parties.host_device_id, &rid);
            Err(ApiError::timeout())
        }
    }
}

async fn start_assist_session(
    app: &App,
    auth: &AnyAuth,
    parties: &Parties,
    trusted: bool,
) -> ApiResult<AssistJoin> {
    let hid = parties.host_device_id.clone();
    let host = app
        .db
        .call(move |c| db::get_device(c, &hid))
        .await?
        .ok_or_else(ApiError::device_not_found)?;
    // The host decided; a trusted device gets what the host allows it
    // (applied by the host), a visitor the host's visitor permissions.
    // Neither displaces an existing session: it takes a free participant
    // slot or fails with `busy`.
    let relation = if trusted {
        Relation::Trusted
    } else {
        Relation::Assist
    };
    let (sid, ticket) = app.hub.create_session(SessionSpec {
        host_device_id: host.id.clone(),
        host_public_key: host.public_key.clone(),
        controller_device_id: auth.device.id.clone(),
        controller_auth: auth.auth.clone(),
        controller: device_controller_info(&app, &auth.device, auth.user.as_ref()),
        purpose: Purpose::Desktop,
        connect_type: ConnectType::Assist,
        relation,
        grants: Vec::new(),
        max_participants: host.max_participants(),
        presence: false,
    })?;
    if let (Some(user), false) = (&auth.user, parties.connect_id.is_empty()) {
        let (uid, cid, hid) = (user.id.clone(), parties.connect_id.clone(), host.id.clone());
        app.db
            .call(move |c| db::record_recent(c, &uid, &cid, &hid))
            .await?;
    }
    Ok(AssistJoin {
        session: session_ticket(app, sid, ticket, &host, relation, Vec::new()),
        host: host_info(&host),
    })
}

/// `POST /api/v2/assist/requests/{request_id}/cancel` (controller).
pub async fn cancel(
    State(app): State<App>,
    auth: AnyAuth,
    Path(rid): Path<String>,
) -> ApiResult<Json<Empty>> {
    if let Some(parties) = app.assist.cancel(&rid, &controller_key(&auth), &auth.device.id) {
        push_cancelled(&app, &parties.host_device_id, &rid);
    }
    Ok(Json(Empty {}))
}

/// `POST /api/v2/assist/identity`
pub async fn identity(State(app): State<App>, auth: AnyAuth) -> ApiResult<Json<AssistIdentity>> {
    let id = auth.device.id.clone();
    let connect_id = app.db.call(move |c| db::ensure_connect_id(c, &id)).await?;
    Ok(Json(AssistIdentity { connect_id }))
}

fn reply_result(r: Result<(), ReplyError>) -> ApiResult<Json<Empty>> {
    match r {
        Ok(()) => Ok(Json(Empty {})),
        Err(ReplyError::NotFound) => Err(ApiError::not_found()),
        Err(ReplyError::WrongStage) => Err(ErrorCode::Conflict.into()),
    }
}

/// `POST /api/v2/assist/requests/{request_id}/pake` (host).
pub async fn reply_pake(
    State(app): State<App>,
    auth: AnyAuth,
    Path(rid): Path<String>,
    JsonBody(r): JsonBody<AssistPakeReply>,
) -> ApiResult<Json<Empty>> {
    if !pake_list_ok(&r.pake, ASSIST_PAKE_SLOTS) || !pake_list_ok(&r.key_confirm, ASSIST_PAKE_SLOTS)
    {
        return Err(ApiError::bad_request("invalid pake reply"));
    }
    reply_result(app.assist.reply_pake(&rid, &auth.device.id, r))
}

/// `POST /api/v2/assist/requests/{request_id}/decision` (host).
pub async fn reply_decision(
    State(app): State<App>,
    auth: AnyAuth,
    Path(rid): Path<String>,
    JsonBody(r): JsonBody<AssistDecision>,
) -> ApiResult<Json<Empty>> {
    reply_result(app.assist.reply_decision(&rid, &auth.device.id, r))
}

fn to_saved(rows: Vec<db::SavedRow>) -> SavedList {
    SavedList {
        saved: rows
            .into_iter()
            .map(|r| SavedDevice {
                publisher_device_id: r.publisher_device_id,
                connect_id: r.connect_id,
                remark: r.remark,
                is_favorite: r.is_favorite,
                last_connected_at: r.last_connected_at,
                favorited_at: r.favorited_at,
                public_key: r.public_key,
            })
            .collect(),
    }
}

/// `GET /api/v2/assist/saved/{recent|favorites}`
pub async fn saved_list(
    State(app): State<App>,
    auth: AccountAuth,
    Path(kind): Path<String>,
) -> ApiResult<Json<SavedList>> {
    let uid = auth.user.id.clone();
    let rows = match kind.as_str() {
        "recent" => app.db.call(move |c| db::list_recent(c, &uid)).await?,
        "favorites" => app.db.call(move |c| db::list_favorites(c, &uid)).await?,
        _ => return Err(ApiError::not_found()),
    };
    Ok(Json(to_saved(rows)))
}

/// `PUT /api/v2/assist/saved/favorites`
pub async fn saved_put(
    State(app): State<App>,
    auth: AccountAuth,
    Path(kind): Path<String>,
    JsonBody(req): JsonBody<FavoritesRequest>,
) -> ApiResult<Json<SavedList>> {
    if kind != "favorites" {
        return Err(ApiError::not_found());
    }
    if req.favorites.len() > 500 {
        return Err(ApiError::bad_request("too many favorites"));
    }
    if req
        .favorites
        .iter()
        .any(|f| !ids::is_connect_id(&f.connect_id))
    {
        return Err(ApiError::bad_request("invalid connect_id"));
    }
    let uid = auth.user.id.clone();
    let rows = app
        .db
        .call(move |c| {
            let mut items = Vec::new();
            for f in &req.favorites {
                let device_id = db::get_device_by_connect_id(c, &f.connect_id)?
                    .map(|d| d.id)
                    .unwrap_or_default();
                items.push((
                    f.connect_id.clone(),
                    device_id,
                    db::clip(&f.remark, 128),
                    f.favorited_at,
                ));
            }
            db::replace_favorites(c, &uid, &items)?;
            db::list_favorites(c, &uid)
        })
        .await?;
    Ok(Json(to_saved(rows)))
}

/// `DELETE /api/v2/assist/saved/recent`
pub async fn saved_clear(
    State(app): State<App>,
    auth: AccountAuth,
    Path(kind): Path<String>,
) -> ApiResult<Json<Empty>> {
    if kind != "recent" {
        return Err(ApiError::not_found());
    }
    let uid = auth.user.id.clone();
    app.db.call(move |c| db::clear_recent(c, &uid)).await?;
    Ok(Json(Empty {}))
}

/// `DELETE /api/v2/assist/saved/{kind}/{device_id}`
pub async fn saved_delete(
    State(app): State<App>,
    auth: AccountAuth,
    Path((kind, device_id)): Path<(String, String)>,
) -> ApiResult<Json<Empty>> {
    if kind != "recent" && kind != "favorites" {
        return Err(ApiError::not_found());
    }
    let uid = auth.user.id.clone();
    app.db
        .call(move |c| db::delete_saved(c, &uid, &kind, &device_id))
        .await?;
    Ok(Json(Empty {}))
}
