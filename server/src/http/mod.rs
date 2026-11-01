//! HTTPS control plane (`/api/v2/...`) and the realtime upgrade route.

pub mod account;
pub mod admin_api;
pub mod chat;
pub mod assist;
pub mod auth;
pub mod devices;
pub mod friends;
pub mod me;
pub mod misc;
pub mod notices;
pub mod nodes;
pub mod power;
pub mod records;
pub mod sessions;
pub mod settings_api;
pub mod sfu;
pub mod setup;
pub mod tokens;
pub mod web_static;

use crate::assist::AssistRegistry;
use crate::config::Config;
use crate::db::Db;
use crate::error::{ApiError, ApiResult};
use crate::hub::Hub;
use crate::ratelimit::WindowLimiter;
use axum::Router;
use axum::extract::{ConnectInfo, FromRequest, FromRequestParts, Request, State};
use axum::http::request::Parts;
use axum::http::{HeaderMap, HeaderValue};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, patch, post, put};
use farwing_protocol::api::path;
use farwing_protocol::{ErrorCode, headers};
use ipnet::IpNet;
use serde::de::DeserializeOwned;
use serde_json::json;
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::time::Instant;

pub struct AppState {
    /// The settings in force (`AppState::cfg`).
    pub settings: crate::settings::Settings,
    pub db: Db,
    pub hub: Arc<Hub>,
    pub assist: AssistRegistry,
    pub limiter: WindowLimiter,
    /// Request signatures already used (replay protection).
    pub replay: auth::ReplayCache,
    /// Bounds concurrent Argon2 verifications (memory and CPU heavy).
    pub password_slots: tokio::sync::Semaphore,
    /// `stun:` URLs of the NAT discovery endpoints.
    pub nat_discovery: parking_lot::RwLock<Vec<String>>,
    /// Bound addresses of the builtin relay and discovery endpoints.
    pub relay: parking_lot::RwLock<crate::runtime::RelayAddrs>,
    pub started_at: Instant,
    /// The certificate presented by the TLS listener (`None` behind a
    /// TLS-terminating proxy).
    pub tls: parking_lot::RwLock<Option<crate::tls::Presented>>,
    /// No administrator exists yet: only the setup is open.
    pub setup_pending: std::sync::atomic::AtomicBool,
    /// A listen change waiting for confirmation.
    pub listen_pending: parking_lot::RwLock<Option<crate::runtime::ListenPending>>,
    /// Edge relay nodes.
    pub edges: Arc<crate::edge::EdgeHub>,
    /// Multi-viewer forwarding rooms.
    pub forwarding: Arc<crate::forwarding::Forwarding>,
    /// What the relays and forwarders send for sessions, and who pays.
    pub egress: Arc<crate::egress::Egress>,
    /// What this server's own relay carries (refreshed every ten seconds).
    pub local_relay: parking_lot::RwLock<crate::turn_server::RelayStats>,
    /// The task that applies settings (`crate::runtime`).
    pub control: std::sync::OnceLock<crate::runtime::Control>,
    /// What notifications watch for (`crate::notify`).
    pub watch: crate::notify::Watch,
    /// Chat connections and media tokens (`crate::chat`).
    pub chat: crate::chat::Chat,
}

impl AppState {
    /// The settings in force.
    pub fn cfg(&self) -> Arc<Config> {
        self.settings.get()
    }

    /// Path of `public_url` (e.g. `/farwing`), empty at the root. Signatures
    /// over the prefixed path are accepted behind prefix-stripping proxies.
    pub fn path_prefix(&self) -> String {
        self.cfg().public_path_prefix()
    }

    pub fn endpoints(&self) -> Vec<String> {
        vec![self.cfg().ws_endpoint()]
    }

    pub fn setup_pending(&self) -> bool {
        self.setup_pending.load(std::sync::atomic::Ordering::Acquire)
    }

    pub fn control(&self) -> &crate::runtime::Control {
        self.control.get().expect("runtime started")
    }
}

pub type App = Arc<AppState>;

/// Client address after applying `trusted_proxies`.
#[derive(Clone, Copy, Debug)]
pub struct ClientIp(pub IpAddr);

impl<S: Send + Sync> FromRequestParts<S> for ClientIp {
    type Rejection = ApiError;
    async fn from_request_parts(parts: &mut Parts, _: &S) -> Result<Self, Self::Rejection> {
        parts
            .extensions
            .get::<ClientIp>()
            .copied()
            .ok_or_else(ApiError::internal)
    }
}

fn canonical(ip: IpAddr) -> IpAddr {
    match ip {
        IpAddr::V6(v6) => v6
            .to_ipv4_mapped()
            .map(IpAddr::V4)
            .unwrap_or(IpAddr::V6(v6)),
        v4 => v4,
    }
}

/// Resolve the client IP: the right-most `X-Forwarded-For` address that is not
/// a trusted proxy, when the direct peer is a trusted proxy.
pub fn client_ip(peer: IpAddr, headers: &HeaderMap, trusted: &[IpNet]) -> IpAddr {
    let peer = canonical(peer);
    let is_trusted = |ip: &IpAddr| trusted.iter().any(|n| n.contains(ip));
    if !is_trusted(&peer) {
        return peer;
    }
    // From the right: an entry that does not read as an address (a port,
    // garbage) ends the chain at the last trusted hop, so nothing left of
    // it can be trusted.
    let chain: Vec<Option<IpAddr>> = headers
        .get_all("x-forwarded-for")
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(','))
        .map(|s| s.trim().parse::<IpAddr>().ok().map(canonical))
        .collect();
    let mut ip = peer;
    for addr in chain.iter().rev() {
        let Some(addr) = addr else {
            return ip;
        };
        ip = *addr;
        if !is_trusted(addr) {
            return ip;
        }
    }
    ip
}

/// JSON body (≤ 64 KiB) with protocol error mapping.
pub struct JsonBody<T>(pub T);

impl<T: DeserializeOwned, S: Send + Sync> FromRequest<S> for JsonBody<T> {
    type Rejection = ApiError;
    async fn from_request(req: Request, _: &S) -> Result<Self, Self::Rejection> {
        let bytes = read_body(req, farwing_protocol::limits::MAX_JSON_BODY).await?;
        serde_json::from_slice(&bytes)
            .map(JsonBody)
            .map_err(|e| ApiError::bad_request(format!("invalid JSON body: {e}")))
    }
}

/// Optional JSON body: empty bodies yield `None`.
pub struct OptionalJson<T>(pub Option<T>);

impl<T: DeserializeOwned, S: Send + Sync> FromRequest<S> for OptionalJson<T> {
    type Rejection = ApiError;
    async fn from_request(req: Request, _: &S) -> Result<Self, Self::Rejection> {
        let bytes = read_body(req, farwing_protocol::limits::MAX_JSON_BODY).await?;
        if bytes.iter().all(u8::is_ascii_whitespace) {
            return Ok(OptionalJson(None));
        }
        serde_json::from_slice(&bytes)
            .map(|v| OptionalJson(Some(v)))
            .map_err(|e| ApiError::bad_request(format!("invalid JSON body: {e}")))
    }
}

/// Upper bound of an oversized body that is still read (and discarded) so the
/// client receives the `payload_too_large` response instead of a reset.
const DRAIN_CAP: usize = 16 * 1024 * 1024;

async fn drain(mut body: axum::body::Body) {
    use http_body_util::BodyExt;
    let work = async {
        let mut seen = 0usize;
        while seen < DRAIN_CAP {
            match body.frame().await {
                Some(Ok(f)) => seen += f.data_ref().map_or(0, |d| d.len()),
                _ => break,
            }
        }
    };
    let _ = tokio::time::timeout(std::time::Duration::from_secs(10), work).await;
}

/// Read a request body of at most `limit` bytes (`payload_too_large` beyond).
pub async fn read_body(req: Request, limit: usize) -> ApiResult<bytes::Bytes> {
    let (parts, body) = req.into_parts();
    read_body_from(&parts.headers, body, limit).await
}

async fn read_body_from(
    headers: &HeaderMap,
    mut body: axum::body::Body,
    limit: usize,
) -> ApiResult<bytes::Bytes> {
    use http_body_util::BodyExt;
    let declared = headers
        .get(axum::http::header::CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<u64>().ok());
    if declared.is_some_and(|len| len > limit as u64) {
        if declared.is_some_and(|len| len <= DRAIN_CAP as u64) {
            drain(body).await;
        }
        return Err(ApiError::payload_too_large());
    }
    let mut buf = bytes::BytesMut::new();
    while let Some(frame) = body.frame().await {
        let frame = frame.map_err(|_| ApiError::bad_request("failed to read request body"))?;
        if let Ok(data) = frame.into_data() {
            if buf.len() + data.len() > limit {
                drain(body).await;
                return Err(ApiError::payload_too_large());
            }
            buf.extend_from_slice(&data);
        }
    }
    Ok(buf.freeze())
}

fn valid_request_id(v: &str) -> bool {
    !v.is_empty()
        && v.len() <= 64
        && v.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

/// Largest body accepted on `path` (wallpapers are raw images, the rest JSON).
fn body_limit(path: &str) -> usize {
    if path == path::SELF_WALLPAPER {
        farwing_protocol::limits::MAX_WALLPAPER_BYTES
    } else if path == "/api/v2/me/avatar" || (path.starts_with("/api/v2/chat/groups/") && path.ends_with("/icon")) {
        farwing_protocol::chat::limits::MAX_IMAGE_BYTES
    } else {
        farwing_protocol::limits::MAX_JSON_BODY
    }
}

/// Buffer the body of a signed request: the signature covers its SHA-256,
/// so the exact bytes are kept in [`auth::RawBody`] and handed on unchanged.
async fn buffer_signed_body(req: Request) -> ApiResult<Request> {
    if !auth::is_signed(req.headers()) {
        return Ok(req);
    }
    let limit = body_limit(req.uri().path());
    let (mut parts, body) = req.into_parts();
    let bytes = read_body_from(&parts.headers, body, limit).await?;
    parts.extensions.insert(auth::RawBody(bytes.clone()));
    Ok(Request::from_parts(parts, axum::body::Body::from(bytes)))
}

/// Request ID, client IP, protocol version check, signed-body buffering,
/// response headers and access log.
async fn common_layer(State(app): State<App>, mut req: Request, next: Next) -> Response {
    let started = Instant::now();
    let request_id = req
        .headers()
        .get(headers::REQUEST_ID)
        .and_then(|v| v.to_str().ok())
        .filter(|v| valid_request_id(v))
        .map(str::to_owned)
        .unwrap_or_else(crate::crypto::new_uuid);
    let peer = req
        .extensions()
        .get::<ConnectInfo<SocketAddr>>()
        .map(|c| c.0.ip())
        .unwrap_or(IpAddr::from([127, 0, 0, 1]));
    let ip = client_ip(peer, req.headers(), &app.cfg().trusted_proxies);
    req.extensions_mut().insert(ClientIp(ip));
    let method = req.method().clone();
    let path = req.uri().path().to_owned();

    // A browser cannot set headers on a WebSocket; it offers the protocol
    // as a subprotocol instead (see `crate::ws::BROWSER_PROTOCOL`).
    let browser_ws = path == farwing_protocol::WS_PATH
        && req
            .headers()
            .get_all("sec-websocket-protocol")
            .iter()
            .filter_map(|v| v.to_str().ok())
            .flat_map(|v| v.split(','))
            .any(|p| p.trim() == crate::ws::BROWSER_PROTOCOL);
    let version_error = if path.starts_with("/api/") && path != path::SERVER && !browser_ws {
        check_protocol(req.headers()).err()
    } else {
        None
    };
    // Until the setup, only the setup and the server description answer.
    let setup_error = (app.setup_pending()
        && path.starts_with("/api/")
        && path != path::SERVER
        && !path.starts_with(setup::PATH))
        .then(|| {
            ApiError::new(ErrorCode::SetupRequired, "this server has not been set up").into_response()
        });
    let mut resp = match version_error.map(IntoResponse::into_response).or(setup_error) {
        Some(e) => e,
        None => match buffer_signed_body(req).await {
            Ok(req) => next.run(req).await,
            Err(e) => e.into_response(),
        },
    };
    let h = resp.headers_mut();
    h.insert(
        headers::PROTOCOL,
        HeaderValue::from(farwing_protocol::PROTOCOL_VERSION),
    );
    if let Ok(v) = HeaderValue::from_str(&request_id) {
        h.insert(headers::REQUEST_ID, v);
    }
    // API answers carry personal and administrative data: never cached.
    if path.starts_with("/api/") {
        if !h.contains_key(axum::http::header::CACHE_CONTROL) {
            h.insert(axum::http::header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
        }
        h.insert(
            axum::http::header::X_CONTENT_TYPE_OPTIONS,
            HeaderValue::from_static("nosniff"),
        );
    }
    let status = resp.status().as_u16();
    let ms = started.elapsed().as_millis() as u64;
    if path == crate::http::misc::HEALTHZ {
        tracing::trace!(%method, %path, status, ms, "request");
    } else {
        tracing::info!(%method, %path, status, ms, %ip, request_id = %request_id, "request");
    }
    resp
}

fn check_protocol(h: &HeaderMap) -> ApiResult<()> {
    let Some(v) = h.get(headers::PROTOCOL) else {
        return Err(ApiError::bad_request("missing X-Farwing-Protocol header"));
    };
    let version = v.to_str().ok().and_then(|s| s.trim().parse::<u32>().ok());
    match version {
        Some(v)
            if (farwing_protocol::MIN_PROTOCOL_VERSION..=farwing_protocol::PROTOCOL_VERSION)
                .contains(&v) =>
        {
            Ok(())
        }
        _ => Err(
            ApiError::new(ErrorCode::ProtocolUnsupported, "unsupported protocol version").with_details(json!({
                "min": farwing_protocol::MIN_PROTOCOL_VERSION,
                "max": farwing_protocol::PROTOCOL_VERSION,
            })),
        ),
    }
}


async fn method_not_allowed() -> ApiError {
    ApiError::bad_request("method not allowed")
}

pub fn router(app: App) -> Router {
    let api = Router::new()
        .route(path::SERVER, get(misc::server_info))
        .route(path::DEVICE_INIT, post(devices::init))
        .route(path::LOGIN, post(account::login))
        .route(path::LOGOUT, post(account::logout))
        .route(path::AUTH_SESSIONS, get(account::auth_sessions))
        .route(
            "/api/v2/auth/sessions/{device_id}",
            delete(account::revoke_auth_session),
        )
        .route(path::ME, get(account::me).patch(me::set_profile))
        .route("/api/v2/me/security", get(me::security))
        .route(path::ACCOUNT_EVENTS, get(records::account_events))
        .route(path::NOTICES, get(notices::list))
        .route(path::NOTICES_READ, post(notices::read))
        .route(path::NOTICE_SETTINGS, get(notices::settings).put(notices::save_settings))
        .route("/api/v2/me/password", post(me::set_password))
        .route("/api/v2/me/totp/setup", post(me::totp_setup))
        .route("/api/v2/me/totp/enable", post(me::totp_enable))
        .route("/api/v2/me/totp/disable", post(me::totp_disable))
        .route("/api/v2/auth/totp", post(me::verify_login))
        .route("/api/v2/auth/register", post(me::register))
        .route("/api/v2/web/info", get(me::info))
        .route("/api/v2/admin/overview", get(admin_api::overview))
        .route(
            "/api/v2/admin/users",
            get(admin_api::users).post(admin_api::create_user),
        )
        .route(
            "/api/v2/admin/users/{id}",
            get(admin_api::user)
                .patch(admin_api::update_user)
                .delete(admin_api::delete_user),
        )
        .route("/api/v2/admin/users/{id}/password", post(admin_api::reset_password))
        .route("/api/v2/admin/users/{id}/totp/reset", post(admin_api::reset_totp))
        .route(
            "/api/v2/admin/invites",
            get(admin_api::invites).post(admin_api::create_invite),
        )
        .route("/api/v2/admin/invites/{id}", delete(admin_api::revoke_invite))
        .route("/api/v2/admin/invites/{id}/uses", get(admin_api::invite_uses))
        .route("/api/v2/admin/devices", get(admin_api::devices))
        .route("/api/v2/admin/devices/{id}", delete(admin_api::unbind_device))
        .route(
            "/api/v2/admin/devices/{id}/disconnect",
            post(admin_api::disconnect_device),
        )
        .route("/api/v2/admin/sessions", get(admin_api::sessions))
        .route("/api/v2/admin/sessions/{id}", delete(admin_api::end_session))
        .route(
            "/api/v2/admin/updates",
            get(admin_api::updates).post(admin_api::save_update),
        )
        .route(
            "/api/v2/admin/updates/{id}",
            put(admin_api::save_update).delete(admin_api::delete_update),
        )
        .route(
            "/api/v2/admin/settings",
            get(settings_api::get).put(settings_api::put),
        )
        .route(
            "/api/v2/admin/settings/listen/confirm",
            post(settings_api::confirm_listen),
        )
        .route(
            "/api/v2/admin/settings/listen/revert",
            post(settings_api::revert_listen),
        )
        .route(
            "/api/v2/admin/settings/certificate",
            put(settings_api::put_certificate),
        )
        .route("/api/v2/admin/settings/preview", post(settings_api::preview))
        .route("/api/v2/admin/settings/export", get(settings_api::export))
        .route("/api/v2/admin/settings/import", post(settings_api::import))
        .route(
            "/api/v2/admin/settings/relay-secret",
            post(settings_api::rotate_relay_secret),
        )
        .route(
            "/api/v2/admin/tokens",
            get(tokens::list).post(tokens::create),
        )
        .route("/api/v2/admin/tokens/{id}", delete(tokens::revoke))
        .route("/api/v2/admin/nodes", get(nodes::list).post(nodes::create))
        .route(
            "/api/v2/admin/nodes/{id}",
            patch(nodes::update).delete(nodes::remove),
        )
        .route("/api/v2/admin/nodes/{id}/rejoin", post(nodes::rejoin))
        .route("/api/v2/edge/ws", get(nodes::connect))
        .route("/api/v2/admin/sfu", get(admin_api::forwarding))
        .route(path::SFU_ROOMS, post(sfu::create))
        .route(
            "/api/v2/sfu/rooms/{room}",
            put(sfu::update).delete(sfu::close),
        )
        .route("/api/v2/sfu/rooms/{room}/subscribe", post(sfu::subscribe))
        .route(
            "/api/v2/sfu/rooms/{room}/subscribe/{session}/layer",
            axum::routing::put(sfu::layer),
        )
        .route(
            "/api/v2/sfu/rooms/{room}/subscribe/{session}/{connection}",
            delete(sfu::unsubscribe),
        )
        .route(setup::PATH, get(setup::info).post(setup::complete))
        .route("/api/v2/setup/recover", post(setup::recover))
        .route("/api/v2/admin/audit", get(admin_api::audit_log))
        .route("/api/v2/admin/records", get(records::admin_list))
        .route(path::GUEST_SESSION, post(account::guest_session))
        .route(path::DEVICES, get(devices::list))
        .route(
            "/api/v2/devices/{id}",
            patch(devices::rename).delete(devices::remove),
        )
        .route("/api/v2/devices/{id}/detail", get(devices::detail))
        .route(
            "/api/v2/devices/{id}/wallpaper",
            get(devices::get_wallpaper),
        )
        .route("/api/v2/devices/{id}/power", post(power::request))
        .route(path::SELF_POLICY, put(devices::set_policy))
        .route(path::SELF_EVENTS, post(records::device_event))
        .route(path::SELF_FRIEND_ACCESS, get(devices::friend_access))
        .route(
            path::SELF_WALLPAPER,
            put(devices::put_wallpaper),
        )
        .route(path::SELF_WOL, put(power::set_wol_info))
        .route("/api/v2/power/{request_id}/result", post(power::result))
        .route("/api/v2/updates/{platform}", get(misc::updates))
        .route(path::PRESENCE_TICKET, post(sessions::presence_ticket))
        .route(path::SESSIONS, post(sessions::create))
        .route(path::RECORDS, get(records::list))
        .route(path::RECORD_REPORTS, post(records::report))
        .route(
            "/api/v2/sessions/{session_id}/ticket",
            post(sessions::refresh_ticket),
        )
        .route(path::FRIENDS, get(friends::list))
        .route("/api/v2/friends/{user_id}", delete(friends::remove))
        .route(
            path::FRIEND_REQUESTS,
            get(friends::requests).post(friends::send_request),
        )
        .route(
            "/api/v2/friends/requests/{id}",
            delete(friends::cancel_request),
        )
        .route(
            "/api/v2/friends/requests/{id}/accept",
            post(friends::accept_request),
        )
        .route(
            "/api/v2/friends/requests/{id}/reject",
            post(friends::reject_request),
        )
        .route(
            "/api/v2/grants/{user_id}",
            get(friends::get_grants).put(friends::put_grants),
        )
        .route(path::ASSIST_JOIN, post(assist::join))
        .route(path::ASSIST_IDENTITY, post(assist::identity))
        .route(
            "/api/v2/assist/saved/{kind}",
            get(assist::saved_list)
                .put(assist::saved_put)
                .delete(assist::saved_clear),
        )
        .route(
            "/api/v2/assist/saved/{kind}/{device_id}",
            delete(assist::saved_delete),
        )
        .route(
            "/api/v2/assist/requests/{request_id}/accept",
            post(assist::accept),
        )
        .route(
            "/api/v2/assist/requests/{request_id}/cancel",
            post(assist::cancel),
        )
        .route(
            "/api/v2/assist/requests/{request_id}/pake",
            post(assist::reply_pake),
        )
        .route(
            "/api/v2/assist/requests/{request_id}/decision",
            post(assist::reply_decision),
        )
        .route(farwing_protocol::WS_PATH, get(crate::ws::upgrade))
        .route(misc::HEALTHZ, get(misc::healthz));
    let api = chat::routes(api);

    api.fallback(web_static::serve)
        .method_not_allowed_fallback(method_not_allowed)
        .layer(axum::middleware::from_fn_with_state(
            Arc::clone(&app),
            common_layer,
        ))
        .with_state(app)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn routes_match_protocol_paths() {
        // Literal route patterns must stay in sync with `api::path`.
        let id = "{id}";
        assert_eq!(path::device(id), "/api/v2/devices/{id}");
        assert_eq!(path::device_detail(id), "/api/v2/devices/{id}/detail");
        assert_eq!(path::device_wallpaper(id), "/api/v2/devices/{id}/wallpaper");
        assert_eq!(path::device_power(id), "/api/v2/devices/{id}/power");
        assert_eq!(
            path::session_ticket("{session_id}"),
            "/api/v2/sessions/{session_id}/ticket"
        );
        assert_eq!(
            path::auth_session("{device_id}"),
            "/api/v2/auth/sessions/{device_id}"
        );
        assert_eq!(
            path::assist_request("{request_id}", "accept"),
            "/api/v2/assist/requests/{request_id}/accept"
        );
        assert_eq!(
            path::power_result("{request_id}"),
            "/api/v2/power/{request_id}/result"
        );
        assert_eq!(path::friend("{user_id}"), "/api/v2/friends/{user_id}");
        assert_eq!(
            path::friend_request(id, "accept"),
            "/api/v2/friends/requests/{id}/accept"
        );
        assert_eq!(
            path::friend_request_entry(id),
            "/api/v2/friends/requests/{id}"
        );
        assert_eq!(path::grants("{user_id}"), "/api/v2/grants/{user_id}");
        assert_eq!(path::updates("{platform}"), "/api/v2/updates/{platform}");
        assert_eq!(
            path::assist_saved_entry("{kind}", "{device_id}"),
            "/api/v2/assist/saved/{kind}/{device_id}"
        );
    }

    #[test]
    fn forwarded_for() {
        let trusted: Vec<IpNet> = vec!["10.0.0.0/8".parse().unwrap()];
        let mut h = HeaderMap::new();
        h.insert("x-forwarded-for", "203.0.113.5, 10.0.0.2".parse().unwrap());
        let peer: IpAddr = "10.0.0.1".parse().unwrap();
        assert_eq!(client_ip(peer, &h, &trusted).to_string(), "203.0.113.5");
        let untrusted: IpAddr = "198.51.100.7".parse().unwrap();
        assert_eq!(client_ip(untrusted, &h, &trusted), untrusted);
    }
}
