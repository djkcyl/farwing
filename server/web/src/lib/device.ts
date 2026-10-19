// This browser as a Farwing device (`docs/web-console.md` §7.1): an Ed25519
// key generated here and kept in IndexedDB. WebCrypto keeps the private key
// non-extractable; browsers without WebCrypto Ed25519 fall back to
// @noble/curves with the seed stored in IndexedDB.
import { ed25519 } from '@noble/curves/ed25519.js';
import { b64, deviceIdOf, unb64 } from './encoding';

export interface DeviceKey {
  /// base64url raw public key.
  publicKey: string;
  /// Device ID derived from the key.
  deviceId: string;
  /// Installation ID registered with the server.
  installationId: string;
  sign(message: Uint8Array): Promise<string>;
}

interface Stored {
  installationId: string;
  publicKey: string;
  webcrypto?: CryptoKeyPair;
  seed?: Uint8Array;
}

const DB = 'farwing';
const STORE = 'device';
const KEY = 'identity';

function open(): Promise<IDBDatabase> {
  return new Promise((resolve, reject) => {
    const request = indexedDB.open(DB, 1);
    request.onupgradeneeded = () => request.result.createObjectStore(STORE);
    request.onsuccess = () => resolve(request.result);
    request.onerror = () => reject(request.error);
  });
}

async function load(): Promise<Stored | undefined> {
  const db = await open();
  return new Promise((resolve, reject) => {
    const request = db.transaction(STORE).objectStore(STORE).get(KEY);
    request.onsuccess = () => resolve(request.result as Stored | undefined);
    request.onerror = () => reject(request.error);
  });
}

async function save(value: Stored): Promise<void> {
  const db = await open();
  return new Promise((resolve, reject) => {
    const tx = db.transaction(STORE, 'readwrite');
    tx.objectStore(STORE).put(value, KEY);
    tx.oncomplete = () => resolve();
    tx.onerror = () => reject(tx.error);
  });
}

async function generate(): Promise<Stored> {
  const installationId = crypto.randomUUID();
  try {
    const pair = (await crypto.subtle.generateKey({ name: 'Ed25519' }, false, ['sign', 'verify'])) as CryptoKeyPair;
    const raw = new Uint8Array(await crypto.subtle.exportKey('raw', pair.publicKey));
    return { installationId, publicKey: b64(raw), webcrypto: pair };
  } catch {
    const seed = crypto.getRandomValues(new Uint8Array(32));
    return { installationId, publicKey: b64(ed25519.getPublicKey(seed)), seed };
  }
}

let cached: Promise<DeviceKey> | undefined;

/// This browser's device key, created on first use.
export function deviceKey(): Promise<DeviceKey> {
  cached ??= (async () => {
    let stored = await load().catch(() => undefined);
    if (!stored) {
      stored = await generate();
      await save(stored);
    }
    const value = stored;
    const raw = unb64(value.publicKey);
    return {
      publicKey: value.publicKey,
      deviceId: await deviceIdOf(raw),
      installationId: value.installationId,
      async sign(message: Uint8Array): Promise<string> {
        if (value.webcrypto) {
          const signature = await crypto.subtle.sign(
            { name: 'Ed25519' },
            value.webcrypto.privateKey,
            message as BufferSource,
          );
          return b64(new Uint8Array(signature));
        }
        return b64(ed25519.sign(message, value.seed!));
      },
    };
  })();
  return cached;
}

/// Verify an Ed25519 signature by another device.
export function verify(publicKey: Uint8Array, message: Uint8Array, signature: Uint8Array): boolean {
  try {
    return ed25519.verify(signature, message, publicKey, { zip215: false });
  } catch {
    return false;
  }
}

/// A readable name for this browser device, e.g. "网页 · Chrome · Windows".
export function browserName(): string {
  const ua = navigator.userAgent;
  const browser = /Edg\//.test(ua)
    ? 'Edge'
    : /Firefox\//.test(ua)
      ? 'Firefox'
      : /Chrome\//.test(ua)
        ? 'Chrome'
        : /Safari\//.test(ua)
          ? 'Safari'
          : '浏览器';
  const os = /Windows/.test(ua)
    ? 'Windows'
    : /Mac OS X/.test(ua)
      ? 'macOS'
      : /Android/.test(ua)
        ? 'Android'
        : /iPhone|iPad/.test(ua)
          ? 'iOS'
          : /Linux/.test(ua)
            ? 'Linux'
            : '';
  return ['网页', browser, os].filter(Boolean).join(' · ');
}
