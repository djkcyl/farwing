//! Realtime plane (`wss://.../api/v2/ws`): envelope, message types and bodies.

pub use crate::api::WolTarget;
use crate::Platform;
use crate::api::{Device, Purpose, Relation, UserSummary};
use crate::error::ErrorBody;
use crate::session::{ConnectOptions, ConnectType};
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// One JSON text frame.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Envelope {
    pub v: u32,
    #[serde(rename = "type")]
    pub kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub re: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sid: Option<String>,
    /// Negotiation generation (`gen` on the wire).
    #[serde(rename = "gen", default, skip_serializing_if = "Option::is_none")]
    pub generation: Option<u64>,
    #[serde(default, skip_serializing_if = "Value::is_null")]
    pub data: Value,
}

impl Envelope {
    pub fn new(kind: &str, data: impl Serialize) -> Self {
        Self {
            v: crate::PROTOCOL_VERSION,
            kind: kind.to_owned(),
            id: None,
            re: None,
            sid: None,
            generation: None,
            data: serde_json::to_value(data).unwrap_or(Value::Null),
        }
    }
    pub fn with_id(mut self, id: impl Into<String>) -> Self {
        self.id = Some(id.into());
        self
    }
    pub fn with_sid(mut self, sid: impl Into<String>) -> Self {
        self.sid = Some(sid.into());
        self
    }
    pub fn with_generation(mut self, generation: u64) -> Self {
        self.generation = Some(generation);
        self
    }
    pub fn ack(re: &str, data: impl Serialize) -> Self {
        let mut envelope = Self::new(types::ACK, data);
        envelope.re = Some(re.to_owned());
        envelope
    }
    pub fn error(re: Option<&str>, body: ErrorBody) -> Self {
        let mut envelope = Self::new(types::ERROR, body);
        envelope.re = re.map(str::to_owned);
        envelope
    }
    pub fn encode(&self) -> String {
        serde_json::to_string(self).expect("realtime envelope serializes")
    }
    pub fn decode(text: &str) -> Result<Self, serde_json::Error> {
        serde_json::from_str(text)
    }
    /// Decode `data` as a typed body.
    pub fn body<T: for<'de> Deserialize<'de>>(&self) -> Result<T, serde_json::Error> {
        T::deserialize(&self.data)
    }
}

/// Message type names.
pub mod types {
    // common
    pub const HELLO: &str = "hello";
    pub const PING: &str = "ping";
    pub const PONG: &str = "pong";
    pub const ACK: &str = "ack";
    pub const ERROR: &str = "error";
    pub const RESUME_REFRESH: &str = "resume.refresh";
    pub const BYE: &str = "bye";
    // sessions
    pub const SESSION_START: &str = "session.start";
    pub const SESSION_INCOMING: &str = "session.incoming";
    pub const SESSION_SETTING: &str = "session.setting";
    pub const SESSION_END: &str = "session.end";
    pub const SESSION_ENDED: &str = "session.ended";
    /// Controller → server on a presence connection that carries the
    /// controller session `sid`: the controller leaves (like `bye` on a
    /// per-session connection).
    pub const SESSION_LEAVE: &str = "session.leave";
    pub const RTC_OFFER: &str = "rtc.offer";
    pub const RTC_ANSWER: &str = "rtc.answer";
    pub const RTC_CANDIDATE: &str = "rtc.candidate";
    /// Server → host and controller: the participant's capabilities changed.
    pub const SESSION_PERMISSIONS: &str = "session.permissions";
    // pushes
    pub const DEVICE_CHANGED: &str = "device.changed";
    pub const FRIEND_CHANGED: &str = "friend.changed";
    pub const ASSIST_JOIN: &str = "assist.join";
    pub const ASSIST_ACCEPT: &str = "assist.accept";
    pub const ASSIST_CANCELLED: &str = "assist.cancelled";
    pub const POWER_REQUEST: &str = "power.request";
    pub const WOL_WAKE: &str = "wol.wake";
}

/// WebSocket close codes.
pub mod close {
    pub const NORMAL: u16 = 1000;
    pub const SUPERSEDED: u16 = 4001;
    pub const REVOKED: u16 = 4003;
    pub const SESSION_ENDED: u16 = 4004;
    pub const PROTOCOL_ERROR: u16 = 4008;
    pub const VERSION_UNSUPPORTED: u16 = 4009;
    pub const RATE_LIMITED: u16 = 4029;
    pub const INVALID_TICKET: u16 = 4401;

    /// Whether a client should reconnect (with its resume token) after `code`.
    pub fn reconnectable(code: u16) -> bool {
        !matches!(
            code,
            NORMAL | SUPERSEDED | REVOKED | SESSION_ENDED | VERSION_UNSUPPORTED | INVALID_TICKET
        )
    }
}

/// Connection role.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    Host,
    Controller,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Limits {
    pub max_frame_bytes: usize,
    pub max_sdp_bytes: usize,
    pub max_candidates: usize,
    pub rate_per_s: u32,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_frame_bytes: crate::limits::MAX_WS_FRAME,
            max_sdp_bytes: crate::limits::MAX_SDP_BYTES,
            max_candidates: crate::limits::MAX_CANDIDATES,
            rate_per_s: crate::limits::WS_RATE_PER_SECOND,
        }
    }
}

#[derive(Clone, Serialize, Deserialize)]
pub struct Hello {
    pub conn_id: String,
    pub role: Role,
    #[serde(default)]
    pub epoch: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    pub resume_token: String,
    pub heartbeat_ms: u32,
    pub idle_timeout_ms: u32,
    #[serde(default)]
    pub limits: Limits,
    #[serde(default)]
    pub resumed: bool,
    #[serde(default)]
    pub server_time_ms: i64,
}

impl std::fmt::Debug for Hello {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Hello")
            .field("role", &self.role)
            .field("epoch", &self.epoch)
            .field("resumed", &self.resumed)
            .field("heartbeat_ms", &self.heartbeat_ms)
            .finish_non_exhaustive()
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Ping {
    pub t: i64,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct ResumeRefreshAck {
    pub resume_token: String,
}

/// `session.start` (controller → server). Purpose and type come from the
/// session ticket; the server never takes them from the controller here.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SessionStart {
    pub attempt_id: String,
    pub options: ConnectOptions,
    /// Controller decoder/display capability (opaque to the server).
    #[serde(default)]
    pub capability: Value,
    /// What the session is for. The server takes the purpose from the
    /// session ticket; a host's direct endpoint, which has no ticket, takes
    /// it from here and checks it against the participant's grants (§12.2).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub purpose: Option<Purpose>,
}

/// A peer's device identity as stated by the server.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct PeerIdentity {
    pub device_id: String,
    /// base64url Ed25519 public key.
    pub public_key: String,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct IceServer {
    pub urls: Vec<String>,
    #[serde(default)]
    pub username: String,
    #[serde(default)]
    pub credential: String,
}

impl std::fmt::Debug for IceServer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("IceServer")
            .field("urls", &self.urls.len())
            .finish_non_exhaustive()
    }
}

/// `ack` body of `session.start`.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SessionStarted {
    pub participant_id: String,
    pub session_id: String,
    pub ice_servers: Vec<IceServer>,
    #[serde(default)]
    pub force_relay: bool,
    #[serde(default)]
    pub host: PeerIdentity,
    #[serde(default)]
    pub relation: Relation,
    /// Capabilities granted to this participant.
    #[serde(default)]
    pub grants: Vec<String>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ControllerInfo {
    pub device_id: String,
    pub alias: String,
    pub user_id: String,
    pub username: String,
    pub nickname: String,
    pub platform: Platform,
    pub version: String,
    /// base64url Ed25519 public key of the controller device.
    pub public_key: String,
    /// The server's upper bound on this controller's capabilities, whatever
    /// the relation grants (browsers: the server's web setting).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub caps_limit: Option<Vec<String>>,
}

impl ControllerInfo {
    /// What this controller may ever do: the server's limit.
    pub fn effective_caps_limit(&self) -> Option<Vec<String>> {
        self.caps_limit.clone()
    }

    /// `grants` within this controller's limit.
    pub fn limit_grants(&self, grants: Vec<String>) -> Vec<String> {
        match self.effective_caps_limit() {
            Some(limit) => grants.into_iter().filter(|c| limit.contains(c)).collect(),
            None => grants,
        }
    }

    /// Nickname, falling back to the username.
    pub fn display_name(&self) -> &str {
        if self.nickname.is_empty() {
            &self.username
        } else {
            &self.nickname
        }
    }
}

/// `session.incoming` (server → host). `sid` carries the session ID. The
/// purpose, connect type, relation, grants and controller identity are
/// authoritative; hosts never take them from `options`.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SessionIncoming {
    pub participant_id: String,
    pub attempt_id: String,
    pub purpose: Purpose,
    pub connect_type: ConnectType,
    #[serde(default)]
    pub relation: Relation,
    /// Capabilities granted by the server (own devices: all; friends: the
    /// owner's grants; assistance: empty, the host applies its template).
    #[serde(default)]
    pub grants: Vec<String>,
    #[serde(default)]
    pub controller: ControllerInfo,
    pub options: ConnectOptions,
    #[serde(default)]
    pub capability: Value,
    pub ice_servers: Vec<IceServer>,
    #[serde(default)]
    pub force_relay: bool,
}

/// ICE network selection carried by `rtc.offer` / `rtc.answer`.
pub mod network {
    /// Relay only, TURNS preferred (plain TURN when no TURNS is offered).
    pub const RELAY: u8 = 1;
    /// All candidates.
    pub const ALL: u8 = 3;
}

/// `rtc.offer` / `rtc.answer`. `sid` and `gen` are set on the envelope.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RtcDescription {
    pub attempt_id: String,
    #[serde(default)]
    pub restart: bool,
    #[serde(default)]
    pub network: u8,
    pub sdp: String,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Candidate {
    pub candidate: String,
    #[serde(rename = "sdpMid", default, skip_serializing_if = "Option::is_none")]
    pub sdp_mid: Option<String>,
    #[serde(
        rename = "sdpMLineIndex",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub sdp_mline_index: Option<u16>,
    #[serde(
        rename = "usernameFragment",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub username_fragment: Option<String>,
}

/// `rtc.candidate`.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RtcCandidate {
    pub attempt_id: String,
    pub candidate: Candidate,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct SenderInfo {
    pub video_track_index: u64,
    pub capture_impl: String,
    pub encoder_impl: String,
}

/// `session.setting` (host → controller, relayed).
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SessionSetting {
    Capability {
        capability: Value,
    },
    SenderInfo {
        infos: Vec<SenderInfo>,
    },
}

/// Reasons in `session.end` / `session.ended`.
pub mod reason {
    pub const REPLACED: &str = "replaced";
    pub const HOST_CLOSED: &str = "host_closed";
    pub const HOST_OFFLINE: &str = "host_offline";
    pub const CONTROLLER_LEFT: &str = "controller_left";
    pub const REVOKED: &str = "revoked";
    pub const EXPIRED: &str = "expired";
    pub const MEDIA_FAILED: &str = "media_failed";
    pub const SERVER_SHUTDOWN: &str = "server_shutdown";
    /// The host removed this participant.
    pub const KICKED: &str = "kicked";
    /// Grants no longer allow this session's purpose.
    pub const PERMISSION_REVOKED: &str = "permission_revoked";
    /// A peer's DTLS fingerprint was not signed by its device key.
    pub const IDENTITY_MISMATCH: &str = "identity_mismatch";
}

/// `session.permissions` (server → host and controller, `sid` set).
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct SessionPermissions {
    #[serde(default)]
    pub grants: Vec<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SessionEnd {
    pub reason: String,
}

// ------------------------------------------------------------------- pushes

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DeviceChangeKind {
    Added,
    Updated,
    Removed,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DeviceChanged {
    pub kind: DeviceChangeKind,
    pub device: Device,
    /// A friend's device (as opposed to one of this account's devices).
    #[serde(default)]
    pub shared: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FriendChangeKind {
    RequestReceived,
    RequestAccepted,
    RequestRejected,
    RequestCancelled,
    Removed,
    /// The friend changed the grants this account holds.
    GrantsChanged,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct FriendChanged {
    pub kind: FriendChangeKind,
    pub user: UserSummary,
}

/// `assist.join` (server → host): a controller asks for assistance.
#[derive(Clone, Serialize, Deserialize)]
pub struct AssistJoinPush {
    pub request_id: String,
    pub controller: ControllerInfo,
    /// Controller SPAKE2 messages; empty when no code was entered.
    #[serde(default)]
    pub pake: Vec<String>,
}

impl std::fmt::Debug for AssistJoinPush {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AssistJoinPush")
            .field("request_id", &self.request_id)
            .field("pake", &self.pake.len())
            .finish_non_exhaustive()
    }
}

/// `assist.accept` (server → host): the controller's key confirmation.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AssistAcceptPush {
    pub request_id: String,
    #[serde(default)]
    pub key_confirm: String,
}

/// `assist.cancelled` (server → host).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AssistCancelled {
    pub request_id: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PowerRequestPush {
    pub request_id: String,
    pub action: crate::api::PowerAction,
    /// Who asked, for the host's confirmation and audit log.
    #[serde(default)]
    pub requester: UserSummary,
    #[serde(default)]
    pub requester_device: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct WolWake {
    pub request_id: String,
    pub target: WolTarget,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn controller_caps_limit_comes_from_the_server() {
        let grants: Vec<String> = ["view", "control", "audio", "ports"].map(String::from).to_vec();
        let mut browser = ControllerInfo {
            platform: crate::Platform::Web,
            ..Default::default()
        };
        browser.caps_limit = Some(["view", "control"].map(String::from).to_vec());
        assert_eq!(browser.limit_grants(grants.clone()), ["view", "control"]);
        let wire = serde_json::to_value(&browser).unwrap();
        assert_eq!(wire["caps_limit"], serde_json::json!(["view", "control"]));
        // Native controllers carry no limit and keep their grants.
        let native = ControllerInfo::default();
        assert!(serde_json::to_value(&native).unwrap().get("caps_limit").is_none());
        assert_eq!(native.limit_grants(grants.clone()), grants);
    }

    #[test]
    fn envelope_round_trip() {
        let envelope = Envelope::new(types::RTC_CANDIDATE, RtcCandidate {
            attempt_id: "a".into(),
            candidate: Candidate {
                candidate: "candidate:1 1 udp 1 1.2.3.4 5 typ host".into(),
                sdp_mid: Some("0".into()),
                sdp_mline_index: Some(0),
                username_fragment: None,
            },
        })
        .with_sid("s")
        .with_generation(2);
        let text = envelope.encode();
        let decoded = Envelope::decode(&text).unwrap();
        assert_eq!(decoded.kind, types::RTC_CANDIDATE);
        assert_eq!(decoded.generation, Some(2));
        assert!(text.contains("\"gen\":2"));
        let body: RtcCandidate = decoded.body().unwrap();
        assert_eq!(body.candidate.sdp_mid.as_deref(), Some("0"));
        let setting = serde_json::to_value(SessionSetting::SenderInfo { infos: Vec::new() })
            .unwrap();
        assert_eq!(setting["kind"], "sender_info");
    }
}
