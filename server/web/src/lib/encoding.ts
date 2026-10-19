// Byte helpers shared by the device identity, the API client and the peer
// protocol. base64url without padding matches the server
// (`farwing_protocol::crypto::b64`).

export function b64(bytes: Uint8Array): string {
  let text = '';
  for (const byte of bytes) text += String.fromCharCode(byte);
  return btoa(text).replace(/\+/g, '-').replace(/\//g, '_').replace(/=+$/, '');
}

export function unb64(text: string): Uint8Array {
  const normal = text.trim().replace(/-/g, '+').replace(/_/g, '/');
  const padded = normal + '='.repeat((4 - (normal.length % 4)) % 4);
  const raw = atob(padded);
  const out = new Uint8Array(raw.length);
  for (let i = 0; i < raw.length; i++) out[i] = raw.charCodeAt(i);
  return out;
}

export function hex(bytes: Uint8Array): string {
  return Array.from(bytes, (b) => b.toString(16).padStart(2, '0')).join('');
}

export function utf8(text: string): Uint8Array {
  return new TextEncoder().encode(text);
}

export async function sha256(bytes: Uint8Array): Promise<Uint8Array> {
  return new Uint8Array(await crypto.subtle.digest('SHA-256', bytes as BufferSource));
}

/// The device ID of a public key: the first 16 characters of the lower-case
/// base32 (a-z2-7) SHA-256 of the key (`farwing_protocol::crypto::local_device_id`).
export async function deviceIdOf(publicKey: Uint8Array): Promise<string> {
  const alphabet = 'abcdefghijklmnopqrstuvwxyz234567';
  const digest = await sha256(publicKey);
  let out = '';
  let buffer = 0;
  let bits = 0;
  for (const byte of digest) {
    buffer = ((buffer << 8) | byte) >>> 0;
    bits += 8;
    while (bits >= 5 && out.length < 16) {
      bits -= 5;
      out += alphabet[(buffer >>> bits) & 31];
    }
    buffer &= (1 << bits) - 1;
    if (out.length === 16) break;
  }
  return out;
}

/// A device ID for display: four groups of four.
export function deviceIdText(id: string): string {
  return id.length === 16 ? id.match(/.{4}/g)!.join(' ') : id;
}
