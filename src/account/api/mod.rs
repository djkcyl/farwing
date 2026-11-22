//! Signed REST client for the UU Remote interoperability API.
//!
//! Requests are emitted directly over HTTPS. No official executable or DLL is
//! loaded. The request signing and QR contracts in this module were first
//! validated with the supplied Python probe before being ported here.

use std::{
    fmt,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use crate::account::nrd_http::RetryPolicy;
use anyhow::{Context, Result, bail};
use hmac::{Hmac, Mac};
use reqwest::{
    Method as HttpMethod,
    header::{AUTHORIZATION, CONTENT_TYPE, HeaderMap, HeaderName, HeaderValue},
};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use sha2::Sha256;

pub const BASE_URL: &str = crate::account::nrd_http::PRIMARY;
pub const PROTOCOL_VERSION: &str = crate::protocol::official_version::CLIENT;
pub const DEFAULT_CHANNEL: &str = "gwqd";

const SIGNING_KEY: &[u8] = b"alWiSzXZTLu3WfFnw13uBru3";
const PACKAGE_NAME: &str = "com.netease.uuremote";
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
const LONG_POLL_TIMEOUT: Duration = Duration::from_secs(65);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum IdentityScope {
    Platform,
    Device,
    AccountDevice,
}

type HmacSha256 = Hmac<Sha256>;
mod assist;
mod entitlements;
pub(crate) use entitlements::Entitlements;
pub(crate) mod guest;
mod host_assist;
mod power;
pub(crate) mod wallpaper;
mod wol;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Method {
    Get,
    Post,
    Put,
    Delete,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Contract {
    pub method: Method,
    pub path: &'static str,
    identity: IdentityScope,
    timeout: Duration,
    json_content_type: bool,
    retry: RetryPolicy,
}

pub const LOGIN_QR_GENERATE: Contract = Contract {
    retry: RetryPolicy::Replayable,
    method: Method::Post,
    path: "/api/v1/qrcode/gen/login",
    identity: IdentityScope::AccountDevice,
    timeout: REQUEST_TIMEOUT,
    json_content_type: true,
};

pub const LOGIN_QR_STATUS: Contract = Contract {
    retry: RetryPolicy::Replayable,
    method: Method::Post,
    path: "/api/v1/qrcode/login/status",
    identity: IdentityScope::AccountDevice,
    timeout: LONG_POLL_TIMEOUT,
    json_content_type: true,
};

pub const LOGIN_BY_QR: Contract = Contract {
    retry: RetryPolicy::Replayable,
    method: Method::Post,
    path: "/api/v1/login/by_qrcode",
    identity: IdentityScope::Device,
    timeout: REQUEST_TIMEOUT,
    json_content_type: true,
};

pub(crate) const LOGIN_SMS_CODE: Contract = Contract {
    retry: RetryPolicy::ConnectOnly,
    path: "/api/v1/security/mobile/code",
    ..LOGIN_BY_QR
};
pub(crate) const LOGIN_BY_MOBILE: Contract = Contract {
    retry: RetryPolicy::ConnectOnly,
    path: "/api/v1/login/by_mobile",
    ..LOGIN_BY_QR
};

pub const USER_INFO: Contract = Contract {
    retry: RetryPolicy::Replayable,
    method: Method::Get,
    path: "/api/v1/user/info",
    identity: IdentityScope::AccountDevice,
    timeout: REQUEST_TIMEOUT,
    json_content_type: false,
};

pub(crate) const CONFIGURES_WITH_VERSION: Contract = Contract {
    retry: RetryPolicy::Replayable,
    method: Method::Post,
    path: "/api/v1/tool/configures_with_version",
    identity: IdentityScope::AccountDevice,
    timeout: REQUEST_TIMEOUT,
    json_content_type: true,
};

#[derive(Clone, Deserialize)]
pub(crate) struct VersionedConfigure {
    pub version: String,
    pub status: i32,
    pub data: Option<serde_json::Value>,
}

pub const USER_LOGOUT: Contract = Contract {
    retry: RetryPolicy::Replayable,
    method: Method::Post,
    path: "/api/v1/user/logout",
    identity: IdentityScope::AccountDevice,
    timeout: REQUEST_TIMEOUT,
    json_content_type: false,
};

pub const DEVICE_WINDOWS_INIT: Contract = Contract {
    retry: RetryPolicy::Replayable,
    method: Method::Post,
    path: "/api/v1/device/windows/init",
    identity: IdentityScope::Platform,
    timeout: REQUEST_TIMEOUT,
    json_content_type: true,
};

pub const DEVICE_LIST: Contract = Contract {
    retry: RetryPolicy::Replayable,
    method: Method::Get,
    path: "/api/v1/device/list",
    identity: IdentityScope::AccountDevice,
    timeout: REQUEST_TIMEOUT,
    json_content_type: false,
};

pub const DEVICE_GROUPS: Contract = Contract {
    path: "/api/v1/device/groups/of/my",
    ..DEVICE_LIST
};

pub const DEVICE_DETAIL: Contract = Contract {
    path: "/api/v1/device",
    ..DEVICE_LIST
};

const DEVICE_RENAME: Contract = Contract {
    retry: RetryPolicy::ConnectOnly,
    method: Method::Put,
    json_content_type: true,
    ..DEVICE_DETAIL
};
const DEVICE_UNBIND: Contract = Contract {
    retry: RetryPolicy::ConnectOnly,
    method: Method::Post,
    json_content_type: true,
    ..DEVICE_DETAIL
};

pub const DEVICE_CONTROLLABLE: Contract = Contract {
    retry: RetryPolicy::Replayable,
    method: Method::Post,
    path: "/api/v1/device/controllable",
    identity: IdentityScope::AccountDevice,
    timeout: Duration::from_secs(2),
    json_content_type: true,
};

pub const ROOM_CREATE: Contract = Contract {
    retry: RetryPolicy::Replayable,
    method: Method::Post,
    path: "/api/v1/room/create",
    identity: IdentityScope::AccountDevice,
    timeout: REQUEST_TIMEOUT,
    json_content_type: true,
};

pub const ROOM_JOIN_BY_DEVICE: Contract = Contract {
    retry: RetryPolicy::Replayable,
    method: Method::Post,
    path: "/api/v1/room/join/by_device",
    identity: IdentityScope::AccountDevice,
    timeout: REQUEST_TIMEOUT,
    json_content_type: true,
};

pub fn url(contract: Contract) -> String {
    format!("{BASE_URL}{}", contract.path)
}

pub const CLIENT_ID_HEADER: &str = "X-Param-client-id";
pub const DEVICE_ID_HEADER: &str = "X-Param-device-id";
pub const SYSTEM_ID_HEADER: &str = "X-Param-system-id";
pub const USER_ID_HEADER: &str = "X-Param-user-id";

pub fn validate_device_id(id: &str) -> Result<()> {
    if id.len() != 16
        || !id
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
    {
        bail!("invalid device_id");
    }
    Ok(())
}

#[derive(Clone)]
pub struct ClientIdentity {
    pub client_id: String,
    pub device_id: String,
    pub system_id: String,
}

impl ClientIdentity {
    pub fn new(
        client_id: impl Into<String>,
        device_id: impl Into<String>,
        system_id: impl Into<String>,
    ) -> Result<Self> {
        let identity = Self {
            client_id: client_id.into(),
            device_id: device_id.into(),
            system_id: system_id.into(),
        };
        if identity.client_id.is_empty() || identity.system_id.is_empty() {
            bail!("client_id and system_id must not be empty");
        }
        Ok(identity)
    }
}

impl fmt::Debug for ClientIdentity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ClientIdentity")
            .field("client_id", &"***REDACTED***")
            .field("device_id", &"***REDACTED***")
            .field("system_id", &"***REDACTED***")
            .finish()
    }
}

#[derive(Clone, Deserialize)]
pub struct ApiEnvelope<T> {
    pub code: i32,
    #[serde(default)]
    pub msg: String,
    pub data: Option<T>,
}

#[derive(Clone, Debug)]
pub struct ApiFailure {
    pub code: i32,
    pub message: String,
}

impl fmt::Display for ApiFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "NRD API returned code {}: {}", self.code, self.message)
    }
}
impl std::error::Error for ApiFailure {}

impl ApiFailure {
    /// Only an authentication-restoration consumer may apply this policy.
    /// A generic room/device API failure does not end the account generation.
    pub(crate) fn invalid_saved_credentials(&self) -> bool {
        matches!(self.code, 1001 | 1002 | 1120)
    }
}

impl<T> ApiEnvelope<T> {
    pub fn into_data(self) -> Result<T> {
        if self.code != 0 {
            return Err(ApiFailure {
                code: self.code,
                message: self.msg,
            }
            .into());
        }
        self.data.context("NRD API response did not contain data")
    }
}

#[derive(Clone, Deserialize, Eq, PartialEq)]
pub struct LoginQrCode {
    pub qrcode_jump_url: String,
    pub token: String,
    #[serde(default)]
    pub status_query_ticket: Option<String>,
}

impl LoginQrCode {
    pub fn validate(&self) -> Result<()> {
        if self.qrcode_jump_url.is_empty()
            || self.token.is_empty()
            || self.status_ticket().is_empty()
        {
            bail!("QR response did not contain a valid jump URL and login/status ticket");
        }
        Ok(())
    }

    pub fn status_ticket(&self) -> &str {
        // The QR response supplies URL + token. Use a distinct status
        // ticket when the service supplies one; an explicitly empty one fails validation.
        self.status_query_ticket.as_deref().unwrap_or(&self.token)
    }
}

impl fmt::Debug for LoginQrCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("LoginQrCode")
            .field("qrcode_jump_url", &"***REDACTED***")
            .field("token", &"***REDACTED***")
            .field("status_query_ticket", &"***REDACTED***")
            .finish()
    }
}

#[derive(Clone, Serialize)]
pub struct LoginQrStatusRequest {
    pub qrcode_jump_url: String,
    pub login_status: i32,
    pub token: String,
}

impl LoginQrStatusRequest {
    pub fn initial(qr: &LoginQrCode) -> Self {
        Self {
            qrcode_jump_url: qr.qrcode_jump_url.clone(),
            login_status: 1,
            token: qr.status_ticket().to_owned(),
        }
    }
}

impl fmt::Debug for LoginQrStatusRequest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("LoginQrStatusRequest")
            .field("qrcode_jump_url", &"***REDACTED***")
            .field("login_status", &self.login_status)
            .field("token", &"***REDACTED***")
            .finish()
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq)]
pub struct LoginQrStatus {
    #[serde(default)]
    pub status: Option<i32>,
    #[serde(default)]
    pub login_status: Option<i32>,
}

impl LoginQrStatus {
    pub fn resolved_status(self) -> Result<i32> {
        match (self.status, self.login_status) {
            (Some(status), Some(login_status)) if status != login_status => bail!(
                "NRD API returned conflicting QR states: status={status}, login_status={login_status}"
            ),
            (Some(status), _) | (_, Some(status)) => Ok(status),
            (None, None) => bail!("NRD API QR response did not contain a status"),
        }
    }
}

#[derive(Clone, Serialize)]
pub struct LoginByQrRequest {
    pub qrcode_jump_url: String,
    pub token: String,
}

impl fmt::Debug for LoginByQrRequest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("LoginByQrRequest")
            .field("qrcode_jump_url", &"***REDACTED***")
            .field("token", &"***REDACTED***")
            .finish()
    }
}

#[derive(Clone, Deserialize)]
pub struct LoginResponse {
    pub token: String,
    pub user_id: String,
    #[serde(default)]
    pub nickname: String,
}

impl fmt::Debug for LoginResponse {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("LoginResponse")
            .field("token", &"***REDACTED***")
            .field("user_id", &"***REDACTED***")
            .field("nickname", &"***REDACTED***")
            .finish()
    }
}

#[derive(Clone, Serialize)]
pub struct WindowsDeviceInitRequest {
    pub name: String,
    pub client_id: String,
    pub system_id: String,
    pub machine_guid: String,
    pub os: String,
    pub base_board: String,
    pub cpu: String,
    pub video: Vec<String>,
    pub mac: String,
    pub memory: i64,
    pub screen: String,
    pub platform: i32,
    pub controllable: bool,
}

impl fmt::Debug for WindowsDeviceInitRequest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("WindowsDeviceInitRequest")
            .field("name", &self.name)
            .field("identity", &"***REDACTED***")
            .field("hardware", &"***REDACTED***")
            .field("platform", &self.platform)
            .field("controllable", &self.controllable)
            .finish()
    }
}

#[derive(Clone, Deserialize)]
pub struct DeviceInitResponse {
    pub device_id: String,
    #[serde(default)]
    pub alias: String,
}

impl DeviceInitResponse {
    pub fn validated_device_id(&self) -> Result<&str> {
        let valid = self.device_id.len() == 16
            && self
                .device_id
                .bytes()
                .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit());
        if !valid {
            bail!("device initialization returned an invalid device_id");
        }
        Ok(&self.device_id)
    }
}

impl fmt::Debug for DeviceInitResponse {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DeviceInitResponse")
            .field("device_id", &"***REDACTED***")
            .field("alias", &"***REDACTED***")
            .finish()
    }
}

/// Device-list shape returned by the current production API. This differs
/// from the account-management platform groups. Follow the official consumer:
/// current_device and my_binded_devices, not unused legacy server fields.
#[derive(Clone, Deserialize)]
pub struct DeviceList {
    pub current_device: DeviceInfo,
    #[serde(default)]
    pub my_binded_devices: Vec<DeviceInfo>,
}

/// Account management is not the connectable-device list. The official groups
/// include this client, mobile/tablet clients and TVs, without implying viewing
/// permission. Cloud devices use a different business API and are not included.
#[derive(Clone, Deserialize)]
pub struct DeviceGroups {
    pub current_device_id: String,
    #[serde(default)]
    pub desktop_devices: Vec<DeviceInfo>,
    #[serde(default)]
    pub mobile_devices: Vec<DeviceInfo>,
    #[serde(default)]
    pub tv_devices: Vec<DeviceInfo>,
}

impl DeviceGroups {
    pub fn entries(&self) -> impl Iterator<Item = (&'static str, &DeviceInfo)> {
        self.desktop_devices
            .iter()
            .map(|d| ("电脑", d))
            .chain(self.mobile_devices.iter().map(|d| ("手机 / 平板", d)))
            .chain(self.tv_devices.iter().map(|d| ("电视", d)))
    }
}

#[derive(Clone, Default, Deserialize)]
pub struct DeviceDetail {
    // NrdDevice::queryDeviceDetail consumes ordered string pairs. Preserve the
    // labels supplied by the server instead of inventing hardware JSON fields.
    #[serde(default, deserialize_with = "detail_pairs")]
    pub details: Vec<(String, String)>,
}

#[derive(Deserialize)]
pub struct RenamedDevice {
    pub device_id: String,
    pub alias: String,
}

fn detail_pairs<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Vec<(String, String)>, D::Error> {
    let value = serde_json::Value::deserialize(d)?;
    Ok(value
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|row| {
            let row = row.as_array()?;
            Some((
                row.first()?.as_str()?.to_owned(),
                row.get(1)?.as_str()?.to_owned(),
            ))
        })
        .collect())
}

impl fmt::Debug for DeviceList {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DeviceList")
            .field("current_device", &self.current_device)
            .field("my_binded_devices", &self.my_binded_devices)
            .finish()
    }
}

#[derive(Clone, Deserialize)]
pub struct DeviceInfo {
    pub device_id: String,
    #[serde(default)]
    pub alias: String,
    #[serde(default)]
    pub status: String,
    #[serde(default)]
    pub platform: i32,
    #[serde(default)]
    pub controllable: bool,
    #[serde(default)]
    pub controlled_support: bool,
    #[serde(default)]
    pub support_wol: bool,
    #[serde(default)]
    pub version_name: String,
    #[serde(default)]
    pub wallpaper_url: String,
    #[serde(default)]
    pub update_started_at: i64,
    #[serde(default)]
    pub participants_info: Vec<serde_json::Value>,
}

impl DeviceInfo {
    pub fn is_connected(&self) -> bool {
        self.status == "CONNECTED"
    }

    pub fn participant_count(&self) -> usize {
        self.participants_info.len()
    }

    pub fn platform_label(&self) -> String {
        match self.platform {
            1 => "Windows".to_owned(),
            2 => "Android".to_owned(),
            3 => "iOS".to_owned(),
            4 => "macOS".to_owned(),
            5 => "TV".to_owned(),
            value => format!("未知平台 ({value})"),
        }
    }

    pub fn status_label(&self) -> &str {
        match self.status.as_str() {
            "CONNECTED" => "在线",
            "DISCONNECTED" => "离线",
            "" => "未提供",
            _ => &self.status,
        }
    }

    pub fn validated_device_id(&self) -> Result<&str> {
        let valid = self.device_id.len() == 16
            && self
                .device_id
                .bytes()
                .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit());
        if !valid {
            bail!("device list returned an invalid device_id");
        }
        Ok(&self.device_id)
    }
}

impl fmt::Debug for DeviceInfo {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DeviceInfo")
            .field("device_id", &"***REDACTED***")
            .field("alias", &"***REDACTED***")
            .field("status", &self.status)
            .field("platform", &self.platform)
            .field("controllable", &self.controllable)
            .field("controlled_support", &self.controlled_support)
            .field("support_wol", &self.support_wol)
            .field("version_name", &self.version_name)
            .field("wallpaper_url", &"***REDACTED***")
            .field("update_started_at", &self.update_started_at)
            .field("participant_count", &self.participants_info.len())
            .finish()
    }
}

#[derive(Clone, Copy, Debug, Serialize)]
pub struct JoinByDeviceRequest {
    pub force_join: bool,
}

#[derive(Clone, Copy, Debug, Serialize)]
pub struct SetControllableRequest {
    pub controllable: bool,
}

#[derive(Clone, Copy, Debug, Serialize)]
pub struct CreateRoomRequest {
    pub last_controlled_interval: i64,
}

#[derive(Clone, Deserialize)]
pub struct RoomSession {
    #[serde(default)]
    pub(crate) international_connect: bool,
    #[serde(skip)]
    local_session: bool,
    #[serde(skip)]
    self_hosted_session: bool,
    token: String,
    pub ws_connect_timeout_ms: i32,
    pub streamer_retry_delta_ms: i32,
    pub max_reconnect_delta: i32,
    #[serde(default)]
    signaling_list: Vec<String>,
    #[serde(default)]
    signaling_server: String,
}

impl RoomSession {
    pub(crate) fn from_assist(reply: &crate::account::assist::JoinReply, self_hosted: bool) -> Self {
        Self {
            international_connect: reply.international_connect,
            local_session: false,
            self_hosted_session: self_hosted,
            token: reply.token.clone(),
            ws_connect_timeout_ms: reply.ws_connect_timeout_ms,
            streamer_retry_delta_ms: reply.streamer_retry_delta_ms,
            max_reconnect_delta: 0,
            signaling_list: reply.signaling_list.clone(),
            signaling_server: String::new(),
        }
    }
    pub(crate) fn from_local(ticket: String, signaling_url: String) -> Result<Self> {
        let room = Self {
            international_connect: false,
            local_session: true,
            self_hosted_session: true,
            token: ticket,
            ws_connect_timeout_ms: 10_000,
            streamer_retry_delta_ms: 1_000,
            max_reconnect_delta: 300,
            signaling_list: Vec::new(),
            signaling_server: signaling_url,
        };
        room.validate()?;
        Ok(room)
    }
    pub fn validate(&self) -> Result<()> {
        if self.token.is_empty() {
            bail!("room response did not contain an authorization token");
        }
        if self.signaling_list.iter().all(String::is_empty) && self.signaling_server.is_empty() {
            bail!("room response did not contain a signaling server");
        }
        Ok(())
    }

    pub(crate) fn token(&self) -> &str {
        &self.token
    }

    pub(crate) fn is_local(&self) -> bool {
        self.local_session
    }

    pub(crate) fn is_self_hosted(&self) -> bool {
        self.self_hosted_session
    }

    pub(crate) fn signaling_endpoints(&self) -> Vec<&str> {
        let endpoints = self
            .signaling_list
            .iter()
            .map(String::as_str)
            .filter(|value| !value.is_empty())
            .collect::<Vec<_>>();
        if endpoints.is_empty() && !self.signaling_server.is_empty() {
            vec![self.signaling_server.as_str()]
        } else {
            endpoints
        }
    }
}

impl fmt::Debug for RoomSession {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RoomSession")
            .field("token", &"***REDACTED***")
            .field("ws_connect_timeout_ms", &self.ws_connect_timeout_ms)
            .field("streamer_retry_delta_ms", &self.streamer_retry_delta_ms)
            .field("max_reconnect_delta", &self.max_reconnect_delta)
            .field(
                "signaling_endpoint_count",
                &self.signaling_endpoints().len(),
            )
            .finish_non_exhaustive()
    }
}

#[derive(Clone)]
pub struct NrdApi {
    backend: Backend,
    identity: ClientIdentity,
    version_name: String,
    channel: String,
    user_id: Option<String>,
    guest_id: Option<String>,
    authorization: Option<HeaderValue>,
}

mod self_hosted;

#[derive(Clone)]
enum Backend {
    Official(crate::account::nrd_http::NrdHttp),
    SelfHosted(self_hosted::DesktopApi),
}

impl NrdApi {
    pub fn new(identity: ClientIdentity) -> Result<Self> {
        Ok(Self::with_backend(identity, Backend::Official(crate::account::nrd_http::NrdHttp::new()?)))
    }

    pub(crate) fn self_hosted(identity: ClientIdentity, settings: crate::account::self_hosted::SelfHosted) -> Result<Self> {
        Ok(Self::with_backend(identity, Backend::SelfHosted(self_hosted::DesktopApi::new(settings)?)))
    }

    fn with_backend(identity: ClientIdentity, backend: Backend) -> Self {
        Self {
            backend,
            identity,
            version_name: PROTOCOL_VERSION.to_owned(),
            channel: DEFAULT_CHANNEL.to_owned(),
            user_id: None,
            guest_id: None,
            authorization: None,
        }
    }

    fn official_http(&self) -> Result<&crate::account::nrd_http::NrdHttp> {
        match &self.backend {
            Backend::Official(http) => Ok(http),
            Backend::SelfHosted(_) => bail!("当前自建服务器未提供该功能"),
        }
    }

    pub(crate) fn is_self_hosted(&self) -> bool {
        matches!(self.backend, Backend::SelfHosted(_))
    }

    pub fn set_identity(&mut self, identity: ClientIdentity) {
        self.identity = identity;
    }

    pub fn set_user_id(&mut self, user_id: Option<&str>) -> Result<()> {
        match user_id {
            Some("") => bail!("user_id must not be empty"),
            Some(user_id) => {
                HeaderValue::from_str(user_id).context("user_id is not a valid header value")?;
                self.user_id = Some(user_id.to_owned());
                self.guest_id = None;
            }
            None => self.user_id = None,
        }
        Ok(())
    }

    pub fn set_authorization(&mut self, value: Option<&str>) -> Result<()> {
        self.authorization = value
            .map(HeaderValue::from_str)
            .transpose()
            .context("invalid Authorization header")?;
        if let Some(value) = &mut self.authorization {
            value.set_sensitive(true);
        }
        Ok(())
    }

    pub fn set_bearer_token(&mut self, token: Option<&str>) -> Result<()> {
        match token {
            Some("") => bail!("bearer token must not be empty"),
            Some(token) => self.set_authorization(Some(&format!("Bearer {token}"))),
            None => self.set_authorization(None),
        }
    }

    pub async fn generate_login_qr(&self) -> Result<ApiEnvelope<LoginQrCode>> {
        self.send(LOGIN_QR_GENERATE, LOGIN_QR_GENERATE.path, Vec::new())
            .await
    }

    pub(crate) async fn query_configures(
        &self,
        versions: &[(String, String)],
    ) -> Result<ApiEnvelope<std::collections::BTreeMap<String, VersionedConfigure>>> {
        if versions.is_empty() || versions.len() > 30 {
            bail!("configuration query requires 1..=30 names");
        }
        let configures = versions
            .iter()
            .map(|(name, version)| serde_json::json!({"name": name, "version": version}))
            .collect::<Vec<_>>();
        self.post_json(
            CONFIGURES_WITH_VERSION,
            CONFIGURES_WITH_VERSION.path,
            &serde_json::json!({"configures": configures}),
        )
        .await
    }

    pub(crate) async fn trigger_controlled_update(
        &self,
        device_id: &str,
        immediate: bool,
    ) -> Result<ApiEnvelope<serde_json::Value>> {
        validate_device_id(device_id)?;
        let contract = Contract {
            retry: RetryPolicy::ConnectOnly,
            method: Method::Post,
            path: "/api/v1/release/trigger",
            identity: IdentityScope::AccountDevice,
            timeout: REQUEST_TIMEOUT,
            json_content_type: true,
        };
        // Official 4.40.1 sendRealseTrigger: extra is JSON encoded as a string.
        // The feature-unavailable dialog uses scene=function for both buttons.
        self.post_json(
            contract,
            &format!("{}/{device_id}/update", contract.path),
            &serde_json::json!({
                "type": 1,
                "scene": "function",
                "extra": serde_json::json!({"updateType": if immediate {1} else {2}}).to_string(),
            }),
        )
        .await
    }

    pub async fn get_login_qr_status(
        &self,
        state: &LoginQrStatusRequest,
    ) -> Result<ApiEnvelope<LoginQrStatus>> {
        self.post_json(LOGIN_QR_STATUS, LOGIN_QR_STATUS.path, state)
            .await
    }

    pub async fn login_by_qr(
        &self,
        request: &LoginByQrRequest,
    ) -> Result<ApiEnvelope<LoginResponse>> {
        self.post_json(LOGIN_BY_QR, LOGIN_BY_QR.path, request).await
    }

    pub(crate) async fn request_sms_code(&self, country_code: &str, mobile: &str) -> Result<()> {
        let response: ApiEnvelope<serde_json::Value> = self.post_json(
            LOGIN_SMS_CODE, LOGIN_SMS_CODE.path,
            &serde_json::json!({"country_code": country_code, "mobile": mobile, "type": "login"}),
        ).await?;
        // The official callback consumes CommonResponse, which need not have data.
        if response.code != 0 {
            return Err(ApiFailure {
                code: response.code,
                message: response.msg,
            }
            .into());
        }
        Ok(())
    }

    pub(crate) async fn login_by_mobile(
        &self,
        country_code: &str,
        mobile: &str,
        code: &str,
    ) -> Result<ApiEnvelope<LoginResponse>> {
        self.post_json(
            LOGIN_BY_MOBILE,
            LOGIN_BY_MOBILE.path,
            &serde_json::json!({"country_code": country_code, "mobile": mobile, "code": code}),
        )
        .await
    }

    pub async fn get_user_info(&self) -> Result<ApiEnvelope<serde_json::Value>> {
        self.send(USER_INFO, USER_INFO.path, Vec::new()).await
    }

    pub async fn logout(&self) -> Result<()> {
        // Server AE8610: POST, authenticated device headers, empty body.
        // A successful logout need not contain a data object.
        let response: ApiEnvelope<serde_json::Value> =
            self.send(USER_LOGOUT, USER_LOGOUT.path, Vec::new()).await?;
        if response.code != 0 {
            return Err(ApiFailure {
                code: response.code,
                message: response.msg,
            }
            .into());
        }
        Ok(())
    }

    pub async fn init_windows_device(
        &self,
        request: &WindowsDeviceInitRequest,
    ) -> Result<ApiEnvelope<DeviceInitResponse>> {
        self.post_json(DEVICE_WINDOWS_INIT, DEVICE_WINDOWS_INIT.path, request)
            .await
    }

    pub(crate) async fn register_self_hosted(
        &self,
        hardware: &crate::platform::windows::device_profile::Hardware,
    ) -> Result<ApiEnvelope<DeviceInitResponse>> {
        anyhow::ensure!(self.is_self_hosted(), "自建设备注册需要自建服务地址");
        self.post_json(DEVICE_WINDOWS_INIT, DEVICE_WINDOWS_INIT.path, &serde_json::json!({
            "name": hardware.name,
            "os": hardware.os,
            "cpu": hardware.cpu,
            "video": hardware.video,
            "screen": hardware.screen,
            "memory": hardware.memory,
            "version_name": crate::protocol::official_version::CLIENT,
        })).await
    }

    pub async fn list_devices(&self) -> Result<ApiEnvelope<DeviceList>> {
        self.send(DEVICE_LIST, DEVICE_LIST.path, Vec::new()).await
    }

    pub async fn device_groups(&self) -> Result<ApiEnvelope<DeviceGroups>> {
        self.send(DEVICE_GROUPS, DEVICE_GROUPS.path, Vec::new())
            .await
    }

    pub async fn device_detail(&self, device_id: &str) -> Result<ApiEnvelope<DeviceDetail>> {
        validate_device_id(device_id)?;
        self.send(
            DEVICE_DETAIL,
            &format!("/api/v1/device/{device_id}/detail"),
            Vec::new(),
        )
        .await
    }

    pub async fn rename_device(&self, id: &str, alias: &str) -> Result<ApiEnvelope<RenamedDevice>> {
        validate_device_id(id)?;
        if alias.trim().is_empty() || alias.chars().any(char::is_control) {
            bail!("设备名称不能为空或包含控制字符");
        }
        self.post_json(
            DEVICE_RENAME,
            &format!("/api/v1/device/{id}"),
            &serde_json::json!({"alias":alias}),
        )
        .await
    }

    pub async fn unbind_device(&self, id: &str) -> Result<ApiEnvelope<serde_json::Value>> {
        validate_device_id(id)?;
        let path = format!("/api/v1/device/{id}/unbind");
        let mut response: ApiEnvelope<serde_json::Value> =
            self.send(DEVICE_UNBIND, &path, Vec::new()).await?;
        // This official callback consumes CommonResponse, not a data object.
        if response.code == 0 && response.data.is_none() {
            response.data = Some(serde_json::Value::Null);
        }
        Ok(response)
    }

    pub async fn set_controllable(
        &self,
        controllable: bool,
    ) -> Result<ApiEnvelope<serde_json::Value>> {
        self.post_json(
            DEVICE_CONTROLLABLE,
            DEVICE_CONTROLLABLE.path,
            &SetControllableRequest { controllable },
        )
        .await
    }

    pub async fn create_room(
        &self,
        last_controlled_interval: i64,
    ) -> Result<ApiEnvelope<RoomSession>> {
        if last_controlled_interval < -1 {
            bail!("last_controlled_interval must be -1 (never controlled) or elapsed seconds");
        }
        let mut response = self.post_json::<_, RoomSession>(
            ROOM_CREATE,
            ROOM_CREATE.path,
            &CreateRoomRequest {
                last_controlled_interval,
            },
        )
        .await?;
        self.configure_self_hosted_room(&mut response)?;
        Ok(response)
    }

    pub async fn join_by_device(
        &self,
        device_id: &str,
        force_join: bool,
    ) -> Result<ApiEnvelope<RoomSession>> {
        if device_id.is_empty()
            || !device_id
                .bytes()
                .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit())
        {
            bail!("device_id must be a lowercase alphanumeric identifier");
        }
        let path = format!("{}/{device_id}", ROOM_JOIN_BY_DEVICE.path);
        let mut response = self.send::<RoomSession>(
            Contract {
                // Forcing a join can displace an existing viewer. Preserve
                // its stronger no-replay boundary even though normal joins
                // use the official common recovery path.
                retry: if force_join {
                    RetryPolicy::ConnectOnly
                } else {
                    ROOM_JOIN_BY_DEVICE.retry
                },
                ..ROOM_JOIN_BY_DEVICE
            },
            &path,
            serde_json::to_vec(&JoinByDeviceRequest { force_join })?,
        )
        .await?;
        self.configure_self_hosted_room(&mut response)?;
        Ok(response)
    }

    fn configure_self_hosted_room(&self, response: &mut ApiEnvelope<RoomSession>) -> Result<()> {
        if let (Backend::SelfHosted(api), Some(room)) = (&self.backend, &mut response.data) {
            room.self_hosted_session = true;
            room.signaling_list.clear();
            room.signaling_server = api.signaling_url()?;
        }
        Ok(())
    }

    async fn post_json<B, T>(
        &self,
        contract: Contract,
        path: &str,
        body: &B,
    ) -> Result<ApiEnvelope<T>>
    where
        B: Serialize + ?Sized,
        T: DeserializeOwned,
    {
        let body = serde_json::to_vec(body).context("failed to encode compact request JSON")?;
        self.send(contract, path, body).await
    }

    async fn send<T>(&self, contract: Contract, path: &str, body: Vec<u8>) -> Result<ApiEnvelope<T>>
    where
        T: DeserializeOwned,
    {
        if !path.starts_with('/') {
            bail!("API path must start with '/'");
        }
        if let Backend::SelfHosted(api) = &self.backend {
            return api.send(contract, path, body).await;
        }
        let method = match contract.method {
            Method::Get => HttpMethod::GET,
            Method::Post => HttpMethod::POST,
            Method::Put => HttpMethod::PUT,
            Method::Delete => HttpMethod::DELETE,
        };
        let mut headers = self.signed_headers(method.as_str(), path, &body, contract.identity)?;
        if contract.json_content_type {
            headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
        }
        if contract.identity == IdentityScope::AccountDevice
            && let Some(value) = &self.authorization
        {
            headers.insert(AUTHORIZATION, value.clone());
        }
        let (status, bytes, _) = self
            .official_http()?
            .send(
                method,
                path,
                headers,
                body,
                contract.timeout,
                contract.retry,
            )
            .await?;
        let envelope: ApiEnvelope<serde_json::Value> = serde_json::from_slice(&bytes)
            .with_context(|| format!("failed to decode NRD API JSON response (HTTP {status})"))?;
        // Official response consumers classify HTTP/business status before
        // deserializing success data. A revoked device returns data={} rather
        // than DeviceList; that must not hide its 1120 behind a missing field.
        let code = if status != reqwest::StatusCode::OK && envelope.code == 0 {
            i32::from(status.as_u16())
        } else {
            envelope.code
        };
        let waiting_for_confirmation = assist::is_join(contract) && code == 1136;
        if code != 0 && !waiting_for_confirmation {
            if contract == LOGIN_SMS_CODE
                || contract == LOGIN_BY_MOBILE
                || assist::is_sensitive(contract)
                || host_assist::is_sensitive(contract)
                || guest::is_sensitive(contract)
                || contract == wallpaper::TOKEN
                || contract == wallpaper::BIND
            {
                // A service error may echo the phone or OTP. Never log its text.
                tracing::debug!(path, %status, code, "NRD phone login request failed");
            } else {
                tracing::debug!(path, %status, code, message = %envelope.msg, "NRD business request failed");
            }
            return Ok(ApiEnvelope {
                code,
                msg: envelope.msg,
                data: None,
            });
        }
        let data = envelope
            .data
            .map(serde_json::from_value)
            .transpose()
            .with_context(|| {
                format!("failed to decode successful NRD API data ({path}, HTTP {status})")
            })?;
        Ok(ApiEnvelope {
            code,
            msg: envelope.msg,
            data,
        })
    }

    fn signed_headers(
        &self,
        method: &str,
        path: &str,
        body: &[u8],
        scope: IdentityScope,
    ) -> Result<HeaderMap> {
        self.signed_headers_with_nonce(method, path, body, scope, None)
    }

    fn signed_headers_with_nonce(
        &self,
        method: &str,
        path: &str,
        body: &[u8],
        scope: IdentityScope,
        nonce: Option<&str>,
    ) -> Result<HeaderMap> {
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .context("system clock is before the Unix epoch")?
            .as_secs();
        let mut pairs = self.common_header_pairs(timestamp, scope);
        if let Some(nonce) = nonce {
            pairs.push(("X-Param-Nonce".into(), nonce.into()));
        }
        let signature = make_signature(method, path, &pairs, body)?;
        let mut headers = HeaderMap::new();
        for (name, value) in pairs {
            let name = HeaderName::from_bytes(name.as_bytes())
                .with_context(|| format!("invalid protocol header name: {name}"))?;
            let value = HeaderValue::from_str(&value)
                .context("protocol identity contains an invalid header value")?;
            headers.insert(name, value);
        }
        headers.insert(
            HeaderName::from_static("x-param-sign"),
            HeaderValue::from_str(&signature).expect("hex HMAC is a valid header value"),
        );
        Ok(headers)
    }

    fn common_header_pairs(&self, timestamp: u64, scope: IdentityScope) -> Vec<(String, String)> {
        let mut pairs = vec![
            ("X-Param-PLAT".into(), "1".into()),
            ("X-Param-VN".into(), self.version_name.clone()),
            // VC is the last component of the version name sent in VN.
            (
                "X-Param-VC".into(),
                self.version_name
                    .rsplit('.')
                    .next()
                    .unwrap_or_default()
                    .into(),
            ),
            ("X-Param-PKGN".into(), PACKAGE_NAME.into()),
            ("X-Param-CHN".into(), self.channel.clone()),
            ("X-Param-LANG".into(), "zh-CN".into()),
            ("X-Param-CNT".into(), "zh-CN".into()),
            ("X-Param-REL".into(), "prod".into()),
            ("X-Param-OPR".into(), String::new()),
            ("X-Param-TS".into(), timestamp.to_string()),
        ];
        if scope != IdentityScope::Platform {
            pairs.extend([
                (CLIENT_ID_HEADER.into(), self.identity.client_id.clone()),
                (DEVICE_ID_HEADER.into(), self.identity.device_id.clone()),
                (SYSTEM_ID_HEADER.into(), self.identity.system_id.clone()),
            ]);
        }
        if scope == IdentityScope::AccountDevice
            && let Some(user_id) = &self.user_id
        {
            pairs.push((USER_ID_HEADER.into(), user_id.clone()));
        }
        if scope == IdentityScope::AccountDevice
            && let Some(guest_id) = &self.guest_id
        {
            pairs.push(("X-Param-Guest-ID".into(), guest_id.clone()));
        }
        pairs
    }
}

impl fmt::Debug for NrdApi {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("NrdApi")
            .field("endpoints", &"NRD HTTPS primary/backup")
            .field("identity", &self.identity)
            .field("version_name", &self.version_name)
            .field("channel", &self.channel)
            .field("user_id", &self.user_id.as_ref().map(|_| "***REDACTED***"))
            .field(
                "authorization",
                &self.authorization.as_ref().map(|_| "***REDACTED***"),
            )
            .finish_non_exhaustive()
    }
}

fn make_signature(
    method: &str,
    path: &str,
    headers: &[(String, String)],
    body: &[u8],
) -> Result<String> {
    let mut signed_headers: Vec<(String, &str)> = headers
        .iter()
        .filter_map(|(name, value)| {
            let name = name.to_ascii_lowercase();
            (name.starts_with("x-param") && name != "x-param-sign")
                .then_some((name, value.as_str()))
        })
        .collect();
    signed_headers.sort_unstable_by(|left, right| left.0.cmp(&right.0));
    let canonical = signed_headers
        .into_iter()
        .map(|(name, value)| format!("{name}={value}"))
        .collect::<Vec<_>>()
        .join("&");

    let mut mac = <HmacSha256 as Mac>::new_from_slice(SIGNING_KEY)
        .context("failed to initialize request signer")?;
    mac.update(method.to_ascii_uppercase().as_bytes());
    mac.update(path.as_bytes());
    mac.update(canonical.as_bytes());
    mac.update(body);
    Ok(hex_lower(&mac.finalize().into_bytes()))
}

fn hex_lower(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut output = String::with_capacity(bytes.len() * 2);
    for &byte in bytes {
        output.push(HEX[(byte >> 4) as usize] as char);
        output.push(HEX[(byte & 0x0f) as usize] as char);
    }
    output
}
