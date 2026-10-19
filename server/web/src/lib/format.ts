// Words and numbers as the console shows them (same wording as the client).

export const PLATFORMS: Record<string, string> = {
  windows: 'Windows',
  macos: 'macOS',
  linux: 'Linux',
  android: 'Android',
  ios: 'iOS',
  web: '网页',
};

export function platformText(p: string): string {
  return PLATFORMS[p] ?? p ?? '';
}

export const RELATIONS: Record<string, string> = {
  own: '本账号',
  friend: '好友',
  assist: '访客',
  trusted: '受信任的访客',
};

export const PURPOSES: Record<string, string> = {
  desktop: '远程桌面',
  audio: '声音',
  files: '文件',
  ports: '端口转发',
};

export const CONNECT_TYPES: Record<string, string> = {
  account: '账号',
  assist: '短号协助',
};

/// Session capabilities (`farwing_protocol::caps`), in display order, with
/// the client's names (`src/application/app/friends.rs` `cap_label`).
export const CAPS: { id: string; label: string }[] = [
  { id: 'view', label: '观看画面' },
  { id: 'control', label: '键鼠控制' },
  { id: 'clipboard', label: '剪贴板' },
  { id: 'files', label: '文件传输' },
  { id: 'ports', label: '端口转发' },
  { id: 'power', label: '关机重启与唤醒' },
  { id: 'audio', label: '接收声音' },
  { id: 'microphone', label: '麦克风' },
  { id: 'display', label: '显示设置' },
  { id: 'diagnostics', label: '诊断包' },
  { id: 'annotation', label: '批注' },
];

/// Capabilities a browser can use (`caps::WEB`), in display order.
export const WEB_CAPS = ['view', 'control', 'clipboard', 'files', 'audio', 'microphone', 'annotation'];

export const CAP_GROUPS: [string, string[]][] = [
  ['观看', ['view', 'audio', 'annotation']],
  ['操作', ['control', 'clipboard', 'microphone', 'files']],
  ['管理', ['display', 'ports', 'power', 'diagnostics']],
];

export const CAP_PRESETS: [string, string[]][] = [
  ['仅观看', ['view', 'audio', 'annotation']],
  ['协作', ['view', 'control', 'clipboard', 'files', 'audio', 'microphone', 'annotation']],
  ['完全控制', CAPS.map((c) => c.id)],
];

/// Granted to a new friend (`caps::DEFAULT_FRIEND`).
export const DEFAULT_FRIEND = ['view', 'annotation'];

export function capText(id: string): string {
  return CAPS.find((c) => c.id === id)?.label ?? id;
}

export function normalizeCaps(list: string[]): string[] {
  return CAPS.map((c) => c.id).filter((id) => list.includes(id));
}

export function sameCaps(a: string[], b: string[]): boolean {
  const x = normalizeCaps(a);
  const y = normalizeCaps(b);
  return x.length === y.length && x.every((v, i) => v === y[i]);
}

export function capsSummary(list: string[]): string {
  if (!list.length) return '无权限';
  const preset = CAP_PRESETS.find(([, p]) => sameCaps(list, p));
  if (preset) return preset[0];
  const labels = normalizeCaps(list).map(capText);
  return labels.length <= 3 ? labels.join('、') : `${labels.slice(0, 3).join('、')} 等 ${labels.length} 项`;
}

/// What a friend sees of one device: nothing at all without capabilities.
export function shareSummary(list: string[]): string {
  return list.length ? capsSummary(list) : '不共享 · 不可见';
}

export function deviceIdText(id: string): string {
  return id.length === 16 ? id.match(/.{4}/g)!.join(' ') : id;
}

export function connectIdText(id: string | null | undefined): string {
  if (!id) return '';
  return id.length === 9 ? `${id.slice(0, 3)} ${id.slice(3, 6)} ${id.slice(6)}` : id;
}

const pad = (n: number) => String(n).padStart(2, '0');

/// Seconds since the epoch → "2026-10-17 08:30".
export function dateTime(seconds: number | null | undefined): string {
  if (!seconds) return '—';
  const d = new Date(seconds * 1000);
  return `${d.getFullYear()}-${pad(d.getMonth() + 1)}-${pad(d.getDate())} ${pad(d.getHours())}:${pad(d.getMinutes())}`;
}

export function date(seconds: number | null | undefined): string {
  if (!seconds) return '—';
  const d = new Date(seconds * 1000);
  return `${d.getFullYear()}-${pad(d.getMonth() + 1)}-${pad(d.getDate())}`;
}

/// "刚刚", "5 分钟前", "3 天前", or the date.
export function ago(seconds: number | null | undefined): string {
  if (!seconds) return '从未';
  const delta = Date.now() / 1000 - seconds;
  if (delta < 60) return '刚刚';
  if (delta < 3600) return `${Math.floor(delta / 60)} 分钟前`;
  if (delta < 86400) return `${Math.floor(delta / 3600)} 小时前`;
  if (delta < 86400 * 30) return `${Math.floor(delta / 86400)} 天前`;
  return date(seconds);
}

export function duration(seconds: number): string {
  const s = Math.max(0, Math.floor(seconds));
  const d = Math.floor(s / 86400);
  const h = Math.floor((s % 86400) / 3600);
  const m = Math.floor((s % 3600) / 60);
  if (d) return `${d} 天 ${h} 小时`;
  if (h) return `${h} 小时 ${m} 分钟`;
  if (m) return `${m} 分钟`;
  return `${s} 秒`;
}

export function bytes(n: number): string {
  if (n < 1024) return `${n} B`;
  const units = ['KB', 'MB', 'GB', 'TB'];
  let v = n / 1024;
  let i = 0;
  while (v >= 1024 && i < units.length - 1) {
    v /= 1024;
    i++;
  }
  return `${v.toFixed(v < 10 ? 1 : 0)} ${units[i]}`;
}

export function bitrate(bps: number): string {
  if (bps >= 1e6) return `${(bps / 1e6).toFixed(1)} Mbps`;
  if (bps >= 1e3) return `${Math.round(bps / 1e3)} kbps`;
  return `${Math.round(bps)} bps`;
}

export function displayName(u: { nickname?: string; username?: string } | null | undefined): string {
  if (!u) return '';
  return u.nickname || u.username || '';
}

/// Audit event kinds.
export const AUDIT_KINDS: Record<string, string> = {
  login: '登录',
  'login.totp': '二次验证',
  register: '邀请码注册',
  'password.change': '修改密码',
  'totp.enable': '开启二次验证',
  'totp.disable': '关闭二次验证',
  'user.create': '创建账号',
  'user.update': '修改账号',
  'user.delete': '删除账号',
  'user.password': '重置密码',
  'user.totp_reset': '重置二次验证',
  'invite.create': '生成邀请码',
  'invite.revoke': '作废邀请码',
  'device.unbind': '解绑设备',
  'device.disconnect': '强制下线',
  'session.end': '结束会话',
  'update.save': '保存更新公告',
  'update.delete': '删除更新公告',
  'settings.save': '修改设置',
};

export function auditKindText(kind: string): string {
  return AUDIT_KINDS[kind] ?? kind;
}
