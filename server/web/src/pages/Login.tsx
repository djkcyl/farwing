// Sign in (password, then the second step for accounts with two-step
// verification) and invitation registration.
import { useState, type FormEvent, type ReactNode } from 'react';
import { useT } from '../i18n/react';
import { ApiError, errorText } from '../lib/api';
import { useAuth } from '../lib/auth';
import { Link, navigate, useLocation } from '../lib/router';
import { Alert, Button, Field, Input } from '../ui/kit';
import { LangSwitch } from '../ui/LangSwitch';
import { BrandLogo } from '../ui/BrandLogo';
import { instanceText } from '../lib/format';

export function AuthFrame({
  title,
  children,
  foot,
  wide,
}: {
  title: string;
  children: ReactNode;
  foot?: ReactNode;
  wide?: boolean;
}) {
  const { info } = useAuth();
  return (
    <div className="auth-page">
      <LangSwitch className="lang-corner" />
      <div className={`auth-card ${wide ? 'wide' : ''}`}>
        <div className="auth-brand">
          <BrandLogo height={30} />
          {instanceText(info?.instance_name) && <div className="small muted">{instanceText(info?.instance_name)}</div>}
        </div>
        <div className="auth-panel">
          <h1>{title}</h1>
          {children}
        </div>
        {foot && <div className="auth-foot">{foot}</div>}
      </div>
    </div>
  );
}

export function LoginPage() {
  const t = useT();
  const auth = useAuth();
  const [username, setUsername] = useState('');
  const [password, setPassword] = useState('');
  const [error, setError] = useState<string>();
  const [busy, setBusy] = useState(false);

  async function submit(e: FormEvent) {
    e.preventDefault();
    setBusy(true);
    setError(undefined);
    try {
      await auth.login(username.trim(), password);
    } catch (err) {
      setError(err instanceof ApiError && err.code === 'unauthorized' ? t.auth.login.badCredentials : errorText(err));
    } finally {
      setBusy(false);
    }
  }

  return (
    <AuthFrame
      title={t.auth.login.title}
      foot={
        <>
          {auth.info?.registration ? <Link to="/register">{t.auth.login.registerLink}</Link> : <span />}
          {auth.info?.guests && <Link to="/visit">{t.auth.login.visitLink}</Link>}
        </>
      }
    >
      <form className="stack" onSubmit={submit}>
        {auth.notice === 'expired' && !error && <Alert tone="amber">{t.auth.sessionExpired}</Alert>}
        {error && <Alert>{error}</Alert>}
        <Field label={t.auth.login.username}>
          <Input
            id="username"
            autoComplete="username"
            value={username}
            onChange={(e) => setUsername(e.target.value)}
            autoFocus
            required
          />
        </Field>
        <Field label={t.auth.login.password}>
          <Input
            id="password"
            type="password"
            autoComplete="current-password"
            value={password}
            onChange={(e) => setPassword(e.target.value)}
            required
          />
        </Field>
        <Button id="login" type="submit" variant="primary" className="btn-block" loading={busy}>
          {t.auth.login.title}
        </Button>
      </form>
    </AuthFrame>
  );
}

export function SecondFactorPage() {
  const t = useT();
  const auth = useAuth();
  const [code, setCode] = useState('');
  const [error, setError] = useState<string>();
  const [busy, setBusy] = useState(false);
  const [recovery, setRecovery] = useState(false);

  async function submit(e: FormEvent) {
    e.preventDefault();
    setBusy(true);
    setError(undefined);
    try {
      await auth.verify(code.replace(/\s/g, ''));
    } catch (err) {
      setError(errorText(err));
      setCode('');
    } finally {
      setBusy(false);
    }
  }

  return (
    <AuthFrame
      title={t.auth.secondFactor.title}
      foot={
        <>
          <a
            href="#"
            onClick={(e) => {
              e.preventDefault();
              setRecovery(!recovery);
              setCode('');
              setError(undefined);
            }}
          >
            {recovery ? t.auth.secondFactor.useCode : t.auth.secondFactor.useRecoveryCode}
          </a>
          <a
            href="#"
            onClick={(e) => {
              e.preventDefault();
              void auth.logout();
            }}
          >
            {t.auth.secondFactor.otherAccount}
          </a>
        </>
      }
    >
      <form className="stack" onSubmit={submit}>
        {error && <Alert>{error}</Alert>}
        <Field label={recovery ? t.auth.secondFactor.recoveryCode : t.auth.secondFactor.code}>
          <Input
            id="totp"
            className={recovery ? 'mono' : 'code'}
            inputMode={recovery ? 'text' : 'numeric'}
            autoComplete="one-time-code"
            maxLength={recovery ? 32 : 6}
            placeholder={recovery ? 'xxxxx-xxxxx' : '000000'}
            value={code}
            onChange={(e) => setCode(recovery ? e.target.value : e.target.value.replace(/\D/g, ''))}
            autoFocus
            required
          />
        </Field>
        <Button
          id="verify"
          type="submit"
          variant="primary"
          className="btn-block"
          loading={busy}
          disabled={!recovery && code.length !== 6}
        >
          {t.auth.secondFactor.verify}
        </Button>
      </form>
    </AuthFrame>
  );
}

export function RegisterPage() {
  const t = useT();
  const r = t.auth.register;
  const auth = useAuth();
  const { query } = useLocation();
  const [invite, setInvite] = useState(query.get('code') ?? '');
  const [username, setUsername] = useState('');
  const [nickname, setNickname] = useState('');
  const [password, setPassword] = useState('');
  const [password2, setPassword2] = useState('');
  const [error, setError] = useState<string>();
  const [busy, setBusy] = useState(false);

  if (auth.info && !auth.info.registration) {
    return (
      <AuthFrame title={r.title} foot={<Link to="/login">{r.backToLogin}</Link>}>
        <Alert tone="amber">{r.disabled}</Alert>
      </AuthFrame>
    );
  }

  async function submit(e: FormEvent) {
    e.preventDefault();
    if (password !== password2) {
      setError(r.passwordMismatch);
      return;
    }
    setBusy(true);
    setError(undefined);
    try {
      await auth.register(invite.trim(), username.trim(), password, nickname.trim());
      navigate('/', true);
    } catch (err) {
      setError(errorText(err));
    } finally {
      setBusy(false);
    }
  }

  return (
    <AuthFrame title={r.titleInvite} foot={<Link to="/login">{r.haveAccount}</Link>}>
      <form className="stack" onSubmit={submit}>
        {error && <Alert>{error}</Alert>}
        <Field label={r.invite}>
          <Input
            className="mono"
            value={invite}
            onChange={(e) => setInvite(e.target.value.toUpperCase())}
            placeholder="XXXX-XXXX-XXXX"
            required
            autoFocus={!invite}
          />
        </Field>
        <Field label={r.username} hint={r.usernameHint}>
          <Input
            autoComplete="username"
            value={username}
            onChange={(e) => setUsername(e.target.value)}
            required
            autoFocus={!!invite}
          />
        </Field>
        <Field label={r.nickname} hint={t.common.optional}>
          <Input value={nickname} onChange={(e) => setNickname(e.target.value)} maxLength={64} />
        </Field>
        <Field label={r.password} hint={r.passwordHint}>
          <Input
            type="password"
            autoComplete="new-password"
            value={password}
            onChange={(e) => setPassword(e.target.value)}
            minLength={8}
            required
          />
        </Field>
        <Field label={r.confirmPassword}>
          <Input
            type="password"
            autoComplete="new-password"
            value={password2}
            onChange={(e) => setPassword2(e.target.value)}
            required
          />
        </Field>
        <Button type="submit" variant="primary" className="btn-block" loading={busy}>
          {r.submit}
        </Button>
      </form>
    </AuthFrame>
  );
}
