// Visitor admission (`docs/web-console.md` §7.3, `docs/access-model.md`
// §2.4): reach a host by its short number or device ID with its access code
// (SPAKE2, the code never leaves the browser) or the host user's
// confirmation. Signed-in browsers ask as their account; others as a guest.
import { t } from '../i18n';
import { ApiError, call, errorText, hasToken, registerDevice } from './api';
import { deviceIdOf, unb64 } from './encoding';
import { ASSIST_PAKE_SLOTS, keyConfirm, keyConfirmMatches, pakeStart } from './spake2';
import type { Admission, SessionTicket } from './viewer';

interface AssistHost {
  device_id: string;
  alias: string;
  platform: string;
  version: string;
  public_key: string;
}

interface AssistPending {
  request_id: string;
  mode: 'password' | 'confirmation' | 'password_confirmation';
  trusted: boolean;
  pake: string[];
  key_confirm: string[];
  host: AssistHost;
}

type AssistJoin = SessionTicket & { host: AssistHost };

export type VisitStep = 'asking' | 'checking' | 'confirming' | 'joining';

/// Why a visit failed: a code with visitor wording (`viewer.visit.errors`),
/// or another failure (`cause`) in its own words. The message is in the
/// language of the moment; `text()` gives it in the current one.
export class VisitError extends Error {
  code: string;
  constructor(code: string, cause?: unknown) {
    super(code, { cause });
    this.code = code;
    this.message = this.text();
  }

  text(): string {
    const known = (t().viewer.visit.errors as Record<string, string>)[this.code];
    if (known) return known;
    return this.cause === undefined ? this.code : errorText(this.cause);
  }
}

const GUEST_KEY = 'farwing.guest';

/// A guest token for this browser device (kept for the tab).
async function guestToken(): Promise<string> {
  const saved = sessionStorage.getItem(GUEST_KEY);
  if (saved) {
    const { token, expires_at } = JSON.parse(saved) as { token: string; expires_at: number };
    if (expires_at * 1000 > Date.now() + 60_000) return token;
  }
  await registerDevice();
  const guest = await call<{ guest_token: string; expires_at: number }>(
    'POST',
    '/api/v2/guest/session',
    {},
    { auth: false },
  );
  sessionStorage.setItem(GUEST_KEY, JSON.stringify({ token: guest.guest_token, expires_at: guest.expires_at }));
  return guest.guest_token;
}

/// "123 456 789" → short number; 16 base32 characters → device ID.
export function parseTarget(text: string): { connect_id?: string; device_id?: string } | undefined {
  const compact = text.replace(/[\s-]/g, '').toLowerCase();
  if (/^\d{9}$/.test(compact)) return { connect_id: compact };
  if (/^[a-z2-7]{16}$/.test(compact)) return { device_id: compact };
  return undefined;
}

/// Server errors keep their code (visitor wording where there is one, the
/// server error's otherwise).
export function visitError(error: unknown): VisitError {
  if (error instanceof VisitError) return error;
  if (error instanceof ApiError) return new VisitError(error.code, error);
  return new VisitError('error', error);
}

/// Ask the host to admit this browser; resolves with the session to join.
export async function visitAdmission(
  target: string,
  code: string,
  visit: string,
  onStep: (step: VisitStep) => void,
): Promise<Admission> {
  const addressed = parseTarget(target);
  if (!addressed) throw new VisitError('bad_target');
  const bearer = hasToken() ? undefined : await guestToken().catch((e) => Promise.reject(visitError(e)));
  const options = { bearer };
  // One SPAKE2 instance per host code slot, all from the typed code.
  const states = code ? Array.from({ length: ASSIST_PAKE_SLOTS }, () => pakeStart(code)) : [];
  let requestId = '';
  try {
    onStep('asking');
    const pending = await call<AssistPending>(
      'POST',
      '/api/v2/assist/join',
      { ...addressed, pake: states.map((s) => s.message) },
      options,
    );
    requestId = pending.request_id;
    let key: Uint8Array | undefined;
    if (!pending.trusted && pending.mode !== 'confirmation') {
      if (!states.length) throw new VisitError('assist_code_required');
      onStep('checking');
      for (let i = 0; i < states.length && !key; i++) {
        const message = pending.pake[i];
        const confirm = pending.key_confirm[i];
        if (!message || !confirm) continue;
        try {
          const candidate = states[i].state.finish(unb64(message));
          if (keyConfirmMatches(candidate, 'host', requestId, confirm)) key = candidate;
        } catch {
          // A malformed slot: try the next.
        }
      }
      // A wrong code is detected here; nothing about it was sent.
      if (!key) throw new VisitError('assist_code_invalid');
    }
    onStep(!pending.trusted && pending.mode !== 'password' ? 'confirming' : 'joining');
    const joined = await call<AssistJoin>(
      'POST',
      `/api/v2/assist/requests/${requestId}/accept`,
      { key_confirm: key ? keyConfirm(key, 'controller', requestId) : '', visit },
      options,
    );
    requestId = '';
    if (
      joined.host.device_id !== pending.host.device_id ||
      joined.host.public_key !== pending.host.public_key ||
      (await deviceIdOf(unb64(joined.host.public_key))) !== joined.host.device_id
    ) {
      throw new VisitError('identity');
    }
    onStep('joining');
    return { ticket: joined, hostId: joined.host.device_id, pakeKey: key, bearer };
  } catch (error) {
    if (requestId) {
      void call('POST', `/api/v2/assist/requests/${requestId}/cancel`, {}, options).catch(() => undefined);
    }
    throw visitError(error);
  }
}

/// The bearer a visitor's later requests use (none when signed in).
export function visitorSignedIn(): boolean {
  return hasToken();
}
