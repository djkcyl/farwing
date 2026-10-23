// Frames of end-to-end encrypted forwarding, decrypted as the native viewer
// does (`src/media/frame_crypto.rs`, `docs/protocol/farwing-protocol.md`
// §6.8): `clear ‖ AES-128-GCM(rest) ‖ tag(16) ‖ counter(8) ‖ key ID(1) ‖
// 0xFE` per unit, the clear part authenticated; H.264 and H.265 units
// escaped, AV1 units with their new size.

export type FrameCodec = 'h264' | 'h265' | 'av1';
type Bytes = Uint8Array<ArrayBuffer>;

export const KEY_BYTES = 20;
/// Slice bytes after the NAL header left in the clear.
const CLEAR_SLICE = 8;
const TRAILER = 16 + 8 + 1 + 1;
const MARK = 0xfe;

export interface FrameKey {
  id: number;
  key: CryptoKey;
  salt: Bytes;
}

export async function importFrameKey(id: number, material: Uint8Array): Promise<FrameKey> {
  if (material.length !== KEY_BYTES) throw new Error(`frame key has ${material.length} bytes`);
  const bytes = new Uint8Array(material);
  const key = await crypto.subtle.importKey('raw', bytes.slice(0, 16), 'AES-GCM', false, ['decrypt']);
  return { id, key, salt: bytes.slice(16) };
}

/// The current and the previous key.
export class FrameDecryptor {
  private keys: FrameKey[] = [];

  /// Accept `key` (newest last); the two newest are kept.
  add(key: FrameKey) {
    this.keys = this.keys.filter((k) => k.id !== key.id);
    this.keys.push(key);
    if (this.keys.length > 2) this.keys.shift();
  }

  /// `frame` as the host encoded it.
  async decrypt(codec: FrameCodec, frame: Bytes): Promise<Bytes> {
    const parts: Uint8Array[] = [];
    if (codec === 'av1') {
      for (const obu of obus(frame)) {
        if ((obu.kind !== 4 && obu.kind !== 6) || !obu.payload.length) {
          parts.push(obu.bytes);
          continue;
        }
        const plain = await this.open(obu.header, obu.payload);
        parts.push(obu.header, leb(plain.length), plain);
      }
      return concat(parts);
    }
    const header = codec === 'h264' ? 1 : 2;
    for (const unit of annexBUnits(frame)) {
      parts.push(START_CODE);
      if (!encryptedNal(codec, unit)) {
        parts.push(unit);
        continue;
      }
      const clear = unit.subarray(0, header + CLEAR_SLICE);
      parts.push(clear, await this.open(clear, unescape(clear, unit.subarray(header + CLEAR_SLICE))));
    }
    return concat(parts);
  }

  private async open(clear: Bytes, sealed: Bytes): Promise<Bytes> {
    if (sealed.length < TRAILER || sealed[sealed.length - 1] !== MARK) throw new Error('not an encrypted unit');
    const body = sealed.length - TRAILER;
    const id = sealed[body + 24];
    const key = this.keys.find((k) => k.id === id);
    if (!key) throw new Error(`no frame key ${id}`);
    const iv = new Uint8Array(12);
    iv.set(key.salt, 0);
    iv.set(sealed.subarray(body + 16, body + 24), 4);
    // WebCrypto takes the tag after the ciphertext.
    const plain = await crypto.subtle.decrypt(
      { name: 'AES-GCM', iv, additionalData: clear, tagLength: 128 },
      key.key,
      sealed.subarray(0, body + 16),
    );
    return new Uint8Array(plain);
  }
}

const START_CODE = new Uint8Array([0, 0, 0, 1]);

/// Slices long enough to hide anything: H.264 types 1–5, H.265 VCL types.
function encryptedNal(codec: FrameCodec, unit: Bytes): boolean {
  const header = codec === 'h264' ? 1 : 2;
  if (unit.length <= header + CLEAR_SLICE) return false;
  if (codec === 'h264') {
    const type = unit[0] & 31;
    return type >= 1 && type <= 5;
  }
  return ((unit[0] >> 1) & 63) < 32;
}

/// NAL units of an Annex B stream, without start codes and trailing zeros.
export function annexBUnits(data: Bytes): Bytes[] {
  const marks: [number, number][] = [];
  let index = 0;
  while (index + 3 <= data.length) {
    if (data[index] === 0 && data[index + 1] === 0 && data[index + 2] === 1) {
      marks.push([index, 3]);
      index += 3;
    } else if (
      index + 4 <= data.length &&
      data[index] === 0 &&
      data[index + 1] === 0 &&
      data[index + 2] === 0 &&
      data[index + 3] === 1
    ) {
      marks.push([index, 4]);
      index += 4;
    } else {
      index += 1;
    }
  }
  const units: Bytes[] = [];
  marks.forEach(([start, prefix], position) => {
    const end = position + 1 < marks.length ? marks[position + 1][0] : data.length;
    let last = end;
    while (last > start + prefix && data[last - 1] === 0) last -= 1;
    if (last > start + prefix) units.push(data.subarray(start + prefix, last));
  });
  return units;
}

/// Removes the escaping that keeps start codes out of encrypted units.
function unescape(before: Bytes, data: Bytes): Bytes {
  const out = new Uint8Array(data.length);
  let length = 0;
  let zeros = 0;
  for (let i = before.length - 1; i >= 0 && before[i] === 0 && zeros < 2; i--) zeros += 1;
  for (const byte of data) {
    if (zeros >= 2 && byte === 3) {
      zeros = 0;
      continue;
    }
    out[length++] = byte;
    zeros = byte === 0 ? zeros + 1 : 0;
  }
  return out.slice(0, length);
}

interface Obu {
  kind: number;
  /// Header and extension, without the size.
  header: Bytes;
  payload: Bytes;
  /// The whole OBU.
  bytes: Bytes;
}

function obus(data: Bytes): Obu[] {
  const out: Obu[] = [];
  let at = 0;
  while (at < data.length) {
    const start = at;
    const first = data[at];
    if (first & 0x80 || !(first & 2)) throw new Error('AV1 OBU without size field');
    const headerEnd = at + 1 + (first & 4 ? 1 : 0);
    let size = 0;
    let shift = 0;
    let sizeAt = headerEnd;
    for (;;) {
      if (sizeAt >= data.length || shift > 49) throw new Error('AV1 OBU truncated');
      const byte = data[sizeAt++];
      size += (byte & 127) * 2 ** shift;
      shift += 7;
      if (!(byte & 128)) break;
    }
    const end = sizeAt + size;
    if (end > data.length) throw new Error('AV1 OBU truncated');
    out.push({
      kind: (first >> 3) & 15,
      header: data.subarray(start, headerEnd),
      payload: data.subarray(sizeAt, end),
      bytes: data.subarray(start, end),
    });
    at = end;
  }
  return out;
}

function leb(value: number): Uint8Array {
  const out: number[] = [];
  while (value >= 128) {
    out.push((value % 128) | 128);
    value = Math.floor(value / 128);
  }
  out.push(value);
  return new Uint8Array(out);
}

function concat(parts: Uint8Array[]): Bytes {
  const out = new Uint8Array(parts.reduce((n, p) => n + p.length, 0));
  let at = 0;
  for (const part of parts) {
    out.set(part, at);
    at += part.length;
  }
  return out;
}
