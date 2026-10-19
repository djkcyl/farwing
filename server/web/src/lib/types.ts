// Shapes of the server API (`crates/protocol/src/api.rs`,
// `server/src/http/{me,admin_api}.rs`).

export interface UserSummary {
  user_id: string;
  username: string;
  nickname: string;
}

export interface Participant {
  participant_id: string;
  session_id: string;
  user_id: string;
  nickname: string;
  device_id: string;
  device_alias: string;
  relation: string;
  purpose: string;
  started_at: number;
}

export interface DevicePolicy {
  control: boolean;
  files: boolean;
  ports: boolean;
  power: boolean;
  wol: boolean;
  assist: boolean;
  max_participants: number;
}

export interface Device {
  device_id: string;
  alias: string;
  status: string;
  platform: string;
  version: string;
  capabilities: string[];
  policy: DevicePolicy;
  wol_supported: boolean;
  wallpaper_url: string;
  participants: Participant[];
  last_seen_at: number;
  owner: UserSummary | null;
  granted: string[];
  public_key: string;
}

export interface DeviceList {
  current_device: Device;
  own: Device[];
  shared: Device[];
}

export interface DeviceDetail {
  device_id: string;
  alias: string;
  platform: string;
  os: string;
  version: string;
  online: boolean;
  registered_at: number;
  last_seen_at: number;
  connect_id: string | null;
  hardware: { cpu: string; base_board: string; video: string[]; memory: number; screen: string; mac: string } | null;
  owner: UserSummary | null;
}

export interface Me {
  user_id: string;
  username: string;
  nickname: string;
}

export interface Security {
  role: 'admin' | 'user';
  totp: boolean;
  recovery_codes_left: number;
  verified: boolean;
}

export interface AuthSession {
  device_id: string;
  device_alias: string;
  platform: string;
  created_at: number;
  last_used_at: number;
  current: boolean;
}

export interface Friend {
  user: UserSummary;
  since: number;
}

export interface FriendRequest {
  id: string;
  user: UserSummary;
  created_at: number;
}

export interface FriendRequests {
  incoming: FriendRequest[];
  outgoing: FriendRequest[];
}

export interface Grants {
  default: string[];
  devices: Record<string, string[]>;
}

export interface SavedDevice {
  publisher_device_id: string;
  connect_id: string;
  remark: string;
  is_favorite: boolean;
  last_connected_at: number | null;
  favorited_at: number | null;
  public_key: string;
}

export interface Announcement {
  id: string;
  platform: string;
  channel: string;
  version: string;
  notes: string;
  url: string;
  published_at: number;
  prerelease: boolean;
}

export interface WebInfo {
  instance_name: string;
  version: string;
  public_url: string;
  tls_fingerprint: string | null;
  registration: boolean;
  guests: boolean;
  downloads: Announcement[];
}

// Admin --------------------------------------------------------------------

export interface AuditEntry {
  id: number;
  at: number;
  actor_user: string | null;
  actor_name: string;
  kind: string;
  target: string;
  detail: string;
  ok: boolean;
  ip: string;
}

export interface Overview {
  version: string;
  instance_name: string;
  public_url: string;
  uptime_s: number;
  tls_fingerprint: string | null;
  /// Automatic certificate (`[tls] mode = "acme"`).
  acme: AcmeStatus | null;
  turn_mode: string;
  nat_discovery: string[];
  db_bytes: number;
  accounts: number;
  admins: number;
  devices: number;
  devices_online: number;
  sessions: number;
  sessions_connected: number;
  guests: boolean;
  registration: boolean;
  recent: AuditEntry[];
}

export interface AcmeStatus {
  domains: string[];
  directory: string;
  challenge: string;
  issued: boolean;
  not_before: number | null;
  not_after: number | null;
  renew_at: number | null;
  last_error: string | null;
  last_attempt: number | null;
}

export interface AccountSummary {
  id: string;
  username: string;
  nickname: string;
  role: 'admin' | 'user';
  disabled: boolean;
  totp: boolean;
  created_at: number;
  devices: number;
  last_seen_at: number;
}

export interface AdminDevice {
  device_id: string;
  alias: string;
  name: string;
  platform: string;
  os: string;
  version: string;
  owner_id: string | null;
  owner: string | null;
  online: boolean;
  host: boolean;
  connect_id: string | null;
  created_ip: string;
  created_at: number;
  last_seen_at: number;
}

export interface AdminLogin {
  device_id: string;
  alias: string;
  platform: string;
  created_at: number;
  last_used_at: number;
}

export interface AdminFriend {
  user_id: string;
  username: string;
  nickname: string;
  since: number;
  grants_given: Grants;
  grants_received: Grants;
}

export interface UserDetail {
  account: AccountSummary;
  devices: AdminDevice[];
  logins: AdminLogin[];
  friends: AdminFriend[];
}

export interface Invite {
  id: string;
  created_by: string;
  note: string;
  max_uses: number | null;
  uses: number;
  expires_at: number | null;
  auto_friend: boolean;
  revoked_at: number | null;
  created_at: number;
}

export interface InviteUse {
  user_id: string;
  username: string;
  ip: string;
  used_at: number;
}

export interface LiveSession {
  id: string;
  host_device_id: string;
  controller_device_id: string;
  controller: {
    device_id: string;
    alias: string;
    user_id: string;
    username: string;
    nickname: string;
    platform: string;
    version: string;
  };
  purpose: string;
  connect_type: string;
  relation: string;
  created_at: number;
  connected: boolean;
  host_alias: string;
  host_owner: string | null;
  controller_alias: string;
}

export interface RuntimeSettings {
  instance_name: string;
  guests: boolean;
  registration: boolean;
  bitrate_limit_mbps: number | null;
  /// What browsers may do at most (always with `view`).
  web_caps: string[];
}

export interface SettingsView {
  runtime: RuntimeSettings;
  config: Record<string, unknown>;
}
