//! Chat (`docs/protocol/chat-design.md`): groups, channels, roles, members,
//! messages, the HTTP bodies under `/api/v2/chat` and the pushes of the chat
//! connection.

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::fmt;

/// A chat object's ID (Snowflake, increasing with time): a decimal string
/// on the wire so browsers keep every bit.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Id(pub i64);

impl Id {
    /// Milliseconds since 2026-01-01 UTC at which this ID was made.
    pub const EPOCH_MS: i64 = 1_767_225_600_000;

    /// Unix milliseconds at which this ID was made.
    pub fn created_ms(self) -> i64 {
        (self.0 >> 22) + Self::EPOCH_MS
    }
}

impl fmt::Display for Id {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl std::str::FromStr for Id {
    type Err = std::num::ParseIntError;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        s.trim().parse().map(Id)
    }
}

impl Serialize for Id {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.collect_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for Id {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Either {
            Text(String),
            Number(i64),
        }
        match Either::deserialize(d)? {
            Either::Text(text) => text.parse().map_err(serde::de::Error::custom),
            Either::Number(n) => Ok(Id(n)),
        }
    }
}

/// Permission bits (§3.1).
pub mod perm {
    pub const VIEW: i64 = 1 << 0;
    pub const SEND: i64 = 1 << 1;
    pub const ATTACH: i64 = 1 << 2;
    pub const MENTION_ALL: i64 = 1 << 3;
    pub const MANAGE_MESSAGES: i64 = 1 << 4;
    pub const READ_HISTORY: i64 = 1 << 5;
    pub const CONNECT: i64 = 1 << 8;
    pub const SPEAK: i64 = 1 << 9;
    pub const VIDEO: i64 = 1 << 10;
    pub const VOICE_MODERATE: i64 = 1 << 11;
    pub const PRIORITY: i64 = 1 << 12;
    pub const INVITE: i64 = 1 << 16;
    pub const KICK: i64 = 1 << 17;
    pub const BAN: i64 = 1 << 18;
    pub const MUTE: i64 = 1 << 19;
    pub const MANAGE_CHANNELS: i64 = 1 << 20;
    pub const MANAGE_ROLES: i64 = 1 << 21;
    pub const MANAGE_GROUP: i64 = 1 << 22;
    pub const ADMINISTRATOR: i64 = 1 << 30;

    /// Every bit.
    pub const ALL: i64 = VIEW
        | SEND
        | ATTACH
        | MENTION_ALL
        | MANAGE_MESSAGES
        | READ_HISTORY
        | CONNECT
        | SPEAK
        | VIDEO
        | VOICE_MODERATE
        | PRIORITY
        | INVITE
        | KICK
        | BAN
        | MUTE
        | MANAGE_CHANNELS
        | MANAGE_ROLES
        | MANAGE_GROUP
        | ADMINISTRATOR;
    /// `@everyone` of a new group.
    pub const EVERYONE: i64 = VIEW | SEND | ATTACH | READ_HISTORY | CONNECT | SPEAK | VIDEO | INVITE;
    /// Both sides of a direct-message channel.
    pub const DM: i64 = VIEW | SEND | ATTACH | READ_HISTORY | CONNECT | SPEAK | VIDEO;
    /// What a channel overwrite can change (group-wide bits are not
    /// per channel).
    pub const CHANNEL: i64 = VIEW
        | SEND
        | ATTACH
        | MENTION_ALL
        | MANAGE_MESSAGES
        | READ_HISTORY
        | CONNECT
        | SPEAK
        | VIDEO
        | VOICE_MODERATE
        | PRIORITY
        | MANAGE_CHANNELS;
    /// Taken away while a member is muted.
    pub const MUTED: i64 = SEND | ATTACH | SPEAK;
    /// Left while the group is frozen.
    pub const FROZEN: i64 = VIEW | READ_HISTORY;
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChannelKind {
    #[default]
    Text,
    Voice,
    Category,
    Announcement,
    /// A direct-message channel (no group).
    Dm,
}

impl ChannelKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Text => "text",
            Self::Voice => "voice",
            Self::Category => "category",
            Self::Announcement => "announcement",
            Self::Dm => "dm",
        }
    }
    pub fn parse(text: &str) -> Option<Self> {
        Some(match text {
            "text" => Self::Text,
            "voice" => Self::Voice,
            "category" => Self::Category,
            "announcement" => Self::Announcement,
            "dm" => Self::Dm,
            _ => return None,
        })
    }
    /// It holds messages (a voice channel has its own text area).
    pub fn has_messages(self) -> bool {
        !matches!(self, Self::Category)
    }
}

/// An account's presence. `invisible` is only ever told to the account
/// itself; others see it `offline`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    Online,
    Idle,
    Dnd,
    Invisible,
    #[default]
    Offline,
}

impl Status {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Online => "online",
            Self::Idle => "idle",
            Self::Dnd => "dnd",
            Self::Invisible => "invisible",
            Self::Offline => "offline",
        }
    }
    pub fn parse(text: &str) -> Option<Self> {
        Some(match text {
            "online" => Self::Online,
            "idle" => Self::Idle,
            "dnd" => Self::Dnd,
            "invisible" => Self::Invisible,
            "offline" => Self::Offline,
            _ => return None,
        })
    }
}

/// Which messages notify.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Notify {
    All,
    Mentions,
    None,
}

/// An account as chat shows it.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ChatUser {
    pub user_id: String,
    pub username: String,
    pub nickname: String,
    /// Version of the avatar (`/media/avatars/{user}?v=`); empty: none.
    pub avatar: String,
    /// Deleted or disabled: shown as such.
    pub gone: bool,
}

impl ChatUser {
    pub fn display_name(&self) -> &str {
        if self.nickname.is_empty() {
            &self.username
        } else {
            &self.nickname
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Group {
    pub id: Id,
    pub name: String,
    /// Version of the icon (`/media/icons/{group}?v=`); empty: none.
    pub icon: String,
    pub owner_id: String,
    pub public: bool,
    pub join_approval: bool,
    pub description: String,
    pub created_at: i64,
    pub frozen: bool,
    pub member_count: u32,
    /// The caller's permissions in the group (before channel overwrites).
    pub permissions: i64,
}

/// A role; the `@everyone` role's ID is the group's.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Role {
    pub id: Id,
    pub name: String,
    /// `0xRRGGBB`; 0: no color.
    pub color: u32,
    pub permissions: i64,
    /// Higher ranks higher; `@everyone` is 0.
    pub position: i32,
    pub mentionable: bool,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Overwrite {
    pub role_id: Id,
    pub allow: i64,
    pub deny: i64,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Channel {
    pub id: Id,
    /// None for a direct-message channel.
    pub group_id: Option<Id>,
    pub kind: ChannelKind,
    pub name: String,
    pub topic: String,
    /// The category it is in.
    pub parent_id: Option<Id>,
    pub position: i32,
    pub slow_mode_s: u32,
    /// kbit/s.
    pub voice_bitrate: u32,
    /// 0: unlimited.
    pub voice_limit: u32,
    /// A voice channel in compatible mode (§10.4): no frame encryption.
    pub voice_compat: bool,
    pub overwrites: Vec<Overwrite>,
    /// The caller's permissions in it.
    pub permissions: i64,
    pub last_message_id: Option<Id>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Member {
    pub user: ChatUser,
    /// In this group.
    pub nickname: String,
    /// Not including `@everyone`.
    pub roles: Vec<Id>,
    pub joined_at: i64,
    /// Muted in text until (Unix seconds).
    pub muted_until: Option<i64>,
}

/// A group with everything its member needs: the snapshot entry, `GET
/// /chat/groups/{id}` and `chat.group`.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct GroupState {
    pub group: Group,
    /// The channels the caller sees, by position.
    pub channels: Vec<Channel>,
    pub roles: Vec<Role>,
    pub me: Member,
}

/// A direct-message channel with the other side.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Dm {
    pub channel: Channel,
    pub user: ChatUser,
    /// Still friends: otherwise it is read-only.
    pub friends: bool,
    /// How far the other side read (none: unknown or receipts off).
    pub peer_read_id: Option<Id>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Mentions {
    pub users: Vec<String>,
    pub roles: Vec<Id>,
    pub everyone: bool,
    pub here: bool,
}

impl Mentions {
    pub fn is_empty(&self) -> bool {
        self.users.is_empty() && self.roles.is_empty() && !self.everyone && !self.here
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Attachment {
    pub id: Id,
    pub name: String,
    pub mime: String,
    pub size: u64,
    /// Images: their size in pixels.
    pub width: u32,
    pub height: u32,
    /// Has a thumbnail (`/media/thumbs/{id}`).
    pub thumb: bool,
}

/// The message a reply answers.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Reply {
    pub id: Id,
    pub author: ChatUser,
    /// Its beginning.
    pub content: String,
    pub deleted: bool,
    pub attachments: u32,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Message {
    pub id: Id,
    pub channel_id: Id,
    pub author: ChatUser,
    pub content: String,
    pub reply: Option<Reply>,
    pub mentions: Mentions,
    pub attachments: Vec<Attachment>,
    pub pinned: bool,
    /// Unix milliseconds.
    pub edited_at: Option<i64>,
    /// Withdrawn: content and attachments are gone.
    pub deleted: bool,
    /// Unix milliseconds.
    pub created_at: i64,
    /// The sender's `nonce`, on the answer and the pushes of a new message.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub nonce: String,
    /// A call's record in a direct message (its author called).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub call: Option<CallRecord>,
}

/// How a call in a direct message went (§7.6).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct CallRecord {
    /// From answer to end; 0 when missed or declined.
    pub duration_s: u32,
    /// Nobody answered (or the other side declined).
    pub missed: bool,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ReadState {
    pub channel_id: Id,
    pub last_read_id: Id,
    /// Mentions of the account after `last_read_id`.
    pub mentions: u32,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Presence {
    pub user_id: String,
    pub status: Status,
}

/// The chat settings of an account, synchronized between its devices
/// (`GET`/`PUT /chat/prefs`).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Prefs {
    /// Per group (`"g:<id>"`) or channel (`"c:<id>"`); absent: the default
    /// (groups `mentions`, direct messages `all`, channels follow the
    /// group).
    pub notify: std::collections::BTreeMap<String, Notify>,
    /// Daily quiet hours as `HH:MM` local times; empty: none.
    pub quiet_start: String,
    pub quiet_end: String,
    /// Notifications say only that there is a new message.
    pub hide_content: bool,
    /// Tell the other side of a direct message how far I read (off on
    /// either side: neither sees it).
    pub read_receipts: bool,
}

impl Prefs {
    pub fn new() -> Self {
        Self {
            read_receipts: true,
            ..Default::default()
        }
    }
}

/// `ack` of `chat.hello`: everything to show at once.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Snapshot {
    pub me: ChatUser,
    /// The account's own status setting.
    pub status: Status,
    pub groups: Vec<GroupState>,
    pub dms: Vec<Dm>,
    pub read_states: Vec<ReadState>,
    /// Friends and people sharing a group, when not offline.
    pub presences: Vec<Presence>,
    pub prefs: Prefs,
    /// Invitations into groups waiting for an answer.
    pub group_invites: Vec<GroupInvite>,
    /// Server limits the clients check before sending.
    pub limits: ChatLimits,
    /// Who is in the voice channels the account sees.
    pub voice: Vec<VoiceChannelState>,
    /// The account's calls that ring or run.
    pub calls: Vec<Call>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ChatLimits {
    pub max_content_chars: u32,
    pub max_attachments: u32,
    pub max_attachment_bytes: u64,
    pub create_groups: bool,
    /// Group owners may put a voice channel in compatible mode (§10.4).
    pub voice_compat: bool,
}

impl Default for ChatLimits {
    fn default() -> Self {
        Self {
            max_content_chars: limits::MAX_CONTENT_CHARS,
            max_attachments: limits::MAX_ATTACHMENTS,
            max_attachment_bytes: 100 * 1024 * 1024,
            create_groups: true,
            voice_compat: false,
        }
    }
}

/// Fixed limits.
pub mod limits {
    /// Unicode scalars in a message.
    pub const MAX_CONTENT_CHARS: u32 = 4000;
    pub const MAX_ATTACHMENTS: u32 = 10;
    /// Messages per page.
    pub const MAX_PAGE: u32 = 100;
    pub const MAX_PINS: u32 = 50;
    pub const MAX_GROUP_NAME: usize = 100;
    pub const MAX_CHANNEL_NAME: usize = 100;
    pub const MAX_TOPIC: usize = 1024;
    pub const MAX_DESCRIPTION: usize = 1024;
    pub const MAX_ROLE_NAME: usize = 100;
    pub const MAX_NICKNAME: usize = 64;
    pub const MAX_ROLES: usize = 250;
    pub const MAX_CHANNELS: usize = 500;
    pub const MAX_FILE_NAME: usize = 255;
    /// Avatars and group icons as uploaded.
    pub const MAX_IMAGE_BYTES: usize = 4 * 1024 * 1024;
    pub const MAX_SLOW_MODE_S: u32 = 6 * 3600;
    pub const VOICE_BITRATES: &[u32] = &[32, 64, 96, 128];
    pub const MAX_VOICE_LIMIT: u32 = 99;
}

// --------------------------------------------------------------- requests

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct GroupCreate {
    pub name: String,
    pub description: String,
    /// Names of what the group starts with, in the creator's language;
    /// empty: English.
    pub admin_role: String,
    pub category: String,
    pub text_channel: String,
    pub voice_channel: String,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct GroupUpdate {
    pub name: Option<String>,
    pub description: Option<String>,
    pub public: Option<bool>,
    pub join_approval: Option<bool>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ChannelCreate {
    pub group_id: Id,
    pub kind: ChannelKind,
    pub name: String,
    pub topic: String,
    pub parent_id: Option<Id>,
}

/// `parent_id`: absent keeps it, `null` takes it out of its category.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ChannelUpdate {
    pub name: Option<String>,
    pub topic: Option<String>,
    #[serde(default, deserialize_with = "double_option", skip_serializing_if = "Option::is_none")]
    pub parent_id: Option<Option<Id>>,
    pub slow_mode_s: Option<u32>,
    pub voice_bitrate: Option<u32>,
    pub voice_limit: Option<u32>,
    pub voice_compat: Option<bool>,
}

/// The channels of a group in their new order: each with its category.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ChannelOrder {
    pub channels: Vec<ChannelPlace>,
}

#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ChannelPlace {
    pub id: Id,
    pub parent_id: Option<Id>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct RoleCreate {
    pub name: String,
    pub color: u32,
    pub permissions: i64,
    pub mentionable: bool,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct RoleUpdate {
    pub name: Option<String>,
    pub color: Option<u32>,
    pub permissions: Option<i64>,
    pub mentionable: Option<bool>,
}

/// The roles other than `@everyone`, lowest first.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct RoleOrder {
    pub roles: Vec<Id>,
}

/// `muted_until`: absent keeps it, `null` lifts it.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct MemberUpdate {
    pub nickname: Option<String>,
    pub roles: Option<Vec<Id>>,
    #[serde(default, deserialize_with = "double_option", skip_serializing_if = "Option::is_none")]
    pub muted_until: Option<Option<i64>>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct MemberPage {
    pub members: Vec<Member>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct UserRef {
    pub user: String,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct BanCreate {
    pub reason: String,
    /// Withdraw their messages of the last this many hours (0–168).
    pub delete_hours: u32,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Ban {
    pub user: ChatUser,
    pub by: ChatUser,
    pub reason: String,
    pub at: i64,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct BanList {
    pub bans: Vec<Ban>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct InviteCreate {
    /// 1–100; none: unlimited.
    pub max_uses: Option<u32>,
    /// Seconds (30 minutes to a year); none: never expires.
    pub expires_s: Option<u64>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Invite {
    pub code: String,
    pub group_id: Id,
    pub by: ChatUser,
    pub max_uses: Option<u32>,
    pub uses: u32,
    pub expires_at: Option<i64>,
    pub revoked: bool,
    pub created_at: i64,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct InviteList {
    pub invites: Vec<Invite>,
}

/// What a group shows before joining.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct GroupPreview {
    pub id: Id,
    pub name: String,
    pub icon: String,
    pub description: String,
    pub member_count: u32,
    pub join_approval: bool,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct InvitePreview {
    pub code: String,
    pub group: GroupPreview,
    /// Used up, revoked or expired.
    pub expired: bool,
    /// The caller is already a member.
    pub member: bool,
}

/// The outcome of joining: in, or waiting for approval.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct JoinOutcome {
    pub joined: Option<GroupState>,
    pub pending: bool,
}

/// A friend asked me into a group.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct GroupInvite {
    pub id: Id,
    pub group: GroupPreview,
    pub by: ChatUser,
    pub at: i64,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct DiscoverList {
    pub groups: Vec<GroupPreview>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct JoinRequest {
    pub user: ChatUser,
    /// `invite` or `public`.
    pub via: String,
    pub at: i64,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct JoinRequestList {
    pub requests: Vec<JoinRequest>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct AuditEntry {
    pub id: i64,
    pub actor: ChatUser,
    pub action: String,
    pub target: String,
    pub details: serde_json::Value,
    pub at: i64,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct AuditList {
    pub entries: Vec<AuditEntry>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct MessageCreate {
    pub content: String,
    pub reply_to: Option<Id>,
    /// Mention the author of `reply_to`.
    pub mention_reply: bool,
    pub attachments: Vec<Id>,
    /// Matches the answer and the push to this send; repeated sends with
    /// the same nonce within a minute make one message.
    pub nonce: String,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct MessageEdit {
    pub content: String,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct MessagePage {
    /// Oldest first.
    pub messages: Vec<Message>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct SearchResult {
    /// Newest first.
    pub messages: Vec<Message>,
    /// More older results (search again with `before` = the last ID).
    pub more: bool,
}

#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ReadBody {
    pub message_id: Id,
}

#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ReadAll {
    /// One group; none: everything.
    pub group: Option<Id>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct AttachmentCreate {
    pub channel_id: Id,
    pub name: String,
    pub mime: String,
    pub size: u64,
}

/// Where to `PUT` the bytes of a new attachment (a path with a one-time
/// token; no signature).
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct AttachmentUpload {
    pub id: Id,
    pub upload: String,
}

/// A token for `/media/...` (attachments, thumbnails, avatars, icons), as
/// `?t=`.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct MediaToken {
    pub token: String,
    pub expires_at: i64,
}

#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct StatusBody {
    pub status: Status,
}

// ----------------------------------------------------- the chat connection

/// Message types of the chat connection.
pub mod types {
    /// Client → server first: `{status, idle}` → `ack{snapshot}`.
    pub const HELLO: &str = "chat.hello";
    /// Client → server: `{idle}` when it goes idle or comes back.
    pub const IDLE: &str = "chat.idle";
    /// Client → server: `{channel_id}` while typing; server → clients:
    /// `{channel_id, user_id}`.
    pub const TYPING: &str = "chat.typing";
    pub const MESSAGE: &str = "chat.message";
    pub const MESSAGE_UPDATED: &str = "chat.message_updated";
    pub const MESSAGE_DELETED: &str = "chat.message_deleted";
    pub const PINS: &str = "chat.pins";
    pub const READ: &str = "chat.read";
    pub const GROUP: &str = "chat.group";
    pub const GROUP_REMOVED: &str = "chat.group_removed";
    pub const MEMBER: &str = "chat.member";
    pub const MEMBER_REMOVED: &str = "chat.member_removed";
    pub const PRESENCE: &str = "chat.presence";
    pub const DM: &str = "chat.dm";
    pub const INVITE: &str = "chat.invite";
    pub const INVITE_REMOVED: &str = "chat.invite_removed";
    pub const REQUEST: &str = "chat.request";
    pub const PREFS: &str = "chat.prefs";
    /// An account's name or avatar changed (a [`ChatUser`]).
    pub const USER: &str = "chat.user";

    // ---- voice (§7)
    /// Who is in a voice channel now ([`VoiceChannelState`]), to everyone
    /// who sees it.
    pub const VOICE_STATE: &str = "voice.state";
    /// Server → the device: its downlink's next description
    /// ([`VoiceOffer`]).
    pub const VOICE_OFFER: &str = "voice.offer";
    /// Device → server: the answer to a [`VOICE_OFFER`] ([`VoiceOffer`]).
    pub const VOICE_ANSWER: &str = "voice.answer";
    /// Server → the device: another participant's frame key for it
    /// ([`VoiceKeyPush`]).
    pub const VOICE_KEYS: &str = "voice.keys";
    /// Server → the device: it is out of the channel ([`VoiceLeft`]).
    pub const VOICE_LEFT: &str = "voice.left";
    /// Server → the device: join another channel, or this one again on
    /// another node ([`VoiceMoved`]).
    pub const VOICE_MOVED: &str = "voice.moved";
    /// Server → the device: tracks someone watches that it does not send
    /// ([`VoiceDemand`]).
    pub const VOICE_DEMAND: &str = "voice.demand";
    /// Server → the sharing device: someone asks to take control
    /// ([`ControlRequest`]).
    pub const CONTROL_REQUEST: &str = "voice.control_request";
    /// Server → the asking device: the sharer's answer ([`ControlAnswered`]).
    pub const CONTROL_ANSWERED: &str = "voice.control_answered";
    /// Server → both: the control given ended ([`ControlEnded`]).
    pub const CONTROL_ENDED: &str = "voice.control_ended";

    // ---- calls in direct messages (§7.6)
    /// To the callee's devices: a call rings ([`Call`]).
    pub const CALL_RING: &str = "call.ring";
    /// To the caller's other devices: it calls from one of them ([`Call`]).
    pub const CALL_OUTGOING: &str = "call.outgoing";
    /// To both devices in the call: who takes part now ([`Call`]).
    pub const CALL_ANSWERED: &str = "call.answered";
    /// The call is over for this device ([`CallEnded`]).
    pub const CALL_ENDED: &str = "call.ended";
    /// Device ↔ device through the server: offer, answer, candidates
    /// ([`CallSignal`]).
    pub const CALL_SIGNAL: &str = "call.signal";
}

// ===================================================================== voice

/// How a voice channel's media is protected (§10.4).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VoiceMode {
    /// Each participant's frames are encrypted with its own key.
    #[default]
    E2ee,
    /// No frame encryption (media is still encrypted to the forwarder).
    Compatible,
}

/// A device in a voice channel.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct VoiceParticipant {
    /// Its number in the channel; its tracks' stream is `p<number>`.
    pub participant: u32,
    pub user_id: String,
    pub device_id: String,
    /// The device's Ed25519 key, which signs its frame-key boxes, and its
    /// X25519 key, which they are sealed to (§10).
    pub ed25519: String,
    pub x25519: String,
    pub self_muted: bool,
    pub self_deafened: bool,
    /// By a moderator.
    pub server_muted: bool,
    pub server_deafened: bool,
    /// The account lost `speak` here meanwhile: nothing it says is
    /// forwarded.
    pub cannot_speak: bool,
    /// The kinds of track it sends now ([`track`]).
    pub tracks: Vec<String>,
    /// Its screen share, as it describes it.
    pub screen: Option<VoiceScreen>,
    /// Unix milliseconds.
    pub joined_at: i64,
}

/// The kinds of track in a voice channel (§7.3).
pub mod track {
    pub const MIC: &str = "mic";
    /// The camera: 720p30 and 320×180@15.
    pub const CAM_HI: &str = "cam_hi";
    pub const CAM_LO: &str = "cam_lo";
    /// The screen share, its sound, and a 720p30 copy encoded while someone
    /// watches it.
    pub const SCREEN: &str = "screen";
    pub const SCREEN_720: &str = "screen_720";
    pub const SCREEN_AUDIO: &str = "screen_audio";
    pub const ALL: [&str; 6] = [MIC, CAM_HI, CAM_LO, SCREEN, SCREEN_720, SCREEN_AUDIO];
    /// Tracks only some receivers want (everyone gets every `mic`).
    pub fn chosen(kind: &str) -> bool {
        kind != MIC
    }
    /// A forwarded track's stream: `p<participant>-<kind>`.
    pub fn stream(participant: u32, kind: &str) -> String {
        format!("p{participant}-{kind}")
    }
    /// (participant, kind) of a forwarded track's stream.
    pub fn parse(stream: &str) -> Option<(u32, &str)> {
        let (p, kind) = stream.strip_prefix('p')?.split_once('-')?;
        Some((p.parse().ok()?, kind))
    }
}

/// A screen share as its sender describes it.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct VoiceScreen {
    /// `720p30`, `720p60`, `1080p30` or `1080p60`.
    pub quality: String,
    /// With sound.
    pub audio: bool,
    /// The whole screen (only then can control be requested), or one window.
    pub whole: bool,
}

/// A track a device publishes: its media section and kind.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct VoiceTrack {
    pub mid: String,
    pub kind: String,
}

/// One participant's track a device wants.
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(default)]
pub struct VoiceTrackRef {
    pub participant: u32,
    pub kind: String,
}

/// `PUT …/voice/subscribe`: the chosen tracks this device receives (all
/// `mic`s come anyway).
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct VoiceSubscribe {
    pub tracks: Vec<VoiceTrackRef>,
}

/// `voice.demand`: kinds someone watches that the device does not send
/// (`screen_720`).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct VoiceDemand {
    pub channel_id: Id,
    pub kinds: Vec<String>,
}

/// `voice.control_request`: `from` (device `from_device`) asks to control
/// the screen this device shares.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ControlRequest {
    pub id: String,
    pub channel_id: Id,
    pub from: ChatUser,
    pub from_device: String,
    pub sharer_device: String,
}

/// `POST …/control-request/{id}/answer`
#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ControlAnswer {
    pub allowed: bool,
    pub clipboard: bool,
}

/// `voice.control_answered`: to the asking device; when allowed it opens a
/// remote desktop session to `sharer_device`.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ControlAnswered {
    pub id: String,
    pub channel_id: Id,
    pub sharer_device: String,
    pub allowed: bool,
    pub clipboard: bool,
}

/// `voice.control_ended`: the control given is over (taken back, the share
/// or the voice ended).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ControlEnded {
    pub id: String,
    pub channel_id: Id,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct VoiceChannelState {
    pub channel_id: Id,
    pub participants: Vec<VoiceParticipant>,
}

/// `POST /chat/channels/{id}/voice/join`
#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct VoiceJoin {
    pub self_muted: bool,
    pub self_deafened: bool,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct VoiceJoined {
    pub channel_id: Id,
    /// This device's number.
    pub participant: u32,
    pub mode: VoiceMode,
    /// The channel's bitrate (kbit/s).
    pub bitrate_kbps: u32,
    /// For reaching the forwarder.
    pub ice_servers: Vec<crate::realtime::IceServer>,
    pub participants: Vec<VoiceParticipant>,
}

/// A session description: `PUT …/voice/publish` (with each media
/// section's kind; none: every audio section is a `mic`) and its answer.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct VoiceDescription {
    pub sdp: String,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub tracks: Vec<VoiceTrack>,
}

/// `PUT …/voice/state`: the device's own mute and deafen, and its screen
/// share's description (sent every time).
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct VoiceSelf {
    pub self_muted: bool,
    pub self_deafened: bool,
    pub screen: Option<VoiceScreen>,
}

/// `PATCH …/voice/participants/{participant}` (`voice_moderate`).
#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct VoiceModerate {
    pub server_muted: Option<bool>,
    pub server_deafened: Option<bool>,
    /// Another voice channel of the group the device should be in.
    pub move_to: Option<Id>,
}

/// A frame key sealed to one device (§10.2): `ephemeral` is the sender's
/// one-time X25519 key; `data` the AES-256-GCM box of
/// `frame_key(20) ‖ key_id(1) ‖ participant(u32 BE)`; `signature` the
/// sender device's Ed25519 signature of [`key_box_message`].
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct VoiceKeyBox {
    pub device_id: String,
    pub ephemeral: String,
    pub data: String,
    pub signature: String,
}

/// `POST …/voice/keys`: this device's key for the others.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct VoiceKeys {
    pub key_id: u8,
    pub boxes: Vec<VoiceKeyBox>,
}

/// `PUT /devices/self/x25519`: the device's X25519 key (base64url), signed
/// by its Ed25519 key over [`x25519_message`] (§10.1).
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct DeviceX25519 {
    pub public: String,
    pub signature: String,
}

pub fn x25519_message(public: &str) -> String {
    format!("FWX25519\n{public}")
}

/// What the sender signs for a key box: the channel (or call), its own
/// participant number, the receiving device and the box.
pub fn key_box_message(scope: &str, participant: u32, device_id: &str, ephemeral: &str, data: &str) -> String {
    format!("FWVKEY1\n{scope}\n{participant}\n{device_id}\n{ephemeral}\n{data}")
}

/// `voice.offer` (server → device) and `voice.answer` (device → server).
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct VoiceOffer {
    pub channel_id: Id,
    /// Answers count only for the latest offer.
    pub generation: u64,
    pub sdp: String,
}

/// `voice.keys`: a participant's frame key for this device.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct VoiceKeyPush {
    pub channel_id: Id,
    /// The sender.
    pub participant: u32,
    pub device_id: String,
    pub key_id: u8,
    pub ephemeral: String,
    pub data: String,
    pub signature: String,
}

/// `voice.moved`: a moderator moved the device to `to`; `None`: the
/// channel's node went away, join it again.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct VoiceMoved {
    pub channel_id: Id,
    pub to: Option<Id>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct VoiceLeft {
    pub channel_id: Id,
    /// `disconnected` (by a moderator), `replaced` (the device joined
    /// elsewhere), `forbidden` (lost the permission), `idle` (no media),
    /// `ended` (the channel went away).
    pub reason: String,
}

// ===================================================================== calls

/// A call in a direct message (§7.6).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Call {
    pub id: String,
    pub channel_id: Id,
    pub caller: ChatUser,
    pub caller_device: String,
    pub callee: ChatUser,
    /// The device that answered; empty while it rings.
    pub callee_device: String,
    /// `ringing` or `active`.
    pub state: String,
    pub video: bool,
    /// Unix milliseconds.
    pub started_at: i64,
    pub answered_at: Option<i64>,
    /// The devices' Ed25519 keys: each signs its descriptions (§10.2).
    pub caller_key: String,
    pub callee_key: String,
    /// Relays for this device (in the pushes to the devices in the call).
    pub ice_servers: Vec<crate::realtime::IceServer>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct CallStart {
    pub video: bool,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct CallEnded {
    pub call_id: String,
    pub channel_id: Id,
    /// `declined`, `missed`, `hangup`, `answered_elsewhere`, `moved`,
    /// `failed`.
    pub reason: String,
}

/// `call.signal`: `kind` is `offer`, `answer` or `candidate`; descriptions
/// carry the sender device's signature of [`call_signal_message`].
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct CallSignal {
    pub call_id: String,
    pub kind: String,
    pub data: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub signature: String,
    /// Set by the server: who sent it.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub from_device: String,
}

/// What a device signs for a description in a call: the call and the
/// description's DTLS fingerprints, which bind the media to the device.
pub fn call_signal_message(call_id: &str, fingerprints: &[String]) -> String {
    format!("FWCALL1\n{call_id}\n{}", fingerprints.join("\n"))
}

/// The DTLS fingerprints (`a=fingerprint:` values) of a description.
pub fn sdp_fingerprints(sdp: &str) -> Vec<String> {
    let mut out: Vec<String> = sdp
        .lines()
        .filter_map(|l| l.trim().strip_prefix("a=fingerprint:"))
        .map(|f| f.trim().to_ascii_lowercase())
        .collect();
    out.sort();
    out.dedup();
    out
}

#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct HelloBody {
    pub status: Option<Status>,
    pub idle: bool,
}

#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct IdleBody {
    pub idle: bool,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Typing {
    pub channel_id: Id,
    pub user_id: String,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct MessageDeleted {
    pub channel_id: Id,
    pub id: Id,
}

#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct PinsChanged {
    pub channel_id: Id,
}

/// The account's own read position (from another of its devices), or the
/// other side's in a direct message (`user_id` set).
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ReadPush {
    pub channel_id: Id,
    pub last_read_id: Id,
    pub mentions: u32,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub user_id: String,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct GroupRemoved {
    pub group_id: Id,
    /// `left`, `kicked`, `banned` or `dissolved`.
    pub reason: String,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct MemberPush {
    pub group_id: Id,
    pub member: Member,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct MemberRemoved {
    pub group_id: Id,
    pub user_id: String,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct InviteRemoved {
    pub id: Id,
}

/// A join request was answered (to the one who asked), or a new one waits
/// (to those who approve: `user` set).
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct RequestPush {
    pub group: GroupPreview,
    pub user: Option<ChatUser>,
    /// For the one who asked: approved or not.
    pub approved: bool,
}

fn double_option<'de, T: Deserialize<'de>, D: Deserializer<'de>>(d: D) -> Result<Option<Option<T>>, D::Error> {
    Option::<T>::deserialize(d).map(Some)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_are_strings_on_the_wire() {
        let id = Id(1 << 60);
        assert_eq!(serde_json::to_string(&id).unwrap(), "\"1152921504606846976\"");
        assert_eq!(serde_json::from_str::<Id>("\"1152921504606846976\"").unwrap(), id);
        assert_eq!(serde_json::from_str::<Id>("12").unwrap(), Id(12));
    }

    #[test]
    fn absent_and_null_differ_in_updates() {
        let kept: MemberUpdate = serde_json::from_str("{}").unwrap();
        assert_eq!(kept.muted_until, None);
        let lifted: MemberUpdate = serde_json::from_str(r#"{"muted_until":null}"#).unwrap();
        assert_eq!(lifted.muted_until, Some(None));
        let set: MemberUpdate = serde_json::from_str(r#"{"muted_until":5}"#).unwrap();
        assert_eq!(set.muted_until, Some(Some(5)));
    }
}
