// The server API, signed by this browser's device key exactly like the
// native client (`docs/protocol/farwing-protocol.md` §1.2): every request
// carries the device ID, a timestamp and an Ed25519 signature over method,
// path, timestamp and the SHA-256 of the body; account requests add the
// access token bound to this device.
import { currentLang, t } from '../i18n';
import { errorMessage } from '../i18n/errors';
import { deviceKey, browserName } from './device';
import { sitePath } from './base';
import { hex, sha256, utf8 } from './encoding';

export const PROTOCOL_VERSION = 2;

/// A failed request: the server's error code and details (the server's
/// `message` is for logs; the console shows `errorText`). `http`: an answer
/// without an API error body; `bad_response`: an answer that is not JSON.
export class ApiError extends Error {
  status: number;
  code: string;
  details?: unknown;
  constructor(status: number, code: string, message: string, details?: unknown) {
    super(message);
    this.status = status;
    this.code = code;
    this.details = details;
  }
}

let clockOffset = 0;
// The access token is useless without this browser's device key (every
// request is signed), so it may outlive the tab; the server ends idle web
// logins after `session_idle_minutes`.
const TOKEN_KEY = 'farwing.token';
let token: string | null = localStorage.getItem(TOKEN_KEY);
let onUnauthorized: (() => void) | undefined;

export function setToken(value: string | null) {
  token = value;
  if (value) localStorage.setItem(TOKEN_KEY, value);
  else localStorage.removeItem(TOKEN_KEY);
}

/// Called when the server no longer accepts this login (expired, revoked).
export function setUnauthorizedHandler(handler: (() => void) | undefined) {
  onUnauthorized = handler;
}

// Another tab signed in or out.
window.addEventListener('storage', (event) => {
  if (event.key === TOKEN_KEY) {
    token = event.newValue;
    if (!token) onUnauthorized?.();
  }
});

export function hasToken(): boolean {
  return token !== null;
}

/// Server time minus local time, learned from `GET /api/v2/server`.
export async function syncClock(): Promise<ServerInfo> {
  const before = Date.now();
  const info = await call<ServerInfo>('GET', '/api/v2/server', undefined, { sign: false });
  const after = Date.now();
  clockOffset = info.time_ms - Math.round((before + after) / 2);
  return info;
}

export function now(): number {
  return Date.now() + clockOffset;
}

let lastTimestamp = 0;

/// Signing timestamp: strictly increasing, so that two equal requests in
/// the same millisecond never carry the same signature (the server refuses
/// a repeated signature as a replay).
function signingTimestamp(): number {
  lastTimestamp = Math.max(now(), lastTimestamp + 1);
  return lastTimestamp;
}

interface CallOptions {
  sign?: boolean;
  auth?: boolean;
  /// Another credential than the account login (a visitor's guest token).
  bearer?: string;
  /// A background request: an ended login does not sign the page out (the
  /// next ordinary request will).
  quiet?: boolean;
  /// Completes even when the page closes.
  keepalive?: boolean;
}

/// Headers of a signed request (`docs/protocol/farwing-protocol.md` §1.2).
async function signedHeaders(method: string, path: string, bytes: Uint8Array, auth: boolean, bearer?: string) {
  const key = await deviceKey();
  const timestamp = signingTimestamp();
  const message = `FWREQ2
${method.toUpperCase()}
${path}
${timestamp}
${hex(await sha256(bytes))}`;
  const headers: Record<string, string> = {
    'x-farwing-protocol': String(PROTOCOL_VERSION),
    'x-farwing-device-id': key.deviceId,
    'x-farwing-timestamp': String(timestamp),
    'x-farwing-signature': await key.sign(utf8(message)),
  };
  const credential = bearer ?? (auth ? token : null);
  if (credential) headers.authorization = `Bearer ${credential}`;
  return headers;
}

function failed(
  response: Response,
  value: { error?: { code?: string; message?: string; details?: unknown } },
  auth: boolean,
): ApiError {
  const error = value?.error ?? {};
  if (response.status === 401 && token && auth && SIGNED_OUT.has(error.code ?? '')) {
    setToken(null);
    onUnauthorized?.();
  }
  return new ApiError(response.status, error.code ?? 'http', error.message ?? response.statusText, error.details);
}

export async function call<T>(method: string, path: string, body?: unknown, options: CallOptions = {}): Promise<T> {
  const sign = options.sign ?? true;
  const auth = options.auth ?? sign;
  const bytes = utf8(body === undefined ? '' : JSON.stringify(body));
  const headers: Record<string, string> = sign
    ? await signedHeaders(method, sitePath(path), bytes, auth, options.bearer)
    : { 'x-farwing-protocol': String(PROTOCOL_VERSION) };
  if (body !== undefined) headers['content-type'] = 'application/json';
  const response = await fetch(sitePath(path), {
    method,
    headers,
    body: body === undefined ? undefined : (bytes as BufferSource),
    keepalive: options.keepalive,
  });
  const raw = await response.text();
  let value: { error?: { code?: string; message?: string; details?: unknown } } = {};
  try {
    value = raw ? JSON.parse(raw) : {};
  } catch {
    if (!response.ok) throw new ApiError(response.status, 'http', response.statusText);
    throw new ApiError(response.status, 'bad_response', 'unrecognized response');
  }
  if (!response.ok) throw failed(response, value, auth && !options.bearer && !options.quiet);
  return value as T;
}

/// A signed GET of binary content (wallpapers).
export async function fetchBlob(path: string): Promise<Blob> {
  const response = await fetch(sitePath(path), {
    headers: await signedHeaders('GET', sitePath(path), new Uint8Array(), true),
  });
  if (!response.ok) throw failed(response, await response.json().catch(() => ({})), true);
  return response.blob();
}

/// The `reason` in an API error's details, if any.
export function errorReason(details: unknown): string | undefined {
  const reason = (details as { reason?: unknown } | null | undefined)?.reason;
  return typeof reason === 'string' ? reason : undefined;
}

/// A short message for a failed request, in the current language.
export function errorText(error: unknown): string {
  if (error instanceof ApiError) {
    if (error.code === 'http') return t().common.serverError(error.status);
    if (error.code === 'bad_response') return t().common.badResponse;
    return errorMessage(currentLang(), error.code, errorReason(error.details), error.details);
  }
  if (error instanceof TypeError) return t().common.cannotConnect;
  return (error as Error)?.message ?? String(error);
}

/// Answers meaning this browser's login is gone (expired, revoked, unbound).
const SIGNED_OUT = new Set(['unauthorized', 'session_revoked', 'device_mismatch']);

export interface ServerInfo {
  server: { name: string; version: string; instance_name: string };
  time_ms: number;
}

/// Register this browser as a device (or refresh its registration).
export async function registerDevice(): Promise<{ device_id: string; alias: string }> {
  const key = await deviceKey();
  const timestamp = signingTimestamp();
  const proof = await key.sign(utf8(`FWINIT2\n${key.publicKey}\n${timestamp}`));
  return call(
    'POST',
    '/api/v2/devices/init',
    {
      installation_id: key.installationId,
      name: browserName(),
      platform: 'web',
      os: navigator.platform || '',
      version: '0.1.0',
      public_key: key.publicKey,
      timestamp,
      proof,
      capabilities: [],
      policy: {
        control: false,
        files: false,
        ports: false,
        power: false,
        wol: false,
        assist: false,
        max_participants: 1,
      },
      hardware: null,
    },
    { sign: false },
  );
}

export interface LoginResponse {
  access_token: string;
  user_id: string;
  username: string;
  nickname: string;
}

export async function login(username: string, password: string): Promise<LoginResponse> {
  await registerDevice();
  const response = await call<LoginResponse>('POST', '/api/v2/auth/login', { username, password }, { auth: false });
  setToken(response.access_token);
  return response;
}

export async function register(
  invite: string,
  username: string,
  password: string,
  nickname: string,
): Promise<LoginResponse> {
  await registerDevice();
  const response = await call<LoginResponse>(
    'POST',
    '/api/v2/auth/register',
    { invite, username, password, nickname },
    { auth: false },
  );
  setToken(response.access_token);
  return response;
}

export async function logout(): Promise<void> {
  try {
    if (token) await call('POST', '/api/v2/auth/logout', {});
  } finally {
    setToken(null);
  }
}
