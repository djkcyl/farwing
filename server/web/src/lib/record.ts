// Connection records (`docs/connection-records.md`): the visit ID that
// keeps a viewer's reconnections in one record, and the viewer's summary
// sent when it leaves.
import { call } from './api';
import { deviceKey } from './device';
import type { ViewStats } from './stats';
import type { ViewerReport } from './types';

/// A new visit: one viewer screen, all its sessions.
export function newVisit(): string {
  return crypto.randomUUID().replace(/-/g, '');
}

/// What the viewer saw, sampled once a second while playing.
export class VisitSummary {
  private routes = new Map<string, number>();
  private relay = '';
  private seconds = 0;
  private fpsSum = 0;
  private rttSum = 0;
  private rttCount = 0;
  private width = 0;
  private height = 0;
  stalls = 0;
  reconnects = 0;

  add(stats: ViewStats) {
    if (!stats.path || !stats.width) return;
    this.seconds += 1;
    this.routes.set(stats.path, (this.routes.get(stats.path) ?? 0) + 1);
    if (stats.path === 'relay') this.relay = stats.protocol;
    this.fpsSum += stats.fps;
    if (stats.rttMs > 0) {
      this.rttSum += stats.rttMs;
      this.rttCount += 1;
    }
    this.width = stats.width;
    this.height = stats.height;
  }

  get watched(): boolean {
    return this.seconds > 0;
  }

  report(): ViewerReport {
    const route = [...this.routes].sort((a, b) => b[1] - a[1])[0]?.[0] ?? '';
    return {
      route,
      relay: route === 'relay' ? this.relay : '',
      seconds: this.seconds,
      width: this.width,
      height: this.height,
      fps: Math.round((this.fpsSum / Math.max(1, this.seconds)) * 10) / 10,
      rtt_ms: Math.round(this.rttSum / Math.max(1, this.rttCount)),
      stalls: this.stalls,
      reconnects: this.reconnects,
    };
  }
}

/// Send the viewer's summary of a visit; also while the page closes.
export async function sendViewerReport(visit: string, hostId: string, summary: VisitSummary, bearer?: string) {
  if (!summary.watched) return;
  const own = await deviceKey();
  await call(
    'POST',
    '/api/v2/records/reports',
    { visit, host_device_id: hostId, controller_device_id: own.deviceId, viewer: summary.report() },
    { bearer, quiet: true, keepalive: true },
  ).catch(() => undefined);
}
