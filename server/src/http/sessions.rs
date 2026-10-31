//! Realtime admission: presence tickets and session tickets.

use super::auth::{AccountAuth, AnyAuth};
use super::devices::visible_device;
use super::{App, ClientIp, JsonBody};
use crate::db::{self, DeviceRow};
use crate::error::{ApiError, ApiResult};
use crate::hub::SessionSpec;
use axum::Json;
use axum::extract::{Path, State};
use farwing_protocol::api::{
    CreateSessionRequest, PresenceTicket, Purpose, Relation, SessionTarget, SessionTicket,
};
use farwing_protocol::realtime::ControllerInfo;
use crate::hub::ConnectType;
use farwing_protocol::error::reason;
use farwing_protocol::{ErrorCode, Platform, timing};

/// `POST /api/v2/presence/ticket` (empty body).
pub async fn presence_ticket(
    State(app): State<App>,
    auth: AnyAuth,
) -> ApiResult<Json<PresenceTicket>> {
    let ticket = app
        .hub
        .issue_presence_ticket(&auth.device.id, auth.auth.clone());
    tracing::debug!(
        device_id = %auth.device.id,
        guest = auth.auth.is_guest(),
        "presence ticket issued"
    );
    Ok(Json(PresenceTicket {
        ticket,
        endpoints: app.endpoints(),
        connect_timeout_ms: timing::CONNECT_TIMEOUT_MS,
        retry_delay_ms: timing::RETRY_DELAY_MS,
        max_reconnect_delay_s: timing::MAX_RECONNECT_DELAY_S,
    }))
}

pub fn session_ticket(
    app: &App,
    session_id: String,
    ticket: String,
    target: &DeviceRow,
    relation: Relation,
    granted: Vec<String>,
) -> SessionTicket {
    SessionTicket {
        session_id,
        ticket,
        endpoints: app.endpoints(),
        connect_timeout_ms: timing::CONNECT_TIMEOUT_MS,
        retry_delay_ms: timing::RETRY_DELAY_MS,
        target: SessionTarget {
            device_id: target.id.clone(),
            alias: target.alias.clone(),
            platform: target.platform,
            version: target.version.clone(),
            capabilities: target.capabilities.clone(),
            relation,
            granted,
            public_key: target.public_key.clone(),
        },
    }
}

pub fn controller_info(app: &App, auth: &AccountAuth) -> ControllerInfo {
    device_controller_info(app, &auth.device, Some(&auth.user))
}

/// A controller device, with its account when it has one. Browsers carry
/// the server's web limit, which hosts apply to whatever they grant.
pub fn device_controller_info(app: &App, device: &DeviceRow, user: Option<&db::UserRow>) -> ControllerInfo {
    ControllerInfo {
        caps_limit: (device.platform == Platform::Web).then(|| app.cfg().web.caps.clone()),
        device_id: device.id.clone(),
        alias: device.alias.clone(),
        user_id: user.map(|u| u.id.clone()).unwrap_or_default(),
        username: user.map(|u| u.username.clone()).unwrap_or_default(),
        nickname: user.map(|u| u.nickname.clone()).unwrap_or_default(),
        platform: device.platform,
        version: device.version.clone(),
        public_key: device.public_key.clone(),
    }
}

/// Whether the owner's policy admits sessions of `purpose`. `control` is the
/// host's "accept sessions at all" switch: hosts refuse every purpose without it.
pub fn policy_allows(target: &DeviceRow, purpose: Purpose) -> bool {
    let p = &target.policy;
    target.host_supported()
        && p.control
        && match purpose {
            Purpose::Desktop | Purpose::Audio | Purpose::Diagnostics => true,
            Purpose::Files => p.files,
            Purpose::Ports => p.ports,
            Purpose::Camera => p.camera,
        }
}

/// `POST /api/v2/sessions`: own devices, or friends' devices whose grants
/// include the capability the purpose requires.
pub async fn create(
    State(app): State<App>,
    auth: AccountAuth,
    ClientIp(ip): ClientIp,
    JsonBody(req): JsonBody<CreateSessionRequest>,
) -> ApiResult<Json<SessionTicket>> {
    let target = visible_device(&app, &auth, &req.target_device_id).await?;
    if target.row.id == auth.device.id {
        return Err(ApiError::reason(
            ErrorCode::BadRequest,
            reason::SELF_CONNECT,
            "a device cannot control itself",
        ));
    }
    if !target
        .granted
        .iter()
        .any(|c| c == req.purpose.required_cap())
    {
        return Err(ApiError::reason(
            ErrorCode::Forbidden,
            reason::NOT_GRANTED,
            "the grants do not include this purpose",
        ));
    }
    if !app.hub.is_online(&target.row.id) {
        return Err(ApiError::device_offline());
    }
    if !policy_allows(&target.row, req.purpose) {
        let e = ApiError::from(ErrorCode::HostDisabled);
        let p = &target.row.policy;
        return Err(match req.purpose {
            Purpose::Files if target.row.host_supported() && p.control => {
                ApiError::reason(e.code, reason::FILES_DISABLED, "host does not allow file transfer")
            }
            Purpose::Ports if target.row.host_supported() && p.control => {
                ApiError::reason(e.code, reason::PORTS_DISABLED, "host does not allow port forwarding")
            }
            Purpose::Camera if target.row.host_supported() && p.control => {
                ApiError::reason(e.code, reason::CAMERA_DISABLED, "host does not allow its cameras to be seen")
            }
            _ => e,
        });
    }
    // A browser gets no more than the server allows browsers.
    let controller = controller_info(&app, &auth);
    let mut target = target;
    target.granted = controller.limit_grants(std::mem::take(&mut target.granted));
    if !target.granted.iter().any(|c| c == req.purpose.required_cap()) {
        return Err(ApiError::reason(
            ErrorCode::Forbidden,
            reason::WEB_NOT_ALLOWED,
            "the server does not allow this to browsers",
        ));
    }
    let record = crate::records::Opened {
        visit: req.visit.clone(),
        host: crate::records::host_party(&target.row),
        controller: crate::records::controller_party(&controller),
        relation: target.relation,
        purpose: req.purpose,
        ip: ip.to_string(),
        ..crate::records::Opened::default()
    };
    let (sid, ticket) = app.hub.create_session(SessionSpec {
        host_device_id: target.row.id.clone(),
        host_public_key: target.row.public_key.clone(),
        controller_device_id: auth.device.id.clone(),
        controller_auth: auth.link_auth(),
        controller,
        host_owner: target.row.user_id.clone(),
        purpose: req.purpose,
        connect_type: ConnectType::Account,
        relation: target.relation,
        grants: target.granted.clone(),
        max_participants: target.row.max_participants(),
        presence: req.presence,
        record,
    })?;
    Ok(Json(session_ticket(
        &app,
        sid,
        ticket,
        &target.row,
        target.relation,
        target.granted,
    )))
}

/// `POST /api/v2/sessions/{session_id}/ticket` (the session's controller,
/// with or without an account).
pub async fn refresh_ticket(
    State(app): State<App>,
    auth: AnyAuth,
    Path(session_id): Path<String>,
) -> ApiResult<Json<SessionTicket>> {
    let (view, ticket) =
        app.hub
            .refresh_session_ticket(&session_id, &auth.device.id, auth.auth.clone())?;
    let host = view.host_device_id.clone();
    let target = app
        .db
        .call(move |c| db::get_device(c, &host))
        .await?
        .ok_or_else(ApiError::not_found)?;
    Ok(Json(session_ticket(
        &app,
        view.id,
        ticket,
        &target,
        view.relation,
        view.grants,
    )))
}
