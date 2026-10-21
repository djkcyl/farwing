// Annotation on the host's screen (ANNOTATION 0x0601,
// `docs/protocol/peer-messages.md`): the host offers it with a token on the
// session channel; commands carry that token and a strictly increasing
// request number, and each gets a result. Pen strokes (sent in pieces while
// drawing), the laser pointer, undo, clear and a whiteboard.
import { t } from '../i18n';
import * as fpp from './fpp';
import { Fields, Writer } from './pb';

/// 0xAARRGGBB as a CSS colour.
export function argbCss(argb: number): string {
  const a = ((argb >>> 24) & 0xff) / 255;
  return `rgba(${(argb >>> 16) & 0xff}, ${(argb >>> 8) & 0xff}, ${argb & 0xff}, ${a})`;
}

export interface Point {
  x: number;
  y: number;
}

/// Results: 0 done, 2 the host has no user signed in, 3 locked, 4 refused.
export function resultText(code: number): string {
  const m = t().viewer.annotation;
  switch (code) {
    case 2:
      return m.noUser;
    case 3:
      return m.locked;
    case 4:
      return m.refused;
    default:
      return m.failed(code);
  }
}

const point = (p: Point) => new Writer().float(1, p.x).float(2, p.y);

export function packet(token: bigint, request: number, command: Writer): Uint8Array {
  return new Writer().fixed64(1, token).int(2, request).message(4, command).finish();
}

const draw = (request: Writer) => new Writer().message(1, request);

export function toggleCommand(enable: boolean): Writer {
  return draw(new Writer().message(3, new Writer().int(1, enable)));
}

export function strokeCommand(id: number, screen: number, points: Point[], width: number, color: number): Writer {
  const stroke = new Writer().int(1, id);
  for (const p of points) stroke.message(2, point(p));
  stroke
    .intAlways(3, screen)
    .floatAlways(4, width)
    .intAlways(5, color >>> 0);
  return draw(new Writer().message(1, stroke));
}

/// Remove one stroke (undo).
export function eraseCommand(id: number, screen: number): Writer {
  return draw(new Writer().message(2, new Writer().int(1, 2).int(2, id).intAlways(3, screen)));
}

/// Clear the ink; `all` also removes whiteboards.
export function clearCommand(all: boolean): Writer {
  return new Writer().intAlways(5, all ? 1 : 0);
}

/// A whiteboard behind the ink on `screen`, or none.
export function boardCommand(screen: number, color?: number): Writer {
  const board = new Writer().int(1, screen);
  if (color !== undefined) board.fixed32Always(2, color);
  return new Writer().message(4, board);
}

export function laserCommand(
  id: number,
  screen: number,
  width: number,
  color: number,
  tailMs: number,
  samples: { point: Point; ageMs: number }[],
): Writer {
  const laser = new Writer().int(1, id).int(2, screen).float(3, width).fixed32(4, color).int(5, tailMs);
  for (const s of samples) laser.message(6, new Writer().message(1, point(s.point)).int(2, s.ageMs));
  return new Writer().message(6, laser);
}

export interface AnnotationState {
  /// The host offered annotation.
  available: boolean;
  enabled: boolean;
  /// The last failure, in words.
  error?: string;
}

const MAX_IN_FLIGHT = 48;
export const LASER_TAIL_MS = 100;
const LASER_SAMPLES = 32;

export class Annotation {
  private send: (frame: Uint8Array) => void;
  private onState: (state: AnnotationState) => void;
  private token = 0n;
  private request = 0;
  private pending = new Map<number, (code: number) => void>();
  private enabled = false;
  private error?: string;
  private nextId = 1;
  /// Strokes in drawing order, for undo.
  private strokes: { id: number; screen: number }[] = [];
  private boards = new Set<number>();
  private laser?: { id: number; screen: number; samples: { at: number; point: Point }[] };

  constructor(send: (frame: Uint8Array) => void, onState: (state: AnnotationState) => void) {
    this.send = send;
    this.onState = onState;
  }

  private emit() {
    this.onState({ available: this.token !== 0n, enabled: this.enabled, error: this.error });
  }

  /// An ANNOTATION frame from the host.
  handle(body: Uint8Array) {
    const f = new Fields(body);
    const token = f.fixed64(1);
    if (token === 0n) return;
    if (f.has(3)) {
      if (token !== this.token) {
        this.token = token;
        this.enabled = false;
        this.strokes = [];
        this.boards.clear();
        this.emit();
      }
      return;
    }
    if (f.has(5) && token === this.token) {
      const request = f.int64(2);
      this.pending.get(request)?.(f.int32(5));
      this.pending.delete(request);
    }
  }

  /// Send a command; resolves with its result code.
  private command(command: Writer, optional = false): Promise<number> {
    if (this.token === 0n) return Promise.resolve(4);
    if (optional && this.pending.size >= MAX_IN_FLIGHT) return Promise.resolve(0);
    this.request += 1;
    const request = this.request;
    const result = new Promise<number>((resolve) => {
      this.pending.set(request, resolve);
      window.setTimeout(() => {
        if (this.pending.delete(request)) resolve(-1);
      }, 10_000);
    });
    this.send(fpp.encode(fpp.Type.ANNOTATION, packet(this.token, request, command)));
    void result.then((code) => {
      if (code !== 0 && code !== -1) {
        this.error = resultText(code);
        if (code === 2) this.enabled = false;
        this.emit();
      }
    });
    return result;
  }

  get active(): boolean {
    return this.enabled;
  }

  async setEnabled(enable: boolean): Promise<boolean> {
    if (this.token === 0n) return false;
    this.error = undefined;
    const code = await this.command(toggleCommand(enable));
    if (code === 0) {
      this.enabled = enable;
      if (!enable) {
        // The host clears everything when annotation ends.
        this.strokes = [];
        this.boards.clear();
      }
    } else if (code === -1) {
      this.error = t().viewer.peer.noResponse;
    }
    this.emit();
    return code === 0;
  }

  /// A new pen stroke; returns its ID for `extend`.
  begin(): number {
    const id = this.nextId;
    this.nextId = (this.nextId % 0xfffffffe) + 1;
    return id;
  }

  /// Add points to a stroke (the first call creates it on the host).
  extend(id: number, screen: number, points: Point[], width: number, color: number) {
    if (!this.enabled || !points.length) return;
    if (!this.strokes.some((s) => s.id === id)) this.strokes.push({ id, screen });
    for (let i = 0; i < points.length; i += 4096) {
      void this.command(strokeCommand(id, screen, points.slice(i, i + 4096), width, color));
    }
  }

  undo() {
    const last = this.strokes.pop();
    if (last && this.enabled) void this.command(eraseCommand(last.id, last.screen));
  }

  get canUndo(): boolean {
    return this.strokes.length > 0;
  }

  clear() {
    if (!this.enabled) return;
    this.strokes = [];
    void this.command(clearCommand(false));
  }

  hasBoard(screen: number): boolean {
    return this.boards.has(screen);
  }

  setBoard(screen: number, on: boolean, color = 0xffffffff) {
    if (!this.enabled) return;
    if (on) this.boards.add(screen);
    else this.boards.delete(screen);
    void this.command(boardCommand(screen, on ? color : undefined));
  }

  /// Move the laser pointer (a transient trail that fades on the host).
  laserTo(screen: number, p: Point, width: number, color: number) {
    if (!this.enabled) return;
    const now = performance.now();
    if (!this.laser || this.laser.screen !== screen) {
      this.laser = { id: this.begin(), screen, samples: [] };
    }
    const laser = this.laser;
    laser.samples.push({ at: now, point: p });
    const life = LASER_TAIL_MS + 220;
    laser.samples = laser.samples.filter((s) => now - s.at <= life).slice(-LASER_SAMPLES);
    void this.command(
      laserCommand(
        laser.id,
        screen,
        width,
        color,
        LASER_TAIL_MS,
        laser.samples.map((s) => ({ point: s.point, ageMs: Math.round(now - s.at) })),
      ),
      true,
    );
  }

  laserEnd() {
    this.laser = undefined;
  }

  /// The connection is gone.
  reset() {
    for (const resolve of this.pending.values()) resolve(-1);
    this.pending.clear();
    this.token = 0n;
    this.enabled = false;
    this.strokes = [];
    this.boards.clear();
    this.emit();
  }
}
