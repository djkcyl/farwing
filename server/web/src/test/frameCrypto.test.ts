// End-to-end encrypted frames: the vectors of `src/media/frame_crypto.rs`
// (`known_vectors`), encrypted by the host, decrypt to the same frames here.
import { describe, expect, test } from 'vitest';
import { hex } from '../lib/encoding';
import { FrameDecryptor, importFrameKey, type FrameCodec } from '../lib/frameCrypto';

const bytes = (text: string) => new Uint8Array(text.match(/../g)!.map((b) => parseInt(b, 16)));

const SLICE = Array.from({ length: 40 }, (_, i) => (i * 37) & 255);
const FRAMES: [FrameCodec, number[], string][] = [
  [
    'h264',
    [0, 0, 0, 1, 0x67, 0x42, 0xc0, 0x1f, 0, 0, 0, 1, 0x65, ...SLICE],
    '000000016742c01f000000016500254a6f94b9de0364edec74a6515bd1d4b30df60eaeb7ab598473def5235b028a47ddefcfc27980e1b382233bbe126eb1168baf42e6023e000003000003000003002a03fe',
  ],
  [
    'h265',
    [0, 0, 0, 1, 0x40, 0x01, 0x0c, 0x01, 0, 0, 0, 1, 0x26, 0x01, ...SLICE],
    '0000000140010c0100000001260100254a6f94b9de0364edec74a6515bd1d4b30df60eaeb7ab598473def5235b028a47ddefcfc279809172c12615cd47a922cac65a105864f3000003000003000003002a03fe',
  ],
  [
    'av1',
    [0x12, 0x00, 0x0a, 0x03, 0x00, 0x00, 0x00, 0x32, 40, ...SLICE],
    '12000a0300000032424c85d48c8e0983f9ac8be5de56469fd3716c2b861d1b232a521ff507f7aa5158f9430f863a67beccab52cf104a7c268e7254887dec023087000000000000002a03fe',
  ],
  [
    'opus',
    [0x78, ...SLICE],
    '784c85d48c8e0983f9ac8be5de56469fd3716c2b861d1b232a521ff507f7aa5158f9430f863a67becc375db3177f6f76fb2798de01c904d44b000000000000002a03fe',
  ],
];

const material = () => new Uint8Array(Array.from({ length: 20 }, (_, i) => i));

describe('frame decryption', () => {
  test('frames the host encrypted', async () => {
    const decryptor = new FrameDecryptor();
    decryptor.add(await importFrameKey(3, material()));
    for (const [codec, frame, sealed] of FRAMES) {
      expect(hex(await decryptor.decrypt(codec, bytes(sealed)))).toBe(hex(new Uint8Array(frame)));
    }
  });

  test('the previous key still opens frames; older ones do not', async () => {
    const decryptor = new FrameDecryptor();
    decryptor.add(await importFrameKey(3, material()));
    decryptor.add(await importFrameKey(4, new Uint8Array(20).fill(9)));
    const [codec, , sealed] = FRAMES[0];
    await expect(decryptor.decrypt(codec, bytes(sealed))).resolves.toBeDefined();
    decryptor.add(await importFrameKey(5, new Uint8Array(20).fill(7)));
    await expect(decryptor.decrypt(codec, bytes(sealed))).rejects.toThrow('no frame key 3');
  });

  test('altered frames are refused', async () => {
    const decryptor = new FrameDecryptor();
    decryptor.add(await importFrameKey(3, material()));
    const [codec, , sealed] = FRAMES[2];
    const altered = bytes(sealed);
    altered[8] ^= 1;
    await expect(decryptor.decrypt(codec, altered)).rejects.toThrow();
    // The clear header is authenticated too.
    const header = bytes(FRAMES[0][2]);
    header[14] ^= 1;
    await expect(decryptor.decrypt('h264', header)).rejects.toThrow();
  });
});
