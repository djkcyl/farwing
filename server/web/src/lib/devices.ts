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
