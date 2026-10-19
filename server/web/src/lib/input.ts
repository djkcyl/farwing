// Keyboard, mouse, touch and pen input of a controlling browser
// (`docs/protocol/web-control-brief.md` §2, `touch-pen-brief.md` §5): INPUT,
// TOUCH and PEN frames on the control channel, keys as HID usages from
// `KeyboardEvent.code`, absolute pointer positions normalized to the host
// screen, relative motion under pointer lock, wheel in 1/120 notches,
// heartbeats while anything is held, lock-key state, and release of
// everything held in a fixed order.
import { Writer } from './pb';

// Messages ---------------------------------------------------------------------

export const Button = { LEFT: 1, RIGHT: 2, MIDDLE: 16, X1: 32, X2: 64 } as const;

export const SystemCommand = { SAS: 1, LOCK: 2, TASK_MANAGER: 3, SHOW_DESKTOP: 4, TASK_VIEW: 5 } as const;

export function keyMessage(usage: number, down: boolean, repeat = false): Uint8Array {
  return new Writer()
    .message(
      1,
      new Writer()
        .int(1, usage)
        .int(2, down)
        .int(3, down && repeat),
    )
    .finish();
}

export function lockStateMessage(caps: boolean, num: boolean, scroll: boolean, seq: number): Uint8Array {
  return new Writer().message(2, new Writer().int(1, caps).int(2, num).int(3, scroll).int(4, seq)).finish();
}

/// `screen_id` is written even when 0: without it the host uses its own
/// current screen.
export function mouseAbsolute(screenId: number, x: number, y: number): Uint8Array {
  return new Writer().message(3, new Writer().intAlways(1, screenId).double(2, x).double(3, y)).finish();
}

export function mouseRelative(dx: number, dy: number): Uint8Array {
  return new Writer().message(4, new Writer().sint(1, dx).sint(2, dy)).finish();
}

export function mouseButton(button: number, down: boolean): Uint8Array {
  return new Writer().message(5, new Writer().int(1, button).int(2, down)).finish();
}

export function mouseWheel(dx: number, dy: number): Uint8Array {
  return new Writer().message(6, new Writer().sint(1, dx).sint(2, dy)).finish();
}

export function systemCommand(command: number): Uint8Array {
  return new Writer().int(7, command).finish();
}

export function heartbeat(): Uint8Array {
  return new Writer().message(8, new Writer()).finish();
}

export const TouchPhase = { DOWN: 1, MOVE: 2, UP: 3, CANCEL: 4 } as const;
export const PenPhase = { HOVER: 1, DOWN: 2, MOVE: 3, UP: 4, LEAVE: 5, CANCEL: 6 } as const;

export interface TouchContact {
  /// 1..=255.
  id: number;
  phase: number;
  x: number;
  y: number;
  /// 0 = not reported.
  pressure?: number;
  /// Normalized to the screen; 0 = not reported.
  width?: number;
  height?: number;
  orientation?: number;
}

/// A TOUCH body: every active contact; none cancels all.
export function touchMessage(screenId: number, contacts: TouchContact[], timeUs = 0): Uint8Array {
  const w = new Writer().intAlways(1, screenId);
  for (const c of contacts) {
    w.message(
      2,
      new Writer()
        .int(1, c.id)
        .int(2, c.phase)
        .float(3, c.x)
        .float(4, c.y)
        .float(5, c.pressure ?? 0)
        .float(6, c.width ?? 0)
        .float(7, c.height ?? 0)
        .int(8, c.orientation ?? 0),
    );
  }
  return w.int(3, timeUs >>> 0).finish();
}

export interface PenSample {
  phase: number;
  x: number;
  y: number;
  pressure?: number;
  tiltX?: number;
  tiltY?: number;
  twist?: number;
  /// bit 0 barrel, bit 1 eraser.
  buttons?: number;
  ageUs?: number;
}

/// A PEN body: 1–16 samples, oldest first.
export function penMessage(screenId: number, samples: PenSample[], timeUs = 0): Uint8Array {
  const w = new Writer().intAlways(1, screenId);
  for (const s of samples) {
    w.message(
      2,
      new Writer()
        .int(1, s.phase)
        .float(2, s.x)
        .float(3, s.y)
        .float(4, s.pressure ?? 0)
        .sint(5, Math.round(s.tiltX ?? 0))
        .sint(6, Math.round(s.tiltY ?? 0))
        .int(7, Math.round(s.twist ?? 0) % 360)
        .int(8, s.buttons ?? 0)
        .int(9, Math.round(s.ageUs ?? 0)),
    );
  }
  return w.int(3, timeUs >>> 0).finish();
}

/// Pen tilt in degrees from a pointer event (tiltX/tiltY, or altitude and
/// azimuth where only those exist).
export function penTilt(e: {
  tiltX?: number;
  tiltY?: number;
  altitudeAngle?: number;
  azimuthAngle?: number;
}): [number, number] {
  if (e.tiltX || e.tiltY || e.altitudeAngle === undefined || e.azimuthAngle === undefined) {
    return [e.tiltX ?? 0, e.tiltY ?? 0];
  }
  const alt = e.altitudeAngle;
  if (alt >= Math.PI / 2 - 1e-6) return [0, 0];
  const deg = (r: number) => Math.round((r * 180) / Math.PI);
  const t = Math.tan(alt);
  return [deg(Math.atan(Math.cos(e.azimuthAngle) / t)), deg(Math.atan(Math.sin(e.azimuthAngle) / t))];
}

// Key table ----------------------------------------------------------------------

const K = 0x070000;
const C = 0x0c0000;

/// `KeyboardEvent.code` → HID usage, only keys the host's table has (§2.4).
export const KEY_USAGE: Record<string, number> = (() => {
  const t: Record<string, number> = {};
  for (let i = 0; i < 26; i++) t[`Key${String.fromCharCode(65 + i)}`] = K | (0x04 + i);
  for (let i = 1; i <= 9; i++) t[`Digit${i}`] = K | (0x1e + i - 1);
  t.Digit0 = K | 0x27;
  for (let i = 1; i <= 12; i++) t[`F${i}`] = K | (0x3a + i - 1);
  for (let i = 13; i <= 24; i++) t[`F${i}`] = K | (0x68 + i - 13);
  for (let i = 1; i <= 9; i++) t[`Numpad${i}`] = K | (0x59 + i - 1);
  t.Numpad0 = K | 0x62;
  const named: [string, number][] = [
    ['Enter', 0x28],
    ['Escape', 0x29],
    ['Backspace', 0x2a],
    ['Tab', 0x2b],
    ['Space', 0x2c],
    ['Minus', 0x2d],
    ['Equal', 0x2e],
    ['BracketLeft', 0x2f],
    ['BracketRight', 0x30],
    ['Backslash', 0x31],
    ['Semicolon', 0x33],
    ['Quote', 0x34],
    ['Backquote', 0x35],
    ['Comma', 0x36],
    ['Period', 0x37],
    ['Slash', 0x38],
    ['CapsLock', 0x39],
    ['PrintScreen', 0x46],
    ['ScrollLock', 0x47],
    ['Pause', 0x48],
    ['Insert', 0x49],
    ['Home', 0x4a],
    ['PageUp', 0x4b],
    ['Delete', 0x4c],
    ['End', 0x4d],
    ['PageDown', 0x4e],
    ['ArrowRight', 0x4f],
    ['ArrowLeft', 0x50],
    ['ArrowDown', 0x51],
    ['ArrowUp', 0x52],
    ['NumLock', 0x53],
    ['NumpadDivide', 0x54],
    ['NumpadMultiply', 0x55],
    ['NumpadSubtract', 0x56],
    ['NumpadAdd', 0x57],
    ['NumpadEnter', 0x58],
    ['NumpadDecimal', 0x63],
    ['IntlBackslash', 0x64],
    ['ContextMenu', 0x65],
    ['NumpadEqual', 0x67],
    ['NumpadComma', 0x85],
    ['IntlRo', 0x87],
    ['KanaMode', 0x88],
    ['IntlYen', 0x89],
    ['Convert', 0x8a],
    ['NonConvert', 0x8b],
    ['Lang1', 0x90],
    ['Lang2', 0x91],
    ['ControlLeft', 0xe0],
    ['ShiftLeft', 0xe1],
    ['AltLeft', 0xe2],
    ['MetaLeft', 0xe3],
    ['OSLeft', 0xe3],
    ['ControlRight', 0xe4],
    ['ShiftRight', 0xe5],
    ['AltRight', 0xe6],
    ['MetaRight', 0xe7],
    ['OSRight', 0xe7],
  ];
  for (const [code, id] of named) t[code] = K | id;
  const consumer: [string, number][] = [
    ['MediaTrackNext', 0xb5],
    ['MediaTrackPrevious', 0xb6],
    ['MediaStop', 0xb7],
    ['MediaPlayPause', 0xcd],
    ['AudioVolumeMute', 0xe2],
    ['AudioVolumeUp', 0xe9],
    ['AudioVolumeDown', 0xea],
    ['MediaSelect', 0x183],
    ['LaunchMail', 0x18a],
    ['LaunchApp2', 0x192],
    ['LaunchApp1', 0x194],
    ['BrowserSearch', 0x221],
    ['BrowserHome', 0x223],
    ['BrowserBack', 0x224],
    ['BrowserForward', 0x225],
    ['BrowserStop', 0x226],
    ['BrowserRefresh', 0x227],
    ['BrowserFavorites', 0x22a],
  ];
  for (const [code, id] of consumer) t[code] = C | id;
  return t;
})();

/// Usages by name, for the special-keys menu.
export const Usage = {
  CTRL: K | 0xe0,
  SHIFT: K | 0xe1,
  ALT: K | 0xe2,
  WIN: K | 0xe3,
  TAB: K | 0x2b,
  ESC: K | 0x29,
  F4: K | 0x3d,
  R: K | 0x15,
  E: K | 0x08,
  V: K | 0x19,
  PRINT_SCREEN: K | 0x46,
} as const;

const MODIFIERS = new Set([0xe0, 0xe1, 0xe2, 0xe3, 0xe4, 0xe5, 0xe6, 0xe7].map((id) => K | id));
const LOCKS = new Set(['CapsLock', 'NumLock', 'ScrollLock']);

/// `MouseEvent.button` → wire button.
export function wireButton(button: number): number | undefined {
  return [Button.LEFT, Button.MIDDLE, Button.RIGHT, Button.X1, Button.X2][button];
}

// Controller ----------------------------------------------------------------------

export interface InputSink {
  /// Send an INPUT body on the control channel.
  send(body: Uint8Array): void;
  /// Send a whole TOUCH or PEN frame body of `type` on the control channel.
  sendFrame?(type: number, body: Uint8Array): void;
  /// Bytes queued on the control channel.
  buffered(): number;
}

/// Queued moves wait while the control channel holds more than this.
const MOVE_BUDGET = 1024;
const HEARTBEAT_MS = 50;

interface Locks {
  caps: boolean;
  num: boolean;
  scroll: boolean;
}

/// The input state of one control session: what is held, when to send a
/// heartbeat, and the lock keys the host should match.
export class InputController {
  private sink: InputSink;
  private keys = new Map<string, number>();
  private buttons = new Set<number>();
  private lastSent = 0;
  private heartbeatTimer = 0;
  private flushTimer = 0;
  private pendingAbsolute?: { screen: number; x: number; y: number };
  private pendingRelative = { dx: 0, dy: 0 };
  private wheelRest = { x: 0, y: 0 };
  private lockSeq = 0;
  private lockKnown?: Locks;
  /// Send the local lock state with the next event (control just started).
  private lockSend = true;
  /// Adopt the next observed state without sending (a forwarded lock key
  /// toggled both sides).
  private lockAdopt = false;
  /// macOS reports NumLock as off and CapsLock oddly: never correct locks.
  private readonly mac = /Mac|iPhone|iPad/.test(navigator.platform);
  /// Active touch contacts by browser pointer ID.
  private touches = new Map<number, TouchContact>();
  private touchScreen = 0;
  private pendingTouch?: Uint8Array;
  private pen: 'away' | 'hover' | 'contact' = 'away';
  private penSamples: PenSample[] = [];
  private penScreen = 0;
  private penFlushTimer = 0;
  active = false;

  constructor(sink: InputSink) {
    this.sink = sink;
  }

  private send(body: Uint8Array) {
    this.sink.send(body);
    this.lastSent = performance.now();
  }

  start() {
    if (this.active) return;
    this.active = true;
    this.lockSend = true;
    this.lockAdopt = false;
    this.lockKnown = undefined;
    this.lockSeq = 0;
  }

  /// Stop forwarding: release everything held first.
  stop() {
    if (!this.active) return;
    this.releaseAll();
    this.active = false;
    this.pendingAbsolute = undefined;
    this.pendingRelative = { dx: 0, dy: 0 };
    window.clearTimeout(this.flushTimer);
    this.flushTimer = 0;
  }

  get holding(): boolean {
    return this.keys.size > 0 || this.buttons.size > 0 || this.touches.size > 0 || this.pen === 'contact';
  }

  private sendPointerFrame(type: number, body: Uint8Array) {
    this.sink.sendFrame?.(type, body);
    this.lastSent = performance.now();
  }

  // Touch ------------------------------------------------------------------------

  /// A contact changed: send every active contact, this one with `phase`.
  touch(pointerId: number, phase: number, screen: number, x: number, y: number, extra: Partial<TouchContact> = {}) {
    if (!this.active) return;
    let contact = this.touches.get(pointerId);
    if (!contact) {
      if (phase !== TouchPhase.DOWN || this.touches.size >= 10) return;
      if (this.touches.size === 0) this.touchScreen = screen;
      const used = new Set([...this.touches.values()].map((c) => c.id));
      let id = 1;
      while (used.has(id)) id++;
      contact = { id, phase, x, y };
      this.touches.set(pointerId, contact);
    }
    Object.assign(contact, extra, { phase, x, y });
    const contacts = [...this.touches.values()].map((c) =>
      c === contact ? { ...c } : { ...c, phase: TouchPhase.MOVE },
    );
    if (phase === TouchPhase.UP || phase === TouchPhase.CANCEL) this.touches.delete(pointerId);
    const body = touchMessage(this.touchScreen, contacts, performance.now() * 1000);
    if (phase === TouchPhase.MOVE && this.sink.buffered() > MOVE_BUDGET) {
      // Only the newest pure-move frame matters.
      this.pendingTouch = body;
      this.scheduleFlush();
      return;
    }
    this.pendingTouch = undefined;
    this.flushMove();
    this.sendPointerFrame(0x0104, body);
    this.updateHeartbeat();
  }

  /// Lift every contact (an empty TOUCH frame).
  cancelTouches() {
    if (!this.touches.size) return;
    this.touches.clear();
    this.pendingTouch = undefined;
    this.sendPointerFrame(0x0104, touchMessage(this.touchScreen, []));
  }

  get touching(): boolean {
    return this.touches.size > 0;
  }

  // Pen ---------------------------------------------------------------------------

  /// A pen sample; MOVE/HOVER samples are batched (≤16) while the channel
  /// is busy, other phases go at once.
  penSample(screen: number, sample: PenSample, at: number) {
    if (!this.active) return;
    if (sample.phase === PenPhase.DOWN) this.pen = 'contact';
    else if (sample.phase === PenPhase.UP || sample.phase === PenPhase.HOVER) this.pen = 'hover';
    else if (sample.phase === PenPhase.LEAVE || sample.phase === PenPhase.CANCEL) this.pen = 'away';
    if (this.penSamples.length && screen !== this.penScreen) this.flushPen();
    this.penScreen = screen;
    this.penSamples.push({ ...sample, ageUs: at * 1000 });
    const quiet = sample.phase === PenPhase.MOVE || sample.phase === PenPhase.HOVER;
    if (this.penSamples.length > 16) {
      // Drop the oldest movement, never a down/up.
      const i = this.penSamples.findIndex((s) => s.phase === PenPhase.MOVE || s.phase === PenPhase.HOVER);
      this.penSamples.splice(i >= 0 ? i : 0, 1);
    }
    if (!quiet || this.sink.buffered() <= MOVE_BUDGET) this.flushPen();
    else if (!this.penFlushTimer) {
      this.penFlushTimer = window.setTimeout(() => {
        this.penFlushTimer = 0;
        this.flushPen();
      }, 8);
    }
    this.updateHeartbeat();
  }

  private flushPen() {
    if (!this.penSamples.length) return;
    const samples = this.penSamples;
    this.penSamples = [];
    // `ageUs` holds each sample's time; turn it into age before the last.
    const last = samples[samples.length - 1].ageUs ?? 0;
    for (const s of samples) s.ageUs = Math.max(0, last - (s.ageUs ?? 0));
    this.flushMove();
    this.sendPointerFrame(0x0105, penMessage(this.penScreen, samples, last));
  }

  /// Take the pen away (cancel a contact).
  cancelPen(x = 0, y = 0) {
    if (this.pen === 'away') return;
    this.penSample(this.penScreen, { phase: PenPhase.CANCEL, x, y }, performance.now());
  }

  get penState(): 'away' | 'hover' | 'contact' {
    return this.pen;
  }

  private updateHeartbeat() {
    if (this.holding && !this.heartbeatTimer) {
      this.heartbeatTimer = window.setInterval(() => {
        if (!this.holding) {
          window.clearInterval(this.heartbeatTimer);
          this.heartbeatTimer = 0;
        } else if (performance.now() - this.lastSent >= HEARTBEAT_MS) {
          this.send(heartbeat());
        }
      }, 15);
    }
  }

  /// Lock keys: the host follows the local CapsLock / NumLock / ScrollLock.
  observeLocks(e: KeyboardEvent | MouseEvent) {
    if (!this.active || this.mac || typeof e.getModifierState !== 'function') return;
    if (e instanceof KeyboardEvent && LOCKS.has(e.code)) return;
    const now: Locks = {
      caps: e.getModifierState('CapsLock'),
      num: e.getModifierState('NumLock'),
      scroll: e.getModifierState('ScrollLock'),
    };
    if (this.lockAdopt && !this.lockSend) {
      this.lockAdopt = false;
      this.lockKnown = now;
      return;
    }
    const known = this.lockKnown;
    if (this.lockSend || !known || known.caps !== now.caps || known.num !== now.num || known.scroll !== now.scroll) {
      this.lockSend = false;
      this.lockKnown = now;
      this.flushMove();
      this.send(lockStateMessage(now.caps, now.num, now.scroll, this.lockSeq));
    }
  }

  /// A key event; false when the key has no host usage (not forwarded).
  key(e: KeyboardEvent, down: boolean): boolean {
    if (!this.active || e.isComposing) return false;
    const usage = KEY_USAGE[e.code];
    if (usage === undefined) return false;
    this.observeLocks(e);
    this.flushMove();
    if (down) {
      const repeat = this.keys.has(e.code) || e.repeat;
      this.keys.set(e.code, usage);
      if (!repeat && LOCKS.has(e.code)) {
        this.lockSeq += 1;
        this.lockAdopt = true;
      }
      this.send(keyMessage(usage, true, repeat));
    } else {
      // Only release keys pressed while controlling.
      if (!this.keys.delete(e.code)) return true;
      this.send(keyMessage(usage, false));
    }
    this.updateHeartbeat();
    return true;
  }

  /// Press keys in order and release them in reverse (menu shortcuts).
  chord(usages: number[]) {
    if (!this.active) return;
    this.flushMove();
    for (const u of usages) this.send(keyMessage(u, true));
    for (const u of [...usages].reverse()) this.send(keyMessage(u, false));
  }

  /// Hold a key from a menu (released by `releaseKey` or `releaseAll`).
  holdKey(name: string, usage: number) {
    if (!this.active) return;
    this.flushMove();
    this.keys.set(name, usage);
    this.send(keyMessage(usage, true));
    this.updateHeartbeat();
  }

  releaseKey(name: string) {
    const usage = this.keys.get(name);
    if (usage === undefined) return;
    this.keys.delete(name);
    this.send(keyMessage(usage, false));
  }

  heldKey(name: string): boolean {
    return this.keys.has(name);
  }

  command(command: number) {
    if (!this.active) return;
    this.releaseAll();
    this.send(systemCommand(command));
  }

  /// Pointer position normalized to the host screen; coalesced while the
  /// control channel is backed up.
  moveTo(screen: number, x: number, y: number) {
    if (!this.active) return;
    this.pendingRelative = { dx: 0, dy: 0 };
    this.pendingAbsolute = { screen, x, y };
    this.scheduleFlush();
  }

  /// Relative motion (pointer lock), accumulated while backed up.
  moveBy(dx: number, dy: number) {
    if (!this.active || (dx === 0 && dy === 0)) return;
    this.pendingAbsolute = undefined;
    this.pendingRelative.dx += dx;
    this.pendingRelative.dy += dy;
    this.scheduleFlush();
  }

  private scheduleFlush() {
    if (this.sink.buffered() <= MOVE_BUDGET) {
      this.flushMove();
      return;
    }
    if (!this.flushTimer) {
      this.flushTimer = window.setTimeout(() => {
        this.flushTimer = 0;
        if (this.pendingAbsolute || this.pendingTouch || this.pendingRelative.dx || this.pendingRelative.dy) {
          this.scheduleFlush();
        }
      }, 4);
    }
  }

  /// Send the queued move now (before buttons, keys and wheel).
  private flushMove() {
    const touch = this.pendingTouch;
    if (touch) {
      this.pendingTouch = undefined;
      this.sendPointerFrame(0x0104, touch);
    }
    const a = this.pendingAbsolute;
    if (a) {
      this.pendingAbsolute = undefined;
      this.send(mouseAbsolute(a.screen, a.x, a.y));
    }
    const r = this.pendingRelative;
    if (r.dx || r.dy) {
      this.pendingRelative = { dx: 0, dy: 0 };
      // The host refuses more than 1 000 000 per message.
      const clamp = (v: number) => Math.max(-1_000_000, Math.min(1_000_000, Math.round(v)));
      this.send(mouseRelative(clamp(r.dx), clamp(r.dy)));
    }
  }

  button(button: number, down: boolean) {
    if (!this.active) return;
    if (!down && !this.buttons.has(button)) return;
    this.flushMove();
    if (down) this.buttons.add(button);
    else this.buttons.delete(button);
    this.send(mouseButton(button, down));
    this.updateHeartbeat();
  }

  /// A wheel event in 1/120 notches; dy > 0 scrolls up like Windows.
  wheel(e: Pick<WheelEvent, 'deltaX' | 'deltaY' | 'deltaMode'>) {
    if (!this.active) return;
    const scale = e.deltaMode === 1 ? 40 : e.deltaMode === 2 ? 120 : 1.2;
    this.wheelRest.x += e.deltaX * scale;
    this.wheelRest.y += -e.deltaY * scale;
    const dx = Math.trunc(this.wheelRest.x);
    const dy = Math.trunc(this.wheelRest.y);
    this.wheelRest.x -= dx;
    this.wheelRest.y -= dy;
    if (!dx && !dy) return;
    this.flushMove();
    const clamp = (v: number) => Math.max(-120_000, Math.min(120_000, v));
    this.send(mouseWheel(clamp(dx), clamp(dy)));
  }

  /// Release ordinary keys, then modifiers, then mouse buttons; lift
  /// contacts and the pen.
  releaseAll() {
    if (!this.active) return;
    this.flushMove();
    this.cancelTouches();
    this.cancelPen();
    this.flushPen();
    const held = [...this.keys.values()];
    this.keys.clear();
    for (const usage of held.filter((u) => !MODIFIERS.has(u))) this.send(keyMessage(usage, false));
    for (const usage of held.filter((u) => MODIFIERS.has(u))) this.send(keyMessage(usage, false));
    for (const b of [...this.buttons]) this.send(mouseButton(b, false));
    this.buttons.clear();
  }
}
