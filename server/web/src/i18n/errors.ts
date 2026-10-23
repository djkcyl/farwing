// Server errors in the interface language (`docs/i18n.md`): a text per error
// code and per `details.reason` (`farwing_protocol::error::reason`). The
// server's `message` is for logs and is never shown. Texts with a parameter
// read it from `details` and have a plainer form when it is missing.
import type { Lang } from './lang';

/** An error's `details` (any JSON; only object fields are read). */
export type ErrorDetails = unknown;

type Param = number | undefined;

const zhCodes = {
  bad_request: '请求无效',
  unauthorized: '需要登录',
  device_unauthorized: '设备验证失败',
  device_mismatch: '登录与本设备不匹配',
  session_revoked: '登录已失效',
  forbidden: '无权访问',
  not_found: '不存在',
  device_not_found: '设备不存在',
  device_offline: '设备离线',
  host_disabled: '对方不允许连接',
  busy: '对方的连接数已达上限',
  conflict: '状态已变化',
  assist_unavailable: '对方未开启访问码，也未信任本设备',
  assist_code_required: '需要访问码',
  assist_code_invalid: '访问码不正确',
  assist_rejected: '对方拒绝了连接',
  user_not_found: '用户不存在',
  already_friends: '已经是好友',
  timeout: '等待对方响应超时',
  payload_too_large: '请求数据过大',
  rate_limited: '操作过于频繁',
  protocol_unsupported: '网页端与服务器版本不兼容',
  registration_disabled: '服务器未开放注册',
  internal: '服务器内部错误',
  unavailable: '服务不可用',
  setup_required: '服务器尚未初始化',
  network: '无法连接服务器',
  unknown: '未知错误',
};

type Codes = typeof zhCodes;

const enCodes: Codes = {
  bad_request: 'Invalid request',
  unauthorized: 'Sign-in required',
  device_unauthorized: 'Device verification failed',
  device_mismatch: "The sign-in doesn't match this device",
  session_revoked: 'Sign-in expired',
  forbidden: 'Access denied',
  not_found: 'Not found',
  device_not_found: 'Device not found',
  device_offline: 'The device is offline',
  host_disabled: "The device doesn't allow connections",
  busy: 'The device has reached its connection limit',
  conflict: 'The state has changed',
  assist_unavailable: "The device has no access code and doesn't trust this device",
  assist_code_required: 'Access code required',
  assist_code_invalid: 'Incorrect access code',
  assist_rejected: 'The connection was declined',
  user_not_found: 'User not found',
  already_friends: 'Already friends',
  timeout: 'Timed out waiting for a response',
  payload_too_large: 'Request too large',
  rate_limited: 'Too many requests',
  protocol_unsupported: "This web console version isn't compatible with the server",
  registration_disabled: 'Registration is closed on this server',
  internal: 'Internal server error',
  unavailable: 'Service unavailable',
  setup_required: 'This server has not been set up',
  network: "Can't connect to the server",
  unknown: 'Unknown error',
};

const zhReasons = {
  // Signing in and the account
  login_failed: '用户名或密码不正确，或账号已停用',
  password_incorrect: '密码不正确',
  current_password_incorrect: '当前密码不正确',
  totp_required: '需要二次验证',
  totp_code_incorrect: '验证码不正确',
  totp_admin_only: '二次验证仅用于管理员账号',
  totp_already_enabled: '二次验证已开启',
  totp_setup_required: '未开始设置二次验证',
  admin_required: '需要管理员权限',
  admin_network_denied: '不允许从此网络访问管理后台',
  guests_disabled: '服务器未开启游客访问',
  clock_skew: '本机时间与服务器时间相差过大',
  // Input checks
  username_invalid: (max: Param) => `用户名应为 1–${max ?? 64} 位字母、数字或 . _ - @`,
  username_taken: '用户名已被使用',
  password_too_short: (min: Param) => (min === undefined ? '密码过短' : `密码至少 ${min} 位`),
  password_too_long: (max: Param) => (max === undefined ? '密码过长' : `密码不能超过 ${max} 位`),
  nickname_invalid: '昵称不能包含控制字符',
  device_name_invalid: (max: Param) => `名称应为 1–${max ?? 64} 个可打印字符`,
  invite_invalid: '邀请码无效',
  invite_expired: '邀请码已失效',
  invite_uses_invalid: (min: Param, max: Param) =>
    min === undefined || max === undefined ? '可用次数无效' : `可用次数应为 ${min}–${max}`,
  invite_validity_invalid: '有效期应为 1 小时到 1 年',
  last_admin: '至少需要保留一个管理员',
  self_demote: '不能降级或禁用自己的账号',
  self_delete: '不能删除自己的账号',
  update_platform_invalid: '平台无效',
  update_channel_invalid: '渠道应为 stable 或 beta',
  update_version_invalid: '版本号无效',
  update_url_invalid: '下载地址应为 http(s) 链接',
  update_notes_too_long: '说明过长',
  setting_invalid: '设置无效',
  setup_code_invalid: '初始化码错误或已过期',
  setup_done: '服务器已初始化',
  admin_login_required: '需要管理员登录操作',
  nothing_to_confirm: '没有待确认的更改',
  self_friend: '不能添加自己为好友',
  self_connect: '不能连接本机',
  forwarding_disabled: '服务器未开启多人转发',
  forwarding_mode_not_allowed: '服务器不允许兼容模式转发',
  forwarding_unavailable: '多人转发暂不可用',
  // Devices, permissions and sessions
  not_own_device: '只能管理自己账号的设备',
  not_granted: '没有该设备的此项权限',
  web_not_allowed: '服务器不允许网页端使用此功能',
  power_not_granted: '没有该设备的电源权限',
  power_disabled: '对方未允许远程关机和重启',
  files_disabled: '对方未允许文件传输',
  ports_disabled: '对方未允许端口转发',
  wol_unavailable: '设备未启用网络唤醒',
  device_online: '设备已在线',
  not_friends: '对方不是好友',
  request_cancelled: '请求已取消',
  session_ended: '会话已结束',
  // A host refusing a direct connection
  not_trusted: '对方未信任本设备',
  no_access_code: '对方未开启访问码',
  host_busy: '对方正在处理另一个连接请求',
  host_locked: '对方的电脑已锁定',
  host_app_closed: '对方未打开 Farwing 设备中心',
  pairing_declined: '对方未同意配对',
  pairing_recently_declined: '对方刚拒绝过配对',
  code_locked: (minutes: Param) =>
    minutes === undefined ? '访问码错误次数过多，已锁定' : `访问码错误次数过多，已锁定 ${minutes} 分钟`,
  // Reaching the server
  timed_out: '连接服务器超时',
  connection_lost: '与服务器的连接已中断',
};

type Reasons = typeof zhReasons;

const enReasons: Reasons = {
  login_failed: 'Incorrect username or password, or the account is disabled',
  password_incorrect: 'Incorrect password',
  current_password_incorrect: 'Incorrect current password',
  totp_required: 'Two-step verification required',
  totp_code_incorrect: 'Incorrect verification code',
  totp_admin_only: 'Two-step verification is only for administrator accounts',
  totp_already_enabled: 'Two-step verification is already on',
  totp_setup_required: "Two-step verification setup hasn't started",
  admin_required: 'Administrator access required',
  admin_network_denied: "The admin console isn't available from this network",
  guests_disabled: 'Guest access is off on this server',
  clock_skew: "This device's clock is too far off the server's",
  username_invalid: (max) => `Username must be 1–${max ?? 64} letters, digits or . _ - @`,
  username_taken: 'Username already taken',
  password_too_short: (min) =>
    min === undefined ? 'Password is too short' : `Password must be at least ${min} characters`,
  password_too_long: (max) =>
    max === undefined ? 'Password is too long' : `Password must be at most ${max} characters`,
  nickname_invalid: "Nickname can't contain control characters",
  device_name_invalid: (max) => `Name must be 1–${max ?? 64} printable characters`,
  invite_invalid: 'Invalid invitation code',
  invite_expired: 'Invitation code is no longer valid',
  invite_uses_invalid: (min, max) =>
    min === undefined || max === undefined ? 'Invalid number of uses' : `Number of uses must be ${min}–${max}`,
  invite_validity_invalid: 'Validity must be between 1 hour and 1 year',
  last_admin: 'At least one administrator is required',
  self_demote: "Can't demote or disable your own account",
  self_delete: "Can't delete your own account",
  update_platform_invalid: 'Invalid platform',
  update_channel_invalid: 'Channel must be stable or beta',
  update_version_invalid: 'Invalid version',
  update_url_invalid: 'Download URL must be an http(s) link',
  update_notes_too_long: 'Notes are too long',
  setting_invalid: 'Invalid setting',
  setup_code_invalid: 'The setup code is wrong or has expired',
  setup_done: 'This server is already set up',
  admin_login_required: 'An administrator must sign in to do this',
  nothing_to_confirm: 'Nothing waits for confirmation',
  self_friend: "Can't add yourself as a friend",
  self_connect: "Can't connect a device to itself",
  forwarding_disabled: "This server doesn't forward to multiple viewers",
  forwarding_mode_not_allowed: "This server doesn't allow compatible forwarding",
  forwarding_unavailable: 'Forwarding is unavailable right now',
  not_own_device: 'Only your own devices can be managed',
  not_granted: 'No permission for this on the device',
  web_not_allowed: "The server doesn't allow this in the browser",
  power_not_granted: 'No power permission for this device',
  power_disabled: "The device doesn't allow remote shutdown and restart",
  files_disabled: "The device doesn't allow file transfer",
  ports_disabled: "The device doesn't allow port forwarding",
  wol_unavailable: "Wake-on-LAN isn't set up on the device",
  device_online: 'The device is already online',
  not_friends: 'Not friends with this user',
  request_cancelled: 'Request cancelled',
  session_ended: 'Session ended',
  not_trusted: "The device doesn't trust this device",
  no_access_code: 'The device has no access code',
  host_busy: 'The device is handling another connection request',
  host_locked: 'The other computer is locked',
  host_app_closed: "Farwing isn't open on the other device",
  pairing_declined: 'Pairing was declined',
  pairing_recently_declined: 'Pairing was declined recently',
  code_locked: (minutes) =>
    minutes === undefined
      ? 'Too many incorrect access codes, locked'
      : `Too many incorrect access codes, locked for ${minutes} min`,
  timed_out: 'Connection to the server timed out',
  connection_lost: 'Connection to the server was lost',
};

const CODES: Record<Lang, Codes> = { 'zh-CN': zhCodes, en: enCodes };
const REASONS: Record<Lang, Reasons> = { 'zh-CN': zhReasons, en: enReasons };

function has<T extends object>(table: T, key: string): key is Extract<keyof T, string> {
  return Object.prototype.hasOwnProperty.call(table, key);
}

function param(details: ErrorDetails, key: string): Param {
  const value = details && typeof details === 'object' ? (details as Record<string, unknown>)[key] : undefined;
  return typeof value === 'number' && Number.isFinite(value) ? value : undefined;
}

/** Text for a reason with its parameters from `details`; `undefined` for a reason this version does not know. */
export function reasonMessage(lang: Lang, reason: string, details?: ErrorDetails): string | undefined {
  const table = REASONS[lang];
  if (!has(table, reason)) return undefined;
  const entry: string | ((a: Param, b: Param) => string) = table[reason];
  if (typeof entry === 'string') return entry;
  switch (reason) {
    case 'password_too_short':
      return entry(param(details, 'min'), undefined);
    case 'invite_uses_invalid':
      return entry(param(details, 'min'), param(details, 'max'));
    case 'code_locked':
      return entry(param(details, 'minutes'), undefined);
    default:
      return entry(param(details, 'max'), undefined);
  }
}

/** Text for a code alone (a code this version does not know: the generic one). */
export function codeMessage(lang: Lang, code: string): string {
  const table = CODES[lang];
  return has(table, code) ? table[code] : table.unknown;
}

/**
 * Text for a server error, by its code and, where the server gives one, its reason (`details.reason`).
 * Pass the error's `details` too: some texts show a limit from it (the shortest password).
 */
export function errorMessage(lang: Lang, code: string, reason?: string, details?: ErrorDetails): string {
  return (reason !== undefined && reasonMessage(lang, reason, details)) || codeMessage(lang, code);
}
