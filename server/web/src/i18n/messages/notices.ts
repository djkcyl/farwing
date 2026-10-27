// Notifications (`docs/notifications.md`): the page, the settings, and
// each kind's words from its data.

import { en as formatEn, zh as formatZh } from './format';

type Data = Record<string, unknown>;
const s = (v: unknown) => (v === undefined || v === null ? '' : String(v));
const list = (v: unknown): string[] => (Array.isArray(v) ? v.map(String) : []);
/// Capability names as labels.
const capsText = (names: Record<string, string>, v: unknown, separator: string) =>
  list(v)
    .map((c) => names[c] ?? c)
    .join(separator);
/// What changed on each device (`changes`: `[{device, added, removed}]`),
/// a line per device.
function grantChanges(
  d: Data,
  names: Record<string, string>,
  separator: string,
  line: (device: string, added: string, removed: string) => string,
): string {
  return (Array.isArray(d.changes) ? (d.changes as Data[]) : [])
    .map((c) => line(s(c.device), capsText(names, c.added, separator), capsText(names, c.removed, separator)))
    .join('\n');
}
/// What a new friend shares (`caps`); nothing when the data has none.
function shared(
  d: Data,
  names: Record<string, string>,
  separator: string,
  some: (caps: string) => string,
  none: string,
) {
  if (d.caps === undefined) return '';
  const caps = capsText(names, d.caps, separator);
  return caps ? some(caps) : none;
}
const zhActions: Record<string, string> = { reboot: '重启', shutdown: '关闭', wake: '唤醒' };
const enActions: Record<string, string> = { reboot: 'restarted', shutdown: 'shut down', wake: 'woke' };

export const zh = {
  empty: '暂无通知',
  loadMore: '加载更早的通知',
  settings: '通知',
  guest: '访客',
  /** Settings: what each kind is. */
  kinds: {
    'login.new_device': '新设备登录',
    'login.failures': '登录连续失败',
    'security.changed': '密码与二次验证变更',
    'device.removed': '设备被移除或下线',
    'connection.incoming': '他人连接我的设备',
    'connection.refused': '连接未成功',
    'power.remote': '好友远程开关机',
    'friend.request': '好友申请',
    'friend.accepted': '好友申请通过',
    'friend.grants': '好友修改权限',
    'friend.removed': '好友解除关系',
    'connection.own': '本账号设备之间的连接',
    'device.offline': '设备离线 10 分钟',
    'access.changed': '访问码与受信任设备变更',
    'admin.certificate': '证书申请失败',
    'admin.node_offline': '中转节点离线',
    'admin.registered': '新账号注册',
  } as Record<string, string>,
  groups: {
    security: '账号安全',
    connections: '连接',
    friends: '好友',
    devices: '设备',
    admin: '服务器',
  },
  outcomes: { rejected: '对方拒绝', no_answer: '无人确认', busy: '人数已满', code_invalid: '访问码错误' } as Record<
    string,
    string
  >,
  /** Title and line of a notification. */
  text: {
    'login.new_device': (d: Data) => ({
      title: '新设备登录了你的账号',
      body: [s(d.device), s(d.ip)].filter(Boolean).join(' · '),
    }),
    'login.failures': (d: Data) => ({
      title: '账号登录连续失败',
      body: `10 分钟内失败 ${s(d.count)} 次 · 最近来自 ${s(d.ip)}`,
    }),
    'security.changed': (d: Data) => ({
      title:
        d.what === 'totp_on'
          ? '已开启二次验证'
          : d.what === 'totp_off'
            ? '已关闭二次验证'
            : d.what === 'totp_reset'
              ? '管理员重置了你的二次验证'
              : d.by
                ? '管理员重置了你的密码'
                : '账号密码已修改',
      body: d.by ? `管理员：${s(d.by)}` : '',
    }),
    'device.removed': (d: Data) => ({
      title:
        d.how === 'signed_out'
          ? `${s(d.device)} 已退出登录`
          : d.how === 'unbound'
            ? `管理员解绑了 ${s(d.device)}`
            : d.how === 'disconnected'
              ? `管理员让 ${s(d.device)} 下线`
              : `${s(d.device)} 已从账号移除`,
      body: d.by ? `管理员：${s(d.by)}` : '',
    }),
    'connection.incoming': (d: Data) => ({ title: `${s(d.who) || '访客'} 连接了 ${s(d.host)}`, body: s(d.controller) }),
    'connection.own': (d: Data) => ({ title: `${s(d.controller)} 连接了 ${s(d.host)}`, body: '' }),
    'connection.refused': (d: Data) => ({
      title: `${s(d.who) || '访客'} 连接 ${s(d.host)} 未成功`,
      body: s(d.controller),
    }),
    'power.remote': (d: Data) => ({
      title: `${s(d.who)} ${zhActions[s(d.action)] ?? '远程操作'}了 ${s(d.device)}`,
      body: '',
    }),
    'friend.request': (d: Data) => ({ title: `${s(d.who)} 请求添加你为好友`, body: '' }),
    'friend.accepted': (d: Data) => ({
      title: `${s(d.who)} 已成为你的好友`,
      body: shared(d, formatZh.caps, '、', (caps) => `你在对方设备上可以：${caps}`, '对方未共享设备'),
    }),
    'friend.grants': (d: Data) => ({
      title: `${s(d.who)} 修改了你在其设备上的权限`,
      body: grantChanges(
        d,
        formatZh.caps,
        '、',
        (device, added, removed) =>
          `${device}：${[added && `新增 ${added}`, removed && `取消 ${removed}`].filter(Boolean).join('；')}`,
      ),
    }),
    'friend.removed': (d: Data) => ({ title: `${s(d.who)} 解除了与你的好友关系`, body: '' }),
    'device.offline': (d: Data) => ({ title: `${s(d.device)} 已离线 ${s(d.minutes)} 分钟`, body: '' }),
    'access.changed': (d: Data) => ({
      title:
        d.what === 'access_code.on'
          ? `${s(d.device)} 开启了访问码`
          : d.what === 'access_code.off'
            ? `${s(d.device)} 关闭了访问码`
            : d.what === 'trusted.add'
              ? `${s(d.device)} 信任了 ${s(d.other)}`
              : `${s(d.device)} 不再信任 ${s(d.other)}`,
      body: '',
    }),
    'admin.certificate': (d: Data) => ({ title: '证书自动申请失败', body: s(d.error) }),
    'admin.node_offline': (d: Data) => ({ title: `中转节点 ${s(d.node)} 已离线`, body: '' }),
    'admin.registered': (d: Data) => ({
      title: `${s(d.who)} 注册了账号`,
      body: d.invite ? `邀请码：${s(d.invite)}` : '',
    }),
  } as Record<string, (d: Data) => { title: string; body: string }>,
};

export const en: typeof zh = {
  empty: 'No notifications',
  loadMore: 'Load earlier notifications',
  settings: 'Notifications',
  guest: 'Visitor',
  kinds: {
    'login.new_device': 'Sign-in from a new device',
    'login.failures': 'Repeated failed sign-ins',
    'security.changed': 'Password and two-step changes',
    'device.removed': 'Device removed or disconnected',
    'connection.incoming': 'Others connecting to my devices',
    'connection.refused': 'Connections not admitted',
    'power.remote': 'Remote power by friends',
    'friend.request': 'Friend requests',
    'friend.accepted': 'Accepted friend requests',
    'friend.grants': 'Permission changes by friends',
    'friend.removed': 'Friends who removed you',
    'connection.own': "Connections between this account's devices",
    'device.offline': 'Device offline for 10 minutes',
    'access.changed': 'Access code and trusted device changes',
    'admin.certificate': 'Certificate failures',
    'admin.node_offline': 'Relay node offline',
    'admin.registered': 'New accounts',
  },
  groups: {
    security: 'Account security',
    connections: 'Connections',
    friends: 'Friends',
    devices: 'Devices',
    admin: 'Server',
  },
  outcomes: { rejected: 'Declined', no_answer: 'No answer', busy: 'Full', code_invalid: 'Wrong access code' },
  text: {
    'login.new_device': (d) => ({
      title: 'A new device signed in to your account',
      body: [s(d.device), s(d.ip)].filter(Boolean).join(' · '),
    }),
    'login.failures': (d) => ({
      title: 'Repeated failed sign-ins',
      body: `${s(d.count)} failures in 10 minutes · last from ${s(d.ip)}`,
    }),
    'security.changed': (d) => ({
      title:
        d.what === 'totp_on'
          ? 'Two-step verification turned on'
          : d.what === 'totp_off'
            ? 'Two-step verification turned off'
            : d.what === 'totp_reset'
              ? 'An administrator reset your two-step verification'
              : d.by
                ? 'An administrator reset your password'
                : 'Your password was changed',
      body: d.by ? `Administrator: ${s(d.by)}` : '',
    }),
    'device.removed': (d) => ({
      title:
        d.how === 'signed_out'
          ? `${s(d.device)} was signed out`
          : d.how === 'unbound'
            ? `An administrator unbound ${s(d.device)}`
            : d.how === 'disconnected'
              ? `An administrator disconnected ${s(d.device)}`
              : `${s(d.device)} was removed from your account`,
      body: d.by ? `Administrator: ${s(d.by)}` : '',
    }),
    'connection.incoming': (d) => ({
      title: `${s(d.who) || 'A visitor'} connected to ${s(d.host)}`,
      body: s(d.controller),
    }),
    'connection.own': (d) => ({ title: `${s(d.controller)} connected to ${s(d.host)}`, body: '' }),
    'connection.refused': (d) => ({
      title: `${s(d.who) || 'A visitor'} could not connect to ${s(d.host)}`,
      body: s(d.controller),
    }),
    'power.remote': (d) => ({
      title: `${s(d.who)} ${enActions[s(d.action)] ?? 'used remote power on'} ${s(d.device)}`,
      body: '',
    }),
    'friend.request': (d) => ({ title: `${s(d.who)} wants to be your friend`, body: '' }),
    'friend.accepted': (d) => ({
      title: `${s(d.who)} is now your friend`,
      body: shared(d, formatEn.caps, ', ', (caps) => `On their devices you may: ${caps}`, 'No devices shared with you'),
    }),
    'friend.grants': (d) => ({
      title: `${s(d.who)} changed what you may do on their devices`,
      body: grantChanges(
        d,
        formatEn.caps,
        ', ',
        (device, added, removed) =>
          `${device}: ${[added && `added ${added}`, removed && `removed ${removed}`].filter(Boolean).join('; ')}`,
      ),
    }),
    'friend.removed': (d) => ({ title: `${s(d.who)} removed you as a friend`, body: '' }),
    'device.offline': (d) => ({ title: `${s(d.device)} has been offline for ${s(d.minutes)} minutes`, body: '' }),
    'access.changed': (d) => ({
      title:
        d.what === 'access_code.on'
          ? `${s(d.device)} turned on its access code`
          : d.what === 'access_code.off'
            ? `${s(d.device)} turned off its access code`
            : d.what === 'trusted.add'
              ? `${s(d.device)} trusts ${s(d.other)}`
              : `${s(d.device)} no longer trusts ${s(d.other)}`,
      body: '',
    }),
    'admin.certificate': (d) => ({ title: 'The automatic certificate failed', body: s(d.error) }),
    'admin.node_offline': (d) => ({ title: `Relay node ${s(d.node)} is offline`, body: '' }),
    'admin.registered': (d) => ({
      title: `${s(d.who)} registered`,
      body: d.invite ? `Invitation: ${s(d.invite)}` : '',
    }),
  },
};
