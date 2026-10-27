// Audit log details in the interface language. The server writes a detail as
// a JSON object with stable keys (`server/src/http/me.rs`, `audit`):
//
//   {"reason": "login_failed"}                              a failed attempt
//   {"device": "PC", "device_id": "…", "platform": "windows"}  login
//   {"invite": "<note>"}                                    register
//   {"admin": true}                                         user.create
//   {"admin": false, "disabled": true}                      user.update (changed fields)
//   {"groups": ["instance_name", "turn"]}                   settings.update (changed groups)
//   {"fingerprint": "AB:…"}                                 settings.certificate
//   {"secrets": true}                                       settings.export
//   {"scope": "read", "expires_days": 30 | null}            token.create
//   {"token": "<name>"}                                     added when an API token did it
//
// Anything else (entries written before, keys this version does not know)
// is shown as it is.
import { reasonMessage } from './errors';
import type { Lang } from './lang';
import { messages } from './messages';

const zh = {
  separator: '；',
  yes: '是',
  no: '否',
  createdAdmin: '管理员',
  admin: (on: boolean): string => (on ? '设为管理员' : '取消管理员'),
  disabled: (on: boolean): string => (on ? '禁用' : '启用'),
  invite: (note: string) => `邀请码：${note}`,
  groups: (names: string) => `修改：${names}`,
  fingerprint: (fp: string) => `指纹：${fp}`,
  secrets: (on: boolean): string => (on ? '含密钥' : '不含密钥'),
  scope: (scope: string) => `权限：${scope}`,
  expiresDays: (days: number | null) => `有效期：${days === null ? '永久' : `${days} 天`}`,
  token: (name: string) => `API 令牌：${name}`,
  previous: (name: string) => `原名：${name}`,
  friend: (name: string) => `好友：${name}`,
  other: (name: string) => `设备：${name}`,
  actions: { reboot: '重启', shutdown: '关机', wake: '唤醒' } as Record<string, string>,
};

type Texts = typeof zh;

const en: Texts = {
  separator: '; ',
  yes: 'Yes',
  no: 'No',
  createdAdmin: 'Administrator',
  admin: (on) => (on ? 'Made administrator' : 'Administrator removed'),
  disabled: (on) => (on ? 'Disabled' : 'Enabled'),
  invite: (note) => `Invitation: ${note}`,
  groups: (names) => `Changed: ${names}`,
  fingerprint: (fp) => `Fingerprint: ${fp}`,
  secrets: (on) => (on ? 'With secrets' : 'Without secrets'),
  scope: (scope) => `Access: ${scope}`,
  expiresDays: (days) => `Valid for: ${days === null ? 'No expiry' : `${days} days`}`,
  token: (name) => `API token: ${name}`,
  previous: (name) => `Was: ${name}`,
  friend: (name) => `Friend: ${name}`,
  other: (name) => `Device: ${name}`,
  actions: { reboot: 'Restart', shutdown: 'Shut down', wake: 'Wake' },
};

const TEXTS: Record<Lang, Texts> = { 'zh-CN': zh, en };

function raw(value: unknown): string {
  return typeof value === 'string' ? value : JSON.stringify(value);
}

/** One field of a detail; `undefined` when it adds nothing to show. */
function field(
  lang: Lang,
  action: string,
  key: string,
  value: unknown,
  all: Record<string, unknown>,
): string | undefined {
  const t = TEXTS[lang];
  const m = messages[lang];
  switch (key) {
    case 'reason':
      return typeof value === 'string' ? (reasonMessage(lang, value) ?? value) : raw(value);
    case 'admin':
      if (typeof value !== 'boolean') break;
      return action === 'user.create' ? (value ? t.createdAdmin : undefined) : t.admin(value);
    case 'disabled':
      return typeof value === 'boolean' ? t.disabled(value) : raw(value);
    case 'invite':
      return typeof value === 'string' && value ? t.invite(value) : undefined;
    case 'groups': {
      if (!Array.isArray(value)) break;
      const names = m.admin.settings.groups;
      return t.groups(value.map((g) => names[String(g)] ?? String(g)).join(m.format.listSeparator));
    }
    case 'fingerprint':
      return t.fingerprint(raw(value));
    case 'secrets':
      return typeof value === 'boolean' ? t.secrets(value) : raw(value);
    case 'scope': {
      const scopes: Record<string, string> = m.admin.tokens.scopes;
      return t.scope(scopes[raw(value)] ?? raw(value));
    }
    case 'expires_days':
      return value === null || typeof value === 'number' ? t.expiresDays(value) : raw(value);
    case 'token':
      return t.token(raw(value));
    // A login: the device, its platform and its ID.
    case 'device': {
      const platforms: Record<string, string> = m.format.platforms;
      const platform = raw(all.platform ?? '');
      const id = raw(all.device_id ?? '').replace(/(.{4})(?=.)/g, '$1 ');
      return [raw(value), platforms[platform] ?? platform, id && `ID ${id}`].filter(Boolean).join(' · ');
    }
    case 'device_id':
    case 'platform':
      return 'device' in all ? undefined : `${key}: ${raw(value)}`;
    case 'previous':
      return t.previous(raw(value));
    case 'friend':
      return t.friend(raw(value));
    // Who did it is the entry's actor.
    case 'by':
    case 'other_id':
      return undefined;
    case 'other': {
      const id = raw(all.other_id ?? '').replace(/(.{4})(?=.)/g, '$1 ');
      return [t.other(raw(value)), id && `ID ${id}`].filter(Boolean).join(' · ');
    }
    case 'action':
      return t.actions[raw(value)] ?? raw(value);
  }
  if (typeof value === 'boolean') return `${key}: ${value ? t.yes : t.no}`;
  return `${key}: ${raw(value)}`;
}

/** Text for an audit log entry's detail, from the server's structured detail. */
export function auditDetail(lang: Lang, action: string, detail: string): string {
  if (!detail) return '';
  let parsed: unknown;
  try {
    parsed = JSON.parse(detail);
  } catch {
    return detail;
  }
  if (!parsed || typeof parsed !== 'object' || Array.isArray(parsed)) return detail;
  const all = parsed as Record<string, unknown>;
  return Object.entries(all)
    .map(([key, value]) => field(lang, action, key, value, all))
    .filter((text): text is string => !!text)
    .join(TEXTS[lang].separator);
}
