// Chat wire types (`crates/protocol/src/chat.rs`, `docs/protocol/chat-design.md`).
// IDs are decimal strings: Snowflakes keep every bit that way.

export type Id = string;

export const PERM = {
  VIEW: 1 << 0,
  SEND: 1 << 1,
  ATTACH: 1 << 2,
  MENTION_ALL: 1 << 3,
  MANAGE_MESSAGES: 1 << 4,
  READ_HISTORY: 1 << 5,
  CONNECT: 1 << 8,
  SPEAK: 1 << 9,
  VIDEO: 1 << 10,
  VOICE_MODERATE: 1 << 11,
  PRIORITY: 1 << 12,
  INVITE: 1 << 16,
  KICK: 1 << 17,
  BAN: 1 << 18,
  MUTE: 1 << 19,
  MANAGE_CHANNELS: 1 << 20,
  MANAGE_ROLES: 1 << 21,
  MANAGE_GROUP: 1 << 22,
  ADMINISTRATOR: 1 << 30,
} as const;

export type PermName = keyof typeof PERM;

/// What a channel overwrite can change.
export const CHANNEL_PERMS: PermName[] = [
  'VIEW',
  'SEND',
  'ATTACH',
  'MENTION_ALL',
  'MANAGE_MESSAGES',
  'READ_HISTORY',
  'CONNECT',
  'SPEAK',
  'VIDEO',
  'VOICE_MODERATE',
  'PRIORITY',
  'MANAGE_CHANNELS',
];

export const ALL_PERMS: PermName[] = Object.keys(PERM) as PermName[];

export function has(perms: number, bit: number): boolean {
  return (perms & bit) === bit;
}

export type ChannelKind = 'text' | 'voice' | 'category' | 'announcement' | 'dm';
export type Status = 'online' | 'idle' | 'dnd' | 'invisible' | 'offline';
export type Notify = 'all' | 'mentions' | 'none';

export interface ChatUser {
  user_id: string;
  username: string;
  nickname: string;
  avatar: string;
  gone: boolean;
}

export interface Group {
  id: Id;
  name: string;
  icon: string;
  owner_id: string;
  public: boolean;
  join_approval: boolean;
  description: string;
  created_at: number;
  frozen: boolean;
  member_count: number;
  permissions: number;
}

export interface Role {
  id: Id;
  name: string;
  color: number;
  permissions: number;
  position: number;
  mentionable: boolean;
}

export interface Overwrite {
  role_id: Id;
  allow: number;
  deny: number;
}

export interface Channel {
  id: Id;
  group_id: Id | null;
  kind: ChannelKind;
  name: string;
  topic: string;
  parent_id: Id | null;
  position: number;
  slow_mode_s: number;
  voice_bitrate: number;
  voice_limit: number;
  /// A voice channel in compatible mode (no frame encryption).
  voice_compat: boolean;
  overwrites: Overwrite[];
  permissions: number;
  last_message_id: Id | null;
}

export interface Member {
  user: ChatUser;
  nickname: string;
  roles: Id[];
  joined_at: number;
  muted_until: number | null;
}

export interface GroupState {
  group: Group;
  channels: Channel[];
  roles: Role[];
  me: Member;
}

export interface Dm {
  channel: Channel;
  user: ChatUser;
  friends: boolean;
  peer_read_id: Id | null;
}

export interface Mentions {
  users: string[];
  roles: Id[];
  everyone: boolean;
  here: boolean;
}

export interface Attachment {
  id: Id;
  name: string;
  mime: string;
  size: number;
  width: number;
  height: number;
  thumb: boolean;
}

export interface Reply {
  id: Id;
  author: ChatUser;
  content: string;
  deleted: boolean;
  attachments: number;
}

export interface Message {
  id: Id;
  channel_id: Id;
  author: ChatUser;
  content: string;
  reply: Reply | null;
  mentions: Mentions;
  attachments: Attachment[];
  pinned: boolean;
  edited_at: number | null;
  deleted: boolean;
  created_at: number;
  nonce?: string;
  /// A call's record in a direct message (its author called).
  call?: CallRecord;
}

export interface CallRecord {
  duration_s: number;
  missed: boolean;
}

export interface ReadState {
  channel_id: Id;
  last_read_id: Id;
  mentions: number;
}

export interface Presence {
  user_id: string;
  status: Status;
}

export interface Prefs {
  notify: Record<string, Notify>;
  quiet_start: string;
  quiet_end: string;
  hide_content: boolean;
  read_receipts: boolean;
}

export interface GroupPreview {
  id: Id;
  name: string;
  icon: string;
  description: string;
  member_count: number;
  join_approval: boolean;
}

export interface GroupInvite {
  id: Id;
  group: GroupPreview;
  by: ChatUser;
  at: number;
}

export interface ChatLimits {
  max_content_chars: number;
  max_attachments: number;
  max_attachment_bytes: number;
  create_groups: boolean;
  /// Group owners may put a voice channel in compatible mode.
  voice_compat: boolean;
}

export interface Snapshot {
  me: ChatUser;
  status: Status;
  groups: GroupState[];
  dms: Dm[];
  read_states: ReadState[];
  presences: Presence[];
  prefs: Prefs;
  group_invites: GroupInvite[];
  limits: ChatLimits;
  voice: VoiceChannelState[];
  calls: Call[];
}

// ------------------------------------------------------------------ voice

export type VoiceMode = 'e2ee' | 'compatible';

export interface VoiceParticipant {
  participant: number;
  user_id: string;
  device_id: string;
  ed25519: string;
  x25519: string;
  self_muted: boolean;
  self_deafened: boolean;
  server_muted: boolean;
  server_deafened: boolean;
  /// The account lost `speak` here: nothing it says is forwarded.
  cannot_speak: boolean;
  /// Kinds of track it sends now (`mic`, `cam_hi`, `cam_lo`, `screen`,
  /// `screen_720`, `screen_audio`).
  tracks: string[];
  screen: VoiceScreen | null;
  joined_at: number;
}

export interface VoiceScreen {
  /// `720p30`, `720p60`, `1080p30` or `1080p60`.
  quality: string;
  audio: boolean;
  /// The whole screen (control can be asked for), or one window.
  whole: boolean;
}

export interface ControlRequest {
  id: string;
  channel_id: Id;
  from: ChatUser;
  from_device: string;
  sharer_device: string;
}

export interface ControlAnswered {
  id: string;
  channel_id: Id;
  sharer_device: string;
  allowed: boolean;
  clipboard: boolean;
}

export interface ControlEnded {
  id: string;
  channel_id: Id;
}

export interface VoiceChannelState {
  channel_id: Id;
  participants: VoiceParticipant[];
}

export interface IceServer {
  urls: string[];
  username: string;
  credential: string;
}

export interface VoiceJoined {
  channel_id: Id;
  participant: number;
  mode: VoiceMode;
  bitrate_kbps: number;
  ice_servers: IceServer[];
  participants: VoiceParticipant[];
}

export interface VoiceOffer {
  channel_id: Id;
  generation: number;
  sdp: string;
}

/// Moved by a moderator to `to`; no `to`: the channel's node went away.
export interface VoiceMoved {
  channel_id: Id;
  to?: Id | null;
}

export interface VoiceKeyPush {
  channel_id: Id;
  participant: number;
  device_id: string;
  key_id: number;
  ephemeral: string;
  data: string;
  signature: string;
}

export interface Call {
  id: string;
  channel_id: Id;
  caller: ChatUser;
  caller_device: string;
  callee: ChatUser;
  callee_device: string;
  state: 'ringing' | 'active';
  video: boolean;
  started_at: number;
  answered_at: number | null;
  caller_key: string;
  callee_key: string;
  ice_servers: IceServer[];
}

export interface CallSignal {
  call_id: string;
  kind: 'offer' | 'answer' | 'candidate';
  data: string;
  signature?: string;
  from_device?: string;
}

/// What a device signs for a key box (`farwing_protocol::chat::key_box_message`).
export function keyBoxMessage(scope: string, participant: number, device: string, ephemeral: string, data: string) {
  return `FWVKEY1\n${scope}\n${participant}\n${device}\n${ephemeral}\n${data}`;
}

/// What a device signs for a call description.
export function callSignalMessage(callId: string, fingerprints: string[]) {
  return `FWCALL1\n${callId}\n${fingerprints.join('\n')}`;
}

/// A description's DTLS fingerprints, sorted, lower case.
export function sdpFingerprints(sdp: string): string[] {
  const out = sdp
    .split(/\r?\n/)
    .map((l) => l.trim())
    .filter((l) => l.startsWith('a=fingerprint:'))
    .map((l) => l.slice('a=fingerprint:'.length).trim().toLowerCase());
  return [...new Set(out)].sort();
}

export interface Invite {
  code: string;
  group_id: Id;
  by: ChatUser;
  max_uses: number | null;
  uses: number;
  expires_at: number | null;
  revoked: boolean;
  created_at: number;
}

export interface InvitePreview {
  code: string;
  group: GroupPreview;
  expired: boolean;
  member: boolean;
}

export interface JoinOutcome {
  joined: GroupState | null;
  pending: boolean;
}

export interface JoinRequest {
  user: ChatUser;
  via: string;
  at: number;
}

export interface Ban {
  user: ChatUser;
  by: ChatUser;
  reason: string;
  at: number;
}

export interface AuditEntry {
  id: number;
  actor: ChatUser;
  action: string;
  target: string;
  details: Record<string, unknown>;
  at: number;
}

export interface AttachmentUpload {
  id: Id;
  upload: string;
}

export interface MediaToken {
  token: string;
  expires_at: number;
}

/// Compare two IDs (decimal strings of the same kind of number).
export function idCmp(a: Id, b: Id): number {
  if (a.length !== b.length) return a.length - b.length;
  return a < b ? -1 : a > b ? 1 : 0;
}

export function idAfter(a: Id | null | undefined, b: Id | null | undefined): boolean {
  if (!a) return false;
  if (!b) return true;
  return idCmp(a, b) > 0;
}

export function userName(u: { nickname?: string; username?: string } | undefined): string {
  return u?.nickname || u?.username || '';
}
