// The Farwing peer protocol (FPP v2) subset the browser needs
// (`docs/protocol/web-viewer-brief.md` §4, `web-control-brief.md`): 10-byte
// frame header and fragments, HELLO, SESSION (screens, quality, capture,
// control turn, microphone), CURSOR, PARTICIPANTS, PERMISSIONS, HOST_EVENT,
// AUDIO, and the TRANSFER / FLOW data plane of the clipboard and files.
import { t } from '../i18n';
import { Fields, Writer } from './pb';

export const MAX_FRAME = 524288;

export const CHANNELS = [
  'farwing.control.v2',
  'farwing.session.v2',
  'farwing.bulk.v2',
  'farwing.tunnel.v2',
  'farwing.dgram.v2',
] as const;
export type ChannelLabel = (typeof CHANNELS)[number];

export const Flag = { RESPONSE: 0x01, ERROR: 0x02, MORE: 0x04 } as const;

export const Type = {
  HELLO: 0x0001,
  ERROR: 0x0002,
  INPUT: 0x0101,
  TOUCH: 0x0104,
  PEN: 0x0105,
  CURSOR: 0x0103,
  POINTER: 0x0106,
  SESSION: 0x0201,
  AUDIO: 0x0202,
  HOST_EVENT: 0x0203,
  CLIPBOARD: 0x0301,
  ANNOTATION: 0x0601,
  FILE: 0x0401,
  TRANSFER: 0x0402,
  PERMISSIONS: 0x0901,
  PARTICIPANTS: 0x0902,
  FLOW: 0x0a01,
} as const;

export interface Frame {
  flags: number;
  type: number;
  requestId: number;
  body: Uint8Array;
}

export function encode(type: number, body: Uint8Array, flags = 0, requestId = 0): Uint8Array {
  const out = new Uint8Array(10 + body.length);
  const view = new DataView(out.buffer);
  out[0] = 0x46;
  out[1] = 0x57;
  out[2] = 2;
  out[3] = flags;
  view.setUint16(4, type);
  view.setUint32(6, requestId);
  out.set(body, 10);
  return out;
}

/// A message split into frames of at most `chunk` body bytes (MORE on all
/// but the last), as CLIPBOARD (60 KiB) and FILE (65526 B) do.
export function encodeFragments(type: number, body: Uint8Array, chunk: number, flags = 0, requestId = 0): Uint8Array[] {
  if (body.length <= chunk) return [encode(type, body, flags, requestId)];
  const out: Uint8Array[] = [];
  for (let at = 0; at < body.length; at += chunk) {
    const last = at + chunk >= body.length;
    out.push(encode(type, body.subarray(at, at + chunk), flags | (last ? 0 : Flag.MORE), requestId));
  }
  return out;
}

/// Joins MORE fragments per (type, request ID, response), with limits.
export class Reassembly {
  private parts = new Map<string, { chunks: Uint8Array[]; size: number }>();
  private maxBytes: number;
  private maxMessages: number;

  constructor(maxBytes: number, maxMessages = 16) {
    this.maxBytes = maxBytes;
    this.maxMessages = maxMessages;
  }

  /// The whole message once its last fragment arrived.
  push(frame: Frame): Frame | undefined {
    const key = `${frame.type}:${frame.requestId}:${frame.flags & Flag.RESPONSE}`;
    const more = (frame.flags & Flag.MORE) !== 0;
    const open = this.parts.get(key);
    if (!open && !more) return frame;
    const entry = open ?? { chunks: [], size: 0 };
    if (!open) {
      if (this.parts.size >= this.maxMessages) throw new Error('too many fragmented messages');
      this.parts.set(key, entry);
    }
    entry.size += frame.body.length;
    if (entry.size > this.maxBytes) {
      this.parts.delete(key);
      throw new Error('fragmented message too large');
    }
    entry.chunks.push(frame.body.slice());
    if (more) return undefined;
    this.parts.delete(key);
    const body = new Uint8Array(entry.size);
    let at = 0;
    for (const c of entry.chunks) {
      body.set(c, at);
      at += c.length;
    }
    return { ...frame, flags: frame.flags & ~Flag.MORE, body };
  }

  clear() {
    this.parts.clear();
  }
}

export function decode(data: ArrayBuffer): Frame | undefined {
  if (data.byteLength < 10 || data.byteLength > MAX_FRAME) return undefined;
  const bytes = new Uint8Array(data);
  if (bytes[0] !== 0x46 || bytes[1] !== 0x57 || bytes[2] !== 2) return undefined;
  const view = new DataView(data);
  return {
    flags: bytes[3],
    type: view.getUint16(4),
    requestId: view.getUint32(6),
    body: bytes.subarray(10),
  };
}

// HELLO --------------------------------------------------------------------

/// Features this viewer implements, besides the video codecs it decodes
/// (`video.h264`, `video.hevc`, `video.av1`). `clipboard` is text only (no
/// `clipboard.files`). Not declared: display.config (screens arrive on the
/// session channel), drag, ports, diagnostics, fct (viewer brief §4.4.1,
/// control brief §0.2).
export const VIEWER_FEATURES = ['cursor.participants', 'clipboard', 'files', 'touch', 'pen'];

export interface Hello {
  protocolVersion: number;
  minProtocolVersion: number;
  clientVersion: string;
  features: Record<string, number>;
}

/// The peer protocol version (`farwing_protocol::PROTOCOL_VERSION`).
export const PROTOCOL = 3;

export function helloBody(): Uint8Array {
  const w = new Writer().int(1, PROTOCOL).int(2, PROTOCOL).string(4, 'web-0.1.0');
  for (const name of VIEWER_FEATURES) {
    w.message(5, new Writer().string(1, name).int(2, 1));
  }
  return w.finish();
}

export function parseHello(body: Uint8Array): Hello {
  const f = new Fields(body);
  const features: Record<string, number> = {};
  for (const entry of f.messages(5)) features[entry.string(1)] = entry.uint32(2);
  return {
    protocolVersion: f.uint32(1),
    minProtocolVersion: f.uint32(2),
    clientVersion: f.string(4),
    features,
  };
}

export function compatible(hello: Hello): boolean {
  return hello.protocolVersion >= PROTOCOL && hello.minProtocolVersion <= PROTOCOL;
}

/// A `PeerError`: a code and the feature's reason key (worded here).
export function parseError(body: Uint8Array): { code: string; reason: string } {
  const f = new Fields(body);
  return { code: f.string(1), reason: f.string(2) };
}

/// A `PeerError` body (with the ERROR flag).
export function errorBody(code: string, reason = ''): Uint8Array {
  return new Writer().string(1, code).string(2, reason).finish();
}

// SESSION ------------------------------------------------------------------

/// What a display is (`wire.rs` `DisplayKind`).
export const DisplayKind = { PHYSICAL: 0, VIRTUAL: 1, FALLBACK: 2, FOREIGN: 3, EXCLUSIVE: 4 } as const;

/// One of the host's displays (SESSION `Displays`).
export interface Screen {
  id: number;
  kind: number;
  name: string;
  primary: boolean;
  width: number;
  height: number;
  /// Refresh rate; 0 when unknown.
  fps: number;
  /// The `video_N` track carrying this screen; -1 when not captured.
  track: number;
}

export interface Screens {
  revision: number;
  screens: Screen[];
  /// This viewer's display; -1 none.
  current: number;
}

/// What one video track carries (SESSION `StreamState`).
export interface TrackState {
  track: number;
  screen: number;
  /// Wire codec: 1 H.264, 2 HEVC, 3 AV1.
  codec: number;
  /// The tier in effect: 1 low … 4 ultra (automatic resolved), 5 custom.
  quality: number;
  fps: number;
  encoder: string;
  capture: string;
}

/// The stream as the host sends it to this viewer.
export interface StreamState {
  revision: number;
  /// Wire codecs this host can send this viewer.
  codecs: number[];
  tracks: TrackState[];
}

export type SessionMessage =
  | { kind: 'screens'; value: Screens }
  | { kind: 'stream'; value: StreamState }
  | { kind: 'microphone_state'; state: number }
  | { kind: 'forward'; value: Forward }
  | { kind: 'other'; tag: number };

/// Where the picture comes from (`docs/protocol/farwing-protocol.md` §6.7):
/// the server's forwarder, or the host directly again (`room` empty).
export interface Forward {
  generation: number;
  room: string;
  /// `e2ee` or `compatible`.
  mode: string;
  /// This viewer's video tracks and the forwarder tracks they come from.
  tracks: { index: number; track: string }[];
  session: string;
  /// End-to-end mode: the frame key (§6.8).
  key: Uint8Array;
  keyId: number;
  /// The forwarder's sound track; empty: the sound stays direct.
  audio: string;
}

export function parseSession(body: Uint8Array): SessionMessage {
  const f = new Fields(body);
  const screens = f.message(1);
  if (screens) {
    return {
      kind: 'screens',
      value: {
        revision: screens.uint32(1),
        current: screens.int32(3),
        screens: screens.messages(2).map((s) => ({
          id: s.int32(1),
          kind: s.int32(2),
          name: s.string(3),
          primary: s.bool(4),
          width: s.uint32(5),
          height: s.uint32(6),
          fps: s.uint32(7),
          track: s.int32(15),
        })),
      },
    };
  }
  const stream = f.message(2);
  if (stream) {
    return {
      kind: 'stream',
      value: {
        revision: Number(stream.big(1)),
        codecs: stream.message(3)?.bigs(1).map(Number) ?? [],
        tracks: stream.messages(4).map((t) => ({
          track: t.uint32(1),
          screen: t.int32(2),
          codec: t.int32(3),
          quality: t.int32(7),
          fps: t.uint32(10),
          encoder: t.string(12),
          capture: t.string(13),
        })),
      },
    };
  }
  const state = f.message(3);
  if (state) return { kind: 'microphone_state', state: state.int32(1) };
  const forward = f.message(24);
  if (forward) {
    return {
      kind: 'forward',
      value: {
        generation: forward.uint32(1),
        room: forward.string(2),
        mode: forward.string(3),
        tracks: forward.messages(4).map((t) => ({ index: t.uint32(1), track: t.string(2) })),
        session: forward.string(5),
        key: forward.bytes(6),
        keyId: forward.uint32(7),
        audio: forward.string(8),
      },
    };
  }
  if (f.has(4)) return { kind: 'other', tag: 4 };
  return { kind: 'other', tag: 0 };
}

/// Whether this viewer receives through the forwarder (a push on the
/// control channel, §6.7).
export function forwardStatus(generation: number, receiving: boolean, error: string): Uint8Array {
  return new Writer().message(25, new Writer().int(1, generation).int(2, receiving).string(3, error)).finish();
}

/// Take or release the control token (a request on the session channel,
/// control brief §1); `automatic` re-takes it after a reconnection; `ask`
/// asks the host's user for control in this connection, without the grant.
export function turnRequest(take: boolean, automatic = false, ask = false): Uint8Array {
  return new Writer().message(17, new Writer().int(1, take).int(2, automatic).int(3, ask)).finish();
}

/// Turn the host's virtual microphone on or off (control brief §5.2).
export function microphonePolicy(on: boolean): Uint8Array {
  return new Writer().message(16, new Writer().int(1, on ? 1 : 0)).finish();
}

export const MicrophoneState = { ACTIVE: 1, INACTIVE: 2, RESTART: 3 } as const;

/// Why the host refused the microphone (`src/protocol/microphone.rs`).
export function microphoneError(reason: string): string {
  switch (reason) {
    case 'component_unavailable':
      return t().viewer.microphone.noDriver;
    case 'not_allowed':
      return t().viewer.microphone.noPermission;
    default:
      return t().viewer.microphone.startFailed;
  }
}

/// The newest frame a video track decoded (`VideoFeedback` with only the
/// SSRC and the RTP timestamp; `docs/protocol/rfi-ltr-brief.md` §4.1): the
/// host works out which long-term references that frame's chain holds.
export function videoAcknowledgement(ssrc: number, rtpTimestamp: number): Uint8Array {
  return new Writer().message(8, new Writer().fixed32Always(1, ssrc).fixed32Always(7, rtpTimestamp)).finish();
}

/// Start (or stop) capturing a screen; pushed, never answered (§4.6).
/// SESSION `Capture` (a request): start or stop capturing a screen, on
/// `track` in place of what it shows.
export function captureControl(screenId: number, start: boolean, track?: number): Uint8Array {
  const capture = new Writer().int(1, screenId).int(2, start);
  if (track !== undefined) capture.intAlways(3, track);
  return new Writer().message(5, capture).finish();
}

/// A change of this viewer's stream (SESSION `StreamRequest`); absent
/// fields stay. `quality` is the wire tier (0 automatic, 1 low … 4 ultra),
/// `codec` the wire codec (0: the host chooses).
export interface StreamChange {
  quality?: number;
  fps?: number;
  codec?: number;
  cursor?: boolean;
}

export function streamRequest(change: StreamChange): Uint8Array {
  const inner = new Writer();
  if (change.quality !== undefined) inner.intAlways(1, change.quality);
  if (change.fps !== undefined) inner.intAlways(3, change.fps);
  if (change.codec !== undefined) inner.intAlways(6, change.codec);
  if (change.cursor !== undefined) inner.intAlways(7, change.cursor ? 1 : 0);
  return new Writer().message(10, inner).finish();
}

// CURSOR / PARTICIPANTS ----------------------------------------------------

export interface CursorShape {
  hotX: number;
  hotY: number;
  width: number;
  height: number;
  png: Uint8Array;
  hidden: boolean;
  x: number;
  y: number;
  screenId: number;
}

export interface ParticipantPointer {
  id: string;
  screenId: number;
  x: number;
  y: number;
  visible: boolean;
  idleMs: number;
  click?: { button: number; phase: number };
  clickSeq: number;
}

export type CursorMessage =
  { kind: 'shape'; shape: CursorShape } | { kind: 'pointers'; pointers: ParticipantPointer[] };

export function parseCursor(body: Uint8Array): CursorMessage | undefined {
  const f = new Fields(body);
  if (f.bool(3)) {
    return {
      kind: 'pointers',
      pointers: f.messages(2).map((p) => {
        const click = p.message(7);
        return {
          id: p.uint64(1),
          screenId: p.int32(2),
          x: p.float(3),
          y: p.float(4),
          visible: p.bool(5),
          idleMs: p.uint32(6),
          click: click ? { button: click.int32(1), phase: click.int32(2) } : undefined,
          clickSeq: p.uint32(8),
        };
      }),
    };
  }
  const s = f.message(1);
  if (!s) return undefined;
  return {
    kind: 'shape',
    shape: {
      hotX: s.int32(1),
      hotY: s.int32(2),
      width: s.int32(3),
      height: s.int32(4),
      png: s.bytes(5),
      hidden: s.int32(6) === -1,
      x: s.double(7),
      y: s.double(8),
      screenId: s.int32(9),
    },
  };
}

export interface Participant {
  id: string;
  nickname: string;
  deviceName: string;
  relation: string;
  color: number;
  control: boolean;
  you: boolean;
  joinedMs: number;
  /// Only hears the host (audio only).
  listening: boolean;
}

export function parseParticipants(body: Uint8Array): { participants: Participant[]; controlling: string } {
  const f = new Fields(body);
  return {
    controlling: f.uint64(2),
    participants: f.messages(1).map((p) => ({
      id: p.uint64(1),
      nickname: p.string(2),
      deviceName: p.string(3),
      relation: p.string(4),
      color: p.fixed32(5),
      control: p.bool(6),
      you: p.bool(7),
      joinedMs: p.int64(8),
      listening: p.bool(9),
    })),
  };
}

/// This viewer's indicator pointer: shown to the others, never injected.
export function pointer(
  seq: number,
  screenId: number,
  x: number,
  y: number,
  visible: boolean,
  click?: { button: number; phase: number },
): Uint8Array {
  const w = new Writer().int(1, seq).int(2, screenId).float(3, x).float(4, y).int(5, visible);
  if (click) w.message(6, new Writer().int(1, click.button).int(2, click.phase));
  return w.finish();
}

// TRANSFER / FLOW (bulk channel) -------------------------------------------

/// Largest data of one TRANSFER frame, and a receiver's initial credit.
export const TRANSFER_CHUNK = 64 * 1024 - 64;
export const INITIAL_CREDIT = 4 * 1024 * 1024;
export const TransferKind = { FILE: 1, CLIPBOARD: 2 } as const;

export interface Chunk {
  stream: number;
  kind: number;
  offset: bigint;
  data: Uint8Array;
  last: boolean;
  entry: number;
}

export function chunk(c: Chunk): Uint8Array {
  return new Writer()
    .int(1, c.stream)
    .int(2, c.kind)
    .int(3, c.offset)
    .raw(4, c.data)
    .int(5, c.last)
    .int(6, c.entry)
    .finish();
}

export function parseChunk(body: Uint8Array): Chunk {
  const f = new Fields(body);
  return {
    stream: f.uint32(1),
    kind: f.int32(2),
    offset: f.big(3),
    data: f.bytes(4),
    last: f.bool(5),
    entry: f.uint32(6),
  };
}

export interface FlowCredit {
  stream: number;
  credit: bigint;
  kind: number;
}

/// FLOW on the bulk channel (channel 1): `credit` bytes in total may be sent.
export function flow(c: FlowCredit): Uint8Array {
  return new Writer().int(1, 1).int(2, c.stream).int(3, c.credit).int(4, c.kind).finish();
}

export function parseFlow(body: Uint8Array): FlowCredit & { channel: number } {
  const f = new Fields(body);
  return { channel: f.int32(1), stream: f.uint32(2), credit: f.big(3), kind: f.int32(4) };
}

// PERMISSIONS / HOST_EVENT / AUDIO -----------------------------------------

export function parsePermissions(body: Uint8Array): string[] {
  return new Fields(body).strings(1);
}

export interface HostEvent {
  kind: number;
  /** DISPLAY_CHANGING: who changed the displays (nickname; may be empty). */
  actor: string;
}

export const HostEventKind = {
  UPDATING: 1,
  RESTARTING: 2,
  SLEEPING: 3,
  SHUTTING_DOWN: 4,
  SESSION_SWITCH: 5,
  DISPLAY_CHANGING: 6,
} as const;

export function parseHostEvent(body: Uint8Array): HostEvent {
  const f = new Fields(body);
  return { kind: f.int32(1), actor: f.string(3) };
}

/// The host's desktop-audio rate (AUDIO `AudioState`).
export interface AudioState {
  revision: number;
  kbps: number;
  effectiveBps: number;
}

/// An AUDIO push or answer; `undefined` for anything else.
export function parseAudio(body: Uint8Array): AudioState | undefined {
  const state = new Fields(body).message(1);
  if (!state) return undefined;
  return { revision: Number(state.big(1)), kbps: state.uint32(2), effectiveBps: state.uint32(3) };
}

/// A rate for this viewer (AUDIO `AudioRequest`).
export function audioRequest(kbps: number): Uint8Array {
  return new Writer().message(2, new Writer().int(1, kbps)).finish();
}
