// The picture and sound through the server's forwarder
// (`docs/protocol/farwing-protocol.md` §6.7–6.8): a second, receive-only
// connection to the forwarder, its tracks in place of the host's;
// end-to-end encrypted frames are decrypted in a worker before decoding.
import { call } from './api';
import type { Forward } from './fpp';
import type { FrameCodec } from './frameCrypto';
import type { WorkerMessage } from './forwardWorker';
import { offerDependencyDescriptor, videoCodecPreferences, withoutAv1Level } from './sdp';

/// The forwarder's media must arrive this soon.
const FIRST_FRAME_MS = 10_000;
const GATHER_MS = 3_000;

export interface ForwardContext {
  iceServers: RTCIceServer[];
  relay: boolean;
  /// A visitor's guest token for the server API.
  bearer?: string;
  log(line: string): void;
  /// The answer to an instruction.
  status(generation: number, receiving: boolean, error: string): void;
  /// The forwarded picture and sound and their mode; `undefined`: the
  /// host's again.
  media(video: MediaStreamTrack | undefined, audio: MediaStreamTrack | undefined, mode: string): void;
}

interface Active {
  room: string;
  track: string;
  audio: string;
  session: string;
  mode: string;
  connection: RTCPeerConnection;
  worker?: Worker;
}

export class ForwardReceiver {
  private context: ForwardContext;
  private active?: Active;
  private queue: Promise<void> = Promise.resolve();
  private closed = false;
  /// The newest instruction: older ones still queued or joining are moot.
  private latest?: Forward;

  constructor(context: ForwardContext) {
    this.context = context;
  }

  /// The forwarder connection while the picture comes through it.
  get connection(): RTCPeerConnection | undefined {
    return this.active?.connection;
  }

  follow(instruction: Forward) {
    this.latest = instruction;
    this.queue = this.queue.then(() => this.apply(instruction));
  }

  close() {
    this.closed = true;
    this.leave();
  }

  private async apply(instruction: Forward) {
    if (this.closed || instruction !== this.latest) return;
    if (!instruction.room) {
      if (this.leave()) {
        this.context.log('the host sends the picture directly again');
        this.context.media(undefined, undefined, '');
      }
      return;
    }
    // This viewer shows the host's first track only.
    const wanted = instruction.tracks.find((t) => t.index === 0)?.track ?? '';
    const active = this.active;
    if (active && active.room === instruction.room && active.track === wanted && active.audio === instruction.audio) {
      // A new frame key for the same room.
      if (active.worker && instruction.key.length) {
        const message: WorkerMessage = { key: { id: instruction.keyId, material: instruction.key } };
        active.worker.postMessage(message);
        this.context.log(`new forwarded frame key ${instruction.keyId}`);
      }
      this.context.status(instruction.generation, true, '');
      return;
    }
    if (this.leave()) this.context.media(undefined, undefined, '');
    try {
      if (!wanted) throw new Error('no forwarded track for this picture');
      const { video, audio } = await this.join(instruction, wanted);
      if (this.closed || this.active?.room !== instruction.room) return;
      this.context.media(video, audio, instruction.mode);
      this.context.status(instruction.generation, true, '');
      this.context.log(`receiving through the forwarder room=${instruction.room} mode=${instruction.mode}`);
    } catch (error) {
      const message = (error as Error).message || String(error);
      this.context.log(`cannot receive through the forwarder: ${message}`);
      if (this.leave()) this.context.media(undefined, undefined, '');
      this.context.status(instruction.generation, false, message);
    }
  }

  private async join(
    instruction: Forward,
    track: string,
  ): Promise<{ video: MediaStreamTrack; audio?: MediaStreamTrack }> {
    let worker: Worker | undefined;
    if (instruction.mode === 'e2ee') {
      if (typeof RTCRtpScriptTransform === 'undefined') throw new Error('encoded transform unavailable');
      worker = new Worker(new URL('./forwardWorker.ts', import.meta.url), { type: 'module' });
      const message: WorkerMessage = { key: { id: instruction.keyId, material: instruction.key } };
      worker.postMessage(message);
    } else if (instruction.mode !== 'compatible') {
      throw new Error(`unknown forwarding mode ${instruction.mode}`);
    }
    const connection = new RTCPeerConnection({
      iceServers: this.context.iceServers,
      iceTransportPolicy: this.context.relay ? 'relay' : 'all',
      bundlePolicy: 'max-bundle',
      rtcpMuxPolicy: 'require',
    });
    this.active = {
      room: instruction.room,
      track,
      audio: instruction.audio,
      session: instruction.session,
      mode: instruction.mode,
      connection,
      worker,
    };
    const transceiver = connection.addTransceiver('video', { direction: 'recvonly' });
    const codecs = videoCodecPreferences();
    if (codecs) transceiver.setCodecPreferences(codecs);
    offerDependencyDescriptor(transceiver);
    const sound = instruction.audio ? connection.addTransceiver('audio', { direction: 'recvonly' }) : undefined;
    // Before the offer: Chrome leaves a transform set later out of a
    // receiver whose stream the answer declares.
    if (worker) {
      transceiver.receiver.transform = new RTCRtpScriptTransform(worker, { kind: 'video' });
      if (sound) sound.receiver.transform = new RTCRtpScriptTransform(worker, { kind: 'audio' });
    }
    let audio: MediaStreamTrack | undefined;
    const received = new Promise<MediaStreamTrack>((resolve) => {
      connection.ontrack = (event) => {
        if (event.track.kind === 'audio') audio = event.track;
        else resolve(event.track);
      };
    });
    await connection.setLocalDescription(await connection.createOffer());
    await gathered(connection);
    const offer = connection.localDescription!.sdp;
    if (worker) {
      const message: WorkerMessage = { codecs: payloadCodecs(offer) };
      worker.postMessage(message);
    }
    const subscription = await call<{ answer: string; mode: string }>(
      'POST',
      `/api/v2/sfu/rooms/${encodeURIComponent(instruction.room)}/subscribe`,
      // The forwarder matches fmtp parameters as the host does.
      {
        session: instruction.session,
        offer: withoutAv1Level(offer),
        tracks: instruction.audio ? [track, instruction.audio] : [track],
      },
      { bearer: this.context.bearer, quiet: true },
    );
    if (subscription.mode !== instruction.mode) throw new Error(`the room forwards in ${subscription.mode} mode`);
    await connection.setRemoteDescription({ type: 'answer', sdp: subscription.answer });
    const video = await within(received, FIRST_FRAME_MS);
    await firstFrame(connection, () => this.closed || instruction !== this.latest);
    return { video, audio };
  }

  /// Leave the room; whether there was one.
  private leave(): boolean {
    const active = this.active;
    if (!active) return false;
    this.active = undefined;
    active.connection.close();
    active.worker?.terminate();
    void call(
      'DELETE',
      `/api/v2/sfu/rooms/${encodeURIComponent(active.room)}/subscribe/${encodeURIComponent(active.session)}`,
      undefined,
      { bearer: this.context.bearer, quiet: true },
    ).catch(() => undefined);
    return true;
  }
}

/// The offer with its candidates.
function gathered(connection: RTCPeerConnection): Promise<void> {
  if (connection.iceGatheringState === 'complete') return Promise.resolve();
  return new Promise((resolve) => {
    const timer = window.setTimeout(resolve, GATHER_MS);
    connection.addEventListener('icegatheringstatechange', () => {
      if (connection.iceGatheringState !== 'complete') return;
      window.clearTimeout(timer);
      resolve();
    });
  });
}

function within<T>(promise: Promise<T>, ms: number): Promise<T> {
  return new Promise((resolve, reject) => {
    const timer = window.setTimeout(() => reject(new Error('no media from the forwarder in time')), ms);
    promise.then(
      (value) => {
        window.clearTimeout(timer);
        resolve(value);
      },
      (error) => {
        window.clearTimeout(timer);
        reject(error);
      },
    );
  });
}

/// Resolves once the forwarded picture decodes.
async function firstFrame(connection: RTCPeerConnection, superseded: () => boolean): Promise<void> {
  const deadline = performance.now() + FIRST_FRAME_MS;
  while (performance.now() < deadline) {
    if (superseded()) throw new Error('superseded by a newer instruction');
    if (connection.connectionState === 'failed' || connection.connectionState === 'closed') break;
    const report = await connection.getStats();
    let decoded = 0;
    report.forEach((s) => {
      if (s.type === 'inbound-rtp' && s.kind === 'video') decoded = Math.max(decoded, s.framesDecoded ?? 0);
    });
    if (decoded > 0) return;
    await new Promise((resolve) => window.setTimeout(resolve, 200));
  }
  throw new Error('no media from the forwarder in time');
}

/// The offer's video payload types and their codecs.
function payloadCodecs(sdp: string): Record<number, FrameCodec> {
  const names: Record<string, FrameCodec> = { h264: 'h264', h265: 'h265', av1: 'av1' };
  const out: Record<number, FrameCodec> = {};
  for (const match of sdp.matchAll(/^a=rtpmap:(\d+) ([^/\s]+)\//gm)) {
    const codec = names[match[2].toLowerCase()];
    if (codec) out[Number(match[1])] = codec;
  }
  return out;
}
