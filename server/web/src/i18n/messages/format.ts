// Names of protocol values (platforms, relations, capabilities) and
// relative times and durations, as `lib/format.ts` shows them (same wording
// as the client).

const plural = (n: number, one: string, many: string) => `${n} ${n === 1 ? one : many}`;

export const zh = {
  /** Between items of a list ("观看画面、键鼠控制"). */
  listSeparator: '、',
  justNow: '刚刚',
  minutesAgo: (n: number) => `${n} 分钟前`,
  hoursAgo: (n: number) => `${n} 小时前`,
  daysAgo: (n: number) => `${n} 天前`,
  never: '从未',
  daysHours: (d: number, h: number) => `${d} 天 ${h} 小时`,
  hoursMinutes: (h: number, m: number) => `${h} 小时 ${m} 分钟`,
  minutes: (m: number) => `${m} 分钟`,
  seconds: (s: number) => `${s} 秒`,
  platforms: {
    windows: 'Windows',
    macos: 'macOS',
    linux: 'Linux',
    android: 'Android',
    ios: 'iOS',
    web: '网页',
  },
  /** How a controller is related to the host (`relation`). */
  relations: {
    own: '本账号',
    friend: '好友',
    assist: '访客',
    trusted: '受信任的访客',
    share: '共享屏幕时同意的控制',
  },
  /** What a session is for (`purpose`). */
  purposes: {
    desktop: '远程桌面',
    audio: '声音',
    files: '文件',
    ports: '端口转发',
    diagnostics: '诊断',
    camera: '摄像头',
  },
  /** Session capabilities (`farwing_protocol::caps`). */
  caps: {
    view: '观看画面',
    control: '键鼠控制',
    clipboard: '剪贴板',
    files: '文件传输',
    ports: '端口转发',
    power: '关机重启与唤醒',
    audio: '接收声音',
    microphone: '麦克风',
    display: '显示设置',
    diagnostics: '诊断包',
    annotation: '批注',
    camera: '查看摄像头',
    listen: '收听麦克风',
  },
  capGroups: {
    view: '观看',
    operate: '操作',
    manage: '管理',
  },
  capPresets: {
    viewOnly: '仅观看',
    collaborate: '协作',
    full: '完全控制',
  },
  noCaps: '无权限',
  /** The first capabilities of a longer list, and the list's length. */
  capsMore: (first: string, total: number) => `${first} 等 ${total} 项`,
  /** A device a friend may not see at all. */
  notShared: '不共享 · 不可见',
};

export const en: typeof zh = {
  listSeparator: ', ',
  justNow: 'Just now',
  minutesAgo: (n) => `${plural(n, 'minute', 'minutes')} ago`,
  hoursAgo: (n) => `${plural(n, 'hour', 'hours')} ago`,
  daysAgo: (n) => `${plural(n, 'day', 'days')} ago`,
  never: 'Never',
  daysHours: (d, h) => `${plural(d, 'day', 'days')} ${plural(h, 'hour', 'hours')}`,
  hoursMinutes: (h, m) => `${plural(h, 'hour', 'hours')} ${plural(m, 'minute', 'minutes')}`,
  minutes: (m) => plural(m, 'minute', 'minutes'),
  seconds: (s) => plural(s, 'second', 'seconds'),
  platforms: {
    windows: 'Windows',
    macos: 'macOS',
    linux: 'Linux',
    android: 'Android',
    ios: 'iOS',
    web: 'Web',
  },
  relations: {
    own: 'Same account',
    friend: 'Friend',
    assist: 'Visitor',
    trusted: 'Trusted visitor',
    share: 'Control given while sharing',
  },
  purposes: {
    desktop: 'Remote desktop',
    audio: 'Audio',
    files: 'Files',
    ports: 'Port forwarding',
    diagnostics: 'Diagnostics',
    camera: 'Camera',
  },
  caps: {
    view: 'View screen',
    control: 'Keyboard and mouse',
    clipboard: 'Clipboard',
    files: 'File transfer',
    ports: 'Port forwarding',
    power: 'Power and wake',
    audio: 'Receive audio',
    microphone: 'Microphone',
    display: 'Display settings',
    diagnostics: 'Diagnostics',
    annotation: 'Annotation',
    camera: 'View cameras',
    listen: 'Hear the microphone',
  },
  capGroups: {
    view: 'Viewing',
    operate: 'Interaction',
    manage: 'Management',
  },
  capPresets: {
    viewOnly: 'View only',
    collaborate: 'Collaborate',
    full: 'Full control',
  },
  noCaps: 'No permissions',
  capsMore: (first, total) => `${first} and ${total - 3} more`,
  notShared: 'Not shared · Hidden',
};
