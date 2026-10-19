// Forwarding this browser's keyboard, mouse, touch and pen to the host while
// the viewer holds the control token (`docs/protocol/web-control-brief.md`
// §2, §6; `touch-pen-brief.md` §3.3): absolute positions on the shown
// screen, relative motion under pointer lock, touch as real contacts (or
// mouse gestures where the host has no touch), the pen with pressure and
// tilt (or as a mouse).
import { useEffect, type RefObject } from 'react';
import { Button, PenPhase, penTilt, TouchPhase, wireButton } from '../lib/input';
import type { Viewer } from '../lib/viewer';

export interface PictureMap {
  /// The host screen in the picture and its size in pixels.
  screenId: number;
  width: number;
  height: number;
  /// The picture's rectangle inside the <video> element.
  box(): { x: number; y: number; w: number; h: number } | undefined;
}

export interface RemoteInputOptions {
  stage: RefObject<HTMLDivElement | null>;
  video: RefObject<HTMLVideoElement | null>;
  viewer: RefObject<Viewer | undefined>;
  map: RefObject<PictureMap | undefined>;
  /// Forward at all (the token is held).
  active: boolean;
  /// Relative mouse under pointer lock.
  relative: boolean;
  /// Pointer lock ended (Esc): back to absolute.
  onUnlocked(): void;
  /// Send real touch contacts (the host declared `touch`); else gestures.
  touchNative: boolean;
  /// Send the pen as a pen (the host declared `pen`); else as a mouse.
  penNative: boolean;
}

const LONG_PRESS_MS = 500;
const DRAG_SLOP_PX = 8;

export function useRemoteInput({
  stage,
  video,
  viewer,
  map,
  active,
  relative,
  onUnlocked,
  touchNative,
  penNative,
}: RemoteInputOptions) {
  useEffect(() => {
    const el = stage.current;
    if (!active || !el) return;
    const input = () => viewer.current?.input;
    const locked = () => document.pointerLockElement === el;
    el.focus({ preventScroll: true });

    /// Normalized position on the host screen; outside the picture only
    /// while a button is held (clamped to the edge).
    const position = (clientX: number, clientY: number, clampOutside: boolean): [number, number] | undefined => {
      const m = map.current;
      const v = video.current;
      const box = m?.box();
      if (!m || !v || !box || !box.w || !box.h) return undefined;
      const rect = v.getBoundingClientRect();
      const x = (clientX - rect.left - box.x) / box.w;
      const y = (clientY - rect.top - box.y) / box.h;
      if (!clampOutside && (x < 0 || x >= 1 || y < 0 || y >= 1)) return undefined;
      const clamp = (n: number, size: number) => Math.min(Math.max(n, 0), 1 - 1 / Math.max(size, 2));
      return [clamp(x, m.width), clamp(y, m.height)];
    };
    const moveTo = (clientX: number, clientY: number, clampOutside: boolean): boolean => {
      const p = position(clientX, clientY, clampOutside);
      const m = map.current;
      if (!p || !m) return false;
      input()?.moveTo(m.screenId, p[0], p[1]);
      return true;
    };

    // Keyboard ---------------------------------------------------------------
    const key = (e: KeyboardEvent) => {
      if (input()?.key(e, e.type === 'keydown')) {
        e.preventDefault();
        e.stopPropagation();
      }
    };
    const releaseAll = () => input()?.releaseAll();
    const visibility = () => document.visibilityState === 'hidden' && releaseAll();

    // Real touch contacts and pen samples --------------------------------------
    const box = () => map.current?.box();
    const contactSize = (e: PointerEvent) => {
      const b = box();
      if (!b || e.width <= 1 || e.height <= 1) return {};
      return { width: Math.min(1, e.width / b.w), height: Math.min(1, e.height / b.h) };
    };
    const contacts = new Set<number>();
    const nativeTouch = (e: PointerEvent, phase: number) => {
      const m = map.current;
      if (!m) return;
      if (phase === TouchPhase.DOWN) {
        const p = position(e.clientX, e.clientY, false);
        if (!p) return;
        contacts.add(e.pointerId);
        input()?.touch(e.pointerId, phase, m.screenId, p[0], p[1], {
          pressure: e.pressure,
          ...contactSize(e),
        });
        return;
      }
      if (!contacts.has(e.pointerId)) return;
      if (phase !== TouchPhase.MOVE) contacts.delete(e.pointerId);
      const p = position(e.clientX, e.clientY, true) ?? [0, 0];
      input()?.touch(e.pointerId, phase, m.screenId, p[0], p[1], { pressure: e.pressure, ...contactSize(e) });
    };
    const penButtons = (e: PointerEvent) => (e.buttons & 2 ? 1 : 0) | (e.buttons & 32 || e.button === 5 ? 2 : 0);
    const penSample = (e: PointerEvent, phase: number) => {
      const m = map.current;
      const p = position(e.clientX, e.clientY, phase !== PenPhase.HOVER);
      if (!m || !p) {
        // Out of the picture while hovering: out of range for the host.
        if (input()?.penState === 'hover') {
          input()?.penSample(m?.screenId ?? 0, { phase: PenPhase.LEAVE, x: 0, y: 0 }, e.timeStamp);
        }
        return;
      }
      const [tiltX, tiltY] = penTilt(e);
      input()?.penSample(
        m.screenId,
        { phase, x: p[0], y: p[1], pressure: e.pressure, tiltX, tiltY, twist: e.twist, buttons: penButtons(e) },
        e.timeStamp,
      );
    };
    const penDown = (e: PointerEvent) => {
      // No compatibility mouse events: the pen is not also a mouse.
      e.preventDefault();
      el.focus({ preventScroll: true });
      try {
        el.setPointerCapture(e.pointerId);
      } catch {
        // Still works while inside the stage.
      }
      penSample(e, PenPhase.DOWN);
    };
    const penMove = (e: PointerEvent) => {
      const coalesced = e.getCoalescedEvents?.() ?? [];
      for (const ev of coalesced.length ? coalesced : [e]) {
        penSample(ev, ev.buttons & 1 || ev.buttons & 32 ? PenPhase.MOVE : PenPhase.HOVER);
      }
    };

    // Mouse ------------------------------------------------------------------
    let swallow = false;
    const pointerDown = (e: PointerEvent) => {
      if (e.pointerType === 'touch') {
        if (touchNative) {
          e.preventDefault();
          el.focus({ preventScroll: true });
          return nativeTouch(e, TouchPhase.DOWN);
        }
        return touchDown(e);
      }
      if (e.pointerType === 'pen' && penNative) return penDown(e);
      el.focus({ preventScroll: true });
      if (relative && !locked()) {
        swallow = true;
        const request = el.requestPointerLock as (options?: { unadjustedMovement?: boolean }) => Promise<void> | void;
        Promise.resolve(request.call(el, { unadjustedMovement: true })).catch(() =>
          Promise.resolve(request.call(el)).catch(() => undefined),
        );
        return;
      }
      swallow = false;
      el.setPointerCapture(e.pointerId);
    };
    const pointerMove = (e: PointerEvent) => {
      if (e.pointerType === 'touch') return touchNative ? nativeTouch(e, TouchPhase.MOVE) : touchMove(e);
      if (e.pointerType === 'pen' && penNative) return penMove(e);
      if (locked()) input()?.moveBy(e.movementX, e.movementY);
      else moveTo(e.clientX, e.clientY, !!input()?.holding);
    };
    const mouseDown = (e: MouseEvent) => {
      if (swallow) return;
      const button = wireButton(e.button);
      if (button === undefined) return;
      e.preventDefault();
      input()?.observeLocks(e);
      if (!locked() && !moveTo(e.clientX, e.clientY, false)) return;
      input()?.button(button, true);
    };
    const mouseUp = (e: MouseEvent) => {
      const button = wireButton(e.button);
      if (button === undefined) return;
      // X1/X2 would go back/forward in the browser.
      if (e.button >= 3 || el.contains(e.target as Node)) e.preventDefault();
      if (swallow) {
        swallow = false;
        return;
      }
      if (!locked()) moveTo(e.clientX, e.clientY, true);
      input()?.button(button, false);
    };
    const wheel = (e: WheelEvent) => {
      e.preventDefault();
      if (!locked() && !moveTo(e.clientX, e.clientY, false)) return;
      input()?.wheel(e);
    };
    const prevent = (e: Event) => e.preventDefault();
    const lockChange = () => {
      if (!locked() && relative) onUnlocked();
    };

    // Touch: tap = left click, long press = right click, drag = left drag,
    // two fingers = wheel, two-finger tap = right click ------------------------
    const touches = new Map<number, { x: number; y: number }>();
    let gesture:
      | {
          x: number;
          y: number;
          at: number;
          moved: boolean;
          dragging: boolean;
          done: boolean;
          two: boolean;
          scrolled: boolean;
          cx: number;
          cy: number;
        }
      | undefined;
    let longPress = 0;
    const centroid = () => {
      const all = [...touches.values()];
      return {
        x: all.reduce((n, t) => n + t.x, 0) / all.length,
        y: all.reduce((n, t) => n + t.y, 0) / all.length,
      };
    };
    const click = (x: number, y: number, button: number) => {
      if (!moveTo(x, y, false)) return;
      input()?.button(button, true);
      input()?.button(button, false);
    };
    const touchDown = (e: PointerEvent) => {
      e.preventDefault();
      el.focus({ preventScroll: true });
      touches.set(e.pointerId, { x: e.clientX, y: e.clientY });
      if (touches.size === 1) {
        gesture = {
          x: e.clientX,
          y: e.clientY,
          at: performance.now(),
          moved: false,
          dragging: false,
          done: false,
          two: false,
          scrolled: false,
          cx: e.clientX,
          cy: e.clientY,
        };
        window.clearTimeout(longPress);
        longPress = window.setTimeout(() => {
          const g = gesture;
          if (g && !g.moved && !g.two && touches.size === 1) {
            g.done = true;
            click(g.x, g.y, Button.RIGHT);
          }
        }, LONG_PRESS_MS);
      } else if (touches.size === 2 && gesture) {
        window.clearTimeout(longPress);
        if (gesture.dragging) {
          input()?.button(Button.LEFT, false);
          gesture.dragging = false;
        }
        gesture.two = true;
        const c = centroid();
        gesture.cx = c.x;
        gesture.cy = c.y;
      }
    };
    const touchMove = (e: PointerEvent) => {
      if (!touches.has(e.pointerId)) return;
      touches.set(e.pointerId, { x: e.clientX, y: e.clientY });
      const g = gesture;
      if (!g || g.done) return;
      if (g.two) {
        if (touches.size < 2) return;
        const c = centroid();
        const dx = c.x - g.cx;
        const dy = c.y - g.cy;
        g.cx = c.x;
        g.cy = c.y;
        if (dx || dy) {
          g.scrolled = true;
          // Content follows the fingers.
          input()?.wheel({ deltaX: -dx, deltaY: -dy, deltaMode: 0 });
        }
        return;
      }
      if (!g.moved && Math.hypot(e.clientX - g.x, e.clientY - g.y) > DRAG_SLOP_PX) {
        g.moved = true;
        window.clearTimeout(longPress);
        if (moveTo(g.x, g.y, false)) {
          g.dragging = true;
          input()?.button(Button.LEFT, true);
        }
      }
      if (g.dragging) moveTo(e.clientX, e.clientY, true);
    };
    const pointerEnd = (e: PointerEvent) => {
      const cancel = e.type === 'pointercancel';
      if (e.pointerType === 'pen' && penNative) {
        penSample(e, cancel ? PenPhase.CANCEL : PenPhase.UP);
        return;
      }
      if (e.pointerType === 'touch' && touchNative) {
        nativeTouch(e, cancel ? TouchPhase.CANCEL : TouchPhase.UP);
        return;
      }
      touchUp(e);
    };
    const penLeave = (e: PointerEvent) => {
      if (e.pointerType === 'pen' && penNative && input()?.penState === 'hover') {
        input()?.penSample(map.current?.screenId ?? 0, { phase: PenPhase.LEAVE, x: 0, y: 0 }, e.timeStamp);
      }
    };
    const touchUp = (e: PointerEvent) => {
      if (e.pointerType !== 'touch' || !touches.has(e.pointerId)) return;
      const g = gesture;
      const count = touches.size;
      touches.delete(e.pointerId);
      if (!g) return;
      if (g.dragging) {
        moveTo(e.clientX, e.clientY, true);
        input()?.button(Button.LEFT, false);
        g.dragging = false;
        g.done = true;
      } else if (!g.two && !g.done && !g.moved && count === 1) {
        click(g.x, g.y, Button.LEFT);
        g.done = true;
      } else if (g.two && !g.scrolled && !g.done && performance.now() - g.at < 400) {
        click(g.cx, g.cy, Button.RIGHT);
        g.done = true;
      }
      if (touches.size === 0) {
        window.clearTimeout(longPress);
        gesture = undefined;
      }
    };

    el.addEventListener('keydown', key);
    el.addEventListener('keyup', key);
    el.addEventListener('blur', releaseAll);
    el.addEventListener('pointerdown', pointerDown);
    el.addEventListener('pointermove', pointerMove);
    el.addEventListener('pointerup', pointerEnd);
    el.addEventListener('pointercancel', pointerEnd);
    el.addEventListener('pointerleave', penLeave);
    el.addEventListener('mousedown', mouseDown);
    window.addEventListener('mouseup', mouseUp);
    el.addEventListener('wheel', wheel, { passive: false });
    el.addEventListener('contextmenu', prevent);
    el.addEventListener('auxclick', prevent);
    el.addEventListener('dragstart', prevent);
    window.addEventListener('blur', releaseAll);
    document.addEventListener('visibilitychange', visibility);
    document.addEventListener('pointerlockchange', lockChange);
    return () => {
      window.clearTimeout(longPress);
      el.removeEventListener('keydown', key);
      el.removeEventListener('keyup', key);
      el.removeEventListener('blur', releaseAll);
      el.removeEventListener('pointerdown', pointerDown);
      el.removeEventListener('pointermove', pointerMove);
      el.removeEventListener('pointerup', pointerEnd);
      el.removeEventListener('pointercancel', pointerEnd);
      el.removeEventListener('pointerleave', penLeave);
      el.removeEventListener('mousedown', mouseDown);
      window.removeEventListener('mouseup', mouseUp);
      el.removeEventListener('wheel', wheel);
      el.removeEventListener('contextmenu', prevent);
      el.removeEventListener('auxclick', prevent);
      el.removeEventListener('dragstart', prevent);
      window.removeEventListener('blur', releaseAll);
      document.removeEventListener('visibilitychange', visibility);
      document.removeEventListener('pointerlockchange', lockChange);
      if (locked()) document.exitPointerLock();
      releaseAll();
    };
  }, [active, relative, stage, video, viewer, map, onUnlocked, touchNative, penNative]);
}
