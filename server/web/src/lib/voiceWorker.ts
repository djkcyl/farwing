// The encoded transforms of voice channels (`docs/protocol/chat-design.md`
// §10.2): this browser's microphone and camera frames are sealed with its
// own key; each received track is opened with its sender's keys.
import { FrameDecryptor, FrameEncryptor, importFrameKey, type FrameCodec } from './frameCrypto';

/// This device's key (`own`), or a sender's (`participant`).
export type VoiceWorkerMessage =
  { own: { id: number; material: Uint8Array } } | { participant: number; key: { id: number; material: Uint8Array } };

type EncodedFrame = RTCEncodedAudioFrame | RTCEncodedVideoFrame;

interface Transformer {
  readable: ReadableStream<EncodedFrame>;
  writable: WritableStream<EncodedFrame>;
  /// `send`, or `receive` with the sender's number; `video` for pictures.
  options?: { kind?: string; participant?: number; video?: boolean };
  sendKeyFrameRequest?(): Promise<void>;
}

const MIME: Record<string, FrameCodec> = { 'video/h264': 'h264', 'video/h265': 'h265', 'video/av1': 'av1' };

const scope = self as unknown as {
  onmessage: ((event: MessageEvent<VoiceWorkerMessage>) => void) | null;
  onrtctransform: ((event: { transformer: Transformer }) => void) | null;
};

let encryptor: FrameEncryptor | undefined;
const decryptors = new Map<number, FrameDecryptor>();
/// Keys are imported in order; frames wait for the import in progress.
let keys: Promise<void> = Promise.resolve();

function decryptor(participant: number): FrameDecryptor {
  let d = decryptors.get(participant);
  if (!d) {
    d = new FrameDecryptor();
    decryptors.set(participant, d);
  }
  return d;
}

scope.onmessage = (event) => {
  const message = event.data;
  if ('own' in message) {
    const { id, material } = message.own;
    keys = keys
      .then(async () => {
        const key = await importFrameKey(id, material, true);
        if (encryptor) encryptor.setKey(key);
        else encryptor = new FrameEncryptor(key);
      })
      .catch(() => undefined);
    return;
  }
  const { participant, key } = message;
  keys = keys
    .then(async () => decryptor(participant).add(await importFrameKey(key.id, key.material)))
    .catch(() => undefined);
};

scope.onrtctransform = ({ transformer }) => {
  const send = transformer.options?.kind === 'send';
  const video = !!transformer.options?.video;
  const participant = transformer.options?.participant ?? 0;
  let asked = -Infinity;
  void transformer.readable
    .pipeThrough(
      new TransformStream<EncodedFrame, EncodedFrame>({
        async transform(frame, controller) {
          await keys;
          const meta = frame.getMetadata() as { mimeType?: string };
          const codec: FrameCodec | undefined = video ? MIME[meta.mimeType?.toLowerCase() ?? 'video/h264'] : 'opus';
          try {
            if (!codec) throw new Error('codec');
            const data = new Uint8Array(frame.data);
            if (send) {
              // Nothing goes out unsealed.
              if (!encryptor) return;
              frame.data = (
                codec === 'opus'
                  ? await encryptor.encryptOpus(data)
                  : codec === 'av1'
                    ? (() => {
                        throw new Error('AV1 is not sent');
                      })()
                    : await encryptor.encryptAnnexB(codec, data)
              ).buffer;
            } else {
              frame.data = (await decryptor(participant).decrypt(codec, data)).buffer;
            }
            controller.enqueue(frame);
          } catch {
            // A key not here yet, or an altered frame: dropped; a picture
            // starts again with the next keyframe.
            if (video && !send && performance.now() - asked >= 1000) {
              asked = performance.now();
              void transformer.sendKeyFrameRequest?.().catch(() => undefined);
            }
          }
        },
      }),
    )
    .pipeTo(transformer.writable);
};
