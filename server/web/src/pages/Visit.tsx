// Visitor viewing (`docs/web-console.md` §7.3): a host's short number or
// device ID, with its access code or the host user's confirmation. Works
// without an account (as a guest) and signed in (as the account).
import { useEffect, useRef, useState, type FormEvent } from 'react';
import { useT } from '../i18n/react';
import { useAuth } from '../lib/auth';
import { connectIdText, deviceIdText } from '../lib/format';
import { Link, navigate, useLocation } from '../lib/router';
import { warmUpPake } from '../lib/spake2';
import { parseTarget, VisitError, visitAdmission, visitError, type VisitStep } from '../lib/visit';
import type { Admission } from '../lib/viewer';
import { Alert, Button, Field, Input, Spinner } from '../ui/kit';
import { AuthFrame } from './Login';
import { ViewerScreen } from '../ui/ViewerScreen';

export default function Visit() {
  const t = useT();
  const auth = useAuth();
  const { query } = useLocation();
  // A link from the host's invitation: `?id=<device>[&confirm=1][#code=<access code>]`
  // (the fragment never reaches the server).
  const [target, setTarget] = useState(() => {
    const id = query.get('id') ?? '';
    return /^\d{9}$/.test(id) ? connectIdText(id) : deviceIdText(id);
  });
  const [mode, setMode] = useState<'code' | 'confirm'>(() => (query.get('confirm') === '1' ? 'confirm' : 'code'));
  const [code, setCode] = useState(() => new URLSearchParams(window.location.hash.slice(1)).get('code') ?? '');
  const [step, setStep] = useState<VisitStep>();
  const [error, setError] = useState<VisitError>();
  const [watching, setWatching] = useState(false);
  const first = useRef<Admission | undefined>(undefined);
  const [confirmed, setConfirmed] = useState(false);
  const secret = useRef('');

  // The access code does not stay in the address bar or the history.
  useEffect(() => {
    if (window.location.hash)
      history.replaceState(history.state, '', window.location.pathname + window.location.search);
  }, []);

  // The access-code exchange starts sooner on submit.
  useEffect(() => {
    const timer = window.setTimeout(warmUpPake, 0);
    return () => window.clearTimeout(timer);
  }, []);

  async function submit(e: FormEvent) {
    e.preventDefault();
    setError(undefined);
    if (!parseTarget(target)) {
      setError(new VisitError('bad_target'));
      return;
    }
    secret.current = mode === 'code' ? code.trim() : '';
    setConfirmed(false);
    try {
      first.current = await visitAdmission(target, secret.current, (s) => {
        if (s === 'confirming') setConfirmed(true);
        setStep(s);
      });
      setWatching(true);
    } catch (err) {
      setError(visitError(err));
    } finally {
      setStep(undefined);
    }
  }

  if (watching) {
    return (
      <ViewerScreen
        title={parseTarget(target)?.connect_id ? connectIdText(parseTarget(target)!.connect_id) : target}
        subtitle={t.viewer.visit.title}
        // A session needing the host user's confirmation is not re-asked
        // automatically.
        autoRetry={!confirmed}
        admit={async () => {
          const ready = first.current;
          first.current = undefined;
          return ready ?? visitAdmission(target, secret.current, () => undefined);
        }}
        onLeave={() => {
          setWatching(false);
          if (auth.status !== 'signed_in') setCode('');
        }}
      />
    );
  }

  const busy = !!step;
  return (
    <AuthFrame
      title={t.viewer.visit.title}
      foot={
        auth.status === 'signed_in' ? (
          <Link to="/">{t.viewer.visit.backHome}</Link>
        ) : (
          <>
            <Link to="/login">{t.viewer.visit.signIn}</Link>
            <span />
          </>
        )
      }
    >
      <form className="stack" onSubmit={submit}>
        {error && <Alert>{error.text()}</Alert>}
        {auth.info && !auth.info.guests && auth.status !== 'signed_in' && (
          <Alert tone="amber">{t.viewer.visit.guestsOff}</Alert>
        )}
        <Field label={t.viewer.visit.target}>
          <Input
            className="mono"
            value={target}
            onChange={(e) => setTarget(e.target.value)}
            placeholder="123 456 789"
            autoFocus={!target}
            disabled={busy}
            required
          />
        </Field>
        <div className="row gap-sm">
          <button type="button" className={`chip ${mode === 'code' ? 'on' : ''}`} onClick={() => setMode('code')}>
            {t.viewer.visit.useCode}
          </button>
          <button type="button" className={`chip ${mode === 'confirm' ? 'on' : ''}`} onClick={() => setMode('confirm')}>
            {t.viewer.visit.askHost}
          </button>
        </div>
        {mode === 'code' && (
          <Field label={t.viewer.visit.code}>
            <Input
              type="password"
              autoComplete="off"
              value={code}
              onChange={(e) => setCode(e.target.value)}
              autoFocus={!!target}
              disabled={busy}
              required
            />
          </Field>
        )}
        {busy && (
          <div className="row gap-sm muted">
            <Spinner size={16} /> {t.viewer.visit.steps[step!]}
          </div>
        )}
        <Button type="submit" variant="primary" className="btn-block" loading={busy}>
          {t.viewer.visit.start}
        </Button>
        {busy && step === 'confirming' && (
          <Button variant="ghost" onClick={() => navigate('/visit', true)}>
            {t.common.cancel}
          </Button>
        )}
      </form>
    </AuthFrame>
  );
}
