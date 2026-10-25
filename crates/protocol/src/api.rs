//! HTTPS control-plane request and response bodies (`/api/v2/...`).
//!
//! Field names are the wire names. Optional response fields default so that a
//! newer server can add fields without breaking older clients.

use crate::Platform;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// Endpoint paths. `{id}`-style segments are filled by callers.
pub mod path {
    pub const SERVER: &str = "/api/v2/server";
    pub const DEVICE_INIT: &str = "/api/v2/devices/init";
    pub const LOGIN: &str = "/api/v2/auth/login";
    pub const LOGOUT: &str = "/api/v2/auth/logout";
    /// Login sessions of the current account (one per device).
    pub const AUTH_SESSIONS: &str = "/api/v2/auth/sessions";
    pub const ME: &str = "/api/v2/me";
    pub const GUEST_SESSION: &str = "/api/v2/guest/session";
    pub const DEVICES: &str = "/api/v2/devices";
    pub const SELF_POLICY: &str = "/api/v2/devices/self/policy";
    /// Friends' devices that may control the calling device, with their
    /// grants (`docs/access-model.md` §2.2: friends over direct connections).
    pub const SELF_FRIEND_ACCESS: &str = "/api/v2/devices/self/friend-access";
    pub const SELF_WALLPAPER: &str = "/api/v2/devices/self/wallpaper";
    pub const SELF_WOL: &str = "/api/v2/devices/self/wol";
    pub const CONFIG: &str = "/api/v2/config";
    pub const PRESENCE_TICKET: &str = "/api/v2/presence/ticket";
    pub const SESSIONS: &str = "/api/v2/sessions";
    pub const ASSIST_JOIN: &str = "/api/v2/assist/join";
    pub const ASSIST_IDENTITY: &str = "/api/v2/assist/identity";
    pub const ASSIST_SAVED: &str = "/api/v2/assist/saved";
    pub const ASSIST_REQUESTS: &str = "/api/v2/assist/requests";
    pub const POWER_RESULTS: &str = "/api/v2/power";
    pub const UPDATES: &str = "/api/v2/updates";
    pub const FRIENDS: &str = "/api/v2/friends";
    pub const FRIEND_REQUESTS: &str = "/api/v2/friends/requests";
    pub const GRANTS: &str = "/api/v2/grants";
    /// Multi-viewer forwarding (`docs/protocol/deployment-design.md` §5).
    pub const SFU_ROOMS: &str = "/api/v2/sfu/rooms";

    pub fn device(id: &str) -> String {
        format!("{DEVICES}/{id}")
    }
    pub fn device_detail(id: &str) -> String {
        format!("{DEVICES}/{id}/detail")
    }
    pub fn device_wallpaper(id: &str) -> String {
        format!("{DEVICES}/{id}/wallpaper")
    }
    pub fn device_power(id: &str) -> String {
        format!("{DEVICES}/{id}/power")
    }
    pub fn session_ticket(session_id: &str) -> String {
        format!("{SESSIONS}/{session_id}/ticket")
    }
    pub fn auth_session(device_id: &str) -> String {
        format!("{AUTH_SESSIONS}/{device_id}")
    }
    pub fn assist_saved(kind: &str) -> String {
        format!("{ASSIST_SAVED}/{kind}")
    }
    pub fn assist_saved_entry(kind: &str, device_id: &str) -> String {
        format!("{ASSIST_SAVED}/{kind}/{device_id}")
    }
    /// `{ASSIST_REQUESTS}/{request_id}/{action}`; actions: `accept`, `cancel`
    /// (controller) and `pake`, `decision` (host).
    pub fn assist_request(request_id: &str, action: &str) -> String {
        format!("{ASSIST_REQUESTS}/{request_id}/{action}")
    }
    pub fn power_result(request_id: &str) -> String {
        format!("{POWER_RESULTS}/{request_id}/result")
    }
    pub fn updates(platform: &str) -> String {
        format!("{UPDATES}/{platform}")
    }
    pub fn sfu_room(room: &str) -> String {
        format!("{SFU_ROOMS}/{room}")
    }
    pub fn sfu_subscribe(room: &str) -> String {
        format!("{SFU_ROOMS}/{room}/subscribe")
    }
    pub fn sfu_subscription(room: &str, session_id: &str, connection: u32) -> String {
        format!("{SFU_ROOMS}/{room}/subscribe/{session_id}/{connection}")
    }
    pub fn friend(user_id: &str) -> String {
        format!("{FRIENDS}/{user_id}")
    }
    /// `{FRIEND_REQUESTS}/{id}/{action}`; actions: `accept`, `reject`.
    pub fn friend_request(id: &str, action: &str) -> String {
        format!("{FRIEND_REQUESTS}/{id}/{action}")
    }
    pub fn friend_request_entry(id: &str) -> String {
        format!("{FRIEND_REQUESTS}/{id}")
    }
    pub fn grants(user_id: &str) -> String {
        format!("{GRANTS}/{user_id}")
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Empty {}

// ---------------------------------------------------------------- server info

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ServerInfo {
    pub protocol: ProtocolRange,
    pub server: ServerIdentity,
    #[serde(default)]
    pub features: ServerFeatures,
    #[serde(default)]
    pub time_ms: i64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ProtocolRange {
    pub name: String,
    pub min: u32,
    pub max: u32,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ServerIdentity {
    pub name: String,
    pub version: String,
    #[serde(default)]
    pub instance_name: String,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ServerFeatures {
    pub guest_hosts: bool,
    pub turn: bool,
    pub force_relay: bool,
    pub updates: bool,
    pub friends: bool,
    /// `stun:` URLs of the RFC 5780 NAT discovery endpoints (`[[turn.stun]]`).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub nat_discovery: Vec<String>,
    /// Hosts may forward through the server; absent when it does not.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub forwarding: Option<Forwarding>,
}

/// The server's multi-viewer forwarding.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Forwarding {
    /// Viewers of one host from which it forwards.
    pub threshold: u32,
    /// Modes hosts may choose.
    pub modes: Vec<ForwardMode>,
}

/// How a host's picture passes the forwarder.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ForwardMode {
    /// The host encrypts every frame for its viewers; the forwarder passes
    /// ciphertext.
    #[default]
    E2ee,
    /// The forwarder can see the picture.
    Compatible,
}

// ------------------------------------------------------------------- devices

/// What the device owner currently allows; the server enforces it before a
/// session exists and hosts enforce it again for every session.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct DevicePolicy {
    /// Accept remote control sessions at all.
    pub control: bool,
    pub files: bool,
    pub ports: bool,
    pub power: bool,
    pub wol: bool,
    pub assist: bool,
    /// Concurrent participants this host accepts.
    pub max_participants: u32,
}

impl Default for DevicePolicy {
    fn default() -> Self {
        Self {
            control: false,
            files: true,
            ports: false,
            power: false,
            wol: false,
            assist: false,
            max_participants: crate::limits::DEFAULT_MAX_PARTICIPANTS,
        }
    }
}

/// Optional hardware description. Reported only when the user enabled it.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct HardwareInfo {
    pub cpu: String,
    pub base_board: String,
    pub video: Vec<String>,
    pub memory: i64,
    pub screen: String,
    pub mac: String,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct DeviceInitRequest {
    pub installation_id: String,
    pub name: String,
    pub platform: Platform,
    pub os: String,
    pub version: String,
    /// base64url Ed25519 public key of this installation.
    pub public_key: String,
    /// Unix milliseconds signed in `proof`.
    pub timestamp: i64,
    /// Signature over [`crate::crypto::init_proof_message`].
    pub proof: String,
    pub capabilities: Vec<String>,
    pub policy: DevicePolicy,
    pub hardware: Option<HardwareInfo>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DeviceInitResponse {
    pub device_id: String,
    #[serde(default)]
    pub alias: String,
}

/// How a participant relates to the device it controls.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Relation {
    /// A device of the same account.
    #[default]
    Own,
    /// A friend's device, under the owner's grants.
    Friend,
    /// A visitor admitted with the host's access code or its user's
    /// confirmation, for this connection only.
    Assist,
    /// A device the host trusts (`docs/access-model.md` §2.2): admitted by
    /// its device key, over the server or directly.
    Trusted,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Participant {
    pub participant_id: String,
    pub session_id: String,
    pub user_id: String,
    pub nickname: String,
    pub device_id: String,
    pub device_alias: String,
    pub relation: Relation,
    pub purpose: String,
    pub started_at: i64,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct UserSummary {
    pub user_id: String,
    pub username: String,
    pub nickname: String,
}

impl UserSummary {
    /// Nickname, falling back to the username.
    pub fn display_name(&self) -> &str {
        if self.nickname.is_empty() {
            &self.username
        } else {
            &self.nickname
        }
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Device {
    pub device_id: String,
    pub alias: String,
    /// `online` or `offline`.
    pub status: String,
    pub platform: Platform,
    pub version: String,
    pub capabilities: Vec<String>,
    pub policy: DevicePolicy,
    /// Wake-on-LAN registered and enabled.
    pub wol_supported: bool,
    /// Server-relative URL (authenticated download) or empty.
    pub wallpaper_url: String,
    pub participants: Vec<Participant>,
    /// Unix seconds, 0 when unknown.
    pub last_seen_at: i64,
    /// Owner of a shared (friend) device; `None` for own devices.
    pub owner: Option<UserSummary>,
    /// Capabilities the requesting account holds on this device.
    pub granted: Vec<String>,
    /// base64url Ed25519 public key of the device: lets an account's devices
    /// recognise each other on direct connections (§12.2).
    pub public_key: String,
}

pub const STATUS_ONLINE: &str = "online";
pub const STATUS_OFFLINE: &str = "offline";

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DeviceList {
    pub current_device: Device,
    /// Other devices of this account.
    #[serde(default)]
    pub own: Vec<Device>,
    /// Friends' devices this account holds at least one grant on.
    #[serde(default)]
    pub shared: Vec<Device>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct DeviceDetail {
    pub device_id: String,
    pub alias: String,
    pub platform: Platform,
    pub os: String,
    pub version: String,
    pub online: bool,
    pub registered_at: i64,
    pub last_seen_at: i64,
    pub connect_id: Option<String>,
    pub hardware: Option<HardwareInfo>,
    pub owner: Option<UserSummary>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RenameRequest {
    pub alias: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RenameResponse {
    pub device_id: String,
    pub alias: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct WallpaperResponse {
    pub wallpaper_url: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct WolInfoRequest {
    pub mac: String,
    pub inner_ip: String,
    pub subnet_mask: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct WolInfoResponse {
    pub supported: bool,
}

// ------------------------------------------------------------------- account

#[derive(Clone, Serialize, Deserialize)]
pub struct LoginRequest {
    pub username: String,
    pub password: String,
}

impl std::fmt::Debug for LoginRequest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LoginRequest")
            .field("username", &self.username)
            .field("password", &"***")
            .finish()
    }
}

#[derive(Clone, Serialize, Deserialize)]
pub struct LoginResponse {
    /// Bound to the device that signed the login request.
    pub access_token: String,
    pub user_id: String,
    #[serde(default)]
    pub username: String,
    #[serde(default)]
    pub nickname: String,
}

impl std::fmt::Debug for LoginResponse {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LoginResponse")
            .field("user_id", &self.user_id)
            .finish_non_exhaustive()
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Me {
    pub user_id: String,
    #[serde(default)]
    pub username: String,
    #[serde(default)]
    pub nickname: String,
}

/// One login of the current account (`GET /auth/sessions`).
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct AuthSession {
    pub device_id: String,
    pub device_alias: String,
    pub platform: Platform,
    pub created_at: i64,
    pub last_used_at: i64,
    /// The device that made this request.
    pub current: bool,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct AuthSessions {
    #[serde(default)]
    pub sessions: Vec<AuthSession>,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct GuestSession {
    pub guest_token: String,
    pub expires_at: i64,
}

// ------------------------------------------------------------------- friends

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Friend {
    pub user: UserSummary,
    pub since: i64,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct FriendList {
    #[serde(default)]
    pub friends: Vec<Friend>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct FriendRequestCreate {
    pub username: String,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct FriendRequest {
    pub id: String,
    /// The other party: the sender for incoming, the recipient for outgoing.
    pub user: UserSummary,
    pub created_at: i64,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct FriendRequests {
    pub incoming: Vec<FriendRequest>,
    pub outgoing: Vec<FriendRequest>,
}

/// Outcome of `POST /friends/requests`.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FriendRequestOutcome {
    /// Waiting for the other user.
    Sent { request: FriendRequest },
    /// The other user had already asked: now friends.
    Accepted { friend: Friend },
}

/// What this account grants one friend (`GET`/`PUT /grants/{user_id}`).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Grants {
    /// Applies to every device of this account without its own entry.
    pub default: Vec<String>,
    /// Per-device overrides, keyed by device ID.
    pub devices: BTreeMap<String, Vec<String>>,
}

// -------------------------------------------------------------------- config

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ClientConfig {
    pub bitrate_limit_mbps: Option<u32>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct UpdateInfo {
    #[serde(default)]
    pub latest: Option<ReleaseInfo>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ReleaseInfo {
    pub version: String,
    pub prerelease: bool,
    pub published_at: i64,
    pub notes: String,
    pub url: String,
}

// ------------------------------------------------------------------ sessions

#[derive(Clone, Serialize, Deserialize)]
pub struct PresenceTicket {
    pub ticket: String,
    pub endpoints: Vec<String>,
    pub connect_timeout_ms: u32,
    pub retry_delay_ms: u32,
    pub max_reconnect_delay_s: u32,
}

/// Session purpose. `audio` is a desktop session without video.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Purpose {
    Desktop,
    Audio,
    Files,
    Ports,
}

impl Purpose {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Desktop => "desktop",
            Self::Audio => "audio",
            Self::Files => "files",
            Self::Ports => "ports",
        }
    }
    /// Capability a participant needs to open a session of this purpose.
    pub fn required_cap(self) -> &'static str {
        match self {
            Self::Desktop | Self::Audio => crate::caps::VIEW,
            Self::Files => crate::caps::FILES,
            Self::Ports => crate::caps::PORTS,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CreateSessionRequest {
    pub target_device_id: String,
    pub purpose: Purpose,
    /// The controller signals this session over its own presence connection
    /// (`session.start` with `sid`); no realtime ticket is issued and the
    /// response's `ticket` is empty.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub presence: bool,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct SessionTarget {
    pub device_id: String,
    pub alias: String,
    pub platform: Platform,
    pub version: String,
    pub capabilities: Vec<String>,
    pub relation: Relation,
    pub granted: Vec<String>,
    /// base64url Ed25519 public key of the host.
    pub public_key: String,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct SessionTicket {
    pub session_id: String,
    /// Empty when the session is signalled over the controller's presence
    /// connection (`CreateSessionRequest::presence`).
    #[serde(default)]
    pub ticket: String,
    pub endpoints: Vec<String>,
    pub connect_timeout_ms: u32,
    pub retry_delay_ms: u32,
    #[serde(default)]
    pub target: SessionTarget,
    #[serde(default)]
    pub bitrate_limit_mbps: Option<u32>,
}

impl std::fmt::Debug for SessionTicket {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SessionTicket")
            .field("session_id", &self.session_id)
            .field("endpoints", &self.endpoints.len())
            .finish_non_exhaustive()
    }
}

// -------------------------------------------------------------------- assist

/// Host assistance verification mode.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AssistMode {
    /// The controller must know the host's code.
    Password,
    /// The host user must confirm each request.
    Confirmation,
    /// Both: a valid code, then confirmation.
    PasswordConfirmation,
}

impl AssistMode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Password => "password",
            Self::Confirmation => "confirmation",
            Self::PasswordConfirmation => "password_confirmation",
        }
    }
    pub fn needs_password(self) -> bool {
        self != Self::Confirmation
    }
    pub fn needs_confirmation(self) -> bool {
        self != Self::Password
    }
}

/// `POST /assist/join` (controller; an account or a guest device): ask a
/// host to admit this device (`docs/access-model.md` §2.4). The host is
/// addressed by its short number (`connect_id`) or its device ID
/// (`device_id`). `pake` holds [`crate::crypto::ASSIST_PAKE_SLOTS`] SPAKE2
/// messages derived from the typed access code, or is empty without a code
/// (a host that trusts this device, or one that only asks its user).
#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct AssistJoinRequest {
    pub connect_id: String,
    pub device_id: String,
    pub pake: Vec<String>,
}

impl std::fmt::Debug for AssistJoinRequest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AssistJoinRequest")
            .field("connect_id", &self.connect_id)
            .field("device_id", &self.device_id)
            .field("pake", &self.pake.len())
            .finish()
    }
}

/// Identity of an assistance host returned to the controller.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct AssistHost {
    pub device_id: String,
    pub alias: String,
    pub platform: Platform,
    pub version: String,
    pub public_key: String,
}

/// Successful reply of `POST /assist/join`: the host answered the PAKE.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AssistPending {
    pub request_id: String,
    pub mode: AssistMode,
    /// The host trusts this device: no code and no confirmation.
    #[serde(default)]
    pub trusted: bool,
    /// Host SPAKE2 messages, one per controller slot; empty in confirmation mode.
    #[serde(default)]
    pub pake: Vec<String>,
    /// Host key confirmations (label `host`), one per slot.
    #[serde(default)]
    pub key_confirm: Vec<String>,
    pub host: AssistHost,
}

/// `POST /assist/requests/{id}/accept` (controller, long poll).
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct AssistAcceptRequest {
    /// Controller key confirmation (label `controller`); empty in
    /// confirmation mode.
    pub key_confirm: String,
}

/// Reply of a successful `accept`: the session to join.
#[derive(Clone, Serialize, Deserialize)]
pub struct AssistJoin {
    #[serde(flatten)]
    pub session: SessionTicket,
    pub host: AssistHost,
}

/// `POST /assist/requests/{id}/pake` (host).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AssistPakeReply {
    pub allowed: bool,
    pub mode: AssistMode,
    #[serde(default)]
    pub pake: Vec<String>,
    #[serde(default)]
    pub key_confirm: Vec<String>,
    /// The host trusts the controller's device: it is admitted by its key.
    #[serde(default)]
    pub trusted: bool,
}

/// Why a host refused an assistance request.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AssistRefusal {
    /// The controller's key confirmation did not match any code.
    CodeInvalid,
    /// A code is required but none was given.
    CodeRequired,
    /// The host user declined.
    Rejected,
    /// Assistance is off, locked or temporarily blocked.
    Unavailable,
}

/// `POST /assist/requests/{id}/decision` (host).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AssistDecision {
    pub allowed: bool,
    #[serde(default)]
    pub refusal: Option<AssistRefusal>,
    /// Admitted as a trusted device rather than a visitor; the host applies
    /// what it allows that device.
    #[serde(default)]
    pub trusted: bool,
}

/// A friend's device that may control the calling device
/// (`GET` [`path::SELF_FRIEND_ACCESS`]).
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct FriendAccess {
    pub device_id: String,
    pub public_key: String,
    pub alias: String,
    pub user_id: String,
    pub nickname: String,
    /// What the friend may do on the calling device.
    pub grants: Vec<String>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct FriendAccessList {
    pub devices: Vec<FriendAccess>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct SavedDevice {
    pub publisher_device_id: String,
    pub connect_id: String,
    pub remark: String,
    pub is_favorite: bool,
    pub last_connected_at: Option<i64>,
    pub favorited_at: Option<i64>,
    /// The device key of the assisted device (empty when unknown), shown
    /// as its fingerprint.
    #[serde(default)]
    pub public_key: String,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct SavedList {
    #[serde(default)]
    pub saved: Vec<SavedDevice>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct FavoriteItem {
    pub connect_id: String,
    pub remark: String,
    pub favorited_at: i64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct FavoritesRequest {
    pub favorites: Vec<FavoriteItem>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AssistIdentity {
    pub connect_id: String,
}

// --------------------------------------------------------------------- power

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PowerAction {
    Wake,
    Reboot,
    Shutdown,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PowerRequest {
    pub action: PowerAction,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct PowerReceipt {
    pub request_id: String,
    /// The requesting device is on the target's LAN and should broadcast the
    /// magic packet itself.
    pub current_device_in_same_network: Option<bool>,
    /// Helper devices asked to wake the target.
    pub assist_count: Option<u32>,
    /// Wake-on-LAN target for a requester on the same LAN, so it can send the
    /// magic packet itself.
    pub target: Option<WolTarget>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct WolTarget {
    pub mac: String,
    pub inner_ip: String,
    pub subnet_mask: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PowerResult {
    pub code: i32,
    #[serde(default)]
    pub message: String,
    #[serde(default)]
    pub state: String,
}

// ------------------------------------------------------------- forwarding

/// `POST /api/v2/sfu/rooms` (host): forward the offered tracks to the
/// controllers of `sessions` (sessions this device hosts).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CreateRoomRequest {
    pub sessions: Vec<String>,
    #[serde(default)]
    pub mode: ForwardMode,
    /// The host's offer (send-only media, candidates included).
    pub offer: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RoomCreated {
    pub room: String,
    pub mode: ForwardMode,
    pub answer: String,
    /// Name of the edge node forwarding; absent: the server itself.
    #[serde(default)]
    pub node: Option<String>,
}

/// `PUT /api/v2/sfu/rooms/{room}` (host): the sessions that may subscribe
/// (others lose their subscription) and, with an offer, new tracks (every
/// subscription ends; viewers subscribe again).
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct UpdateRoomRequest {
    pub sessions: Option<Vec<String>>,
    pub offer: Option<String>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct RoomUpdated {
    pub answer: Option<String>,
}

/// `POST /api/v2/sfu/rooms/{room}/subscribe` (the controller of `session`).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SubscribeRequest {
    pub session: String,
    /// The viewer's connection to the forwarder (from 1): a new one may be
    /// opened before the old one is left, so that the picture never waits.
    pub connection: u32,
    /// Receive-only media; the published tracks fill each kind's sections
    /// in `tracks` order (default: publishing order).
    pub offer: String,
    #[serde(default)]
    pub tracks: Option<Vec<String>>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Subscription {
    pub answer: String,
    pub mode: ForwardMode,
}
