// Words and numbers as the console shows them (same wording as the client),
// in the current language.
import { locale, t } from '../i18n';

const named = (names: Record<string, string>, id: string) => names[id] ?? id;

export function platformText(p: string): string {
  return p ? named(t().format.platforms, p) : '';
}

/// How a controller is related to the host (`relation`).
export function relationText(relation: string): string {
  return named(t().format.relations, relation);
}

/// What a session is for (`purpose`).
export function purposeText(purpose: string): string {
  return named(t().format.purposes, purpose);
}

/// Session capabilities (`farwing_protocol::caps`), in display order.
export const CAPS = [
  'view',
  'control',
  'clipboard',
  'files',
  'ports',
  'power',
  'audio',
  'microphone',
  'display',
  'diagnostics',
  'annotation',
];

/// Capabilities a browser can use (`caps::WEB`), in display order.
export const WEB_CAPS = ['view', 'control', 'clipboard', 'files', 'audio', 'microphone', 'annotation'];

/// Capability groups: label and members.
export function capGroups(): [string, string[]][] {
  const g = t().format.capGroups;
  return [
    [g.view, ['view', 'audio', 'annotation']],
    [g.operate, ['control', 'clipboard', 'microphone', 'files']],
    [g.manage, ['display', 'ports', 'power', 'diagnostics']],
  ];
}

/// Capability presets: label and capabilities.
export function capPresets(): [string, string[]][] {
  const p = t().format.capPresets;
  return [
    [p.viewOnly, ['view', 'audio', 'annotation']],
    [p.collaborate, ['view', 'control', 'clipboard', 'files', 'audio', 'microphone', 'annotation']],
    [p.full, CAPS],
  ];
}

/// Granted to a new friend (`caps::DEFAULT_FRIEND`).
export const DEFAULT_FRIEND = ['view', 'audio', 'annotation'];

export function capText(id: string): string {
  return named(t().format.caps, id);
}

/// Items of a list in one line ("a、b" / "a, b").
export function listText(items: string[]): string {
  return items.join(t().format.listSeparator);
}

export function normalizeCaps(list: string[]): string[] {
  return CAPS.filter((id) => list.includes(id));
}

export function sameCaps(a: string[], b: string[]): boolean {
  const x = normalizeCaps(a);
  const y = normalizeCaps(b);
  return x.length === y.length && x.every((v, i) => v === y[i]);
}

export function capsSummary(list: string[]): string {
  if (!list.length) return t().format.noCaps;
  const preset = capPresets().find(([, p]) => sameCaps(list, p));
  if (preset) return preset[0];
  const labels = normalizeCaps(list).map(capText);
  return labels.length <= 3 ? listText(labels) : t().format.capsMore(listText(labels.slice(0, 3)), labels.length);
}

/// What a friend sees of one device: nothing at all without capabilities.
export function shareSummary(list: string[]): string {
  return list.length ? capsSummary(list) : t().format.notShared;
}

export function deviceIdText(id: string): string {
  return id.length === 16 ? id.match(/.{4}/g)!.join(' ') : id;
}

export function connectIdText(id: string | null | undefined): string {
  if (!id) return '';
  return id.length === 9 ? `${id.slice(0, 3)} ${id.slice(3, 6)} ${id.slice(6)}` : id;
}

const DATE: Intl.DateTimeFormatOptions = { year: 'numeric', month: '2-digit', day: '2-digit' };
const DATE_EN: Intl.DateTimeFormatOptions = { year: 'numeric', month: 'short', day: 'numeric' };
const TIME: Intl.DateTimeFormatOptions = { hour: '2-digit', minute: '2-digit', hourCycle: 'h23' };

const formats = new Map<string, Intl.DateTimeFormat | Intl.NumberFormat>();

function cached<T extends Intl.DateTimeFormat | Intl.NumberFormat>(key: string, make: () => T): T {
  const id = `${locale()} ${key}`;
  let f = formats.get(id);
  if (!f) formats.set(id, (f = make()));
  return f as T;
}

function dateFormat(withTime: boolean): Intl.DateTimeFormat {
  return cached(withTime ? 'datetime' : 'date', () => {
    const day = locale() === 'en' ? DATE_EN : DATE;
    return new Intl.DateTimeFormat(locale(), withTime ? { ...day, ...TIME } : day);
  });
}

/// Seconds since the epoch → "2026/10/17 08:30", "Oct 17, 2026, 08:30".
export function dateTime(seconds: number | null | undefined): string {
  if (!seconds) return '—';
  return dateFormat(true).format(new Date(seconds * 1000));
}

export function date(seconds: number | null | undefined): string {
  if (!seconds) return '—';
  return dateFormat(false).format(new Date(seconds * 1000));
}

/// "Just now", "5 minutes ago", "3 days ago", or the date.
export function ago(seconds: number | null | undefined): string {
  const f = t().format;
  if (!seconds) return f.never;
  const delta = Date.now() / 1000 - seconds;
  if (delta < 60) return f.justNow;
  if (delta < 3600) return f.minutesAgo(Math.floor(delta / 60));
  if (delta < 86400) return f.hoursAgo(Math.floor(delta / 3600));
  if (delta < 86400 * 30) return f.daysAgo(Math.floor(delta / 86400));
  return date(seconds);
}

export function duration(seconds: number): string {
  const s = Math.max(0, Math.floor(seconds));
  const d = Math.floor(s / 86400);
  const h = Math.floor((s % 86400) / 3600);
  const m = Math.floor((s % 3600) / 60);
  const f = t().format;
  if (d) return f.daysHours(d, h);
  if (h) return f.hoursMinutes(h, m);
  if (m) return f.minutes(m);
  return f.seconds(s);
}

/// A number with `digits` decimals.
export function num(n: number, digits = 0): string {
  return cached(
    `num${digits}`,
    () =>
      new Intl.NumberFormat(locale(), {
        minimumFractionDigits: digits,
        maximumFractionDigits: digits,
        useGrouping: false,
      }),
  ).format(n);
}

export function bytes(n: number): string {
  if (n < 1024) return `${num(n)} B`;
  const units = ['KB', 'MB', 'GB', 'TB'];
  let v = n / 1024;
  let i = 0;
  while (v >= 1024 && i < units.length - 1) {
    v /= 1024;
    i++;
  }
  return `${num(v, v < 10 ? 1 : 0)} ${units[i]}`;
}

export function bitrate(bps: number): string {
  if (bps >= 1e6) return `${num(bps / 1e6, 1)} Mbps`;
  if (bps >= 1e3) return `${num(bps / 1e3)} kbps`;
  return `${num(bps)} bps`;
}

/// The instance's name, when it says more than the product's.
export function instanceText(name: string | null | undefined): string {
  const value = (name ?? '').trim();
  return value.toLowerCase() === 'farwing' ? '' : value;
}

export function displayName(u: { nickname?: string; username?: string } | null | undefined): string {
  if (!u) return '';
  return u.nickname || u.username || '';
}

/// Audit event kinds, in the filter's order.
export function auditKinds(): [string, string][] {
  return Object.entries(t().admin.auditKinds);
}

export function auditKindText(kind: string): string {
  return named(t().admin.auditKinds, kind);
}
