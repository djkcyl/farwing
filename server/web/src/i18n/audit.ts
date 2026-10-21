// Audit log details in the interface language. The server writes a detail as
// a JSON object with stable keys (`server/src/http/me.rs`, `audit`):
//
//   {"reason": "login_failed"}                              a failed attempt
//   {"device": "PC", "device_id": "…", "platform": "windows"}  login
//   {"invite": "<note>"}                                    register
//   {"admin": true}                                         user.create
//   {"admin": false, "disabled": true}                      user.update (changed fields)
//   {"instance_name": "Lab", "guests": false, "registration": true,
//    "bitrate_limit_mbps": 40 | null, "web_caps": ["view"]}  settings.save (changed fields)
//
// Anything else (entries written before, keys this version does not know)
// is shown as it is.
import { reasonMessage } from './errors';
import type { Lang } from './lang';
import { messages } from './messages';

const zh = {
  separator: '；',
  on: '开',
  off: '关',
  yes: '是',
  no: '否',
  createdAdmin: '管理员',
  admin: (on: boolean): string => (on ? '设为管理员' : '取消管理员'),
  disabled: (on: boolean): string => (on ? '禁用' : '启用'),
  invite: (note: string) => `邀请码：${note}`,
  instanceName: (name: string) => `实例名称：${name}`,
  guests: (state: string) => `游客：${state}`,
  registration: (state: string) => `邀请码注册：${state}`,
  bitrateLimit: (mbps: number | null) => `码率上限：${mbps === null ? '不限' : `${mbps} Mbit/s`}`,
  webCaps: (caps: string) => `网页端可用功能：${caps || '无'}`,
};

type Texts = typeof zh;

const en: Texts = {
  separator: '; ',
  on: 'On',
  off: 'Off',
  yes: 'Yes',
  no: 'No',
  createdAdmin: 'Administrator',
  admin: (on) => (on ? 'Made administrator' : 'Administrator removed'),
  disabled: (on) => (on ? 'Disabled' : 'Enabled'),
  invite: (note) => `Invitation: ${note}`,
  instanceName: (name) => `Instance name: ${name}`,
  guests: (state) => `Guests: ${state}`,
  registration: (state) => `Registration by invitation: ${state}`,
  bitrateLimit: (mbps) => `Bitrate limit: ${mbps === null ? 'Unlimited' : `${mbps} Mbit/s`}`,
  webCaps: (caps) => `Features available on the web: ${caps || 'None'}`,
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
  const onOff = (v: boolean) => (v ? t.on : t.off);
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
    case 'instance_name':
      return t.instanceName(raw(value));
    case 'guests':
      return typeof value === 'boolean' ? t.guests(onOff(value)) : raw(value);
    case 'registration':
      return typeof value === 'boolean' ? t.registration(onOff(value)) : raw(value);
    case 'bitrate_limit_mbps':
      return value === null || typeof value === 'number' ? t.bitrateLimit(value) : raw(value);
    case 'web_caps': {
      if (!Array.isArray(value)) break;
      const m = messages[lang].format;
      const names: Record<string, string> = m.caps;
      return t.webCaps(value.map((c) => names[String(c)] ?? String(c)).join(m.listSeparator));
    }
    // A login: the device, its platform and its ID.
    case 'device': {
      const platforms: Record<string, string> = messages[lang].format.platforms;
      const platform = raw(all.platform ?? '');
      const id = raw(all.device_id ?? '').replace(/(.{4})(?=.)/g, '$1 ');
      return [raw(value), platforms[platform] ?? platform, id && `ID ${id}`].filter(Boolean).join(' · ');
    }
    case 'device_id':
    case 'platform':
      return 'device' in all ? undefined : `${key}: ${raw(value)}`;
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
