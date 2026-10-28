// Text clipboard over CLIPBOARD v2 (`docs/protocol/web-control-brief.md` §3):
// Windows CF_UNICODETEXT (format 13, UTF-16LE with a NUL, CRLF line ends).
// Up to 32 KiB travels inline in `Data`; more as a TRANSFER stream (kind 2)
// on the bulk channel, paced by FLOW credits.
import * as fpp from './fpp';
import { Fields, Writer } from './pb';
import type { PeerIo } from './peerio';

const TEXT = 13;
const INLINE_MAX = 32 * 1024;
const MAX_TEXT = 16 * 1024 * 1024;
const WINDOW = 4 * 1024 * 1024;
const FETCH_TIMEOUT_MS = 30_000;

export function encodeText(text: string): Uint8Array {
  const crlf = text.replace(/\r?\n/g, '\r\n');
  const out = new Uint8Array((crlf.length + 1) * 2);
  const view = new DataView(out.buffer);
  for (let i = 0; i < crlf.length; i++) view.setUint16(i * 2, crlf.charCodeAt(i), true);
  return out;
}

/// Up to the first NUL (host data may be padded); CRLF back to LF.
export function decodeText(bytes: Uint8Array): string {
  const view = new DataView(bytes.buffer, bytes.byteOffset, bytes.byteLength);
  const units: number[] = [];
  for (let i = 0; i + 1 < bytes.length; i += 2) {
    const u = view.getUint16(i, true);
    if (u === 0) break;
    units.push(u);
  }
  let text = '';
  for (let i = 0; i < units.length; i += 8192) text += String.fromCharCode(...units.slice(i, i + 8192));
  return text.replace(/\r\n/g, '\n');
}

export function offerMessage(offer: number, formats: number[]): Uint8Array {
  const inner = new Writer().int(1, offer);
  for (const id of formats) inner.message(2, new Writer().int(1, id));
  return new Writer().message(1, inner).finish();
}

export function fetchMessage(offer: number, format: number, stream: number): Uint8Array {
  return new Writer()
    .message(2, new Writer().int(1, offer).message(2, new Writer().int(1, format)).int(3, stream))
    .finish();
}

export function dataMessage(size: number, inline: Uint8Array): Uint8Array {
  return new Writer().message(3, new Writer().int(1, size).raw(2, inline)).finish();
}

export function pullMessage(): Uint8Array {
  return new Writer().message(9, new Writer()).finish();
}

export function abortMessage(stream: number): Uint8Array {
  return new Writer().message(8, new Writer().int(1, stream)).finish();
}

interface Incoming {
  chunks: Uint8Array[];
  received: number;
  granted: number;
  done: boolean;
  finished?: () => void;
}

interface Outgoing {
  data: Uint8Array;
  credit: number;
  aborted: boolean;
  wake?: () => void;
}

export interface ClipboardEvents {
  /// Text the host copied (automatic mode) or that a pull fetched.
  remoteText?(text: string): void;
  log?(line: string): void;
}

export class ClipboardSync {
  private io: PeerIo;
  private events: ClipboardEvents;
  private offerSeq = 0;
  private current?: { offer: number; data: Uint8Array; text: string };
  private lastRemote?: string;
  private streamSeq = 0;
  private incoming = new Map<number, Incoming>();
  private outgoing = new Map<number, Outgoing>();
  /// Fetch what the host offers by itself (automatic synchronization).
  auto = true;

  constructor(io: PeerIo, events: ClipboardEvents) {
    this.io = io;
    this.events = events;
  }

  /// Offer local text to the host; false when unchanged, just received
  /// from the host, empty or too large.
  offer(text: string): boolean {
    if (!text || text === this.current?.text || text === this.lastRemote) return false;
    const data = encodeText(text);
    if (data.length > MAX_TEXT) return false;
    this.offerSeq = (this.offerSeq % 0xffffffff) + 1;
    this.current = { offer: this.offerSeq, data, text };
    this.io.push(fpp.Type.CLIPBOARD, offerMessage(this.offerSeq, [TEXT]));
    return true;
  }

  /// The host's clipboard text, without the automatic mode.
  async pull(): Promise<string | undefined> {
    const body = await this.io.request(fpp.Type.CLIPBOARD, pullMessage());
    const offer = new Fields(body).message(1);
    if (!offer) return undefined;
    if (!offer.messages(2).some((f) => f.uint32(1) === TEXT)) return undefined;
    return this.fetch(offer.uint32(1));
  }

  private async fetch(offer: number): Promise<string> {
    this.streamSeq = (this.streamSeq % 0xffffffff) + 1;
    const stream = this.streamSeq;
    const incoming: Incoming = { chunks: [], received: 0, granted: WINDOW, done: false };
    this.incoming.set(stream, incoming);
    try {
      const body = await this.io.request(fpp.Type.CLIPBOARD, fetchMessage(offer, TEXT, stream), FETCH_TIMEOUT_MS);
      const data = new Fields(body).message(3);
      if (!data) throw new Error('unexpected clipboard reply');
      const size = Number(data.big(1));
      if (size > MAX_TEXT) throw new Error('clipboard text too large');
      let bytes = data.bytes(2);
      if (size > INLINE_MAX && bytes.length === 0) {
        if (!incoming.done) {
          await new Promise<void>((resolve, reject) => {
            const timer = window.setTimeout(() => {
              this.io.push(fpp.Type.CLIPBOARD, abortMessage(stream));
              reject(new Error('clipboard transfer timed out'));
            }, FETCH_TIMEOUT_MS);
            incoming.finished = () => {
              window.clearTimeout(timer);
              resolve();
            };
          });
        }
        bytes = join(incoming.chunks, incoming.received);
      }
      const text = decodeText(bytes);
      this.lastRemote = text;
      return text;
    } finally {
      this.incoming.delete(stream);
    }
  }

  /// CLIPBOARD frames that are not responses to our requests.
  async handle(frame: fpp.Frame) {
    const f = new Fields(frame.body);
    const offer = f.message(1);
    if (offer && frame.requestId === 0) {
      if (!this.auto || !offer.messages(2).some((x) => x.uint32(1) === TEXT)) return;
      try {
        const text = await this.fetch(offer.uint32(1));
        if (text) this.events.remoteText?.(text);
      } catch (error) {
        this.events.log?.(`clipboard fetch failed: ${(error as Error).message}`);
      }
      return;
    }
    const fetch = f.message(2);
    if (fetch && frame.requestId !== 0) {
      const current = this.current;
      if (!current || fetch.uint32(1) !== current.offer || fetch.message(2)?.uint32(1) !== TEXT) {
        this.io.replyError(fpp.Type.CLIPBOARD, frame.requestId, 'not_found', 'format_unavailable');
        return;
      }
      const stream = fetch.uint32(3);
      if (current.data.length <= INLINE_MAX) {
        this.io.reply(fpp.Type.CLIPBOARD, frame.requestId, dataMessage(current.data.length, current.data));
        return;
      }
      if (!stream) {
        this.io.replyError(fpp.Type.CLIPBOARD, frame.requestId, 'internal', 'failed');
        return;
      }
      this.io.reply(fpp.Type.CLIPBOARD, frame.requestId, dataMessage(current.data.length, new Uint8Array()));
      void this.sendStream(stream, current.data);
      return;
    }
    const abort = f.message(8);
    if (abort) {
      const out = this.outgoing.get(abort.uint32(1));
      if (out) {
        out.aborted = true;
        out.wake?.();
      }
      return;
    }
    if (frame.requestId !== 0)
      this.io.replyError(fpp.Type.CLIPBOARD, frame.requestId, 'bad_request', 'invalid_request');
  }

  private async sendStream(stream: number, data: Uint8Array) {
    const out: Outgoing = { data, credit: fpp.INITIAL_CREDIT, aborted: false };
    this.outgoing.set(stream, out);
    try {
      for (let at = 0; at < data.length && !out.aborted; at += fpp.TRANSFER_CHUNK) {
        const piece = data.subarray(at, at + fpp.TRANSFER_CHUNK);
        while (at + piece.length > out.credit && !out.aborted) {
          await new Promise<void>((resolve) => {
            out.wake = resolve;
            window.setTimeout(resolve, 30_000);
          });
        }
        if (out.aborted) break;
        await this.io.bulk(
          fpp.Type.TRANSFER,
          fpp.chunk({
            stream,
            kind: fpp.TransferKind.CLIPBOARD,
            offset: BigInt(at),
            data: piece,
            last: at + piece.length >= data.length,
            entry: 0,
          }),
        );
      }
    } finally {
      this.outgoing.delete(stream);
    }
  }

  onChunk(c: fpp.Chunk) {
    const incoming = this.incoming.get(c.stream);
    if (!incoming || incoming.done) return;
    incoming.chunks.push(c.data.slice());
    incoming.received += c.data.length;
    if (incoming.received > MAX_TEXT) {
      incoming.done = true;
      this.io.push(fpp.Type.CLIPBOARD, abortMessage(c.stream));
      return;
    }
    if (incoming.received + WINDOW - incoming.granted >= WINDOW / 4) {
      incoming.granted = incoming.received + WINDOW;
      void this.io.bulk(
        fpp.Type.FLOW,
        fpp.flow({ stream: c.stream, credit: BigInt(incoming.granted), kind: fpp.TransferKind.CLIPBOARD }),
      );
    }
    if (c.last) {
      incoming.done = true;
      incoming.finished?.();
    }
  }

  onFlow(f: fpp.FlowCredit) {
    const out = this.outgoing.get(f.stream);
    if (!out) return;
    out.credit = Math.max(out.credit, Number(f.credit));
    out.wake?.();
  }

  reset() {
    this.current = undefined;
    for (const out of this.outgoing.values()) {
      out.aborted = true;
      out.wake?.();
    }
    this.incoming.clear();
  }
}

function join(chunks: Uint8Array[], size: number): Uint8Array {
  const out = new Uint8Array(size);
  let at = 0;
  for (const c of chunks) {
    out.set(c, at);
    at += c.length;
  }
  return out;
}
