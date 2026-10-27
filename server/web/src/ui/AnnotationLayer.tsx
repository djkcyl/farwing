// Drawing on the host's screen from the browser (`docs/web-console.md`
// §7.7): a layer over the picture takes the pointer (mouse, pen or touch).
// The pen sends its points every 30 ms while drawing and previews the stroke
// locally until the host's ink shows in the video.
import { useEffect, useLayoutEffect, useRef } from 'react';
import { argbCss, type Annotation, type Point } from '../lib/annotation';

export interface Box {
  x: number;
  y: number;
  w: number;
  h: number;
}

const FLUSH_MS = 30;
const PREVIEW_MS = 900;

export function AnnotationLayer({
  annotation,
  box,
  screenId,
  screenWidth,
  color,
  width,
}: {
  annotation: Annotation;
  box: Box;
  screenId: number;
  /// Host screen width in pixels: line widths are host pixels.
  screenWidth: number;
  color: number;
  width: number;
}) {
  const canvas = useRef<HTMLCanvasElement>(null);
  /// The pen stroke being drawn or previewed; `done` once the pen lifts.
  const live = useRef<{ id: number; points: Point[]; unsent: Point[]; done: number } | undefined>(undefined);
  const settings = useRef({ box, screenId, screenWidth, color, width });
  useLayoutEffect(() => {
    settings.current = { box, screenId, screenWidth, color, width };
  });

  // Local preview of the live stroke.
  useEffect(() => {
    let frame = 0;
    const paint = () => {
      frame = requestAnimationFrame(paint);
      const c = canvas.current;
      const s = settings.current;
      if (!c) return;
      const dpr = devicePixelRatio || 1;
      const w = Math.round(s.box.w * dpr);
      const h = Math.round(s.box.h * dpr);
      if (c.width !== w || c.height !== h) {
        c.width = w;
        c.height = h;
      }
      const ctx = c.getContext('2d');
      if (!ctx) return;
      ctx.clearRect(0, 0, w, h);
      const scale = (s.box.w / Math.max(s.screenWidth, 1)) * dpr;
      ctx.lineCap = 'round';
      ctx.lineJoin = 'round';
      const stroke = live.current;
      const now = performance.now();
      if (stroke && (!stroke.done || now - stroke.done < PREVIEW_MS)) {
        ctx.strokeStyle = argbCss(s.color);
        ctx.lineWidth = Math.max(1, s.width * scale);
        ctx.beginPath();
        stroke.points.forEach((p, i) => (i ? ctx.lineTo(p.x * w, p.y * h) : ctx.moveTo(p.x * w, p.y * h)));
        if (stroke.points.length === 1) ctx.lineTo(stroke.points[0].x * w + 0.1, stroke.points[0].y * h);
        ctx.stroke();
      }
    };
    paint();
    return () => cancelAnimationFrame(frame);
  }, []);

  useEffect(() => {
    const c = canvas.current;
    if (!c) return;
    const at = (e: PointerEvent): Point => {
      const r = c.getBoundingClientRect();
      const clamp = (n: number) => Math.min(Math.max(n, 0), 1);
      return { x: clamp((e.clientX - r.left) / r.width), y: clamp((e.clientY - r.top) / r.height) };
    };
    let timer = 0;
    // The pointer drawing now (capture can fail for a pointer already gone).
    let pointer: number | undefined;
    const flush = () => {
      const stroke = live.current;
      const s = settings.current;
      if (!stroke || !stroke.unsent.length) return;
      annotation.extend(stroke.id, s.screenId, stroke.unsent, s.width, s.color);
      stroke.unsent = [];
    };
    const down = (e: PointerEvent) => {
      if (e.button !== 0) return;
      e.preventDefault();
      pointer = e.pointerId;
      try {
        c.setPointerCapture(e.pointerId);
      } catch {
        // Moves outside the layer are lost; the stroke still works inside.
      }
      const p = at(e);
      live.current = { id: annotation.begin(), points: [p], unsent: [p], done: 0 };
      flush();
      window.clearInterval(timer);
      timer = window.setInterval(flush, FLUSH_MS);
    };
    const move = (e: PointerEvent) => {
      if (e.pointerId !== pointer) return;
      const coalesced = e.getCoalescedEvents?.() ?? [];
      const events = coalesced.length ? coalesced : [e];
      const stroke = live.current;
      if (!stroke) return;
      for (const ev of events) {
        const p = at(ev);
        const last = stroke.points[stroke.points.length - 1];
        if (Math.abs(p.x - last.x) < 0.0005 && Math.abs(p.y - last.y) < 0.0005) continue;
        stroke.points.push(p);
        stroke.unsent.push(p);
      }
    };
    const up = (e: PointerEvent) => {
      if (e.pointerId !== pointer) return;
      pointer = undefined;
      if (c.hasPointerCapture(e.pointerId)) c.releasePointerCapture(e.pointerId);
      window.clearInterval(timer);
      flush();
      if (live.current) live.current.done = performance.now();
    };
    c.addEventListener('pointerdown', down);
    c.addEventListener('pointermove', move);
    c.addEventListener('pointerup', up);
    c.addEventListener('pointercancel', up);
    return () => {
      window.clearInterval(timer);
      flush();
      c.removeEventListener('pointerdown', down);
      c.removeEventListener('pointermove', move);
      c.removeEventListener('pointerup', up);
      c.removeEventListener('pointercancel', up);
    };
  }, [annotation]);

  return (
    <canvas
      ref={canvas}
      className="annotation-layer"
      style={{ left: box.x, top: box.y, width: box.w, height: box.h }}
    />
  );
}
