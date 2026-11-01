// Frame keys between devices (`docs/protocol/chat-design.md` §10), as the
// native client does (`src/features/voice/keys.rs`): this browser's X25519
// key (kept in IndexedDB, registered once), and frame keys sealed to each
// other device — a one-time X25519 key, HKDF-SHA256 (salt `FWVOICE1`, info
// one-time public ‖ recipient public) to an AES-256-GCM key and nonce, the
// scope as associated data — signed with this device's Ed25519 key.
import { x25519 } from '@noble/curves/ed25519.js';
import { hkdf } from '@noble/hashes/hkdf.js';
import { sha256 } from '@noble/hashes/sha2.js';
import { call } from './api';
import { keyBoxMessage, type VoiceKeyPush } from './chat/types';
import { deviceKey, verify } from './device';
import { b64, unb64, utf8 } from './encoding';

const DB = 'farwing';
const STORE = 'device';
const KEY = 'x25519';

function open(): Promise<IDBDatabase> {
  return new Promise((resolve, reject) => {
    const request = indexedDB.open(DB, 1);
    request.onupgradeneeded = () => request.result.createObjectStore(STORE);
    request.onsuccess = () => resolve(request.result);
    request.onerror = () => reject(request.error);
  });
}

async function stored(): Promise<Uint8Array> {
  const db = await open();
  const found = await new Promise<Uint8Array | undefined>((resolve, reject) => {
    const request = db.transaction(STORE).objectStore(STORE).get(KEY);
    request.onsuccess = () => resolve(request.result as Uint8Array | undefined);
    request.onerror = () => reject(request.error);
  });
  if (found) return found;
  const secret = crypto.getRandomValues(new Uint8Array(32));
  await new Promise<void>((resolve, reject) => {
    const tx = db.transaction(STORE, 'readwrite');
    tx.objectStore(STORE).put(secret, KEY);
    tx.oncomplete = () => resolve();
    tx.onerror = () => reject(tx.error);
  });
  return secret;
}

let registered: Promise<Uint8Array> | undefined;

/// This browser's X25519 secret, registered with the server once.
export function voiceSecret(): Promise<Uint8Array> {
  registered ??= (async () => {
    const secret = await stored();
    const pub = b64(x25519.getPublicKey(secret));
    const key = await deviceKey();
    const signature = await key.sign(utf8(`FWX25519\n${pub}`));
    await call('PUT', '/api/v2/devices/self/x25519', { public: pub, signature });
    return secret;
  })().catch((e) => {
    registered = undefined;
    throw e;
  });
  return registered;
}

function derive(shared: Uint8Array, ephemeral: Uint8Array, recipient: Uint8Array) {
  const info = new Uint8Array(64);
  info.set(ephemeral, 0);
  info.set(recipient, 32);
  const okm = hkdf(sha256, shared, utf8('FWVOICE1'), info, 44);
  return { key: okm.slice(0, 32), nonce: okm.slice(32) };
}

async function aes(key: Uint8Array, usage: 'encrypt' | 'decrypt') {
  return crypto.subtle.importKey('raw', key as BufferSource, 'AES-GCM', false, [usage]);
}

export interface KeyBox {
  device_id: string;
  ephemeral: string;
  data: string;
  signature: string;
}

/// `frameKey` (ID `keyId`) of `participant` sealed to device `device` with
/// X25519 key `x25519Key`.
export async function seal(
  scope: string,
  participant: number,
  device: string,
  x25519Key: string,
  keyId: number,
  frameKey: Uint8Array,
): Promise<KeyBox> {
  const recipient = unb64(x25519Key);
  const eph = crypto.getRandomValues(new Uint8Array(32));
  const ephPub = x25519.getPublicKey(eph);
  const { key, nonce } = derive(x25519.getSharedSecret(eph, recipient), ephPub, recipient);
  const plain = new Uint8Array(frameKey.length + 5);
  plain.set(frameKey, 0);
  plain[frameKey.length] = keyId;
  new DataView(plain.buffer).setUint32(frameKey.length + 1, participant);
  const sealed = new Uint8Array(
    await crypto.subtle.encrypt(
      { name: 'AES-GCM', iv: nonce as BufferSource, additionalData: utf8(scope) as BufferSource },
      await aes(key, 'encrypt'),
      plain as BufferSource,
    ),
  );
  const ephemeral = b64(ephPub);
  const data = b64(sealed);
  const signature = await (await deviceKey()).sign(utf8(keyBoxMessage(scope, participant, device, ephemeral, data)));
  return { device_id: device, ephemeral, data, signature };
}

/// The frame key in `push`, signed by the device with Ed25519 key `sender`.
export async function open_(
  scope: string,
  push: VoiceKeyPush,
  sender: string,
): Promise<{ key: Uint8Array; id: number }> {
  const me = (await deviceKey()).deviceId;
  const message = keyBoxMessage(scope, push.participant, me, push.ephemeral, push.data);
  if (!verify(unb64(sender), utf8(message), unb64(push.signature))) throw new Error('key box not signed by its sender');
  const secret = await voiceSecret();
  const ephemeral = unb64(push.ephemeral);
  const { key, nonce } = derive(x25519.getSharedSecret(secret, ephemeral), ephemeral, x25519.getPublicKey(secret));
  const plain = new Uint8Array(
    await crypto.subtle.decrypt(
      { name: 'AES-GCM', iv: nonce as BufferSource, additionalData: utf8(scope) as BufferSource },
      await aes(key, 'decrypt'),
      unb64(push.data) as BufferSource,
    ),
  );
  if (plain.length !== 25) throw new Error('invalid key box');
  const id = plain[20];
  const participant = new DataView(plain.buffer).getUint32(21);
  if (participant !== push.participant || id !== push.key_id) throw new Error('key box for someone else');
  return { key: plain.slice(0, 20), id };
}
