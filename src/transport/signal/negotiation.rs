//! Signaling requests and negotiated session descriptions.
use super::{STREAMER_FLAG, STREAMER_FLAG_HEADER, STREAMER_VERSION, STREAMER_VERSION_HEADER};
use crate::diagnostics::performance::RemoteSenderInfo;
use anyhow::{Context as _, Result, anyhow, bail};
use serde::Deserialize;
use serde_json::Value;
use std::time::{SystemTime, UNIX_EPOCH};
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::{HeaderName, HeaderValue, Request};
use url::Url;
use webrtc::ice_transport::ice_candidate::RTCIceCandidateInit;

pub(super) enum ForwardSettingEvent {
    DeviceCapability(crate::protocol::capability::DeviceCapability),
    SenderParaInfo {
        ice_id: String,
        infos: Vec<RemoteSenderInfo>,
    },
    SwitchNetwork {
        transport_type: u8,
        ice_id: String,
        attempt_switch_type: u8,
    },
}

pub(super) fn parse_forward_setting(
    args: &[Value],
    expected: &ControlSessionInfo,
) -> Result<Option<ForwardSettingEvent>> {
    let Some(envelope) = args.first() else {
        return Ok(None);
    };
    if envelope
        .get("client_id")
        .and_then(Value::as_str)
        .is_some_and(|client_id| client_id != expected.client_id)
    {
        return Ok(None);
    }
    let data = envelope
        .get("data")
        .context("forward_setting omitted data")?;
    match data.get("type").and_then(Value::as_str) {
        Some("device_capability") => {
            let payload = data
                .get("device_capability")
                .context("device_capability event omitted payload")?;
            Ok(Some(ForwardSettingEvent::DeviceCapability(
                serde_json::from_value(payload.clone()).context("decode device_capability")?,
            )))
        }
        Some("sender_para_info") => {
            let payload = data
                .get("sender_para_info")
                .context("sender_para_info event omitted payload")?;
            let ice_id = payload
                .get("ice_id")
                .and_then(Value::as_str)
                .unwrap_or_default();
            if ice_id != expected.ice_id {
                return Ok(None);
            }
            // Current sender reports carry one entry per video track.
            let entries = payload
                .get("sender_media_infos")
                .and_then(Value::as_array)
                .context("sender_para_info omitted sender_media_infos")?;
            let parse = |entry: &Value| RemoteSenderInfo {
                video_track_index: entry
                    .get("video_track_index")
                    .and_then(Value::as_u64)
                    .unwrap_or_default(),
                capture_impl: entry
                    .get("capture_impl")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned(),
                encoder_impl: entry
                    .get("encoder_impl")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned(),
            };
            let infos = entries.iter().map(parse).collect();
            Ok(Some(ForwardSettingEvent::SenderParaInfo {
                ice_id: ice_id.to_owned(),
                infos,
            }))
        }
        Some("switch_network_notify") => {
            let payload = data
                .get("switch_network_notify")
                .context("switch_network_notify event omitted payload")?;
            let transport_type = payload
                .get("transport_type")
                .and_then(Value::as_u64)
                .and_then(|value| u8::try_from(value).ok())
                .unwrap_or_default();
            let attempt_switch_type = payload
                .get("attempt_switch_type")
                .and_then(Value::as_u64)
                .and_then(|value| u8::try_from(value).ok())
                .unwrap_or_default();
            Ok(Some(ForwardSettingEvent::SwitchNetwork {
                transport_type,
                ice_id: payload
                    .get("ice_id")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned(),
                attempt_switch_type,
            }))
        }
        _ => Ok(None),
    }
}

#[derive(Clone, Deserialize)]
pub struct ControlSessionInfo {
    pub code: i64,
    pub client_id: String,
    pub ice_id: String,
    #[serde(rename = "iceServers", default)]
    pub ice_servers: Vec<ControlIceServer>,
    #[serde(default)]
    pub force_relay: bool,
    #[serde(default)]
    pub auto_switch_network: bool,
    #[serde(default)]
    pub possible_auto_switch_pkt_loss: u32,
    #[serde(default)]
    pub possible_auto_switch_latency: u32,
    #[serde(default)]
    pub possible_auto_switch_min_latency: u32,
    #[serde(default)]
    pub force_auto_switch_pkt_loss: u32,
    #[serde(default)]
    pub force_auto_switch_latency: u32,
    #[serde(skip)]
    pub app_control_id: String,
    #[serde(skip)]
    pub(crate) local_capability: crate::protocol::capability::DeviceCapability,
}

#[derive(Clone, Deserialize)]
pub struct ControlIceServer {
    pub urls: String,
    #[serde(default)]
    pub username: String,
    #[serde(default)]
    pub credential: String,
}

impl std::fmt::Debug for ControlSessionInfo {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ControlSessionInfo")
            .field("code", &self.code)
            .field("client_id", &"***REDACTED***")
            .field("ice_id", &"***REDACTED***")
            .field("ice_server_count", &self.ice_servers.len())
            .field("force_relay", &self.force_relay)
            .field("auto_switch_network", &self.auto_switch_network)
            .field(
                "possible_auto_switch_pkt_loss",
                &self.possible_auto_switch_pkt_loss,
            )
            .field(
                "possible_auto_switch_latency",
                &self.possible_auto_switch_latency,
            )
            .field(
                "possible_auto_switch_min_latency",
                &self.possible_auto_switch_min_latency,
            )
            .field(
                "force_auto_switch_pkt_loss",
                &self.force_auto_switch_pkt_loss,
            )
            .field("force_auto_switch_latency", &self.force_auto_switch_latency)
            .field("app_control_id", &"***REDACTED***")
            .finish()
    }
}

impl std::fmt::Debug for ControlIceServer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ControlIceServer")
            .field("urls", &"***REDACTED***")
            .field("username", &"***REDACTED***")
            .field("credential", &"***REDACTED***")
            .finish()
    }
}

pub(super) enum RemoteSoac {
    Answer { sdp: String, restart_ice: bool },
    Candidate(RTCIceCandidateInit),
}

pub(super) fn parse_remote_soac(
    args: &[Value],
    expected: Option<&ControlSessionInfo>,
) -> Result<Option<RemoteSoac>> {
    let Some(data) = args
        .first()
        .and_then(|value| value.get("data"))
        .and_then(Value::as_object)
    else {
        return Ok(None);
    };
    if let Some(expected) = expected
        && (data.get("ice_id").and_then(Value::as_str) != Some(expected.ice_id.as_str())
            || data
                .get("app_control_id")
                .and_then(Value::as_str)
                .is_some_and(|value| value != expected.app_control_id.as_str()))
    {
        return Ok(None);
    }
    match data.get("type").and_then(Value::as_str) {
        Some(kind @ ("answer" | "restart_ice")) => {
            let sdp = data
                .get("sdp")
                .and_then(Value::as_str)
                .filter(|value| !value.is_empty())
                .context("soac answer did not contain SDP")?;
            Ok(Some(RemoteSoac::Answer {
                sdp: sdp.to_owned(),
                restart_ice: kind == "restart_ice",
            }))
        }
        Some("candidate") => {
            let candidate = data
                .get("candidate")
                .cloned()
                .context("soac candidate did not contain candidate data")?;
            let candidate = serde_json::from_value(candidate)
                .context("soac candidate has an unexpected payload")?;
            Ok(Some(RemoteSoac::Candidate(candidate)))
        }
        _ => Ok(None),
    }
}

pub(super) fn signaling_request(
    endpoint: &str,
    token: &str,
    controlling: bool,
    reconnect_key: Option<&str>,
) -> Result<Request<()>> {
    let mut url = if endpoint.contains("://") {
        Url::parse(endpoint)
    } else {
        Url::parse(&format!("wss://{endpoint}"))
    }
    .map_err(|_| anyhow!("room response contained an invalid signaling endpoint"))?;
    if url.host_str().is_none()
        || (url.scheme() != "wss"
            && !(url.scheme() == "ws"
                && url
                    .host_str()
                    .is_some_and(super::is_loopback_host)))
    {
        bail!("room response contained an unsupported signaling endpoint");
    }
    if url.path().is_empty() || url.path() == "/" {
        url.set_path("/socket.io/");
    }
    url.set_query(None);
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .context("system clock is before the Unix epoch")?
        .as_millis()
        .to_string();
    url.query_pairs_mut()
        .append_pair("EIO", "4")
        .append_pair("transport", "websocket")
        .append_pair("t", &timestamp);

    let mut request = url
        .as_str()
        .into_client_request()
        .map_err(|_| anyhow!("failed to build signaling WebSocket request"))?;
    let headers = request.headers_mut();
    let mut auth =
        HeaderValue::from_str(token).context("room token is not a valid header value")?;
    auth.set_sensitive(true);
    headers.insert(HeaderName::from_static("x-nrd-auth"), auth);
    headers.insert(
        HeaderName::from_static("x-nrd-controlling"),
        HeaderValue::from_static(if controlling { "1" } else { "0" }),
    );
    if let Some(reconnect_key) = reconnect_key.filter(|value| !value.is_empty()) {
        let mut value = HeaderValue::from_str(reconnect_key)
            .context("reconnect key is not a valid header value")?;
        value.set_sensitive(true);
        headers.insert(HeaderName::from_static("x-nrd-reconn-key"), value);
    }
    headers.insert(
        HeaderName::from_static(STREAMER_VERSION_HEADER),
        HeaderValue::from_static(STREAMER_VERSION),
    );
    headers.insert(
        HeaderName::from_static(STREAMER_FLAG_HEADER),
        HeaderValue::from_static(STREAMER_FLAG),
    );
    Ok(request)
}
