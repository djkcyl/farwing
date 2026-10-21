// Device rules shared by the pages.
import { t } from '../i18n';
import type { Device } from './types';

/// Why this browser cannot watch the device, or `undefined` when it can.
export function viewBlocker(d: Device): string | undefined {
  const m = t().devices;
  if (d.platform === 'web' || !d.capabilities.includes('host')) return m.notHost;
  if (d.status !== 'online') return m.notOnline;
  if (!d.granted.includes('view')) return m.noViewPermission;
  if (!d.policy.control) return m.controlOff;
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
