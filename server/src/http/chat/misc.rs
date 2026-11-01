//! The chat connection's ticket, the snapshot over HTTP, personal chat
//! settings, status and media tokens (`docs/protocol/chat-design.md` §6).

use crate::chat::{media, push, store};
use crate::error::{ApiError, ApiResult};
use crate::http::auth::AccountAuth;
use crate::http::{App, JsonBody};
use axum::Json;
use axum::extract::State;
use farwing_protocol::api::{Empty, PresenceTicket};
use farwing_protocol::chat::{MediaToken, Prefs, Snapshot, Status, StatusBody, types};
use farwing_protocol::realtime::Envelope;
use farwing_protocol::timing;

/// `POST /chat/ticket`: a ticket for `GET /ws` as a chat connection.
pub async fn ticket(State(app): State<App>, auth: AccountAuth) -> ApiResult<Json<PresenceTicket>> {
    let ticket = app.hub.issue_chat_ticket(&auth.device.id, auth.link_auth());
    Ok(Json(PresenceTicket {
        ticket,
        endpoints: app.endpoints(),
        connect_timeout_ms: timing::CONNECT_TIMEOUT_MS,
        retry_delay_ms: timing::RETRY_DELAY_MS,
        max_reconnect_delay_s: timing::MAX_RECONNECT_DELAY_S,
    }))
}

/// `GET /chat/state`: what `chat.hello` answers, over HTTP.
pub async fn state(State(app): State<App>, auth: AccountAuth) -> ApiResult<Json<Snapshot>> {
    let u = auth.user.id.clone();
    let limits = crate::chat::conn::chat_limits(&app);
    let (mut snap, audience) = app.db.call(move |c| crate::chat::snapshot(c, &u, limits)).await?;
    crate::chat::fill_live(&app, &auth.user.id, &mut snap);
    let online = app.chat.hub.online();
    let wanted: Vec<String> = audience.into_iter().filter(|u| online.contains(u)).collect();
    let w = wanted.clone();
    let chosen = app
        .db
        .call(move |c| w.iter().map(|u| store::status(c, u)).collect::<Result<Vec<_>, _>>())
        .await?;
    snap.presences = wanted
        .into_iter()
        .zip(chosen)
        .filter_map(|(u, chosen)| {
            let s = crate::chat::effective_status(chosen, app.chat.hub.activity(&u));
            (s != Status::Offline).then_some(farwing_protocol::chat::Presence { user_id: u, status: s })
        })
        .collect();
    Ok(Json(snap))
}

/// `GET /chat/prefs`
pub async fn prefs(State(app): State<App>, auth: AccountAuth) -> ApiResult<Json<Prefs>> {
    let u = auth.user.id.clone();
    Ok(Json(app.db.call(move |c| store::prefs(c, &u)).await?))
}

fn valid_time(t: &str) -> bool {
    t.is_empty()
        || t.split_once(':').is_some_and(|(h, m)| {
            h.len() == 2
                && m.len() == 2
                && h.parse::<u8>().is_ok_and(|h| h < 24)
                && m.parse::<u8>().is_ok_and(|m| m < 60)
        })
}

/// `PUT /chat/prefs`: the whole settings; the account's other devices
/// follow (`chat.prefs`).
pub async fn put_prefs(State(app): State<App>, auth: AccountAuth, JsonBody(prefs): JsonBody<Prefs>) -> ApiResult<Json<Prefs>> {
    if prefs.notify.len() > 2000
        || prefs.notify.keys().any(|k| {
            !k.split_once(':')
                .is_some_and(|(kind, id)| (kind == "g" || kind == "c") && id.parse::<i64>().is_ok())
        })
    {
        return Err(ApiError::bad_request("notify keys are g:<id> or c:<id>"));
    }
    if !valid_time(&prefs.quiet_start) || !valid_time(&prefs.quiet_end) {
        return Err(ApiError::bad_request("quiet hours are HH:MM"));
    }
    let u = auth.user.id.clone();
    let p = prefs.clone();
    app.db.call(move |c| store::set_prefs(c, &u, &p)).await?;
    app.chat
        .hub
        .send_user(&auth.user.id, Some(&auth.device.id), &Envelope::new(types::PREFS, &prefs));
    Ok(Json(prefs))
}

/// `PUT /me/status {status}`: online, idle, dnd or invisible.
pub async fn put_status(State(app): State<App>, auth: AccountAuth, JsonBody(req): JsonBody<StatusBody>) -> ApiResult<Json<Empty>> {
    if req.status == Status::Offline {
        return Err(ApiError::bad_request("status is online, idle, dnd or invisible"));
    }
    let u = auth.user.id.clone();
    app.db.call(move |c| store::set_status(c, &u, req.status)).await?;
    push::presence(&app, &auth.user.id).await;
    Ok(Json(Empty {}))
}

/// `POST /chat/media-token`: a token for `/media/...` (a day).
pub async fn media_token(State(app): State<App>, auth: AccountAuth) -> ApiResult<Json<MediaToken>> {
    let expires_at = crate::db::now_s() + media::TOKEN_TTL_S;
    Ok(Json(MediaToken {
        token: app.chat.media.token(&auth.user.id, expires_at),
        expires_at,
    }))
}
