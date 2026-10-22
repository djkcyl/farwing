//! Server info, client configuration, update announcements and health.

use super::App;
use super::auth::AnyAuth;
use crate::config::normalize_platform;
use crate::error::ApiResult;
use axum::Json;
use axum::extract::{Path, Query, State};
use farwing_protocol::api::{
    ClientConfig, ProtocolRange, ReleaseInfo, ServerFeatures, ServerIdentity, ServerInfo,
    UpdateInfo,
};
use serde::Deserialize;

pub const HEALTHZ: &str = "/healthz";

pub async fn healthz() -> &'static str {
    "ok"
}

/// `GET /api/v2/server`
pub async fn server_info(State(app): State<App>) -> Json<ServerInfo> {
    let updates = app
        .db
        .call(|c| Ok(!crate::webdb::announcements(c)?.is_empty()))
        .await
        .unwrap_or(false);
    Json(ServerInfo {
        protocol: ProtocolRange {
            name: farwing_protocol::PROTOCOL_NAME.to_owned(),
            min: farwing_protocol::MIN_PROTOCOL_VERSION,
            max: farwing_protocol::PROTOCOL_VERSION,
        },
        server: ServerIdentity {
            name: "Farwing Server".to_owned(),
            version: env!("CARGO_PKG_VERSION").to_owned(),
            instance_name: app.cfg().instance_name.clone(),
        },
        features: ServerFeatures {
            guest_hosts: app.cfg().guest.enabled,
            turn: app.hub.has_turn(),
            force_relay: app.cfg().turn.force_relay,
            updates,
            friends: true,
            nat_discovery: app.nat_discovery.read().clone(),
        },
        time_ms: crate::db::now_ms(),
    })
}

/// `GET /api/v2/config`
pub async fn client_config(State(app): State<App>, _auth: AnyAuth) -> Json<ClientConfig> {
    Json(ClientConfig {
        bitrate_limit_mbps: app.cfg().bitrate_limit_mbps,
    })
}

#[derive(Deserialize)]
pub struct UpdatesQuery {
    #[serde(default)]
    channel: Option<String>,
}

/// `GET /api/v2/updates/{platform}?channel=stable|beta`
pub async fn updates(
    State(app): State<App>,
    Path(platform): Path<String>,
    Query(q): Query<UpdatesQuery>,
) -> ApiResult<Json<UpdateInfo>> {
    let channel = q.channel.as_deref().unwrap_or("stable");
    let beta = channel.eq_ignore_ascii_case("beta");
    let Some(platform) = normalize_platform(&platform) else {
        return Ok(Json(UpdateInfo { latest: None }));
    };
    let entries = app.db.call(|c| crate::webdb::announcements(c)).await?;
    let latest = entries
        .iter()
        .enumerate()
        .filter(|(_, u)| normalize_platform(&u.platform) == Some(platform))
        .filter(|(_, u)| u.channel == "stable" || (beta && u.channel == "beta"))
        .max_by_key(|(i, u)| (u.published_at, std::cmp::Reverse(*i)))
        .map(|(_, u)| ReleaseInfo {
            version: u.version.clone(),
            prerelease: u.prerelease,
            published_at: u.published_at,
            notes: u.notes.clone(),
            url: u.url.clone(),
        });
    Ok(Json(UpdateInfo { latest }))
}
