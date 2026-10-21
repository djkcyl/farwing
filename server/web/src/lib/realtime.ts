// The realtime connection of one controller session
// (`docs/protocol/web-viewer-brief.md` §2.1–§2.4, §2.12–§2.13). Browsers
// cannot set request headers, so the ticket (or resume token) travels as a
// WebSocket subprotocol next to `farwing.v2`.

import { currentLang, t } from '../i18n';
import { errorMessage } from '../i18n/errors';
import { errorReason } from './api';
import { sitePath } from './base';

export const SUBPROTOCOL = 'farwing.v2';

export interface Envelope<T = unknown> {
  v: 2;
  type: string;
  id?: string;
  re?: string;
  sid?: string;
  gen?: number;
  data?: T;
}

export interface ServerHello {
  conn_id: string;
  role: string;
  session_id: string;
  resume_token: string;
  heartbeat_ms: number;
  idle_timeout_ms: number;
  resumed: boolean;
  server_time_ms: number;
}

export interface ErrorBody {
  code: string;
  message: string;
  retryable?: boolean;
  details?: unknown;
}

/// A failed realtime request or connection: a server error (`server`, with
/// the error body's details) or the connection's own (`timeout`,
/// `protocol`, `closed`, `identity_mismatch`). The message is in the
/// language of the moment; `text()` gives it in the current one.
export class RealtimeError extends Error {
  code: string;
  details?: unknown;
  server: boolean;
  /// The WebSocket close code when the connection could not be opened.
  closeCode?: number;
  constructor(code: string, options: { server?: boolean; details?: unknown; closeCode?: number } = {}) {
    super(code);
    this.code = code;
    this.server = !!options.server;
    this.details = options.details;
    this.closeCode = options.closeCode;
    this.message = this.text();
  }

  text(): string {
    if (this.server) return errorMessage(currentLang(), this.code, errorReason(this.details), this.details);
    const m = t().viewer;
    switch (this.code) {
      case 'timeout':
        return m.realtime.timeout;
      case 'protocol':
        return m.realtime.protocol;
      case 'closed':
        return this.closeCode === undefined ? m.realtime.closed : m.realtime.cannotConnect(this.closeCode);
      case 'identity_mismatch':
        return m.ended.reasons.identity_mismatch;
      default:
        return this.code;
    }
  }
}

/// Close codes after which reconnecting cannot help (`realtime.rs` `close`).
const FINAL = new Set([1000, 4001, 4003, 4004, 4009, 4401]);

export function reconnectable(code: number): boolean {
  return !FINAL.has(code);
}

interface Pending {
  resolve(data: unknown): void;
  reject(error: Error): void;
  timer: number;
}

export interface RealtimeEvents {
  message(envelope: Envelope): void;
  closed(code: number, reason: string): void;
}

export class Realtime {
  hello!: ServerHello;
  private ws!: WebSocket;
  private next = 1;
  private pending = new Map<string, Pending>();
  private watchdog = 0;
  private events: RealtimeEvents;
  private closedByUs = false;

  private constructor(events: RealtimeEvents) {
    this.events = events;
  }

  /// Connect with a session ticket; resolves once the server's `hello` arrived.
  static async connect(ticket: string, events: RealtimeEvents): Promise<Realtime> {
    const rt = new Realtime(events);
    await rt.open(`ticket.${ticket}`);
    return rt;
  }

  /// Reconnect after the WebSocket dropped, with the resume token.
  async resume(): Promise<void> {
    await this.open(`resume.${this.hello.resume_token}`);
  }

  /// The page's own origin, not the ticket's endpoints: the server's public
  /// URL may name another host than the one this page was opened with, and
  /// only this origin's certificate is known to be trusted by the browser.
  private endpoint(): string {
    return `${location.protocol === 'https:' ? 'wss' : 'ws'}://${location.host}${sitePath('/api/v2/ws')}`;
  }

  private open(credential: string): Promise<void> {
    return new Promise((resolve, reject) => {
      const ws = new WebSocket(this.endpoint(), [SUBPROTOCOL, credential]);
      this.ws = ws;
      let ready = false;
      const helloTimer = window.setTimeout(() => {
        if (!ready) {
          ws.close();
          reject(new RealtimeError('timeout'));
        }
      }, 10000);
      ws.onmessage = (event) => {
        if (typeof event.data !== 'string') return;
        let envelope: Envelope;
        try {
          envelope = JSON.parse(event.data) as Envelope;
        } catch {
          return;
        }
        this.alive();
        if (!ready) {
          if (envelope.type !== 'hello') return;
          ready = true;
          window.clearTimeout(helloTimer);
          this.hello = envelope.data as ServerHello;
          if (ws.protocol !== SUBPROTOCOL) {
            ws.close();
            reject(new RealtimeError('protocol'));
            return;
          }
          resolve();
          return;
        }
        this.dispatch(envelope);
      };
      ws.onclose = (event) => {
        window.clearTimeout(helloTimer);
        window.clearTimeout(this.watchdog);
        for (const [, p] of this.pending) {
          window.clearTimeout(p.timer);
          p.reject(new RealtimeError('closed'));
        }
        this.pending.clear();
        if (!ready) {
          reject(new RealtimeError('closed', { closeCode: event.code }));
          return;
        }
        if (ws === this.ws && !this.closedByUs) this.events.closed(event.code, event.reason);
      };
    });
  }

  /// The connection is dead when nothing (not even a ping) arrived for
  /// heartbeat + idle timeout.
  private alive() {
    window.clearTimeout(this.watchdog);
    const limit = Math.max(5000, (this.hello?.heartbeat_ms ?? 20000) + (this.hello?.idle_timeout_ms ?? 60000));
    this.watchdog = window.setTimeout(() => this.ws.close(4000, 'stale'), limit);
  }

  private dispatch(envelope: Envelope) {
    if (envelope.type === 'ping') {
      this.raw({ ...envelope, type: 'pong' });
      return;
    }
    if ((envelope.type === 'ack' || envelope.type === 'error') && envelope.re) {
      const pending = this.pending.get(envelope.re);
      if (pending) {
        this.pending.delete(envelope.re);
        window.clearTimeout(pending.timer);
        if (envelope.type === 'ack') pending.resolve(envelope.data);
        else {
          const body = envelope.data as ErrorBody;
          pending.reject(new RealtimeError(body.code, { server: true, details: body.details }));
        }
        return;
      }
    }
    this.events.message(envelope);
  }

  private raw(envelope: Envelope) {
    if (this.ws.readyState === WebSocket.OPEN) this.ws.send(JSON.stringify(envelope));
  }

  send(type: string, data: unknown, extra: Partial<Envelope> = {}) {
    this.raw({ v: 2, type, ...extra, data });
  }

  request<T>(type: string, data: unknown, extra: Partial<Envelope> = {}, timeoutMs = 10000): Promise<T> {
    const id = `c-${this.next++}`;
    return new Promise<T>((resolve, reject) => {
      const timer = window.setTimeout(() => {
        this.pending.delete(id);
        reject(new RealtimeError('timeout'));
      }, timeoutMs);
      this.pending.set(id, { resolve: resolve as (d: unknown) => void, reject, timer });
      this.raw({ v: 2, type, id, ...extra, data });
    });
  }

  /// Leave the session for good.
  bye() {
    this.closedByUs = true;
    this.raw({ v: 2, type: 'bye', data: {} });
    window.setTimeout(() => this.ws.close(1000), 200);
  }

  get connected(): boolean {
    return this.ws.readyState === WebSocket.OPEN;
  }
}
