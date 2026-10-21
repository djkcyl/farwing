// Devices as cards, and why one cannot be watched in the browser.

export const zh = {
  watch: '观看',
  /** "{owner} 共享 · {capabilities}" */
  sharedBy: (owner: string) => `${owner} 共享`,
  watching: (n: number) => `${n} 人正在连接`,
  /** Why the browser cannot watch a device. */
  notHost: '此设备不支持被控',
  notOnline: '设备不在线',
  noViewPermission: '无观看权限',
  controlOff: '对方未开启“允许被控”',
};

export const en: typeof zh = {
  watch: 'Watch',
  sharedBy: (owner) => `Shared by ${owner}`,
  watching: (n) => `${n} connected`,
  notHost: "This device can't be controlled",
  notOnline: 'Device is offline',
  noViewPermission: 'No permission to view',
  controlOff: "The host hasn't turned on “Allow control”",
};
