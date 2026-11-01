// The encoded transforms of voice (`docs/protocol/chat-design.md` §10.2):
// this browser's microphone frames are sealed with its own key; each
// received track is opened with its sender's keys.
import { FrameDecryptor, FrameEncryptor, importFrameKey } from './frameCrypto';

/// This device's key (`own`), or a sender's (`participant`).
export type VoiceWorkerMessage =
  { own: { id: number; material: Uint8Array } } | { participant: number; key: { id: number; material: Uint8Array } };

interface Transformer {
  readable: ReadableStream<RTCEncodedAudioFrame>;
  writable: WritableStream<RTCEncodedAudioFrame>;
  /// `send`, or `receive` with the sender's number.
  options?: { kind?: string; participant?: number };
}

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
  const participant = transformer.options?.participant ?? 0;
  void transformer.readable
    .pipeThrough(
      new TransformStream<RTCEncodedAudioFrame, RTCEncodedAudioFrame>({
        async transform(frame, controller) {
          await keys;
          try {
            if (send) {
              // Nothing goes out unsealed.
              if (!encryptor) return;
              frame.data = (await encryptor.encryptOpus(new Uint8Array(frame.data))).buffer;
            } else {
              frame.data = (await decryptor(participant).decrypt('opus', new Uint8Array(frame.data))).buffer;
            }
            controller.enqueue(frame);
          } catch {
            // A key not here yet, or an altered frame: dropped.
          }
        },
      }),
    )
    .pipeTo(transformer.writable);
};
