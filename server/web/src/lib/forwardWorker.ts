// The encoded transform of an end-to-end encrypted forwarded picture: each
// received frame is decrypted before the browser decodes it
// (`docs/protocol/farwing-protocol.md` §6.8). Keys arrive as messages.
import { FrameDecryptor, importFrameKey, type FrameCodec } from './frameCrypto';

/// A frame key, or the offer's payload types and their codecs (for browsers
/// whose frames carry no MIME type).
export type WorkerMessage = { key: { id: number; material: Uint8Array } } | { codecs: Record<number, FrameCodec> };

interface Transformer {
  readable: ReadableStream<RTCEncodedVideoFrame>;
  writable: WritableStream<RTCEncodedVideoFrame>;
  sendKeyFrameRequest?(): Promise<void>;
}

const scope = self as unknown as {
  onmessage: ((event: MessageEvent<WorkerMessage>) => void) | null;
  onrtctransform: ((event: { transformer: Transformer }) => void) | null;
};

const decryptor = new FrameDecryptor();
/// Keys are imported in order; frames wait for the import in progress.
let keys: Promise<void> = Promise.resolve();
let codecs: Record<number, FrameCodec> = {};

scope.onmessage = (event) => {
  const message = event.data;
  if ('codecs' in message) {
    codecs = message.codecs;
    return;
  }
  const { id, material } = message.key;
  keys = keys.then(async () => decryptor.add(await importFrameKey(id, material))).catch(() => undefined);
};

const MIME: Record<string, FrameCodec> = { 'video/h264': 'h264', 'video/h265': 'h265', 'video/av1': 'av1' };

scope.onrtctransform = ({ transformer }) => {
  let asked = -Infinity;
  void transformer.readable
    .pipeThrough(
      new TransformStream<RTCEncodedVideoFrame, RTCEncodedVideoFrame>({
        async transform(frame, controller) {
          const meta = frame.getMetadata() as RTCEncodedVideoFrameMetadata & { mimeType?: string };
          const codec = MIME[meta.mimeType?.toLowerCase() ?? ''] ?? codecs[meta.payloadType ?? -1];
          if (!codec) return;
          await keys;
          try {
            frame.data = (await decryptor.decrypt(codec, new Uint8Array(frame.data))).buffer;
            controller.enqueue(frame);
          } catch {
            // Not decryptable (a key not received yet, or altered): the
            // next keyframe starts the picture again.
            if (performance.now() - asked >= 1000) {
              asked = performance.now();
              void transformer.sendKeyFrameRequest?.().catch(() => undefined);
            }
          }
        },
      }),
    )
    .pipeTo(transformer.writable);
};
