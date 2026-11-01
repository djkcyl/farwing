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
