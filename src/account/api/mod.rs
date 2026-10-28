//! Farwing Server control-plane client (Farwing Protocol v2, HTTPS).
//!
//! Every request carries the protocol version and a request ID. Device-scoped
//! requests are signed with this installation's Ed25519 key; account requests
//! additionally carry the account (or guest) bearer token, which the server
//! binds to the signing device. Server errors are returned as [`ApiFailure`]
//! with a machine-readable [`ErrorCode`]. Nothing here retries a request the
//! server may have executed: callers decide whether an operation is replayed.

use std::{
    fmt,
    sync::{
        Arc,
        atomic::{AtomicI64, Ordering},
    },
    time::Duration,
};

use anyhow::{Context, Result, bail};
use bytes::Bytes;
use farwing_protocol::Platform;
use farwing_protocol::api::{self as wire, path};
use farwing_protocol::crypto::DeviceKey;
use reqwest::{
    Method,
    header::{AUTHORIZATION, HeaderMap, HeaderName, HeaderValue},
};
use serde::{Deserialize, Serialize, de::DeserializeOwned, de::IgnoredAny};

pub use farwing_protocol::ErrorCode;
pub use farwing_protocol::api::{
    AssistMode, DeviceInitRequest, DeviceInitResponse, DevicePolicy,
    Purpose, Relation, ReleaseInfo, SavedDevice as WireSavedDevice,
};

use crate::account::http::ServerHttp;
use crate::account::server::ServerConfig;

pub const PROTOCOL_VERSION: u32 = farwing_protocol::PROTOCOL_VERSION;

const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
/// Records per page (`GET /records`, `GET /me/events`).
pub(crate) const RECORD_PAGE: usize = 50;
const SHORT_TIMEOUT: Duration = Duration::from_secs(5);
const LONG_POLL_TIMEOUT: Duration =
    Duration::from_millis(farwing_protocol::limits::ASSIST_DECISION_WAIT_MS + 10_000);
const JOIN_TIMEOUT: Duration =
    Duration::from_millis(farwing_protocol::limits::ASSIST_HOST_REPLY_WAIT_MS + 10_000);
const MAX_RESPONSE_BYTES: usize = 8 * 1024 * 1024;

/// Difference between the server clock and the local clock, learned from
/// `GET /server`. Request signatures carry server-relative time.
static CLOCK_OFFSET_MS: AtomicI64 = AtomicI64::new(0);
/// A correction smaller than this does not justify replaying a rejected request.
const CLOCK_RETRY_THRESHOLD_MS: i64 = 60_000;

/// Last timestamp signed by this process.
static LAST_SIGNED_MS: AtomicI64 = AtomicI64::new(0);

/// Server-relative time for a signature, strictly increasing within this
/// process: identical requests in the same millisecond would otherwise carry
/// identical signatures, which the server rejects as replays.
fn signed_now_ms() -> i64 {
    let now = chrono::Utc::now().timestamp_millis() + CLOCK_OFFSET_MS.load(Ordering::Relaxed);
    let mut last = LAST_SIGNED_MS.load(Ordering::Relaxed);
    loop {
        let next = now.max(last + 1);
        match LAST_SIGNED_MS.compare_exchange_weak(last, next, Ordering::AcqRel, Ordering::Relaxed) {
            Ok(_) => return next,
            Err(current) => last = current,
        }
    }
}

pub fn validate_device_id(id: &str) -> Result<()> {
    if !farwing_protocol::ids::is_device_id(id) {
        bail!("invalid device_id");
    }
    Ok(())
}

/// Local installation identity: installation ID, device key and the server
/// device ID once registered.
#[derive(Clone)]
pub struct ClientIdentity {
    /// Installation ID (UUID). Stable per profile; not derived from hardware.
    pub client_id: String,
    /// Server device ID; empty until registered.
    pub device_id: String,
    key: Arc<DeviceKey>,
}

impl ClientIdentity {
    pub fn new(
        client_id: impl Into<String>,
        device_id: impl Into<String>,
        key_seed: &[u8; 32],
    ) -> Result<Self> {
        let identity = Self {
            client_id: client_id.into(),
            device_id: device_id.into(),
            key: Arc::new(DeviceKey::from_seed(key_seed)),
        };
        if identity.client_id.is_empty() {
            bail!("installation id must not be empty");
        }
        Ok(identity)
    }

    pub fn registered(&self) -> bool {
        !self.device_id.is_empty()
    }

    /// base64url Ed25519 public key of this installation.
    pub fn public_key(&self) -> String {
        self.key.public_key()
    }

    /// Detached base64url signature with the device key.
    pub(crate) fn sign(&self, message: &[u8]) -> String {
        self.key.sign(message)
    }

    /// The same key under another device ID: a device without a server
    /// registration identifies itself by an ID derived from its key (§12.7).
    pub(crate) fn with_device_id(&self, device_id: impl Into<String>) -> Self {
        Self {
            client_id: self.client_id.clone(),
            device_id: device_id.into(),
            key: Arc::clone(&self.key),
        }
    }
}

impl fmt::Debug for ClientIdentity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ClientIdentity")
            .field("client_id", &"***REDACTED***")
            .field("device_id", &"***REDACTED***")
            .field("registered", &self.registered())
            .finish()
    }
}

/// A server-reported failure, or a local network failure (`ErrorCode::Network`).
///
/// Shown (`Display`) as the text for `code` and `reason` in the interface
/// language (`crate::i18n::errors`); `message` is the sender's developer
/// text, for logs only.
#[derive(Clone, Debug)]
pub struct ApiFailure {
    pub code: ErrorCode,
    pub message: String,
    /// `details.reason`: which case of `code` this is
    /// (`farwing_protocol::error::reason`).
    pub reason: Option<String>,
    pub status: u16,
    pub details: serde_json::Value,
}

impl fmt::Display for ApiFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&crate::i18n::t().errors.text(
            self.code,
            self.reason.as_deref(),
            &self.details,
        ))
    }
}

impl std::error::Error for ApiFailure {}

impl ApiFailure {
    /// A failure found here; `message` is for logs.
    pub(crate) fn new(code: ErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            reason: None,
            status: code.http_status(),
            details: serde_json::Value::Null,
        }
    }

    /// A failure found here, of a particular case (`reason`) of `code`.
    pub(crate) fn with_reason(mut self, reason: &str) -> Self {
        self.reason = Some(reason.to_owned());
        if !self.details.is_object() {
            self.details = serde_json::json!({});
        }
        if let Some(details) = self.details.as_object_mut() {
            details.insert("reason".into(), reason.into());
        }
        self
    }

    /// Add one `details` field (a parameter of the reason's text).
    pub(crate) fn with_detail(mut self, key: &str, value: impl Into<serde_json::Value>) -> Self {
        if !self.details.is_object() {
            self.details = serde_json::json!({});
        }
        if let Some(details) = self.details.as_object_mut() {
            details.insert(key.to_owned(), value.into());
        }
        self
    }

    /// An error received from a server or a host (HTTPS, realtime or
    /// direct connection), keeping its code, reason and details.
    pub(crate) fn from_body(body: farwing_protocol::ErrorBody, status: u16) -> Self {
        Self {
            code: body.code,
            reason: body
                .reason()
                .map(|reason| reason.chars().filter(|c| !c.is_control()).take(64).collect()),
            message: sanitize_message(&body.message),
            status,
            details: body.details,
        }
    }

    /// An error received on the realtime plane or from a host over a
    /// direct connection (no HTTP status).
    pub(crate) fn received(body: farwing_protocol::ErrorBody) -> Self {
        let status = body.code.http_status();
        let failure = Self::from_body(body, status);
        tracing::debug!(code = %failure.code, reason = ?failure.reason, message = %failure.message, "peer reported an error");
        failure
    }

    pub(crate) fn network(error: &reqwest::Error) -> Self {
        use farwing_protocol::error::reason;
        let (message, reason) = if error.is_timeout() {
            ("timed out", Some(reason::TIMED_OUT))
        } else if error.is_connect() {
            ("cannot connect", None)
        } else {
            ("connection lost", Some(reason::CONNECTION_LOST))
        };
        tracing::debug!(error = %error, "Farwing HTTPS transport failed");
        let failure = Self {
            status: 0,
            ..Self::new(ErrorCode::Network, message)
        };
        match reason {
            Some(reason) => failure.with_reason(reason),
            None => failure,
        }
    }

    pub fn is(&self, code: ErrorCode) -> bool {
        self.code == code
    }

    /// Only an authentication-restoration consumer may apply this policy.
    pub(crate) fn invalid_saved_credentials(&self) -> bool {
        self.code.invalidates_login()
    }

    /// The device signature itself is no longer accepted.
    pub(crate) fn invalid_device(&self) -> bool {
        self.code == ErrorCode::DeviceUnauthorized
    }

    /// The account generation must be retired (logout elsewhere, device removed).
    pub(crate) fn retires_account(&self) -> bool {
        matches!(
            self.code,
            ErrorCode::SessionRevoked | ErrorCode::Unauthorized | ErrorCode::DeviceMismatch
        )
    }
}

/// Whether an error is a failure reported by (or while reaching) the server.
pub fn failure_code(error: &anyhow::Error) -> Option<ErrorCode> {
    error.downcast_ref::<ApiFailure>().map(|failure| failure.code)
}

// ------------------------------------------------------------- device model

/// Display label of a platform.
/// A device ID for display, in groups of four (`abcd efgh ijkl mnop`). The
/// ID is derived from the device key, so it is also its fingerprint.
pub fn device_id_text(device_id: &str) -> String {
    if device_id.len() == farwing_protocol::ids::DEVICE_ID_LEN && device_id.is_ascii() {
        device_id
            .as_bytes()
            .chunks(4)
            .map(|chunk| String::from_utf8_lossy(chunk).into_owned())
            .collect::<Vec<_>>()
            .join(" ")
    } else {
        device_id.to_owned()
    }
}

/// The device ID of a device key, for display; `未知` without a valid key.
pub fn key_id_text(public_key: &str) -> String {
    farwing_protocol::crypto::local_device_id(public_key)
        .map_or_else(|| crate::i18n::t().common.unknown.to_owned(), |id| device_id_text(&id))
}

/// A typed device ID: case, spaces and dashes are ignored.
pub fn parse_device_id(text: &str) -> Option<String> {
    let id: String = text
        .chars()
        .filter(|c| !matches!(c, ' ' | '-' | '\u{3000}'))
        .flat_map(char::to_lowercase)
        .collect();
    (id.len() == farwing_protocol::ids::DEVICE_ID_LEN
        && id.bytes().all(|b| matches!(b, b'a'..=b'z' | b'2'..=b'7')))
    .then_some(id)
}

pub fn platform_label(platform: Platform) -> &'static str {
    match platform {
        Platform::Windows => "Windows",
        Platform::Macos => "macOS",
        Platform::Linux => "Linux",
        Platform::Android => "Android",
        Platform::Ios => "iOS",
        Platform::Web => crate::i18n::t().session.platform_web,
        Platform::Unknown => crate::i18n::t().session.platform_unknown,
    }
}

/// A device as shown to this client: one of the account's devices, or a
/// friend's device this account holds grants on.
#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(from = "wire::Device", into = "wire::Device")]
pub struct DeviceInfo {
    pub device_id: String,
    pub alias: String,
    /// `online` or `offline`.
    pub status: String,
    pub platform: Platform,
    /// The owner allows remote control (`policy.control`).
    pub controllable: bool,
    /// The device has a host implementation (`host` capability).
    pub controlled_support: bool,
    pub support_wol: bool,
    pub version_name: String,
    pub capabilities: Vec<String>,
    pub policy: DevicePolicy,
    /// Server-relative, authenticated download path, or empty.
    pub wallpaper_url: String,
    pub participants_info: Vec<wire::Participant>,
    pub last_seen_at: i64,
    /// Owner of a friend's device; `None` for the account's own devices.
    pub owner: Option<wire::UserSummary>,
    /// Session capabilities this account holds on the device.
    pub granted: Vec<String>,
    /// base64url Ed25519 public key of the device.
    pub public_key: String,
}

impl From<wire::Device> for DeviceInfo {
    fn from(value: wire::Device) -> Self {
        Self {
            controllable: value.policy.control,
            controlled_support: value
                .capabilities
                .iter()
                .any(|c| c == farwing_protocol::capability::HOST),
            device_id: value.device_id,
            alias: value.alias,
            status: value.status,
            platform: value.platform,
            support_wol: value.wol_supported,
            version_name: value.version,
            capabilities: value.capabilities,
            policy: value.policy,
            wallpaper_url: value.wallpaper_url,
            participants_info: value.participants,
            last_seen_at: value.last_seen_at,
            owner: value.owner,
            granted: value.granted,
            public_key: value.public_key,
        }
    }
}

impl From<DeviceInfo> for wire::Device {
    fn from(value: DeviceInfo) -> Self {
        Self {
            device_id: value.device_id,
            alias: value.alias,
            status: value.status,
            platform: value.platform,
            version: value.version_name,
            capabilities: value.capabilities,
            policy: DevicePolicy {
                control: value.controllable,
                ..value.policy
            },
            wol_supported: value.support_wol,
            wallpaper_url: value.wallpaper_url,
            participants: value.participants_info,
            last_seen_at: value.last_seen_at,
            owner: value.owner,
            granted: value.granted,
            public_key: value.public_key,
        }
    }
}

impl DeviceInfo {
    pub fn is_connected(&self) -> bool {
        self.status == wire::STATUS_ONLINE
    }

    /// The server reports the device as offline (`"offline"`).
    pub fn is_offline(&self) -> bool {
        self.status == wire::STATUS_OFFLINE
    }

    pub fn participant_count(&self) -> usize {
        self.participants_info.len()
    }

    /// A friend's device rather than one of this account's.
    pub fn is_shared(&self) -> bool {
        self.owner.is_some()
    }

    /// This account holds session capability `cap` on the device.
    pub fn granted(&self, cap: &str) -> bool {
        self.granted.iter().any(|value| value == cap)
    }

    pub fn platform_label(&self) -> String {
        platform_label(self.platform).to_owned()
    }

    pub fn status_label(&self) -> &str {
        match self.status.as_str() {
            wire::STATUS_ONLINE => crate::i18n::t().common.online,
            wire::STATUS_OFFLINE => crate::i18n::t().common.offline,
            "" => crate::i18n::t().center.not_provided,
            _ => &self.status,
        }
    }

    pub fn validated_device_id(&self) -> Result<&str> {
        if !farwing_protocol::ids::is_device_id(&self.device_id) {
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
            .field("policy", &self.policy)
            .field("controlled_support", &self.controlled_support)
            .field("support_wol", &self.support_wol)
            .field("version_name", &self.version_name)
            .field("capabilities", &self.capabilities)
            .field("shared", &self.is_shared())
            .field("granted", &self.granted)
            .field("participant_count", &self.participants_info.len())
            .finish()
    }
}

/// `GET /devices`: this device, the account's other devices and friends'
/// devices shared with this account.
#[derive(Clone, Serialize, Deserialize)]
pub struct DeviceList {
    pub current_device: DeviceInfo,
    /// Other devices of this account.
    #[serde(default, rename = "own")]
    pub devices: Vec<DeviceInfo>,
    /// Friends' devices this account holds grants on.
    #[serde(default)]
    pub shared: Vec<DeviceInfo>,
}

/// Every device list shows devices by name, then by ID: a fixed order,
/// whatever order they arrive in.
pub fn list_order(name: &str, id: &str, other_name: &str, other_id: &str) -> std::cmp::Ordering {
    name.to_lowercase()
        .cmp(&other_name.to_lowercase())
        .then_with(|| name.cmp(other_name))
        .then_with(|| id.cmp(other_id))
}

impl DeviceInfo {
    /// See [`list_order`].
    pub fn list_order(&self, other: &Self) -> std::cmp::Ordering {
        list_order(&self.alias, &self.device_id, &other.alias, &other.device_id)
    }
}

impl DeviceList {
    /// In [`list_order`].
    pub fn sort(&mut self) {
        self.devices.sort_by(DeviceInfo::list_order);
        self.shared.sort_by(DeviceInfo::list_order);
    }

    /// Every listed device: the current one, own devices, then shared ones.
    pub fn all(&self) -> impl Iterator<Item = &DeviceInfo> {
        std::iter::once(&self.current_device)
            .chain(&self.devices)
            .chain(&self.shared)
    }

    /// A device this account may use `cap` on: one of its own devices, or a
    /// friend's device whose owner granted `cap`.
    pub fn with_capability(&self, device_id: &str, cap: &str) -> Result<&DeviceInfo> {
        if let Some(device) = self.devices.iter().find(|d| d.device_id == device_id) {
            return Ok(device);
        }
        let device = self
            .shared
            .iter()
            .find(|d| d.device_id == device_id)
            .ok_or_else(|| {
                ApiFailure::new(ErrorCode::DeviceNotFound, "device not in this account or shared")
            })?;
        if !device.granted.iter().any(|granted| granted == cap) {
            return Err(ApiFailure::new(ErrorCode::Forbidden, format!("{cap} not granted"))
                .with_reason(farwing_protocol::error::reason::NOT_GRANTED)
                .into());
        }
        Ok(device)
    }
}

impl fmt::Debug for DeviceList {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DeviceList")
            .field("current_device", &self.current_device)
            .field("devices", &self.devices)
            .field("shared", &self.shared)
            .finish()
    }
}

/// The account's devices (including this one) grouped for device management.
#[derive(Clone)]
pub struct DeviceGroups {
    pub current_device_id: String,
    pub desktop_devices: Vec<DeviceInfo>,
    pub mobile_devices: Vec<DeviceInfo>,
}

impl DeviceGroups {
    pub fn from_list(list: &DeviceList) -> Self {
        let mut groups = Self {
            current_device_id: list.current_device.device_id.clone(),
            desktop_devices: Vec::new(),
            mobile_devices: Vec::new(),
        };
        for device in std::iter::once(&list.current_device).chain(&list.devices) {
            groups.insert(device.clone());
        }
        groups
    }

    pub fn insert(&mut self, device: DeviceInfo) {
        let list = if device.platform.is_desktop() {
            &mut self.desktop_devices
        } else {
            &mut self.mobile_devices
        };
        let at = list.partition_point(|d| d.list_order(&device).is_lt());
        list.insert(at, device);
    }

    pub fn entries(&self) -> impl Iterator<Item = (&'static str, &DeviceInfo)> {
        self.desktop_devices
            .iter()
            .map(|d| (crate::i18n::t().session.group_computers, d))
            .chain(self.mobile_devices.iter().map(|d| (crate::i18n::t().session.group_mobile, d)))
    }
}

/// `GET /devices/{id}/detail`, flattened into labelled rows for display.
#[derive(Clone, Default)]
pub struct DeviceDetail {
    pub details: Vec<(DetailRow, String)>,
}

/// One row of a device's details; its label is in the reader's language.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DetailRow {
    Name,
    DeviceId,
    Owner,
    Platform,
    System,
    Version,
    Status,
    Registered,
    LastSeen,
    ConnectId,
    Processor,
    Board,
    Graphics,
    Memory,
    Screen,
    Mac,
}

impl DetailRow {
    pub fn label(self) -> &'static str {
        let text = &crate::i18n::t().session;
        match self {
            Self::Name => text.detail_name,
            Self::DeviceId => text.detail_device_id,
            Self::Owner => text.detail_owner,
            Self::Platform => text.detail_platform,
            Self::System => text.detail_system,
            Self::Version => text.detail_version,
            Self::Status => text.detail_status,
            Self::Registered => text.detail_registered,
            Self::LastSeen => text.detail_last_seen,
            Self::ConnectId => text.detail_connect_id,
            Self::Processor => text.detail_processor,
            Self::Board => text.detail_board,
            Self::Graphics => text.detail_graphics,
            Self::Memory => text.detail_memory,
            Self::Screen => text.detail_screen,
            Self::Mac => text.detail_mac,
        }
    }
}

impl From<wire::DeviceDetail> for DeviceDetail {
    fn from(value: wire::DeviceDetail) -> Self {
        fn time(seconds: i64) -> String {
            chrono::DateTime::from_timestamp(seconds, 0)
                .map(|t| {
                    t.with_timezone(&chrono::Local)
                        .format("%Y-%m-%d %H:%M")
                        .to_string()
                })
                .unwrap_or_default()
        }
        let mut rows = Vec::new();
        let mut push = |row: DetailRow, value: String| {
            if !value.trim().is_empty() {
                rows.push((row, value));
            }
        };
        let common = &crate::i18n::t().common;
        push(DetailRow::Name, value.alias);
        push(DetailRow::DeviceId, value.device_id);
        if let Some(owner) = &value.owner {
            push(DetailRow::Owner, owner.display_name().to_owned());
        }
        push(DetailRow::Platform, platform_label(value.platform).to_owned());
        push(DetailRow::System, value.os);
        push(DetailRow::Version, value.version);
        push(
            DetailRow::Status,
            if value.online { common.online } else { common.offline }.to_owned(),
        );
        if value.registered_at > 0 {
            push(DetailRow::Registered, time(value.registered_at));
        }
        if value.last_seen_at > 0 {
            push(DetailRow::LastSeen, time(value.last_seen_at));
        }
        if let Some(connect_id) = value.connect_id {
            push(DetailRow::ConnectId, connect_id);
        }
        if let Some(hardware) = value.hardware {
            push(DetailRow::Processor, hardware.cpu);
            push(DetailRow::Board, hardware.base_board);
            push(DetailRow::Graphics, hardware.video.join(" / "));
            if hardware.memory > 0 {
                // Bytes.
                push(
                    DetailRow::Memory,
                    format!("{:.1} GB", hardware.memory as f64 / 1024f64.powi(3)),
                );
            }
            push(DetailRow::Screen, hardware.screen);
            push(DetailRow::Mac, hardware.mac);
        }
        Self { details: rows }
    }
}

#[derive(Deserialize)]
pub struct RenamedDevice {
    pub device_id: String,
    pub alias: String,
}

// -------------------------------------------------------- realtime tickets

/// A single-use realtime entry ticket and the endpoints it is valid for.
#[derive(Clone)]
pub struct RealtimeTicket {
    ticket: String,
    endpoints: Vec<String>,
    pub connect_timeout_ms: u32,
    pub retry_delay_ms: u32,
    pub max_reconnect_delay_s: u32,
    /// Controller sessions: the server session this ticket enters.
    pub session_id: Option<String>,
    /// Server policy for this session's video bitrate ceiling.
    pub bitrate_limit_mbps: Option<u32>,
    /// The session is signalled over this process's presence connection;
    /// there is no ticket (`docs/protocol/v2-design.md` §11).
    pub via_presence: bool,
}

impl RealtimeTicket {
    pub fn validate(&self) -> Result<()> {
        if !farwing_protocol::ids::is_token(&self.ticket, farwing_protocol::ids::TICKET_PREFIX) {
            bail!("{}", crate::i18n::t().session.api_no_ticket);
        }
        if self.endpoints.iter().all(String::is_empty) {
            bail!("{}", crate::i18n::t().session.api_no_endpoint);
        }
        Ok(())
    }

    pub(crate) fn ticket(&self) -> &str {
        &self.ticket
    }

    pub(crate) fn endpoints(&self) -> Vec<&str> {
        self.endpoints
            .iter()
            .map(String::as_str)
            .filter(|value| !value.is_empty())
            .collect()
    }
}

impl From<wire::PresenceTicket> for RealtimeTicket {
    fn from(value: wire::PresenceTicket) -> Self {
        Self {
            ticket: value.ticket,
            endpoints: value.endpoints,
            connect_timeout_ms: value.connect_timeout_ms,
            retry_delay_ms: value.retry_delay_ms,
            max_reconnect_delay_s: value.max_reconnect_delay_s,
            session_id: None,
            bitrate_limit_mbps: None,
            via_presence: false,
        }
    }
}

impl From<wire::SessionTicket> for RealtimeTicket {
    fn from(value: wire::SessionTicket) -> Self {
        Self {
            via_presence: value.ticket.is_empty(),
            ticket: value.ticket,
            endpoints: value.endpoints,
            connect_timeout_ms: value.connect_timeout_ms,
            retry_delay_ms: value.retry_delay_ms,
            max_reconnect_delay_s: 0,
            session_id: Some(value.session_id),
            bitrate_limit_mbps: value.bitrate_limit_mbps,
        }
    }
}

impl fmt::Debug for RealtimeTicket {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RealtimeTicket")
            .field("ticket", &"***REDACTED***")
            .field("connect_timeout_ms", &self.connect_timeout_ms)
            .field("retry_delay_ms", &self.retry_delay_ms)
            .field("max_reconnect_delay_s", &self.max_reconnect_delay_s)
            .field("endpoint_count", &self.endpoints().len())
            .field("session", &self.session_id.is_some())
            .field("via_presence", &self.via_presence)
            .finish()
    }
}

/// A granted assistance session.
#[derive(Clone)]
pub struct AssistJoined {
    pub ticket: RealtimeTicket,
    pub host: wire::AssistHost,
    /// PAKE key of a code-verified request, bound to the session's DTLS
    /// fingerprints (`a=x-farwing-pake`).
    pub pake_key: Option<[u8; 32]>,
}

impl AssistJoined {
    pub(crate) fn validate(&self, controller_id: &str) -> Result<()> {
        self.ticket.validate()?;
        if !self.host.platform.is_desktop() {
            bail!("{}", crate::i18n::t().session.api_unsupported_device);
        }
        validate_device_id(&self.host.device_id)?;
        if self.host.device_id == controller_id {
            return Err(ApiFailure::new(ErrorCode::BadRequest, "assisting this device itself")
                .with_reason(farwing_protocol::error::reason::SELF_CONNECT)
                .into());
        }
        anyhow::ensure!(
            farwing_protocol::crypto::valid_public_key(&self.host.public_key),
            "{}",
            crate::i18n::t().session.api_assist_no_key
        );
        Ok(())
    }
}

impl From<wire::AssistJoin> for AssistJoined {
    fn from(value: wire::AssistJoin) -> Self {
        Self {
            ticket: value.session.into(),
            host: value.host,
            pake_key: None,
        }
    }
}

// ----------------------------------------------------------------- client

#[derive(Clone, Copy, PartialEq, Eq)]
enum Auth {
    /// Unauthenticated (server info, device init, update feed).
    None,
    /// Device signature only.
    Device,
    /// Device signature and account/guest bearer token.
    Session,
}

enum Body {
    None,
    Json(Vec<u8>),
    Raw(Bytes, &'static str),
}

#[derive(Clone)]
pub struct ServerApi {
    http: ServerHttp,
    identity: Option<ClientIdentity>,
    bearer: Option<HeaderValue>,
    guest: bool,
}

impl ServerApi {
    pub fn new(server: &ServerConfig) -> Result<Self> {
        Ok(Self {
            http: ServerHttp::new(server)?,
            identity: None,
            bearer: None,
            guest: false,
        })
    }

    /// API bound to the configured server and this installation's identity.
    pub fn for_identity(identity: ClientIdentity) -> Result<Self> {
        let mut api = Self::new(&ServerConfig::require()?)?;
        api.set_identity(identity);
        Ok(api)
    }

    pub fn server(&self) -> &ServerConfig {
        self.http.server()
    }

    pub fn set_identity(&mut self, identity: ClientIdentity) {
        self.identity = Some(identity);
    }

    pub fn identity(&self) -> Option<&ClientIdentity> {
        self.identity.as_ref()
    }

    pub fn set_access_token(&mut self, token: Option<&str>) -> Result<()> {
        self.guest = false;
        self.set_bearer(token)
    }

    fn set_bearer(&mut self, token: Option<&str>) -> Result<()> {
        self.bearer = match token {
            Some("") => bail!("bearer token must not be empty"),
            Some(token) => {
                let mut value = HeaderValue::from_str(&format!("Bearer {token}"))
                    .context("invalid Authorization header")?;
                value.set_sensitive(true);
                Some(value)
            }
            None => None,
        };
        Ok(())
    }

    pub fn is_guest(&self) -> bool {
        self.guest
    }

    // ----------------------------------------------------------- account

    pub async fn server_info(&self) -> Result<wire::ServerInfo> {
        let info: wire::ServerInfo = self
            .call(Method::GET, path::SERVER, Body::None, REQUEST_TIMEOUT, Auth::None)
            .await?;
        learn_clock(&info);
        Ok(info)
    }

    /// Register this installation (or update its registration) with a proof
    /// of possession of the device key.
    pub async fn init_device(&self, request: DeviceInitRequest) -> Result<DeviceInitResponse> {
        let identity = self.identity.as_ref().context(crate::i18n::t().session.api_no_identity)?;
        let mut request = request;
        request.public_key = identity.public_key();
        request.timestamp = signed_now_ms();
        request.proof = identity.sign(&farwing_protocol::crypto::init_proof_message(
            &request.public_key,
            request.timestamp,
        ));
        self.call(
            Method::POST,
            path::DEVICE_INIT,
            json(&request)?,
            REQUEST_TIMEOUT,
            Auth::None,
        )
        .await
    }

    pub async fn login(&self, username: &str, password: &str) -> Result<wire::LoginResponse> {
        self.call(
            Method::POST,
            path::LOGIN,
            json(&wire::LoginRequest {
                username: username.to_owned(),
                password: password.to_owned(),
            })?,
            REQUEST_TIMEOUT,
            Auth::Device,
        )
        .await
    }

    pub async fn me(&self) -> Result<wire::Me> {
        self.call(Method::GET, path::ME, Body::None, REQUEST_TIMEOUT, Auth::Session)
            .await
    }

    pub async fn logout(&self) -> Result<()> {
        self.call::<IgnoredAny>(
            Method::POST,
            path::LOGOUT,
            Body::None,
            REQUEST_TIMEOUT,
            Auth::Session,
        )
        .await
        .map(|_| ())
    }

    /// Logins of this account, one per device.
    pub async fn auth_sessions(&self) -> Result<wire::AuthSessions> {
        self.call(
            Method::GET,
            path::AUTH_SESSIONS,
            Body::None,
            REQUEST_TIMEOUT,
            Auth::Session,
        )
        .await
    }

    /// Sign out another device of this account.
    pub async fn revoke_auth_session(&self, device_id: &str) -> Result<()> {
        validate_device_id(device_id)?;
        self.call::<IgnoredAny>(
            Method::DELETE,
            &path::auth_session(device_id),
            Body::None,
            REQUEST_TIMEOUT,
            Auth::Session,
        )
        .await
        .map(|_| ())
    }

    pub(crate) async fn create_guest(&self) -> Result<wire::GuestSession> {
        self.call(
            Method::POST,
            path::GUEST_SESSION,
            Body::None,
            REQUEST_TIMEOUT,
            Auth::Device,
        )
        .await
    }

    pub(crate) fn authenticate_guest(&mut self, session: wire::GuestSession) -> Result<()> {
        anyhow::ensure!(
            farwing_protocol::ids::is_token(
                &session.guest_token,
                farwing_protocol::ids::GUEST_TOKEN_PREFIX
            ),
            "{}",
            crate::i18n::t().session.api_guest_token_invalid
        );
        self.set_bearer(Some(&session.guest_token))?;
        self.guest = true;
        Ok(())
    }

    pub(crate) async fn latest_release(&self, channel: &str) -> Result<Option<ReleaseInfo>> {
        let path = format!("{}?channel={channel}", path::updates(Platform::Windows.as_str()));
        let info: wire::UpdateInfo = self
            .call(Method::GET, &path, Body::None, REQUEST_TIMEOUT, Auth::None)
            .await?;
        Ok(info.latest)
    }

    // ----------------------------------------------------------- friends

    pub async fn friends(&self) -> Result<wire::FriendList> {
        self.call(Method::GET, path::FRIENDS, Body::None, REQUEST_TIMEOUT, Auth::Session)
            .await
    }

    pub async fn friend_requests(&self) -> Result<wire::FriendRequests> {
        self.call(
            Method::GET,
            path::FRIEND_REQUESTS,
            Body::None,
            REQUEST_TIMEOUT,
            Auth::Session,
        )
        .await
    }

    pub async fn send_friend_request(&self, username: &str) -> Result<wire::FriendRequestOutcome> {
        let username = username.trim();
        if username.is_empty() || username.chars().any(char::is_control) {
            return Err(ApiFailure::new(ErrorCode::BadRequest, "invalid username")
                .with_reason(farwing_protocol::error::reason::USERNAME_INVALID)
                .with_detail("max", 64)
                .into());
        }
        self.call(
            Method::POST,
            path::FRIEND_REQUESTS,
            json(&wire::FriendRequestCreate {
                username: username.to_owned(),
            })?,
            REQUEST_TIMEOUT,
            Auth::Session,
        )
        .await
    }

    /// Accept, sharing `accept` on all this account's devices, or reject
    /// (`None`).
    pub async fn answer_friend_request(&self, id: &str, accept: Option<Vec<String>>) -> Result<()> {
        anyhow::ensure!(farwing_protocol::ids::is_uuid(id), "{}", crate::i18n::t().session.api_friend_request_id_invalid);
        let (action, body) = match accept {
            Some(caps) => ("accept", json(&wire::FriendAccept { caps })?),
            None => ("reject", Body::None),
        };
        self.call::<IgnoredAny>(
            Method::POST,
            &path::friend_request(id, action),
            body,
            REQUEST_TIMEOUT,
            Auth::Session,
        )
        .await
        .map(|_| ())
    }

    pub async fn cancel_friend_request(&self, id: &str) -> Result<()> {
        anyhow::ensure!(farwing_protocol::ids::is_uuid(id), "{}", crate::i18n::t().session.api_friend_request_id_invalid);
        self.call::<IgnoredAny>(
            Method::DELETE,
            &path::friend_request_entry(id),
            Body::None,
            REQUEST_TIMEOUT,
            Auth::Session,
        )
        .await
        .map(|_| ())
    }

    pub async fn remove_friend(&self, user_id: &str) -> Result<()> {
        anyhow::ensure!(farwing_protocol::ids::is_uuid(user_id), "{}", crate::i18n::t().session.api_user_id_invalid);
        self.call::<IgnoredAny>(
            Method::DELETE,
            &path::friend(user_id),
            Body::None,
            REQUEST_TIMEOUT,
            Auth::Session,
        )
        .await
        .map(|_| ())
    }

    /// What this account grants `user_id`.
    pub async fn grants(&self, user_id: &str) -> Result<wire::Grants> {
        anyhow::ensure!(farwing_protocol::ids::is_uuid(user_id), "{}", crate::i18n::t().session.api_user_id_invalid);
        self.call(
            Method::GET,
            &path::grants(user_id),
            Body::None,
            REQUEST_TIMEOUT,
            Auth::Session,
        )
        .await
    }

    pub async fn set_grants(&self, user_id: &str, grants: &wire::Grants) -> Result<()> {
        anyhow::ensure!(farwing_protocol::ids::is_uuid(user_id), "{}", crate::i18n::t().session.api_user_id_invalid);
        self.call::<IgnoredAny>(
            Method::PUT,
            &path::grants(user_id),
            json(grants)?,
            REQUEST_TIMEOUT,
            Auth::Session,
        )
        .await
        .map(|_| ())
    }

    // -------------------------------------------------------- forwarding

    /// Open a forwarding room for sessions this device hosts.
    pub(crate) async fn sfu_create_room(
        &self,
        sessions: Vec<String>,
        mode: wire::ForwardMode,
        offer: String,
    ) -> Result<wire::RoomCreated> {
        self.call(
            Method::POST,
            path::SFU_ROOMS,
            json(&wire::CreateRoomRequest { sessions, mode, offer })?,
            REQUEST_TIMEOUT,
            Auth::Session,
        )
        .await
    }

    pub(crate) async fn sfu_update_room(
        &self,
        room: &str,
        sessions: Option<Vec<String>>,
        offer: Option<String>,
    ) -> Result<wire::RoomUpdated> {
        self.call(
            Method::PUT,
            &path::sfu_room(room),
            json(&wire::UpdateRoomRequest { sessions, offer })?,
            REQUEST_TIMEOUT,
            Auth::Session,
        )
        .await
    }

    pub(crate) async fn sfu_close_room(&self, room: &str) -> Result<()> {
        self.call::<IgnoredAny>(
            Method::DELETE,
            &path::sfu_room(room),
            Body::None,
            REQUEST_TIMEOUT,
            Auth::Session,
        )
        .await
        .map(|_| ())
    }

    /// Receive a room's tracks as the controller of `session`.
    pub(crate) async fn sfu_subscribe(
        &self,
        room: &str,
        session: &str,
        connection: u32,
        offer: String,
        tracks: Option<Vec<String>>,
        layer: wire::ForwardLayer,
    ) -> Result<wire::Subscription> {
        self.call(
            Method::POST,
            &path::sfu_subscribe(room),
            json(&wire::SubscribeRequest {
                session: session.to_owned(),
                connection,
                offer,
                tracks,
                layer,
            })?,
            REQUEST_TIMEOUT,
            Auth::Session,
        )
        .await
    }

    /// The forwarder's stream this viewer chose.
    pub(crate) async fn sfu_layer(&self, room: &str, session: &str, layer: wire::ForwardLayer) -> Result<()> {
        self.call::<IgnoredAny>(
            Method::PUT,
            &path::sfu_layer(room, session),
            json(&wire::LayerRequest { layer })?,
            REQUEST_TIMEOUT,
            Auth::Session,
        )
        .await
        .map(|_| ())
    }

    pub(crate) async fn sfu_unsubscribe(
        &self,
        room: &str,
        session: &str,
        connection: u32,
    ) -> Result<()> {
        self.call::<IgnoredAny>(
            Method::DELETE,
            &path::sfu_subscription(room, session, connection),
            Body::None,
            REQUEST_TIMEOUT,
            Auth::Session,
        )
        .await
        .map(|_| ())
    }

    // ----------------------------------------------------------- devices

    pub async fn list_devices(&self) -> Result<DeviceList> {
        let mut list: DeviceList = self
            .call(Method::GET, path::DEVICES, Body::None, REQUEST_TIMEOUT, Auth::Session)
            .await?;
        list.sort();
        Ok(list)
    }

    pub async fn device_detail(&self, device_id: &str) -> Result<DeviceDetail> {
        validate_device_id(device_id)?;
        let detail: wire::DeviceDetail = self
            .call(
                Method::GET,
                &path::device_detail(device_id),
                Body::None,
                REQUEST_TIMEOUT,
                Auth::Session,
            )
            .await?;
        Ok(detail.into())
    }

    pub async fn rename_device(&self, id: &str, alias: &str) -> Result<RenamedDevice> {
        validate_device_id(id)?;
        if alias.trim().is_empty() || alias.chars().any(char::is_control) {
            return Err(ApiFailure::new(ErrorCode::BadRequest, "invalid device name")
                .with_reason(farwing_protocol::error::reason::DEVICE_NAME_INVALID)
                .with_detail("max", 64)
                .into());
        }
        self.call(
            Method::PATCH,
            &path::device(id),
            json(&wire::RenameRequest {
                alias: alias.to_owned(),
            })?,
            REQUEST_TIMEOUT,
            Auth::Session,
        )
        .await
    }

    pub async fn remove_device(&self, id: &str) -> Result<()> {
        validate_device_id(id)?;
        self.call::<IgnoredAny>(
            Method::DELETE,
            &path::device(id),
            Body::None,
            REQUEST_TIMEOUT,
            Auth::Session,
        )
        .await
        .map(|_| ())
    }

    /// Publish what this device's owner currently allows.
    /// The friends' devices that may control this device.
    pub(crate) async fn friend_access(&self) -> Result<wire::FriendAccessList> {
        self.call(
            Method::GET,
            path::SELF_FRIEND_ACCESS,
            Body::None,
            REQUEST_TIMEOUT,
            Auth::Session,
        )
        .await
    }

    pub async fn set_policy(&self, policy: &DevicePolicy) -> Result<()> {
        self.call::<IgnoredAny>(
            Method::PUT,
            path::SELF_POLICY,
            json(policy)?,
            SHORT_TIMEOUT,
            Auth::Session,
        )
        .await
        .map(|_| ())
    }

    pub(crate) async fn upload_wallpaper(
        &self,
        image: Bytes,
        content_type: &'static str,
    ) -> Result<String> {
        anyhow::ensure!(
            !image.is_empty() && image.len() <= farwing_protocol::limits::MAX_WALLPAPER_BYTES,
            "{}",
            crate::i18n::t().session.api_wallpaper_too_large
        );
        let response: wire::WallpaperResponse = self
            .call(
                Method::PUT,
                path::SELF_WALLPAPER,
                Body::Raw(image, content_type),
                REQUEST_TIMEOUT,
                Auth::Session,
            )
            .await?;
        Ok(response.wallpaper_url)
    }

    /// Download a server-relative wallpaper path from a device list.
    pub(crate) async fn download_wallpaper(&self, relative: &str) -> Result<Bytes> {
        anyhow::ensure!(
            relative.starts_with(&format!("{}/", path::DEVICES)) && !relative.contains(".."),
            "{}",
            crate::i18n::t().session.api_wallpaper_foreign
        );
        let response = self
            .send(
                Method::GET,
                relative,
                Body::None,
                REQUEST_TIMEOUT,
                Auth::Session,
            )
            .await?;
        if !response.status.is_success() {
            return Err(failure_from(response.status.as_u16(), &response.body).into());
        }
        anyhow::ensure!(
            response.body.len() <= farwing_protocol::limits::MAX_WALLPAPER_BYTES,
            "{}",
            crate::i18n::t().session.api_wallpaper_too_large
        );
        Ok(response.body)
    }

    pub(crate) async fn wol_info(
        &self,
        info: crate::features::host::wol::packet::LanInfo,
    ) -> Result<bool> {
        let request = wire::WolInfoRequest {
            mac: info.mac,
            inner_ip: info.inner_ip,
            subnet_mask: info.subnet_mask,
        };
        let response: wire::WolInfoResponse = self
            .call(
                Method::PUT,
                path::SELF_WOL,
                json(&request)?,
                REQUEST_TIMEOUT,
                Auth::Session,
            )
            .await?;
        Ok(response.supported)
    }

    pub(crate) async fn device_power(
        &self,
        id: &str,
        action: crate::account::power::PowerAction,
    ) -> Result<crate::account::power::PowerReceipt> {
        validate_device_id(id)?;
        let action = match action {
            crate::account::power::PowerAction::Wake => wire::PowerAction::Wake,
            crate::account::power::PowerAction::Reboot => wire::PowerAction::Reboot,
            crate::account::power::PowerAction::Shutdown => wire::PowerAction::Shutdown,
        };
        let receipt: wire::PowerReceipt = self
            .call(
                Method::POST,
                &path::device_power(id),
                json(&wire::PowerRequest { action })?,
                REQUEST_TIMEOUT,
                Auth::Session,
            )
            .await?;
        Ok(crate::account::power::PowerReceipt {
            current_device_in_same_network: receipt.current_device_in_same_network,
            assist_count: receipt.assist_count,
            target: receipt.target,
            local_packets: None,
        })
    }

    pub(crate) async fn host_power_response(
        &self,
        request_id: &str,
        code: i32,
        message: String,
    ) -> Result<()> {
        self.call::<IgnoredAny>(
            Method::POST,
            &path::power_result(request_id),
            json(&wire::PowerResult {
                code,
                message,
            })?,
            REQUEST_TIMEOUT,
            Auth::Session,
        )
        .await
        .map(|_| ())
    }

    // ---------------------------------------------------------- sessions

    pub async fn presence_ticket(&self) -> Result<RealtimeTicket> {
        let ticket: wire::PresenceTicket = self
            .call(
                Method::POST,
                path::PRESENCE_TICKET,
                Body::None,
                REQUEST_TIMEOUT,
                Auth::Session,
            )
            .await?;
        let ticket = RealtimeTicket::from(ticket);
        ticket.validate()?;
        Ok(ticket)
    }

    /// Create a session. With `presence`, it is signalled over this
    /// device's presence connection and no ticket is issued.
    pub async fn create_session(
        &self,
        target_device_id: &str,
        purpose: Purpose,
        presence: bool,
        visit: &str,
    ) -> Result<(RealtimeTicket, wire::SessionTarget)> {
        validate_device_id(target_device_id)?;
        let ticket: wire::SessionTicket = self
            .call(
                Method::POST,
                path::SESSIONS,
                json(&wire::CreateSessionRequest {
                    target_device_id: target_device_id.to_owned(),
                    purpose,
                    presence,
                    visit: visit.to_owned(),
                })?,
                REQUEST_TIMEOUT,
                Auth::Session,
            )
            .await?;
        let target = ticket.target.clone();
        let ticket = RealtimeTicket::from(ticket);
        if ticket.via_presence {
            anyhow::ensure!(
                presence
                    && ticket
                        .session_id
                        .as_deref()
                        .is_some_and(farwing_protocol::ids::is_uuid),
                "{}",
                crate::i18n::t().session.api_no_session
            );
        } else {
            ticket.validate()?;
        }
        Ok((ticket, target))
    }

    /// A fresh ticket for an existing session (reconnect after a lost socket).
    pub async fn refresh_session_ticket(&self, session_id: &str) -> Result<RealtimeTicket> {
        anyhow::ensure!(farwing_protocol::ids::is_uuid(session_id), "{}", crate::i18n::t().session.api_session_id_invalid);
        let ticket: wire::SessionTicket = self
            .call(
                Method::POST,
                &path::session_ticket(session_id),
                Body::None,
                REQUEST_TIMEOUT,
                Auth::Session,
            )
            .await?;
        let ticket = RealtimeTicket::from(ticket);
        ticket.validate()?;
        Ok(ticket)
    }

    // ------------------------------------------------- assistance: controller

    /// Ask a host (by short number or device ID) to admit this device.
    /// `pake` holds the controller SPAKE2 messages (empty without a code).
    /// The host answers: trusted, or its half of the exchange.
    pub(crate) async fn assist_join(
        &self,
        target: &str,
        pake: Vec<String>,
    ) -> Result<wire::AssistPending> {
        let target = crate::account::assist::normalize_target(target)?;
        let request = if crate::account::assist::is_short_number(&target) {
            wire::AssistJoinRequest {
                connect_id: target,
                pake,
                ..Default::default()
            }
        } else {
            wire::AssistJoinRequest {
                device_id: target,
                pake,
                ..Default::default()
            }
        };
        let pending: wire::AssistPending = self
            .call(
                Method::POST,
                path::ASSIST_JOIN,
                json(&request)?,
                JOIN_TIMEOUT,
                Auth::Session,
            )
            .await?;
        anyhow::ensure!(
            farwing_protocol::ids::is_uuid(&pending.request_id),
            "{}",
            crate::i18n::t().session.api_assist_id_invalid
        );
        Ok(pending)
    }

    /// Prove knowledge of the code and wait for the host's decision.
    pub(crate) async fn assist_accept(
        &self,
        request_id: &str,
        key_confirm: String,
        visit: &str,
    ) -> Result<AssistJoined> {
        anyhow::ensure!(farwing_protocol::ids::is_uuid(request_id), "{}", crate::i18n::t().session.api_assist_id_invalid);
        let joined: wire::AssistJoin = self
            .call(
                Method::POST,
                &path::assist_request(request_id, "accept"),
                json(&wire::AssistAcceptRequest {
                    key_confirm,
                    visit: visit.to_owned(),
                })?,
                LONG_POLL_TIMEOUT,
                Auth::Session,
            )
            .await?;
        Ok(joined.into())
    }

    /// A side's summary of a connection (`docs/connection-records.md`).
    pub(crate) async fn record_report(&self, report: &wire::RecordReport) -> Result<()> {
        self.call::<IgnoredAny>(
            Method::POST,
            path::RECORD_REPORTS,
            json(report)?,
            REQUEST_TIMEOUT,
            Auth::Session,
        )
        .await
        .map(|_| ())
    }

    /// A change of this device's access settings, for the account's events.
    pub(crate) async fn device_event(&self, event: &wire::DeviceEvent) -> Result<()> {
        self.call::<IgnoredAny>(
            Method::POST,
            path::SELF_EVENTS,
            json(event)?,
            REQUEST_TIMEOUT,
            Auth::Session,
        )
        .await
        .map(|_| ())
    }

    /// The account's connection records, newest first (`before`: an ID).
    pub(crate) async fn records(
        &self,
        device: Option<&str>,
        before: Option<i64>,
    ) -> Result<Vec<wire::ConnectionRecord>> {
        let mut query = vec![format!("limit={}", RECORD_PAGE)];
        if let Some(device) = device {
            validate_device_id(device)?;
            query.push(format!("device={device}"));
        }
        if let Some(before) = before {
            query.push(format!("before={before}"));
        }
        let path = format!("{}?{}", path::RECORDS, query.join("&"));
        self.call(Method::GET, &path, Body::None, REQUEST_TIMEOUT, Auth::Session)
            .await
    }

    /// The account's events, newest first (`before`: an ID).
    pub(crate) async fn account_events(&self, before: Option<i64>) -> Result<Vec<wire::AccountEvent>> {
        let mut path = format!("{}?limit={}", path::ACCOUNT_EVENTS, RECORD_PAGE);
        if let Some(before) = before {
            path.push_str(&format!("&before={before}"));
        }
        self.call(Method::GET, &path, Body::None, REQUEST_TIMEOUT, Auth::Session)
            .await
    }

    /// The account's notifications, newest first, with the unread count.
    pub(crate) async fn notices(&self, before: Option<i64>, limit: usize) -> Result<wire::NoticeList> {
        let mut path = format!("{}?limit={limit}", path::NOTICES);
        if let Some(before) = before {
            path.push_str(&format!("&before={before}"));
        }
        self.call(Method::GET, &path, Body::None, REQUEST_TIMEOUT, Auth::Session)
            .await
    }

    pub(crate) async fn read_notices(&self) -> Result<()> {
        self.call::<IgnoredAny>(
            Method::POST,
            path::NOTICES_READ,
            json(&wire::NoticeRead { ids: Vec::new(), all: true })?,
            REQUEST_TIMEOUT,
            Auth::Session,
        )
        .await
        .map(|_| ())
    }

    pub(crate) async fn notice_settings(&self) -> Result<wire::NoticeSettings> {
        self.call(Method::GET, path::NOTICE_SETTINGS, Body::None, REQUEST_TIMEOUT, Auth::Session)
            .await
    }

    pub(crate) async fn save_notice_setting(&self, kind: &str, enabled: bool) -> Result<wire::NoticeSettings> {
        self.call(
            Method::PUT,
            path::NOTICE_SETTINGS,
            json(&wire::NoticeSettings {
                settings: vec![wire::NoticeSetting { kind: kind.to_owned(), enabled }],
            })?,
            REQUEST_TIMEOUT,
            Auth::Session,
        )
        .await
    }

    pub(crate) async fn assist_cancel(&self, request_id: &str) -> Result<()> {
        anyhow::ensure!(farwing_protocol::ids::is_uuid(request_id), "{}", crate::i18n::t().session.api_assist_id_invalid);
        self.call::<IgnoredAny>(
            Method::POST,
            &path::assist_request(request_id, "cancel"),
            Body::None,
            REQUEST_TIMEOUT,
            Auth::Session,
        )
        .await
        .map(|_| ())
    }

    pub(crate) async fn assist_saved(
        &self,
        kind: crate::account::assist::SavedKind,
    ) -> Result<Vec<WireSavedDevice>> {
        let list: wire::SavedList = self
            .call(
                Method::GET,
                &path::assist_saved(kind.path()),
                Body::None,
                REQUEST_TIMEOUT,
                Auth::Session,
            )
            .await?;
        Ok(list.saved)
    }

    pub(crate) async fn assist_save_favorite(
        &self,
        item: wire::FavoriteItem,
    ) -> Result<Vec<WireSavedDevice>> {
        crate::account::assist::normalize_connect_id(&item.connect_id)?;
        let list: wire::SavedList = self
            .call(
                Method::PUT,
                &path::assist_saved("favorites"),
                json(&wire::FavoritesRequest {
                    favorites: vec![item],
                })?,
                REQUEST_TIMEOUT,
                Auth::Session,
            )
            .await?;
        Ok(list.saved)
    }

    pub(crate) async fn assist_delete_saved(
        &self,
        kind: crate::account::assist::SavedKind,
        key: &str,
    ) -> Result<()> {
        // A device ID, or the short number of a favourite not yet connected to.
        if crate::account::assist::normalize_connect_id(key).is_err() {
            validate_device_id(key)?;
        }
        self.call::<IgnoredAny>(
            Method::DELETE,
            &path::assist_saved_entry(kind.path(), key),
            Body::None,
            REQUEST_TIMEOUT,
            Auth::Session,
        )
        .await
        .map(|_| ())
    }

    pub(crate) async fn assist_clear_recent(&self) -> Result<()> {
        self.call::<IgnoredAny>(
            Method::DELETE,
            &path::assist_saved("recent"),
            Body::None,
            REQUEST_TIMEOUT,
            Auth::Session,
        )
        .await
        .map(|_| ())
    }

    // ------------------------------------------------------ assistance: host

    pub(crate) async fn host_assist_identity(&self) -> Result<String> {
        let identity: wire::AssistIdentity = self
            .call(
                Method::POST,
                path::ASSIST_IDENTITY,
                Body::None,
                REQUEST_TIMEOUT,
                Auth::Session,
            )
            .await
            .map_err(sanitize_assist)?;
        Ok(identity.connect_id)
    }

    /// The host's half of the PAKE (or its refusal) for `assist.join`.
    pub(crate) async fn host_assist_pake(
        &self,
        request_id: &str,
        reply: &wire::AssistPakeReply,
    ) -> Result<()> {
        self.host_assist_reply(request_id, "pake", json(reply)?).await
    }

    /// The host's final decision after `assist.accept`.
    pub(crate) async fn host_assist_decision(
        &self,
        request_id: &str,
        decision: &wire::AssistDecision,
    ) -> Result<()> {
        self.host_assist_reply(request_id, "decision", json(decision)?)
            .await
    }

    async fn host_assist_reply(&self, request_id: &str, action: &str, body: Body) -> Result<()> {
        anyhow::ensure!(farwing_protocol::ids::is_uuid(request_id), "{}", crate::i18n::t().session.api_assist_id_invalid);
        self.call::<IgnoredAny>(
            Method::POST,
            &path::assist_request(request_id, action),
            body,
            REQUEST_TIMEOUT,
            Auth::Session,
        )
        .await
        .map(|_| ())
        .map_err(sanitize_assist)
    }

    // --------------------------------------------------------------- core

    async fn call<T: DeserializeOwned>(
        &self,
        method: Method,
        path: &str,
        body: Body,
        timeout: Duration,
        auth: Auth,
    ) -> Result<T> {
        let body = match body {
            Body::None => None,
            Body::Json(bytes) => Some((Bytes::from(bytes), "application/json")),
            Body::Raw(bytes, content_type) => Some((bytes, content_type)),
        };
        let response = self
            .raw(method.clone(), path, body.clone(), timeout, auth)
            .await?;
        let result = decode(path, response.status.as_u16(), &response.body);
        // A rejected signature was not executed: correct the clock if it is
        // far off and sign the request once more (a fresh timestamp also
        // resolves a signature another process of this device just used).
        if auth != Auth::None
            && result
                .as_ref()
                .err()
                .and_then(|e| e.downcast_ref::<ApiFailure>())
                .is_some_and(ApiFailure::invalid_device)
        {
            self.correct_clock().await;
            let response = self.raw(method, path, body, timeout, auth).await?;
            return decode(path, response.status.as_u16(), &response.body);
        }
        result
    }

    /// Re-learn the server clock; whether the offset changed enough to matter.
    async fn correct_clock(&self) -> bool {
        let before = CLOCK_OFFSET_MS.load(Ordering::Relaxed);
        let Ok(response) = self
            .raw(Method::GET, path::SERVER, None, SHORT_TIMEOUT, Auth::None)
            .await
        else {
            return false;
        };
        let Ok(info) =
            decode::<wire::ServerInfo>(path::SERVER, response.status.as_u16(), &response.body)
        else {
            return false;
        };
        learn_clock(&info);
        (CLOCK_OFFSET_MS.load(Ordering::Relaxed) - before).abs() > CLOCK_RETRY_THRESHOLD_MS
    }

    async fn send(
        &self,
        method: Method,
        path: &str,
        body: Body,
        timeout: Duration,
        auth: Auth,
    ) -> Result<crate::account::http::Response> {
        let body = match body {
            Body::None => None,
            Body::Json(bytes) => Some((Bytes::from(bytes), "application/json")),
            Body::Raw(bytes, content_type) => Some((bytes, content_type)),
        };
        self.raw(method, path, body, timeout, auth).await
    }

    async fn raw(
        &self,
        method: Method,
        path: &str,
        body: Option<(Bytes, &'static str)>,
        timeout: Duration,
        auth: Auth,
    ) -> Result<crate::account::http::Response> {
        if !path.starts_with('/') {
            bail!("API path must start with '/'");
        }
        let payload = body.as_ref().map(|(bytes, _)| bytes.as_ref()).unwrap_or(&[]);
        let headers = self.headers(auth, &method, path, payload)?;
        let response = self
            .http
            .send(method, path, headers, body, timeout)
            .await
            .map_err(|error| ApiFailure::network(&error))?;
        if response.body.len() > MAX_RESPONSE_BYTES {
            bail!("{}", crate::i18n::t().session.api_response_too_large);
        }
        Ok(response)
    }

    fn headers(&self, auth: Auth, method: &Method, path: &str, body: &[u8]) -> Result<HeaderMap> {
        let mut headers = HeaderMap::new();
        headers.insert(
            HeaderName::from_static(farwing_protocol::headers::PROTOCOL),
            HeaderValue::from(PROTOCOL_VERSION),
        );
        headers.insert(
            HeaderName::from_static(farwing_protocol::headers::REQUEST_ID),
            HeaderValue::from_str(&uuid::Uuid::new_v4().to_string()).expect("UUID header"),
        );
        if auth == Auth::None {
            return Ok(headers);
        }
        let identity = self
            .identity
            .as_ref()
            .filter(|identity| identity.registered())
            .context(crate::i18n::t().session.api_not_registered)?;
        let timestamp = signed_now_ms();
        let signature = identity.sign(&farwing_protocol::crypto::request_message(
            method.as_str(),
            path,
            timestamp,
            body,
        ));
        headers.insert(
            HeaderName::from_static(farwing_protocol::headers::DEVICE_ID),
            HeaderValue::from_str(&identity.device_id).context(crate::i18n::t().session.api_device_id_invalid)?,
        );
        headers.insert(
            HeaderName::from_static(farwing_protocol::headers::TIMESTAMP),
            HeaderValue::from(timestamp),
        );
        headers.insert(
            HeaderName::from_static(farwing_protocol::headers::SIGNATURE),
            HeaderValue::from_str(&signature).context(crate::i18n::t().session.api_signature_invalid)?,
        );
        if auth == Auth::Session {
            let bearer = self.bearer.as_ref().context(crate::i18n::t().common.signed_out)?;
            headers.insert(AUTHORIZATION, bearer.clone());
        }
        Ok(headers)
    }
}

fn learn_clock(info: &wire::ServerInfo) {
    if info.time_ms > 0 {
        CLOCK_OFFSET_MS.store(
            info.time_ms - chrono::Utc::now().timestamp_millis(),
            Ordering::Relaxed,
        );
    }
}

impl fmt::Debug for ServerApi {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ServerApi")
            .field("server", self.http.server())
            .field("identity", &self.identity)
            .field("authorized", &self.bearer.is_some())
            .field("guest", &self.guest)
            .finish()
    }
}

fn json<T: Serialize + ?Sized>(body: &T) -> Result<Body> {
    Ok(Body::Json(
        serde_json::to_vec(body).context("failed to encode request JSON")?,
    ))
}

fn decode<T: DeserializeOwned>(path: &str, status: u16, body: &[u8]) -> Result<T> {
    // An error envelope wins regardless of status.
    if let Ok(envelope) = serde_json::from_slice::<farwing_protocol::ErrorEnvelope>(body) {
        let failure = ApiFailure::from_body(envelope.error, status);
        tracing::debug!(
            path,
            status,
            code = %failure.code,
            reason = ?failure.reason,
            message = %failure.message,
            "Farwing API request failed"
        );
        return Err(failure.into());
    }
    if !(200..300).contains(&status) {
        return Err(failure_from(status, body).into());
    }
    let body = if body.is_empty() { b"{}".as_slice() } else { body };
    serde_json::from_slice(body)
        .with_context(|| format!("failed to decode Farwing API response ({path}, HTTP {status})"))
}

fn failure_from(status: u16, body: &[u8]) -> ApiFailure {
    if let Ok(envelope) = serde_json::from_slice::<farwing_protocol::ErrorEnvelope>(body) {
        return ApiFailure::from_body(envelope.error, status);
    }
    let code = match status {
        400 => ErrorCode::BadRequest,
        401 => ErrorCode::Unauthorized,
        403 => ErrorCode::Forbidden,
        404 => ErrorCode::NotFound,
        408 => ErrorCode::Timeout,
        409 => ErrorCode::Conflict,
        413 => ErrorCode::PayloadTooLarge,
        426 => ErrorCode::ProtocolUnsupported,
        429 => ErrorCode::RateLimited,
        502..=504 => ErrorCode::Unavailable,
        _ => ErrorCode::Internal,
    };
    tracing::debug!(status, code = %code, "Farwing API answered without an error body");
    ApiFailure {
        status,
        ..ApiFailure::new(code, format!("HTTP {status}"))
    }
}

fn sanitize_message(message: &str) -> String {
    message
        .chars()
        .filter(|c| !c.is_control())
        .take(200)
        .collect()
}

fn sanitize_assist(error: anyhow::Error) -> anyhow::Error {
    // Never surface server text for assistance challenges: `Display` shows
    // only the code's and reason's text, and the message is dropped for
    // whatever still reads it (logged by `decode`).
    match error.downcast::<ApiFailure>() {
        Ok(mut failure) => {
            failure.message.clear();
            failure.into()
        }
        Err(error) => error,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_error_envelopes() {
        let body = br#"{"error":{"code":"assist_code_required","message":"wait","retryable":false,"details":{"mode":"password"}}}"#;
        let error = decode::<IgnoredAny>("/x", 409, body).unwrap_err();
        let failure = error.downcast_ref::<ApiFailure>().unwrap();
        assert_eq!(failure.code, ErrorCode::AssistCodeRequired);
        assert_eq!(failure.details["mode"], "password");
        assert_eq!(failure.reason, None);
        let body = br#"{"error":{"code":"forbidden","message":"two-step verification required","details":{"reason":"totp_required"}}}"#;
        let error = decode::<IgnoredAny>("/x", 403, body).unwrap_err();
        let failure = error.downcast_ref::<ApiFailure>().unwrap();
        assert_eq!(failure.reason.as_deref(), Some("totp_required"));
        // Shown in the interface language, never the server's message.
        let shown = failure.to_string();
        let texts = [crate::i18n::Language::ZhCn, crate::i18n::Language::En]
            .map(|l| crate::i18n::messages(l).errors.totp_required);
        assert!(texts.contains(&shown.as_str()), "{shown}");
        let unknown = decode::<IgnoredAny>("/x", 401, b"").unwrap_err();
        assert!(unknown.downcast_ref::<ApiFailure>().unwrap().invalid_saved_credentials());
        let device: DeviceInfo = serde_json::from_str(
            r#"{"device_id":"a1b2c3d4e5f6g7h8","status":"online","platform":"windows","capabilities":["host"],"policy":{"control":true},"version":"0.1.0","participants":[{"session_id":"s"}]}"#,
        )
        .unwrap();
        assert!(device.is_connected() && device.controlled_support && device.controllable);
        assert_eq!(device.participant_count(), 1);
        assert!(!device.is_shared());
        let round: wire::Device = device.into();
        assert!(round.policy.control);
    }
}
