//! Server settings in the administration API (`docs/server-admin-api.md`):
//! read, change (applied while the server runs), confirm a moved listener,
//! upload a certificate, export and import.

use super::auth::AdminAuth;
use super::{App, JsonBody};
use crate::config::{Config, SettingError, TlsMode};
use crate::error::{ApiError, ApiResult};
use crate::runtime::{ApplyError, Outcome};
use axum::Json;
use axum::extract::{Query, State};
use farwing_protocol::ErrorCode;
use farwing_protocol::error::reason;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

/// `patch` over `base`: objects merge key by key, everything else replaces.
pub fn merge(base: &mut Value, patch: Value) {
    match (base, patch) {
        (Value::Object(base), Value::Object(patch)) => {
            for (key, value) in patch {
                merge(base.entry(key).or_insert(Value::Null), value);
            }
        }
        (base, patch) => *base = patch,
    }
}

pub fn setting_error(e: SettingError) -> ApiError {
    let message = e.message.clone();
    ApiError::reason(ErrorCode::BadRequest, reason::SETTING_INVALID, message)
        .with_detail("field", e.field)
        .with_detail("problem", e.problem)
}

pub fn apply_error(e: ApplyError) -> ApiError {
    match e {
        ApplyError::Setting(e) => setting_error(e),
        ApplyError::Stopped => ApiError::new(ErrorCode::Unavailable, "the server is stopping"),
    }
}

/// The settings in force with `patch` over them. A secret left empty keeps
/// the current one.
pub fn patched(current: &Config, patch: Value) -> Result<Config, ApiError> {
    let mut value = serde_json::to_value(current).expect("serializable");
    merge(&mut value, patch);
    let mut cfg: Config = serde_json::from_value(value).map_err(|e| {
        setting_error(SettingError::new("settings", "invalid", e.to_string()))
    })?;
    if cfg.turn.shared_secret.is_empty() {
        cfg.turn.shared_secret = current.turn.shared_secret.clone();
    }
    cfg.data_dir = current.data_dir.clone();
    cfg.listen_locked = current.listen_locked;
    canonical_caps(&mut cfg);
    Ok(cfg)
}

/// Browser capabilities in the canonical order, without duplicates
/// (unknown ones kept at the end for validation to refuse).
fn canonical_caps(cfg: &mut Config) {
    let web = farwing_protocol::caps::WEB;
    let caps = &cfg.web.caps;
    cfg.web.caps = web
        .iter()
        .filter(|c| caps.iter().any(|w| w == *c))
        .map(|c| (*c).to_owned())
        .chain(caps.iter().filter(|w| !web.contains(&w.as_str())).cloned())
        .collect();
}

/// The settings for display: secrets replaced by whether they are set.
pub fn redacted(cfg: &Config) -> Value {
    let mut value = serde_json::to_value(cfg).expect("serializable");
    value["turn"]["shared_secret"] = Value::String(String::new());
    value
}

#[derive(Serialize)]
pub struct SettingsView {
    pub settings: Value,
    /// The relay secret is set (it is never shown).
    pub turn_secret_set: bool,
    /// The listen address is fixed by `--listen`.
    pub listen_locked: bool,
    pub data_dir: String,
    pub tls: TlsView,
    pub relay: RelayView,
    /// No administrator exists yet.
    pub setup: bool,
    /// A listen change waiting for confirmation.
    pub listen_pending: Option<crate::runtime::ListenPending>,
}

#[derive(Serialize)]
pub struct TlsView {
    pub fingerprint: Option<String>,
    pub acme: Option<crate::acme::Status>,
    /// An uploaded certificate is stored.
    pub provided: bool,
}

#[derive(Serialize)]
pub struct RelayView {
    pub turn_udp: Option<String>,
    pub turn_tcp: Option<String>,
    pub turn_tls: Option<String>,
    pub stun: Vec<String>,
    /// `stun:` URLs clients get for NAT discovery.
    pub nat_discovery: Vec<String>,
    /// The suggested public address (this machine's default-route IPv4).
    pub local_ipv4: Option<String>,
}

pub fn view(app: &App) -> SettingsView {
    let cfg = app.cfg();
    let relay = app.relay.read().clone();
    let presented = app.tls.read().clone();
    SettingsView {
        settings: redacted(&cfg),
        turn_secret_set: !cfg.turn.shared_secret.is_empty(),
        listen_locked: cfg.listen_locked,
        data_dir: cfg.data_dir.display().to_string(),
        tls: TlsView {
            fingerprint: presented.as_ref().map(|t| t.fingerprint()),
            acme: match &presented {
                Some(crate::tls::Presented::Acme(acme)) => Some(acme.status()),
                _ => None,
            },
            provided: crate::tls::provided_paths(&cfg.tls_dir()).0.exists(),
        },
        relay: RelayView {
            turn_udp: relay.turn_udp.map(|a| a.to_string()),
            turn_tcp: relay.turn_tcp.map(|a| a.to_string()),
            turn_tls: relay.turn_tls.map(|a| a.to_string()),
            stun: relay.stun_udp.iter().map(|a| a.to_string()).collect(),
            nat_discovery: app.nat_discovery.read().clone(),
            local_ipv4: crate::config::local_ipv4().map(|ip| ip.to_string()),
        },
        setup: app.setup_pending(),
        listen_pending: app.listen_pending.read().clone(),
    }
}

/// Top-level groups that differ (for the audit log).
fn changed_groups(old: &Config, new: &Config) -> Vec<String> {
    let (a, b) = (
        serde_json::to_value(old).expect("serializable"),
        serde_json::to_value(new).expect("serializable"),
    );
    let (Value::Object(a), Value::Object(b)) = (a, b) else {
        return Vec::new();
    };
    b.iter()
        .filter(|(k, v)| a.get(*k) != Some(*v))
        .map(|(k, _)| k.clone())
        .collect()
}

#[derive(Serialize)]
pub struct Applied {
    pub outcome: Outcome,
    #[serde(flatten)]
    pub view: SettingsView,
}

/// `GET /api/v2/admin/settings`
pub async fn get(State(app): State<App>, _admin: AdminAuth) -> ApiResult<Json<SettingsView>> {
    Ok(Json(view(&app)))
}

/// Apply `cfg` and record it in the audit log.
async fn apply_and_log(app: &App, admin: &AdminAuth, cfg: Config) -> ApiResult<Json<Applied>> {
    // The administrator saving the list stays in it: one outside it could
    // not open the console again.
    let allow = &cfg.web.admin_allow;
    if !allow.is_empty() && !allow.iter().any(|n| n.contains(&admin.ip)) {
        return Err(setting_error(SettingError::new(
            "web.admin_allow",
            "excludes_you",
            format!("{} is not in the list", admin.ip),
        )));
    }
    let old = app.cfg();
    let groups = changed_groups(&old, &cfg);
    let outcome = app.control().apply(cfg).await.map_err(|e| {
        let e = apply_error(e);
        tracing::info!(error = %e.message, "settings refused");
        e
    })?;
    if !groups.is_empty() {
        super::admin_api::log(
            app,
            admin,
            "settings.update",
            "",
            json!({ "groups": groups }),
        )
        .await;
    }
    Ok(Json(Applied {
        outcome,
        view: view(app),
    }))
}

/// `PUT /api/v2/admin/settings`: a partial settings document, merged over
/// the settings in force.
pub async fn put(
    State(app): State<App>,
    admin: AdminAuth,
    JsonBody(patch): JsonBody<Value>,
) -> ApiResult<Json<Applied>> {
    let cfg = patched(&app.cfg(), patch)?;
    apply_and_log(&app, &admin, cfg).await
}

/// `POST /api/v2/admin/settings/preview`: what saving a partial settings
/// document would do; nothing is saved.
pub async fn preview(
    State(app): State<App>,
    _admin: AdminAuth,
    JsonBody(patch): JsonBody<Value>,
) -> ApiResult<Json<crate::runtime::Preview>> {
    let cfg = patched(&app.cfg(), patch)?;
    Ok(Json(app.control().preview(cfg).await))
}

/// `POST /api/v2/admin/settings/listen/confirm`
pub async fn confirm_listen(
    State(app): State<App>,
    admin: AdminAuth,
) -> ApiResult<Json<SettingsView>> {
    if !app.control().confirm_listen().await {
        return Err(ApiError::reason(
            ErrorCode::Conflict,
            reason::NOTHING_TO_CONFIRM,
            "no listen change waits for confirmation",
        ));
    }
    super::admin_api::log(&app, &admin, "settings.listen_confirm", "", Value::Null).await;
    Ok(Json(view(&app)))
}

/// `POST /api/v2/admin/settings/listen/revert`
pub async fn revert_listen(
    State(app): State<App>,
    admin: AdminAuth,
) -> ApiResult<Json<SettingsView>> {
    if !app.control().revert_listen().await {
        return Err(ApiError::reason(
            ErrorCode::Conflict,
            reason::NOTHING_TO_CONFIRM,
            "no listen change waits for confirmation",
        ));
    }
    super::admin_api::log(&app, &admin, "settings.listen_revert", "", Value::Null).await;
    Ok(Json(view(&app)))
}

#[derive(Deserialize)]
pub struct Certificate {
    pub cert_pem: String,
    pub key_pem: String,
}

/// Check and store an uploaded certificate; `field` names the problem.
pub fn store_certificate(app: &App, c: &Certificate) -> ApiResult<String> {
    crate::tls::store_provided(&app.cfg().tls_dir(), &c.cert_pem, &c.key_pem).map_err(|e| {
        setting_error(SettingError::new("tls.certificate", "invalid", format!("{e:#}")))
    })
}

/// `PUT /api/v2/admin/settings/certificate`: upload a certificate chain and
/// key, and use them.
pub async fn put_certificate(
    State(app): State<App>,
    admin: AdminAuth,
    JsonBody(c): JsonBody<Certificate>,
) -> ApiResult<Json<Applied>> {
    let fingerprint = store_certificate(&app, &c)?;
    super::admin_api::log(
        &app,
        &admin,
        "settings.certificate",
        "",
        json!({ "fingerprint": fingerprint }),
    )
    .await;
    let mut cfg = (*app.cfg()).clone();
    cfg.tls.mode = TlsMode::Provided;
    let outcome = app.control().reload_tls(cfg).await.map_err(apply_error)?;
    Ok(Json(Applied {
        outcome,
        view: view(&app),
    }))
}

#[derive(Deserialize)]
pub struct ExportQuery {
    #[serde(default)]
    pub secrets: bool,
}

/// `GET /api/v2/admin/settings/export[?secrets=true]`
pub async fn export(
    State(app): State<App>,
    admin: AdminAuth,
    Query(q): Query<ExportQuery>,
) -> ApiResult<Json<Value>> {
    if q.secrets && admin.read_only {
        return Err(ApiError::forbidden("a read-only token cannot export secrets"));
    }
    let cfg = app.cfg();
    super::admin_api::log(
        &app,
        &admin,
        "settings.export",
        "",
        json!({ "secrets": q.secrets }),
    )
    .await;
    Ok(Json(if q.secrets {
        serde_json::to_value(&*cfg).expect("serializable")
    } else {
        redacted(&cfg)
    }))
}

/// `POST /api/v2/admin/settings/import`: a whole settings document
/// (as exported); a secret left empty keeps the current one.
pub async fn import(
    State(app): State<App>,
    admin: AdminAuth,
    JsonBody(doc): JsonBody<Value>,
) -> ApiResult<Json<Applied>> {
    let current = app.cfg();
    let mut cfg: Config = serde_json::from_value(doc).map_err(|e| {
        setting_error(SettingError::new("settings", "invalid", e.to_string()))
    })?;
    if cfg.turn.shared_secret.is_empty() {
        cfg.turn.shared_secret = current.turn.shared_secret.clone();
    }
    cfg.data_dir = current.data_dir.clone();
    cfg.listen_locked = current.listen_locked;
    if current.listen_locked {
        cfg.listen = current.listen;
    }
    apply_and_log(&app, &admin, cfg).await
}

/// `POST /api/v2/admin/settings/relay-secret`: a new relay secret (relayed
/// connections reconnect).
pub async fn rotate_relay_secret(
    State(app): State<App>,
    admin: AdminAuth,
) -> ApiResult<Json<Applied>> {
    let mut cfg = (*app.cfg()).clone();
    cfg.turn.shared_secret = crate::crypto::new_shared_secret();
    apply_and_log(&app, &admin, cfg).await
}
