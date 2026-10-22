//! Edge relay nodes: their administration (`/api/v2/admin/nodes`) and the
//! nodes' own connection (`/api/v2/edge/ws`), `docs/server-admin-api.md`.

use super::auth::{AdminAuth, timestamp_fresh};
use super::{App, ClientIp, JsonBody};
use crate::edge::{FromNode, NodeRow, NodeSettings, NodeStatus, ToNode};
use crate::error::{ApiError, ApiResult};
use crate::webdb;
use axum::Json;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Path, State};
use axum::http::HeaderMap;
use axum::response::{IntoResponse, Response};
use farwing_protocol::{ErrorCode, headers};
use futures_util::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::net::IpAddr;
use std::time::Duration;

/// A joining node offers its token…
pub const JOIN_HEADER: &str = "x-farwing-join";
/// …and its new public key; a joined node names itself.
pub const KEY_HEADER: &str = "x-farwing-node-key";
pub const NODE_HEADER: &str = "x-farwing-node";
/// Join tokens are valid this long.
const JOIN_VALID_S: i64 = 3600;
const NAME_MAX: usize = 64;

/// What a node signs to connect: `FWEDGE1\n<node id or new key>\n<ms>`.
pub fn auth_message(subject: &str, timestamp_ms: i64) -> Vec<u8> {
    format!("FWEDGE1\n{subject}\n{timestamp_ms}").into_bytes()
}

fn invalid(field: &str, message: &str) -> ApiError {
    super::settings_api::setting_error(crate::config::SettingError::new(
        field, "invalid", message,
    ))
}

fn node_status(app: &App, id: &str) -> ApiResult<NodeStatus> {
    app.edges
        .status()
        .into_iter()
        .find(|n| n.row.id == id)
        .ok_or_else(ApiError::not_found)
}

/// `GET /api/v2/admin/nodes`
pub async fn list(State(app): State<App>, _admin: AdminAuth) -> ApiResult<Json<Vec<NodeStatus>>> {
    Ok(Json(app.edges.status()))
}

#[derive(Deserialize)]
pub struct NewNode {
    pub name: String,
    #[serde(default)]
    pub region: String,
}

#[derive(Serialize)]
pub struct JoinCommand {
    pub node: NodeStatus,
    /// Run on the node: `farwing-server edge --join "<link>"`.
    pub command: String,
    pub link: String,
    pub expires_at: i64,
}

/// The join link of `token`: the public URL, `/edge/<token>`, and the
/// certificate's fingerprint after `#` (none behind a reverse proxy).
fn join_command(app: &App, token: &str) -> (String, String) {
    let base = app.cfg().public_url.trim_end_matches('/').to_owned();
    let fingerprint = app
        .tls
        .read()
        .as_ref()
        .map(|t| t.fingerprint())
        .unwrap_or_default();
    let link = if fingerprint.is_empty() {
        format!("{base}/edge/{token}")
    } else {
        format!("{base}/edge/{token}#{fingerprint}")
    };
    (format!("farwing-server edge --join \"{link}\""), link)
}

fn checked_name(name: &str) -> ApiResult<String> {
    let name = name.trim();
    if name.is_empty() || name.chars().count() > NAME_MAX {
        return Err(invalid("name", "the name must be 1-64 characters"));
    }
    Ok(name.to_owned())
}

/// `POST /api/v2/admin/nodes`: a node waiting to join, and its command.
pub async fn create(
    State(app): State<App>,
    admin: AdminAuth,
    JsonBody(req): JsonBody<NewNode>,
) -> ApiResult<Json<JoinCommand>> {
    let row = NodeRow {
        id: crate::crypto::new_uuid().replace('-', ""),
        name: checked_name(&req.name)?,
        region: req.region.trim().chars().take(NAME_MAX).collect(),
        public_key: String::new(),
        enabled: true,
        settings: NodeSettings::default(),
        created_at: crate::db::now_s(),
        joined_at: None,
        last_seen_at: None,
        last_ip: String::new(),
    };
    let token = crate::crypto::new_token("fwj_");
    let expires_at = crate::db::now_s() + JOIN_VALID_S;
    let (stored, hash) = (row.clone(), crate::crypto::token_hash(&token));
    app.db
        .call(move |c| webdb::create_edge_node(c, &stored, &hash, expires_at))
        .await?;
    app.edges.upsert(row.clone());
    super::admin_api::log(&app, &admin, "node.create", &row.name, Value::Null).await;
    let (command, link) = join_command(&app, &token);
    Ok(Json(JoinCommand {
        node: node_status(&app, &row.id)?,
        command,
        link,
        expires_at,
    }))
}

/// `POST /api/v2/admin/nodes/{id}/rejoin`: a new join command (the node
/// lost its identity); the next join replaces its key.
pub async fn rejoin(
    State(app): State<App>,
    admin: AdminAuth,
    Path(id): Path<String>,
) -> ApiResult<Json<JoinCommand>> {
    let token = crate::crypto::new_token("fwj_");
    let expires_at = crate::db::now_s() + JOIN_VALID_S;
    let (nid, hash) = (id.clone(), crate::crypto::token_hash(&token));
    let n = app
        .db
        .call(move |c| webdb::rejoin_edge_node(c, &nid, &hash, expires_at))
        .await?;
    if n == 0 {
        return Err(ApiError::not_found());
    }
    let name = app.edges.row(&id).map(|r| r.name).unwrap_or_default();
    super::admin_api::log(&app, &admin, "node.rejoin", &name, Value::Null).await;
    let (command, link) = join_command(&app, &token);
    Ok(Json(JoinCommand {
        node: node_status(&app, &id)?,
        command,
        link,
        expires_at,
    }))
}

#[derive(Deserialize)]
pub struct NodePatch {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub region: Option<String>,
    #[serde(default)]
    pub enabled: Option<bool>,
    /// Partial settings, merged over the node's.
    #[serde(default)]
    pub settings: Option<Value>,
}

/// `PATCH /api/v2/admin/nodes/{id}`
pub async fn update(
    State(app): State<App>,
    admin: AdminAuth,
    Path(id): Path<String>,
    JsonBody(patch): JsonBody<NodePatch>,
) -> ApiResult<Json<NodeStatus>> {
    let mut row = app.edges.row(&id).ok_or_else(ApiError::not_found)?;
    if let Some(name) = &patch.name {
        row.name = checked_name(name)?;
    }
    if let Some(region) = &patch.region {
        row.region = region.trim().chars().take(NAME_MAX).collect();
    }
    if let Some(enabled) = patch.enabled {
        row.enabled = enabled;
    }
    if let Some(settings) = patch.settings {
        let mut value = serde_json::to_value(&row.settings).expect("serializable");
        super::settings_api::merge(&mut value, settings);
        row.settings = serde_json::from_value(value)
            .map_err(|e| invalid("settings", &e.to_string()))?;
        let mut check = crate::config::Config::default();
        check.turn = app
            .edges
            .config_for(&id)
            .map(|mut t| {
                t.listen_udp = row.settings.listen_udp;
                t.listen_tcp = row.settings.listen_tcp;
                t.listen_tls = row.settings.listen_tls;
                t.relay_port_min = row.settings.relay_port_min;
                t.relay_port_max = row.settings.relay_port_max;
                t.relay_bind = row.settings.relay_bind;
                t.stun = row.settings.stun.clone();
                t.public_ip = t.public_ip.or(Some(IpAddr::from([192, 0, 2, 1])));
                t
            })
            .unwrap_or_default();
        check.public_url = "https://node.invalid".into();
        check.listen = "0.0.0.0:1".parse().expect("valid");
        check
            .validate()
            .map_err(super::settings_api::setting_error)?;
    }
    let stored = row.clone();
    app.db
        .call(move |c| webdb::update_edge_node(c, &stored))
        .await?;
    let name = row.name.clone();
    app.edges.upsert(row);
    super::admin_api::log(&app, &admin, "node.update", &name, Value::Null).await;
    Ok(Json(node_status(&app, &id)?))
}

/// `DELETE /api/v2/admin/nodes/{id}`: the node's identity is revoked.
pub async fn remove(
    State(app): State<App>,
    admin: AdminAuth,
    Path(id): Path<String>,
) -> ApiResult<Json<Value>> {
    let name = app.edges.row(&id).map(|r| r.name).unwrap_or_default();
    let nid = id.clone();
    let n = app
        .db
        .call(move |c| webdb::delete_edge_node(c, &nid))
        .await?;
    if n == 0 {
        return Err(ApiError::not_found());
    }
    app.edges.remove(&id);
    super::admin_api::log(&app, &admin, "node.delete", &name, Value::Null).await;
    Ok(Json(json!({})))
}

fn header<'a>(h: &'a HeaderMap, name: &str) -> Option<&'a str> {
    h.get(name)
        .and_then(|v| v.to_str().ok())
        .map(str::trim)
        .filter(|v| !v.is_empty())
}

fn rejected(message: &str) -> Response {
    tracing::info!(message, "edge node refused");
    ApiError::new(ErrorCode::Unauthorized, message).into_response()
}

/// `GET /api/v2/edge/ws`: a node joins (`X-Farwing-Join`, its new key and
/// a signature with it) or connects (`X-Farwing-Node` and a signature).
pub async fn connect(
    State(app): State<App>,
    ClientIp(ip): ClientIp,
    headers: HeaderMap,
    ws: WebSocketUpgrade,
) -> Response {
    let (Some(timestamp), Some(signature)) = (
        header(&headers, headers::TIMESTAMP).and_then(|t| t.parse::<i64>().ok()),
        header(&headers, headers::SIGNATURE),
    ) else {
        return rejected("signature required");
    };
    if !timestamp_fresh(timestamp) || !app.replay.insert(signature) {
        return rejected("stale or replayed signature");
    }
    let (id, joined) = if let Some(token) = header(&headers, JOIN_HEADER) {
        let Some(key) = header(&headers, KEY_HEADER) else {
            return rejected("node key required");
        };
        if farwing_protocol::crypto::verify(key, &auth_message(key, timestamp), signature).is_err() {
            return rejected("invalid signature");
        }
        let (hash, key) = (crate::crypto::token_hash(token), key.to_owned());
        match app
            .db
            .call(move |c| webdb::join_edge_node(c, &hash, &key))
            .await
        {
            Ok(Some(id)) => (id, true),
            Ok(None) => return rejected("invalid or expired join token"),
            Err(e) => return ApiError::from(e).into_response(),
        }
    } else if let Some(id) = header(&headers, NODE_HEADER) {
        let Some(row) = app.edges.row(id) else {
            return rejected("unknown node");
        };
        if row.public_key.is_empty()
            || farwing_protocol::crypto::verify(&row.public_key, &auth_message(id, timestamp), signature)
                .is_err()
        {
            return rejected("invalid signature");
        }
        (id.to_owned(), false)
    } else {
        return rejected("join token or node required");
    };
    if joined {
        let nid = id.clone();
        match app.db.call(move |c| webdb::edge_node(c, &nid)).await {
            Ok(Some(row)) => app.edges.upsert(row),
            Ok(None) => return rejected("unknown node"),
            Err(e) => return ApiError::from(e).into_response(),
        }
        tracing::info!(node = %id, %ip, "edge node joined");
    }
    ws.max_message_size(256 * 1024)
        .on_upgrade(move |socket| run(app, socket, id, ip, joined))
}

async fn run(app: App, socket: WebSocket, id: String, ip: IpAddr, joined: bool) {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<ToNode>();
    let conn_id = crate::crypto::new_uuid();
    if joined {
        let _ = tx.send(ToNode::Joined {
            node_id: id.clone(),
        });
    }
    app.edges.attach(&id, &conn_id, ip, tx);
    tracing::info!(node = %id, %ip, "edge node connected");
    let touch = |app: &App, id: &str| {
        let (app, id, ip) = (app.clone(), id.to_owned(), ip.to_string());
        async move {
            let _ = app
                .db
                .call(move |c| webdb::touch_edge_node(c, &id, &ip))
                .await;
        }
    };
    touch(&app, &id).await;
    let (mut sink, mut stream) = socket.split();
    let mut touched = tokio::time::interval(Duration::from_secs(60));
    touched.tick().await;
    loop {
        tokio::select! {
            out = rx.recv() => match out {
                Some(message) => {
                    let text = serde_json::to_string(&message).expect("serializable");
                    if sink.send(Message::Text(text.into())).await.is_err() {
                        break;
                    }
                }
                // Removed, or replaced by a newer connection.
                None => {
                    let _ = sink.send(Message::Close(None)).await;
                    break;
                }
            },
            incoming = stream.next() => match incoming {
                Some(Ok(Message::Text(text))) => match serde_json::from_str::<FromNode>(&text) {
                    Ok(FromNode::Health(health)) => app.edges.report(&id, &conn_id, health),
                    Err(e) => tracing::debug!(node = %id, error = %e, "unreadable edge message"),
                },
                Some(Ok(Message::Close(_))) | None | Some(Err(_)) => break,
                Some(Ok(_)) => {}
            },
            _ = touched.tick() => touch(&app, &id).await,
        }
    }
    app.edges.detach(&id, &conn_id);
    touch(&app, &id).await;
    tracing::info!(node = %id, "edge node disconnected");
}
