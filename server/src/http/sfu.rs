//! Multi-viewer forwarding (`docs/protocol/deployment-design.md` §5): hosts
//! open rooms for the sessions they host, those sessions' controllers
//! subscribe. The descriptions pass through; the forwarder runs here or on
//! an edge node.

use super::auth::AnyAuth;
use super::{App, JsonBody};
use crate::error::{ApiError, ApiResult};
use crate::forwarding::ForwardError;
use axum::Json;
use axum::extract::{Path, State};
use farwing_protocol::ErrorCode;
use farwing_protocol::api::{
    CreateRoomRequest, Empty, RoomCreated, RoomUpdated, SubscribeRequest, Subscription,
    UpdateRoomRequest,
};
use farwing_protocol::error::reason;

/// Longest description accepted.
const SDP_MAX: usize = 64 * 1024;
/// Most sessions in one room.
const SESSIONS_MAX: usize = 64;

impl From<ForwardError> for ApiError {
    fn from(e: ForwardError) -> Self {
        match e {
            ForwardError::Disabled => ApiError::reason(
                ErrorCode::Forbidden,
                reason::FORWARDING_DISABLED,
                "forwarding is off",
            ),
            ForwardError::ModeNotAllowed => ApiError::reason(
                ErrorCode::Forbidden,
                reason::FORWARDING_MODE_NOT_ALLOWED,
                "compatible forwarding is not allowed",
            ),
            ForwardError::NotFound => ApiError::not_found(),
            ForwardError::Unavailable(detail) => {
                tracing::warn!(%detail, "forwarder failed");
                ApiError::reason(
                    ErrorCode::Unavailable,
                    reason::FORWARDING_UNAVAILABLE,
                    "the forwarder is unavailable",
                )
            }
        }
    }
}

fn check_sdp(sdp: &str) -> ApiResult<()> {
    if sdp.is_empty() || sdp.len() > SDP_MAX {
        return Err(ApiError::new(ErrorCode::BadRequest, "invalid description"));
    }
    Ok(())
}

fn check_sessions(sessions: &[String]) -> ApiResult<()> {
    if sessions.is_empty() || sessions.len() > SESSIONS_MAX {
        return Err(ApiError::new(ErrorCode::BadRequest, "invalid sessions"));
    }
    Ok(())
}

/// `POST /api/v2/sfu/rooms` (host).
pub async fn create(
    State(app): State<App>,
    auth: AnyAuth,
    JsonBody(req): JsonBody<CreateRoomRequest>,
) -> ApiResult<Json<RoomCreated>> {
    check_sdp(&req.offer)?;
    check_sessions(&req.sessions)?;
    let (room, answer) = app
        .forwarding
        .create(
            &app.cfg(),
            &app.hub,
            &auth.device.id,
            &req.sessions,
            req.mode,
            &req.offer,
        )
        .await?;
    Ok(Json(RoomCreated {
        room: room.id,
        mode: room.mode,
        answer,
        node: room.node.map(|n| n.1),
    }))
}

/// `PUT /api/v2/sfu/rooms/{room}` (host).
pub async fn update(
    State(app): State<App>,
    auth: AnyAuth,
    Path(room): Path<String>,
    JsonBody(req): JsonBody<UpdateRoomRequest>,
) -> ApiResult<Json<RoomUpdated>> {
    if let Some(offer) = &req.offer {
        check_sdp(offer)?;
    }
    if let Some(sessions) = &req.sessions {
        check_sessions(sessions)?;
    }
    let answer = app
        .forwarding
        .update(
            &app.hub,
            &auth.device.id,
            &room,
            req.sessions.as_deref(),
            req.offer.as_deref(),
        )
        .await?;
    Ok(Json(RoomUpdated { answer }))
}

/// `DELETE /api/v2/sfu/rooms/{room}` (host).
pub async fn close(
    State(app): State<App>,
    auth: AnyAuth,
    Path(room): Path<String>,
) -> ApiResult<Json<Empty>> {
    if !app.forwarding.close(Some(&auth.device.id), &room).await {
        return Err(ApiError::not_found());
    }
    Ok(Json(Empty {}))
}

/// `POST /api/v2/sfu/rooms/{room}/subscribe` (the session's controller).
pub async fn subscribe(
    State(app): State<App>,
    auth: AnyAuth,
    Path(room): Path<String>,
    JsonBody(req): JsonBody<SubscribeRequest>,
) -> ApiResult<Json<Subscription>> {
    check_sdp(&req.offer)?;
    if req.tracks.as_ref().is_some_and(|t| t.len() > 16) {
        return Err(ApiError::new(ErrorCode::BadRequest, "invalid tracks"));
    }
    // The payer's policy may keep this viewer off forwarding.
    if !app.egress.access(&req.session).forward {
        return Err(ApiError::new(ErrorCode::Forbidden, "this session may not receive through a forwarder"));
    }
    let (answer, mode) = app
        .forwarding
        .subscribe(
            &app.hub,
            &auth.device.id,
            &room,
            &req.session,
            req.connection,
            &req.offer,
            req.tracks,
            req.layer,
        )
        .await?;
    Ok(Json(Subscription { answer, mode }))
}

/// `PUT /api/v2/sfu/rooms/{room}/subscribe/{session}/layer` (the session's
/// controller).
pub async fn layer(
    State(app): State<App>,
    auth: AnyAuth,
    Path((room, session)): Path<(String, String)>,
    JsonBody(req): JsonBody<farwing_protocol::api::LayerRequest>,
) -> ApiResult<Json<Empty>> {
    app.forwarding
        .choose_layer(&app.hub, &auth.device.id, &room, &session, req.layer)
        .await?;
    Ok(Json(Empty {}))
}

/// `DELETE /api/v2/sfu/rooms/{room}/subscribe/{session}/{connection}` (the
/// session's controller or the host).
pub async fn unsubscribe(
    State(app): State<App>,
    auth: AnyAuth,
    Path((room, session, connection)): Path<(String, String, u32)>,
) -> ApiResult<Json<Empty>> {
    if !app
        .forwarding
        .unsubscribe(&app.hub, &auth.device.id, &room, &session, connection)
        .await
    {
        return Err(ApiError::not_found());
    }
    Ok(Json(Empty {}))
}
