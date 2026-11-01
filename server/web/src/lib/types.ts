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
  camera: boolean;
  listen: boolean;
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
  relayed_sessions: number;
  nodes: number;
  nodes_usable: number;
  forwarding: boolean;
  forwarding_rooms: number;
  forwarding_viewers: number;
  egress: EgressSummary;
}

/// What the relays and forwarders send for sessions.
export interface EgressSummary {
  /// Bits per second this server sends now.
  local_bps: number;
  /// The edge nodes', together.
  nodes_bps: number;
  /// This month (UTC), Unix seconds.
  month_start: number;
  month_end: number;
  top: EgressAccount[];
}

export interface EgressAccount {
  /// User ID; empty for guests (sessions with no account).
  payer: string;
  username: string;
  nickname: string;
  bps: number;
  month_bytes: number;
}

export interface AccountEgress {
  bps: number;
  month_bytes: number;
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

export interface UserEntry extends AccountSummary {
  egress: AccountEgress;
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
  egress: AccountEgress;
  policy: UserPolicy;
  policy_default: EgressPolicy;
  policy_effective: EffectivePolicy;
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

/// The controller's summary of a connection.
export interface ViewerReport {
  route: string;
  relay: string;
  seconds: number;
  width: number;
  height: number;
  fps: number;
  rtt_ms: number;
  stalls: number;
  reconnects: number;
}

export interface FileRecord {
  name: string;
  count: number;
  bytes: number;
  upload: boolean;
  outcome: 'done' | 'cancelled' | 'failed' | 'interrupted';
}

/// The host's summary of what a participant did.
export interface HostReport {
  controlled: boolean;
  camera: boolean;
  files: FileRecord[];
  more_files: number;
  ports: string[];
}

export interface RecordParty {
  device_id: string;
  name: string;
  user_id: string;
  username: string;
  nickname: string;
  platform: string;
}

export type RecordOutcome = 'open' | 'ended' | 'rejected' | 'code_invalid' | 'no_answer' | 'busy';

/// A connection (`docs/connection-records.md`).
export interface ConnectionRecord {
  id: number;
  host: RecordParty;
  controller: RecordParty;
  relation: string;
  admission: string;
  purpose: string;
  direct: boolean;
  ip: string;
  started_at: number;
  ended_at: number | null;
  outcome: RecordOutcome;
  end_reason: string;
  reconnects: number;
  viewer: ViewerReport | null;
  host_report: HostReport | null;
}

/// A notification (`docs/notifications.md`).
export interface Notice {
  id: number;
  kind: string;
  data: Record<string, unknown> | null;
  created_at: number;
  read: boolean;
}

export interface NoticeList {
  notices: Notice[];
  unread: number;
}

export interface NoticeSettings {
  settings: { kind: string; enabled: boolean }[];
}

/// An event of the account.
export interface AccountEvent {
  id: number;
  at: number;
  kind: string;
  actor: string;
  target: string;
  detail: Record<string, unknown> | null;
  ok: boolean;
  ip: string;
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
  local_relay: boolean;
  relay_nodes: string[];
  /// Forwarded: the edge node's name, empty for this server.
  forwarded_by: string | null;
  /// Bits per second the relays and forwarders send for it now.
  egress_bps: number;
}

export type TlsMode = 'self_signed' | 'provided' | 'acme' | 'proxy';
export type TurnMode = 'builtin' | 'external';
export type AcmeChallenge = 'auto' | 'tls-alpn-01' | 'http-01';

/// Limits and timeouts (`limits` in the settings).
export interface LimitsSettings {
  heartbeat_ms: number;
  idle_timeout_ms: number;
  resume_ttl_s: number;
  ticket_ttl_s: number;
  max_frame_bytes: number;
  max_sdp_bytes: number;
  max_candidates: number;
  max_candidate_bytes: number;
  ws_rate_per_s: number;
  ws_rate_burst: number;
  device_init_per_ip_hour: number;
  login_failures_per_ip: number;
  login_failures_per_user: number;
  assist_failures_per_minute: number;
  assist_host_reply_ms: number;
  assist_decision_wait_ms: number;
  guest_token_ttl_s: number;
}

export interface StunEndpoint {
  listen: string;
  public: string | null;
}

/// A payer's egress policy (`docs/protocol/egress-policy.md` §3.1).
export interface EgressPolicy {
  relay: boolean;
  forward: boolean;
  rate_mbps: number | null;
  quota_gb: number | null;
}

/// An account's own policy: `null` takes the default; `rate_mbps` and
/// `quota_gb` 0: unlimited.
export interface UserPolicy {
  relay: boolean | null;
  forward: boolean | null;
  rate_mbps: number | null;
  quota_gb: number | null;
}

/// The policy in force.
export interface EffectivePolicy {
  relay: boolean;
  forward: boolean;
  rate_bps: number | null;
  quota_bytes: number | null;
}

export interface EgressSettings {
  limit_mbps: number | null;
  users: EgressPolicy;
  guests: EgressPolicy;
}

/// The server's settings (`docs/server-admin-api.md`); secrets are empty
/// when read.
export interface ServerSettings {
  listen: string;
  public_url: string;
  instance_name: string;
  trusted_proxies: string[];
  log: string;
  registration: boolean;
  tls: {
    mode: TlsMode;
    hostnames: string[];
    acme: {
      domains: string[];
      contact: string;
      directory: string;
      challenge: AcmeChallenge;
      http_listen: string | null;
      ca_root: string | null;
      agree_tos: boolean;
    };
  };
  guest: { enabled: boolean };
  turn: {
    mode: TurnMode;
    urls: string[];
    shared_secret: string;
    credential_ttl: number;
    force_relay: boolean;
    relay: boolean;
    listen_udp: string | null;
    listen_tcp: string | null;
    listen_tls: string | null;
    public_ip: string | null;
    realm: string;
    relay_bind: string;
    relay_ip: string | null;
    relay_port_min: number | null;
    relay_port_max: number | null;
    stun: StunEndpoint[];
  };
  sfu: SfuSettings;
  e2ee: E2eeLevel;
  limits: LimitsSettings;
  web: {
    enabled: boolean;
    admin_allow: string[];
    session_idle_minutes: number;
    session_max_hours: number;
    audit_days: number;
    caps: string[];
  };
  egress: EgressSettings;
}

export interface SfuSettings {
  enabled: boolean;
  threshold: number;
  port: number;
}

export type E2eeLevel = 'required' | 'allowed' | 'off';

export interface ListenPending {
  url: string;
  confirm_by: number;
}

export interface SettingsView {
  settings: ServerSettings;
  turn_secret_set: boolean;
  listen_locked: boolean;
  data_dir: string;
  tls: { fingerprint: string | null; acme: AcmeStatus | null; provided: boolean };
  relay: {
    turn_udp: string | null;
    turn_tcp: string | null;
    turn_tls: string | null;
    stun: string[];
    nat_discovery: string[];
    local_ipv4: string | null;
  };
  setup: boolean;
  listen_pending: ListenPending | null;
}

/// What saving settings did.
export interface Outcome {
  listen_url: string | null;
  confirm_by: number | null;
  fingerprint_changed: boolean;
  relay_restarted: boolean;
  /// Online devices told the new address or certificate.
  clients_told: number | null;
}

/// What saving settings would do.
export interface Preview {
  relay_restarts: boolean;
  relay_allocations: number;
}

export type Applied = SettingsView & { outcome: Outcome };

export interface SetupInfo {
  state: 'setup' | 'recovery' | 'done';
  current?: SettingsView;
}

export interface NodeHealth {
  turn_udp: string | null;
  turn_tcp: string | null;
  turn_tls: string | null;
  stun: string[];
  allocations: number;
  sessions: string[];
  egress_bps: number;
  cpu_percent: number | null;
  memory_mb: number | null;
  error: string | null;
  version: string;
  sfu_port: number | null;
  rooms: unknown[];
}

/// An edge relay node (`/api/v2/admin/nodes`).
export interface EdgeNode {
  id: string;
  name: string;
  region: string;
  /// Empty until the node joined.
  public_key: string;
  enabled: boolean;
  settings: {
    public_ip: string | null;
    listen_udp: string | null;
    listen_tcp: string | null;
    listen_tls: string | null;
    relay_port_min: number | null;
    relay_port_max: number | null;
    relay_bind: string;
    stun: StunEndpoint[];
    egress_limit_mbps: number | null;
    sfu_port: number | null;
  };
  created_at: number;
  joined_at: number | null;
  last_seen_at: number | null;
  last_ip: string;
  online: boolean;
  usable: boolean;
  connected_at: number | null;
  address: string | null;
  health: NodeHealth | null;
}

export interface JoinCommand {
  node: EdgeNode;
  command: string;
  link: string;
  expires_at: number;
}

export interface AdminToken {
  id: string;
  name: string;
  scope: 'read' | 'admin';
  allow: string[];
  created_by: string;
  created_by_name: string;
  created_at: number;
  expires_at: number | null;
  last_used_at: number | null;
  last_used_ip: string;
  revoked_at: number | null;
}
