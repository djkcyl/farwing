// Annotation on the host's screen (ANNOTATION 0x0601,
// `docs/protocol/peer-messages.md`): the host offers it with the
// `annotation` HELLO feature; each operation is a request it answers or
// refuses with a reason, and it pushes `reset` when it dropped the overlay.
// Pen strokes (sent in pieces while drawing), undo, clear and a whiteboard.
import { t } from '../i18n';
import { PeerRequestError } from './peerio';
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

/// Why the host refused (`src/protocol/annotation.rs` `reason`).
export function refusalText(reason: string): string {
  const m = t().viewer.annotation;
  switch (reason) {
    case 'signed_out':
      return m.noUser;
    case 'locked':
      return m.locked;
    default:
      return m.refused;
  }
}

/// Most points one `draw` carries.
const DRAW_POINTS = 4096;

const point = (p: Point) => new Writer().float(1, p.x).float(2, p.y);

function stroke(id: number, screen: number, points: Point[], width: number, color: number): Writer {
  const w = new Writer().int(1, id).intAlways(2, screen);
  for (const p of points) w.message(3, point(p));
  return w.floatAlways(4, width).fixed32Always(5, color);
}

/// The body of each request.
export const op = {
  /// The overlay on or off; off removes everything.
  show: (on: boolean) => new Writer().intAlways(1, on ? 1 : 0).finish(),
  /// Points added to a stroke; the first creates it.
  draw: (id: number, screen: number, points: Point[], width: number, color: number) =>
    new Writer().message(2, stroke(id, screen, points, width, color)).finish(),
  /// Remove one stroke (undo).
  erase: (id: number, screen: number) => new Writer().message(4, new Writer().int(1, id).intAlways(2, screen)).finish(),
  /// Remove every stroke; `boards` also the whiteboards.
  clear: (boards: boolean) => new Writer().intAlways(5, boards ? 1 : 0).finish(),
  /// A whiteboard behind the ink on `screen`, or none.
  board: (screen: number, color?: number) => {
    const board = new Writer().intAlways(1, screen);
    if (color !== undefined) board.fixed32Always(2, color);
    return new Writer().message(6, board).finish();
  },
};

export interface AnnotationState {
  /// The host offers annotation.
  available: boolean;
  enabled: boolean;
  /// The last failure, in words.
  error?: string;
}

export class Annotation {
  private request: (body: Uint8Array) => Promise<unknown>;
  private onState: (state: AnnotationState) => void;
  private available = false;
  private enabled = false;
  private error?: string;
  private nextId = 1;
  /// Strokes in drawing order, for undo.
  private strokes: { id: number; screen: number }[] = [];
  private boards = new Set<number>();

  constructor(request: (body: Uint8Array) => Promise<unknown>, onState: (state: AnnotationState) => void) {
    this.request = request;
    this.onState = onState;
  }

  private emit() {
    this.onState({ available: this.available, enabled: this.enabled, error: this.error });
  }

  private off() {
    this.enabled = false;
    this.strokes = [];
    this.boards.clear();
  }

  /// Whether the host offers annotation (its HELLO).
  setAvailable(available: boolean) {
    if (available === this.available) return;
    this.available = available;
    this.off();
    this.emit();
  }

  /// A push from the host: `reset` (it dropped the overlay).
  handle(body: Uint8Array) {
    if (new Fields(body).has(8) && this.enabled) {
      this.off();
      this.emit();
    }
  }

  /// Send a request; resolves with whether the host applied it.
  private async command(body: Uint8Array): Promise<boolean> {
    if (!this.available) return false;
    try {
      await this.request(body);
      return true;
    } catch (error) {
      const refused = error instanceof PeerRequestError && error.reason;
      this.error = refused ? refusalText(error.reason) : (error as Error).message;
      if (refused && error.reason === 'signed_out') this.off();
      this.emit();
      return false;
    }
  }

  get active(): boolean {
    return this.enabled;
  }

  async setEnabled(enable: boolean): Promise<boolean> {
    if (!this.available) return false;
    this.error = undefined;
    const done = await this.command(op.show(enable));
    if (done) {
      // The host clears everything when annotation ends.
      if (enable) this.enabled = true;
      else this.off();
    }
    this.emit();
    return done;
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
    for (let i = 0; i < points.length; i += DRAW_POINTS) {
      void this.command(op.draw(id, screen, points.slice(i, i + DRAW_POINTS), width, color));
    }
  }

  undo() {
    const last = this.strokes.pop();
    if (last && this.enabled) void this.command(op.erase(last.id, last.screen));
  }

  get canUndo(): boolean {
    return this.strokes.length > 0;
  }

  clear() {
    if (!this.enabled) return;
    this.strokes = [];
    void this.command(op.clear(false));
  }

  hasBoard(screen: number): boolean {
    return this.boards.has(screen);
  }

  setBoard(screen: number, on: boolean, color = 0xffffffff) {
    if (!this.enabled) return;
    if (on) this.boards.add(screen);
    else this.boards.delete(screen);
    void this.command(op.board(screen, on ? color : undefined));
  }

  /// The connection is gone.
  reset() {
    this.available = false;
    this.off();
    this.emit();
  }
}
