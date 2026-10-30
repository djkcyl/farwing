// The performance monitor over the picture, like the native viewer's: a
// compact summary with the sound meter, or the detailed overlay (the path,
// size and codec; frame rate, round trip and bitrate over the last 30
// seconds; loss, jitter, decoding and drops; details on demand).
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

export type PerfMode = 'compact' | 'detailed' | 'off';

type Tone = 'good' | 'warn' | 'bad';

/// The native viewer's health limits: above the first a warning, above the
/// second bad.
function level(value: number, warning: number, bad: number): Tone {
  return value > bad ? 'bad' : value > warning ? 'warn' : 'good';
}

const rttTone = (stats?: ViewStats) => (stats ? level(stats.rttMs, 80, 150) : undefined);
const lossTone = (stats?: ViewStats) => (stats ? level(stats.loss * 100, 1, 5) : undefined);

/// `LAN`/`P2P` good, `TURN` a warning, `SFU` neutral.
function pathTone(label: string): Tone | undefined {
  if (label.startsWith('LAN') || label.startsWith('P2P')) return 'good';
  if (label.startsWith('TURN')) return 'warn';
  return undefined;
}

function Trace({
  label,
  value,
  unit,
  values,
  floor,
  tone,
  limit,
}: {
  label: string;
  value: string;
  unit: string;
  values: number[];
  floor: number;
  tone?: Tone;
  /// A ceiling (the server's budget): `value / limit` and a dashed line.
  limit?: number;
}) {
  const top = Math.max(floor, limit ?? 0, ...values) * 1.15;
  const step = 100 / (PERF_HISTORY - 1);
  const start = PERF_HISTORY - values.length;
  const y = (v: number) => (32 - (v / top) * 30).toFixed(2);
  const points = values.map((v, i) => `${((start + i) * step).toFixed(2)},${y(v)}`);
  return (
    <div className="perf-trace">
      <div className="perf-trace-head">
        <span className="muted">{label}</span>
        <span className={`perf-value ${tone ? `perf-${tone}` : ''}`}>
          {value}
          {limit ? ` / ${num(limit, 1)}` : ''} <span className="muted">{unit}</span>
        </span>
      </div>
      <svg viewBox="0 0 100 34" preserveAspectRatio="none" className="perf-spark">
        {limit ? <line className="perf-limit" x1="0" x2="100" y1={y(limit)} y2={y(limit)} /> : null}
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
  budgetKbps,
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
  /// The server's limit for this session while relayed (kbit/s; 0: none).
  budgetKbps: number;
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
        tone={rttTone(stats)}
      />
      <Trace
        label={m.bitrate}
        value={stats ? num(stats.bitrate / 1e6, 2) : '—'}
        unit="Mbps"
        values={history.map((s) => s.mbps)}
        floor={1}
        limit={budgetKbps > 0 ? budgetKbps / 1000 : undefined}
      />
      <dl className="perf-grid">
        <Pair label={m.loss}>
          <span className={`perf-${lossTone(stats)}`}>{stats ? `${num(stats.loss * 100, 2)}%` : '—'}</span>
        </Pair>
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

/// The compact summary: no controls, clicks go to the remote desktop.
export function PerformanceSummary({
  stats,
  forwarding,
  uptime,
  quality,
  audio,
  muted,
  volume,
}: {
  stats?: ViewStats;
  forwarding: string;
  uptime: number;
  /// The quality tier in use.
  quality: string;
  audio?: MediaStreamTrack;
  muted: boolean;
  volume: number;
}) {
  const m = useT().viewer.stats;
  const path = pathLabel(stats, forwarding);
  const lines: [string, Tone | undefined][] = [
    [clock(uptime), undefined],
    [path, pathTone(path)],
    [stats ? `${num(stats.fps)} fps` : '—', undefined],
    [stats ? `${num(stats.bitrate / 1e6, 1)} Mbps` : '—', undefined],
    [m.hudRtt(stats ? `${num(stats.rttMs)} ms` : '—'), rttTone(stats)],
    [m.hudLoss(stats ? `${num(stats.loss * 100, 2)}%` : '—'), lossTone(stats)],
    [quality, undefined],
  ];
  return (
    <div className="perf-summary">
      <div className="perf-summary-lines">
        {lines.map(([text, tone], i) => (
          <div key={i} className={tone ? `perf-${tone}` : undefined}>
            {text}
          </div>
        ))}
      </div>
      <LevelMeter track={audio} gain={muted ? 0 : volume} />
    </div>
  );
}

const METER_WIDTH = 37;
const METER_HEIGHT = 98;
const SEGMENTS = 24;

/// Stereo output level, as the native summary's: 24 segments a channel from
/// -60 dB, with a peak held for 0.8 s.
function LevelMeter({ track, gain }: { track?: MediaStreamTrack; gain: number }) {
  const canvas = useRef<HTMLCanvasElement>(null);
  useEffect(() => {
    const el = canvas.current;
    const g = el?.getContext('2d');
    if (!el || !g) return;
    const ratio = window.devicePixelRatio || 1;
    el.width = METER_WIDTH * ratio;
    el.height = METER_HEIGHT * ratio;
    g.setTransform(ratio, 0, 0, ratio, 0, 0);
    const peaks: [number, number][] = [
      [0, 0],
      [0, 0],
    ];
    const draw = (fill: number[]) => {
      const now = performance.now();
      for (let c = 0; c < 2; c++) {
        if (fill[c] >= peaks[c][0] || now >= peaks[c][1]) peaks[c] = [fill[c], now + 800];
      }
      paintMeter(g, fill, peaks);
    };
    if (!track || gain <= 0) {
      draw([0, 0]);
      return;
    }
    const ctx = new AudioContext();
    void ctx.resume().catch(() => undefined);
    const source = ctx.createMediaStreamSource(new MediaStream([track]));
    // Mono plays in both channels.
    const stereo = ctx.createGain();
    stereo.channelCount = 2;
    stereo.channelCountMode = 'explicit';
    stereo.channelInterpretation = 'speakers';
    const split = ctx.createChannelSplitter(2);
    const analysers = [ctx.createAnalyser(), ctx.createAnalyser()];
    source.connect(stereo).connect(split);
    analysers.forEach((a, c) => {
      a.fftSize = 1024;
      split.connect(a, c);
    });
    const samples = new Float32Array(1024);
    let frame = 0;
    const tick = () => {
      draw(
        analysers.map((a) => {
          a.getFloatTimeDomainData(samples);
          let peak = 0;
          for (const v of samples) peak = Math.max(peak, Math.abs(v));
          const level = peak * gain;
          return level > 0 ? Math.min(1, Math.max(0, (20 * Math.log10(level) + 60) / 60)) : 0;
        }),
      );
      frame = requestAnimationFrame(tick);
    };
    tick();
    return () => {
      cancelAnimationFrame(frame);
      void ctx.close();
    };
  }, [track, gain]);
  return <canvas ref={canvas} className="perf-meter" />;
}

function paintMeter(g: CanvasRenderingContext2D, fill: number[], peaks: [number, number][]) {
  const css = getComputedStyle(document.documentElement);
  const color = (name: string) => css.getPropertyValue(name).trim();
  const ink = color('--muted');
  g.clearRect(0, 0, METER_WIDTH, METER_HEIGHT);
  g.font = "7.5px 'Cascadia Mono', Consolas, monospace";
  const top = 12;
  const bottom = METER_HEIGHT - 4;
  const height = bottom - top;
  const y = (level: number) => bottom - height * level;
  ['L', 'R'].forEach((name, c) => {
    const left = c * 8;
    g.fillStyle = ink;
    g.textAlign = 'center';
    g.textBaseline = 'top';
    g.fillText(name, left + 2.5, 0);
    for (let s = 0; s < SEGMENTS; s++) {
      g.fillStyle =
        fill[c] * SEGMENTS > s
          ? color(s >= 22 ? '--red' : s >= 17 ? '--amber' : '--green')
          : 'rgba(255, 255, 255, 0.09)';
      g.fillRect(left, y((s + 1) / SEGMENTS), 5, Math.max(1, height / SEGMENTS - 1));
    }
    if (peaks[c][0] > 0) {
      g.fillStyle = color('--text');
      g.fillRect(left, y(peaks[c][0]) - 0.5, 5, 1);
    }
  });
  g.fillStyle = ink;
  g.textAlign = 'right';
  g.fillText('dB', METER_WIDTH, 0);
  g.textBaseline = 'middle';
  for (const db of [0, -6, -12, -24, -36, -48, -60]) {
    const at = y((db + 60) / 60);
    g.fillStyle = 'rgb(90, 90, 90)';
    g.fillRect(16, at - 0.5, 2, 1);
    g.fillStyle = ink;
    g.fillText(String(db), METER_WIDTH, at);
  }
}
