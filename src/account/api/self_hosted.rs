use super::*;
use crate::account::self_hosted::SelfHosted;
use crate::session::local_host;

#[derive(Clone)]
pub(super) struct DesktopApi {
    client: reqwest::Client,
    settings: SelfHosted,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RoomTicket {
    ticket: String,
}

impl DesktopApi {
    pub(super) fn new(settings: SelfHosted) -> Result<Self> {
        Ok(Self { client: local_host::http_client()?, settings })
    }

    pub(super) async fn send<T: DeserializeOwned>(
        &self,
        contract: Contract,
        path: &str,
        body: Vec<u8>,
    ) -> Result<ApiEnvelope<T>> {
        let target = path;
        let mut body = if body.is_empty() {
            serde_json::Value::Null
        } else {
            serde_json::from_slice(&body)?
        };
        let route = if target == DEVICE_WINDOWS_INIT.path {
            body = serde_json::json!({"name": body.get("name"), "platform": "windows"});
            "register".to_owned()
        } else if target == USER_INFO.path {
            "profile".to_owned()
        } else if target == DEVICE_LIST.path {
            "devices".to_owned()
        } else if target == DEVICE_GROUPS.path {
            "groups".to_owned()
        } else if target == DEVICE_CONTROLLABLE.path {
            "controllable".to_owned()
        } else if target == ROOM_CREATE.path {
            "room".to_owned()
        } else if let Some(id) = target.strip_prefix(ROOM_JOIN_BY_DEVICE.path).and_then(|tail| tail.strip_prefix('/')) {
            body["device_id"] = id.into();
            "join".to_owned()
        } else if let Some(id) = target.strip_prefix("/api/v1/device/").and_then(|tail| tail.strip_suffix("/detail")) {
            format!("devices/{id}")
        } else if contract.path == DEVICE_RENAME.path && matches!(contract.method, Method::Put) {
            format!("devices/{}", target.trim_start_matches("/api/v1/device/"))
        } else if target == USER_LOGOUT.path {
            "disconnect".to_owned()
        } else {
            bail!("当前自建服务器未提供该功能");
        };
        let method = match contract.method {
            Method::Get => HttpMethod::GET,
            Method::Post => HttpMethod::POST,
            Method::Put => HttpMethod::PUT,
            Method::Delete => HttpMethod::DELETE,
        };
        let mut request = self.client
            .request(method, self.settings.origin.join(&format!("/v1/desktop/{route}"))?)
            .bearer_auth(&self.settings.token);
        if !body.is_null() {
            request = request.json(&body);
        }
        let response = crate::transport::http::send(crate::transport::http::Operation::Nrd, request).await?;
        let status = response.status();
        if !status.is_success() {
            let message = response.bytes().await.ok()
                .and_then(|bytes| serde_json::from_slice::<serde_json::Value>(&bytes).ok())
                .and_then(|body| body.get("error").and_then(serde_json::Value::as_str).map(str::to_owned))
                .unwrap_or_else(|| "服务器请求失败，检查地址和服务版本".into());
            let code = if status == reqwest::StatusCode::UNAUTHORIZED { 1120 } else { i32::from(status.as_u16()) };
            return Ok(ApiEnvelope { code, msg: message, data: None });
        }
        let mut value: serde_json::Value = serde_json::from_slice(&response.bytes().await?)
            .context("自建服务器返回了无效的 JSON")?;
        if route == "room" || route == "join" {
            let room: RoomTicket = serde_json::from_value(value)?;
            let mut endpoint = self.settings.origin.join("/socket.io/")?;
            let scheme = if endpoint.scheme() == "https" { "wss" } else { "ws" };
            endpoint.set_scheme(scheme).map_err(|()| anyhow::anyhow!("自建服务器信令地址无效"))?;
            value = serde_json::json!({
                "token": room.ticket,
                "signaling_server": endpoint.as_str(),
                "ws_connect_timeout_ms": 10000,
                "streamer_retry_delta_ms": 1000,
                "max_reconnect_delta": 300
            });
        }
        Ok(ApiEnvelope { code: 0, msg: String::new(), data: Some(serde_json::from_value(value)?) })
    }
}
