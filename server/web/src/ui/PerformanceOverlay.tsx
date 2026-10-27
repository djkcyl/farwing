// The performance overlay over the picture, like the native viewer's:
// the path, size and codec; frame rate, round trip and bitrate over the
// last 30 seconds; loss, jitter, decoding and drops; details on demand.
import { useEffect, useRef, useState, type ReactNode } from 'react';
import { useT } from '../i18n/react';
import { bitrate, capText, listText, num } from '../lib/format';
import { pathLabel, type ViewStats } from '../lib/stats';
import { Icon } from './icons';

export interface PerfSample {
  fps: number;
  rttMs: number;
  mbps: number;
}

/// Samples kept: one a second.
export const PERF_HISTORY = 30;

function Trace({
  label,
  value,
  unit,
  values,
  floor,
  tone,
}: {
  label: string;
  value: string;
  unit: string;
  values: number[];
  floor: number;
  tone?: 'good' | 'warn' | 'bad';
}) {
  const top = Math.max(floor, ...values) * 1.15;
  const step = 100 / (PERF_HISTORY - 1);
  const start = PERF_HISTORY - values.length;
  const points = values.map((v, i) => `${((start + i) * step).toFixed(2)},${(32 - (v / top) * 30).toFixed(2)}`);
  return (
    <div className="perf-trace">
      <div className="perf-trace-head">
        <span className="muted">{label}</span>
        <span className={`perf-value ${tone ? `perf-${tone}` : ''}`}>
          {value} <span className="muted">{unit}</span>
        </span>
      </div>
      <svg viewBox="0 0 100 34" preserveAspectRatio="none" className="perf-spark">
        {points.length > 1 && <polyline points={points.join(' ')} />}
      </svg>
    </div>
  );
}

function Pair({ label, children }: { label: string; children: ReactNode }) {
  return (
    <>
      <dt>{label}</dt>
      <dd>{children}</dd>
    </>
  );
}

function clock(seconds: number): string {
  const s = Math.max(0, Math.floor(seconds));
  const h = Math.floor(s / 3600);
  const mm = String(Math.floor((s % 3600) / 60)).padStart(2, '0');
  const ss = String(s % 60).padStart(2, '0');
  return h ? `${h}:${mm}:${ss}` : `${mm}:${ss}`;
}

export function PerformanceOverlay({
  stats,
  history,
  forwarding,
  uptime,
  encoder,
  audioAllowed,
  audioKbps,
  grants,
  onClose,
}: {
  stats?: ViewStats;
  history: PerfSample[];
  forwarding: string;
  /// Seconds since the picture started.
  uptime: number;
  /// The host's encoder and capture.
  encoder: string;
  audioAllowed: boolean;
  /// The audio rate set by the host, when known.
  audioKbps?: number;
  grants: string[];
  onClose(): void;
}) {
  const t = useT();
  const m = t.viewer.stats;
  const [details, setDetails] = useState(false);
  const root = useRef<HTMLDivElement>(null);

  // Clicks and wheel here are not the remote desktop's.
  useEffect(() => {
    const el = root.current;
    if (!el) return;
    const stop = (e: Event) => e.stopPropagation();
    const kinds = ['pointerdown', 'pointermove', 'pointerup', 'mousedown', 'wheel', 'contextmenu', 'dblclick'];
    for (const kind of kinds) el.addEventListener(kind, stop);
    return () => {
      for (const kind of kinds) el.removeEventListener(kind, stop);
    };
  }, []);

  const rtt = stats?.rttMs ?? 0;
  const rttTone = !stats ? undefined : rtt < 60 ? 'good' : rtt < 150 ? 'warn' : 'bad';
  const size = stats?.width ? `${stats.width}×${stats.height}` : '—';
  return (
    <div className="perf-overlay" ref={root}>
      <div className="perf-head">
        <span className="perf-title">{m.monitor}</span>
        <span className="grow muted mono">{clock(uptime)}</span>
        <button type="button" className="perf-close" onClick={onClose} aria-label={t.common.close}>
          <Icon name="close" size={14} />
        </button>
      </div>
      <div className="perf-line muted">{[pathLabel(stats, forwarding), size, stats?.codec || '—'].join('  ·  ')}</div>
      <Trace
        label={m.frameRate}
        value={stats ? num(stats.fps, 1) : '—'}
        unit="FPS"
        values={history.map((s) => s.fps)}
        floor={30}
      />
      <Trace
        label={m.rtt}
        value={stats ? num(rtt) : '—'}
        unit="ms"
        values={history.map((s) => s.rttMs)}
        floor={5}
        tone={rttTone}
      />
      <Trace
        label={m.bitrate}
        value={stats ? num(stats.bitrate / 1e6, 2) : '—'}
        unit="Mbps"
        values={history.map((s) => s.mbps)}
        floor={1}
      />
      <dl className="perf-grid">
        <Pair label={m.loss}>{stats ? `${num(stats.loss * 100, 2)}%` : '—'}</Pair>
        <Pair label={m.jitter}>{stats ? `${num(stats.jitterMs)} ms` : '—'}</Pair>
        <Pair label={m.decodeTime}>{stats?.decodeMs ? `${num(stats.decodeMs, 1)} ms` : '—'}</Pair>
        <Pair label={m.dropped}>{stats?.framesDropped ?? '—'}</Pair>
      </dl>
      <button type="button" className="perf-more" onClick={() => setDetails(!details)}>
        <Icon name="chevron" size={12} className={details ? 'open' : ''} /> {m.details}
      </button>
      {details && (
        <dl className="perf-grid perf-details">
          <Pair label={m.decoder}>
            {stats?.decoder || '—'}
            {stats?.hardware !== undefined && (stats.hardware ? m.hardware : m.software)}
          </Pair>
          <Pair label={m.hostEncoder}>{encoder}</Pair>
          <Pair label={m.remote}>
            <span className="mono">{stats?.remote || '—'}</span>
          </Pair>
          <Pair label={m.audio}>
            {audioAllowed
              ? `${stats ? bitrate(stats.audioBitrate) : '—'}${audioKbps ? m.audioSet(audioKbps) : ''}`
              : m.noPermission}
          </Pair>
          <Pair label={m.grants}>{grants.length ? listText(grants.map(capText)) : '—'}</Pair>
        </dl>
      )}
    </div>
  );
}
