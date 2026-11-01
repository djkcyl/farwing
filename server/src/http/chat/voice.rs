//! Voice channels over HTTP (`docs/protocol/chat-design.md` §7, §10.1).

use super::id;
use crate::chat::voice;
use crate::error::{ApiError, ApiResult};
use crate::http::auth::AccountAuth;
use crate::http::{App, JsonBody};
use axum::Json;
use axum::extract::{Path, State};
use farwing_protocol::api::Empty;
use farwing_protocol::chat::{
    Call, CallStart, DeviceX25519, VoiceDescription, VoiceJoin, VoiceJoined, VoiceKeys, VoiceModerate, VoiceSelf, x25519_message,
};

/// `POST /chat/channels/{id}/voice/join`
pub async fn join(
    State(app): State<App>,
    auth: AccountAuth,
    Path(cid): Path<String>,
    JsonBody(req): JsonBody<VoiceJoin>,
) -> ApiResult<Json<VoiceJoined>> {
    Ok(Json(voice::join(&app, &auth.user.id, &auth.device, id(&cid)?, req).await?))
}

/// `POST /chat/channels/{id}/voice/leave`
pub async fn leave(State(app): State<App>, auth: AccountAuth, Path(cid): Path<String>) -> ApiResult<Json<Empty>> {
    voice::leave(&app, &auth.device.id, id(&cid)?, None).await;
    Ok(Json(Empty {}))
}

/// `PUT /chat/channels/{id}/voice/publish`
pub async fn publish(
    State(app): State<App>,
    auth: AccountAuth,
    Path(cid): Path<String>,
    JsonBody(req): JsonBody<VoiceDescription>,
) -> ApiResult<Json<VoiceDescription>> {
    let sdp = voice::publish(&app, &auth.user.id, &auth.device.id, id(&cid)?, req.sdp).await?;
    Ok(Json(VoiceDescription { sdp }))
}

/// `PUT /chat/channels/{id}/voice/state`
pub async fn state(
    State(app): State<App>,
    auth: AccountAuth,
    Path(cid): Path<String>,
    JsonBody(req): JsonBody<VoiceSelf>,
) -> ApiResult<Json<Empty>> {
    voice::set_self(&app, &auth.device.id, id(&cid)?, req).await?;
    Ok(Json(Empty {}))
}

/// `POST /chat/channels/{id}/voice/keys`
pub async fn keys(
    State(app): State<App>,
    auth: AccountAuth,
    Path(cid): Path<String>,
    JsonBody(req): JsonBody<VoiceKeys>,
) -> ApiResult<Json<Empty>> {
    voice::keys(&app, &auth.device.id, id(&cid)?, req).await?;
    Ok(Json(Empty {}))
}

/// `PATCH /chat/channels/{id}/voice/participants/{participant}`
pub async fn moderate(
    State(app): State<App>,
    auth: AccountAuth,
    Path((cid, participant)): Path<(String, u32)>,
    JsonBody(req): JsonBody<VoiceModerate>,
) -> ApiResult<Json<Empty>> {
    voice::moderate(&app, &auth.user.id, id(&cid)?, participant, req).await?;
    Ok(Json(Empty {}))
}

/// `DELETE /chat/channels/{id}/voice/participants/{participant}`
pub async fn disconnect(
    State(app): State<App>,
    auth: AccountAuth,
    Path((cid, participant)): Path<(String, u32)>,
) -> ApiResult<Json<Empty>> {
    voice::disconnect(&app, &auth.user.id, id(&cid)?, participant).await?;
    Ok(Json(Empty {}))
}

/// `POST /chat/dm/{channel}/call`
pub async fn call(
    State(app): State<App>,
    auth: AccountAuth,
    Path(cid): Path<String>,
    JsonBody(req): JsonBody<CallStart>,
) -> ApiResult<Json<Call>> {
    Ok(Json(crate::chat::calls::start(&app, &auth.user.id, &auth.device, id(&cid)?, req.video).await?))
}

/// `POST /chat/calls/{id}/{answer|decline|hangup|move}`
pub async fn call_action(
    State(app): State<App>,
    auth: AccountAuth,
    Path((call, action)): Path<(String, String)>,
) -> ApiResult<Json<serde_json::Value>> {
    use crate::chat::calls;
    let out = match action.as_str() {
        "answer" => serde_json::to_value(calls::answer(&app, &auth.user.id, &auth.device, &call).await?),
        "move" => serde_json::to_value(calls::take_over(&app, &auth.user.id, &auth.device, &call).await?),
        "decline" | "hangup" => {
            calls::finish(&app, &auth.user.id, &auth.device.id, &call, action == "decline").await?;
            Ok(serde_json::json!({}))
        }
        _ => return Err(ApiError::not_found()),
    };
    Ok(Json(out.unwrap_or_default()))
}

/// `PUT /devices/self/x25519`
pub async fn put_x25519(
    State(app): State<App>,
    auth: AccountAuth,
    JsonBody(req): JsonBody<DeviceX25519>,
) -> ApiResult<Json<Empty>> {
    let key = farwing_protocol::crypto::unb64(&req.public).map_err(|_| ApiError::bad_request("invalid key"))?;
    if key.len() != 32 {
        return Err(ApiError::bad_request("invalid key"));
    }
    farwing_protocol::crypto::verify(&auth.device.public_key, x25519_message(&req.public).as_bytes(), &req.signature)
        .map_err(|_| ApiError::bad_request("invalid signature"))?;
    let (d, k) = (auth.device.id.clone(), req.public.clone());
    app.db.call(move |c| crate::db::set_device_x25519(c, &d, &k)).await?;
    Ok(Json(Empty {}))
}
