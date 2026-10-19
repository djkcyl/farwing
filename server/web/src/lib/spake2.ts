// SPAKE2 for visitor access codes, byte-compatible with the Rust `spake2`
// 0.4 crate as `farwing_protocol::crypto` uses it: Ed25519 group, symmetric
// mode, identity `farwing-assist-v2` (`docs/web-console.md` §7.3). The code
// never leaves the browser; only the blinded messages do.
import { ed25519 } from '@noble/curves/ed25519.js';
import { hkdf } from '@noble/hashes/hkdf.js';
import { hmac } from '@noble/hashes/hmac.js';
import { sha256 } from '@noble/hashes/sha2.js';
import { b64, unb64, utf8 } from './encoding';

const Point = ed25519.Point;
type Point = InstanceType<typeof Point>;
const L = Point.Fn.ORDER;

export const ASSIST_PAKE_IDENTITY = utf8('farwing-assist-v2');
/// Messages a controller always sends: hosts may hold a random code and a
/// custom password in two slots, and a controller cannot tell which.
export const ASSIST_PAKE_SLOTS = 2;

const hexBytes = (h: string) => Uint8Array.from(h.match(/../g)!.map((x) => parseInt(x, 16)));

/// The symmetric blinding point of the crate's Ed25519 group (`const_s`).
const S = Point.fromBytes(hexBytes('6f00dae87c1be1a73b5922ef431cd8f57879569c222d22b1cd71e8546ab8e6f1'));

function bigEndian(bytes: Uint8Array): bigint {
  let n = 0n;
  for (const b of bytes) n = (n << 8n) | BigInt(b);
  return n;
}

function mod(n: bigint): bigint {
  const r = n % L;
  return r < 0n ? r + L : r;
}

/// `ed25519_hash_to_scalar`: HKDF-SHA256(salt "", info "SPAKE2 pw") to 48
/// bytes, read big-endian, reduced mod ℓ.
export function passwordScalar(password: Uint8Array): bigint {
  const okm = hkdf(sha256, password, new Uint8Array(), utf8('SPAKE2 pw'), 48);
  return mod(bigEndian(okm));
}

function randomScalar(): bigint {
  for (;;) {
    const s = mod(bigEndian(crypto.getRandomValues(new Uint8Array(64))));
    if (s !== 0n) return s;
  }
}

function compare(a: Uint8Array, b: Uint8Array): number {
  for (let i = 0; i < Math.min(a.length, b.length); i++) if (a[i] !== b[i]) return a[i] - b[i];
  return a.length - b.length;
}

export class Spake2 {
  private xy: bigint;
  private pw: bigint;
  private password: Uint8Array;
  private identity: Uint8Array;
  /// Our element, without the side byte.
  private mine: Uint8Array;
  /// Our message: `'S'` and the element.
  readonly message: Uint8Array;

  /// `Spake2::start_symmetric`; `xy` is only given by tests.
  constructor(password: Uint8Array, identity: Uint8Array, xy?: bigint) {
    this.password = password;
    this.identity = identity;
    this.pw = passwordScalar(password);
    this.xy = xy ?? randomScalar();
    let element = Point.BASE.multiply(this.xy);
    if (this.pw !== 0n) element = element.add(S.multiply(this.pw));
    this.mine = element.toBytes();
    this.message = new Uint8Array(33);
    this.message[0] = 0x53;
    this.message.set(this.mine, 1);
  }

  /// The shared key from the peer's message (`Spake2::finish`). A wrong code
  /// still yields a key; key confirmation tells.
  finish(peer: Uint8Array): Uint8Array {
    if (peer.length !== 33) throw new Error('PAKE message has the wrong length');
    if (peer[0] !== 0x53) throw new Error('PAKE message from the wrong side');
    const theirs = peer.subarray(1);
    let element: Point = Point.fromBytes(theirs);
    if (this.pw !== 0n) element = element.add(S.multiply(L - this.pw));
    const key = element.multiply(this.xy).toBytes();
    const transcript = new Uint8Array(160);
    transcript.set(sha256(this.password), 0);
    transcript.set(sha256(this.identity), 32);
    const [first, second] = compare(this.mine, theirs) < 0 ? [this.mine, theirs] : [theirs, this.mine];
    transcript.set(first, 64);
    transcript.set(second, 96);
    transcript.set(key, 128);
    return sha256(transcript);
  }
}

/// `pake_start`: a state and the base64url message for the host.
export function pakeStart(code: string): { state: Spake2; message: string } {
  const state = new Spake2(utf8(code), ASSIST_PAKE_IDENTITY);
  return { state, message: b64(state.message) };
}

/// `key_confirm`: HMAC-SHA256(key, "FWAK2\n{label}\n{request_id}").
export function keyConfirm(key: Uint8Array, label: 'host' | 'controller', requestId: string): string {
  return b64(hmac(sha256, key, utf8(`FWAK2\n${label}\n${requestId}`)));
}

export function keyConfirmMatches(
  key: Uint8Array,
  label: 'host' | 'controller',
  requestId: string,
  received: string,
): boolean {
  let theirs: Uint8Array;
  try {
    theirs = unb64(received);
  } catch {
    return false;
  }
  const ours = hmac(sha256, key, utf8(`FWAK2\n${label}\n${requestId}`));
  if (theirs.length !== ours.length) return false;
  let diff = 0;
  for (let i = 0; i < ours.length; i++) diff |= ours[i] ^ theirs[i];
  return diff === 0;
}

/// `pake_fingerprint_binding`: the `a=x-farwing-pake` value binding the DTLS
/// fingerprints of a visitor session to the PAKE key (attempt left empty).
export function pakeBinding(
  key: Uint8Array,
  sessionId: string,
  role: 'host' | 'controller',
  fingerprints: string[],
): string {
  return b64(hmac(sha256, key, utf8(`FWDTLS2\n${sessionId}\n\n${role}\n${fingerprints.join(',')}`)));
}
