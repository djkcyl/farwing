// Device rules shared by the pages.
import type { Device } from './types';

/// Why this browser cannot watch the device, or `undefined` when it can.
export function viewBlocker(d: Device): string | undefined {
  if (d.platform === 'web' || !d.capabilities.includes('host')) return '这台设备不能被控';
  if (d.status !== 'online') return '设备不在线';
  if (!d.granted.includes('view')) return '没有观看权限';
  if (!d.policy.control) return '对方没有开启“允许被控”';
  return undefined;
}

/// Every device list shows devices by name, then by ID: a fixed order that
/// does not move when the list refreshes.
export function byName(a: { alias: string; device_id: string }, b: { alias: string; device_id: string }): number {
  const x = a.alias.toLowerCase();
  const y = b.alias.toLowerCase();
  if (x !== y) return x < y ? -1 : 1;
  if (a.alias !== b.alias) return a.alias < b.alias ? -1 : 1;
  return a.device_id < b.device_id ? -1 : a.device_id > b.device_id ? 1 : 0;
}
