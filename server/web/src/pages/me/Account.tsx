// The account: profile, password, two-step verification (administrators)
// and where it is signed in.
import { useState, type FormEvent } from 'react';
import { encode } from 'uqr';
import { useT } from '../../i18n/react';
import { call, errorText } from '../../lib/api';
import { useAuth } from '../../lib/auth';
import { ago, dateTime, platformText } from '../../lib/format';
import type { AuthSession } from '../../lib/types';
import { useApi } from '../../lib/useApi';
import { Alert, Badge, Button, Card, Copy, Empty, Field, Input, Loading, PageHeader, Row, Rows } from '../../ui/kit';
import { Dialog, useOverlay } from '../../ui/overlay';

function QrCode({ text }: { text: string }) {
  const t = useT();
  const qr = encode(text, { ecc: 'M', border: 2 });
  const path = qr.data.flatMap((row, y) => row.map((on, x) => (on ? `M${x} ${y}h1v1h-1z` : ''))).join('');
  return (
    <svg
      className="qr"
      viewBox={`0 0 ${qr.size} ${qr.size}`}
      shapeRendering="crispEdges"
      aria-label={t.account.twoStep.qrCode}
    >
      <rect width={qr.size} height={qr.size} fill="#fff" />
      <path d={path} fill="#000" />
    </svg>
  );
}

function Profile() {
  const t = useT();
  const auth = useAuth();
  const { toast } = useOverlay();
  const [nickname, setNickname] = useState(auth.me?.nickname ?? '');
  const [busy, setBusy] = useState(false);
  return (
    <Card title={t.account.profile.title}>
      <form
        className="stack"
        onSubmit={async (e) => {
          e.preventDefault();
          setBusy(true);
          try {
            await call('PATCH', '/api/v2/me', { nickname: nickname.trim() });
            toast(t.common.saved);
            await auth.refresh();
          } catch (err) {
            toast(errorText(err), 'red');
          } finally {
            setBusy(false);
          }
        }}
      >
        <Rows>
          <Row label={t.account.username}>{auth.me?.username}</Row>
          <Row label={t.account.profile.role}>
            {auth.isAdmin ? <Badge tone="blue">{t.common.admin}</Badge> : t.common.user}
          </Row>
        </Rows>
        <Field label={t.account.nickname}>
          <Input value={nickname} onChange={(e) => setNickname(e.target.value)} maxLength={64} />
        </Field>
        <div>
          <Button
            type="submit"
            variant="primary"
            loading={busy}
            disabled={nickname.trim() === (auth.me?.nickname ?? '')}
          >
            {t.common.save}
          </Button>
        </div>
      </form>
    </Card>
  );
}

function Password() {
  const t = useT();
  const m = t.account.password;
  const { toast } = useOverlay();
  const [old, setOld] = useState('');
  const [next, setNext] = useState('');
  const [again, setAgain] = useState('');
  const [error, setError] = useState<string>();
  const [busy, setBusy] = useState(false);

  async function submit(e: FormEvent) {
    e.preventDefault();
    if (next !== again) {
      setError(m.mismatch);
      return;
    }
    setBusy(true);
    setError(undefined);
    try {
      await call('POST', '/api/v2/me/password', { old_password: old, new_password: next });
      setOld('');
      setNext('');
      setAgain('');
      toast(m.changed);
    } catch (err) {
      setError(errorText(err));
    } finally {
      setBusy(false);
    }
  }

  return (
    <Card title={m.title}>
      <form className="stack" onSubmit={submit}>
        {error && <Alert>{error}</Alert>}
        <Field label={t.account.currentPassword}>
          <Input
            type="password"
            autoComplete="current-password"
            value={old}
            onChange={(e) => setOld(e.target.value)}
            required
          />
        </Field>
        <Field label={m.newPassword} hint={m.hint}>
          <Input
            type="password"
            autoComplete="new-password"
            value={next}
            onChange={(e) => setNext(e.target.value)}
            minLength={8}
            required
          />
        </Field>
        <Field label={m.confirm}>
          <Input
            type="password"
            autoComplete="new-password"
            value={again}
            onChange={(e) => setAgain(e.target.value)}
            required
          />
        </Field>
        <div>
          <Button type="submit" variant="primary" loading={busy}>
            {m.title}
          </Button>
        </div>
      </form>
    </Card>
  );
}

function TwoStep() {
  const t = useT();
  const m = t.account.twoStep;
  const auth = useAuth();
  const { toast } = useOverlay();
  const security = auth.security;
  const [step, setStep] = useState<'idle' | 'password' | 'scan' | 'codes' | 'disable'>('idle');
  const [password, setPassword] = useState('');
  const [code, setCode] = useState('');
  const [setup, setSetup] = useState<{ secret: string; uri: string }>();
  const [codes, setCodes] = useState<string[]>([]);
  const [error, setError] = useState<string>();
  const [busy, setBusy] = useState(false);

  if (!security) return null;
  if (!auth.isAdmin && !security.totp) return null;

  const close = () => {
    setStep('idle');
    setPassword('');
    setCode('');
    setError(undefined);
    setSetup(undefined);
  };

  async function run(fn: () => Promise<void>) {
    setBusy(true);
    setError(undefined);
    try {
      await fn();
    } catch (err) {
      setError(errorText(err));
    } finally {
      setBusy(false);
    }
  }

  return (
    <Card title={m.title}>
      {security.totp ? (
        <div className="row gap wrap">
          <Badge tone="green">{t.common.enabled}</Badge>
          <span className="small muted">{m.recoveryLeft(security.recovery_codes_left)}</span>
          <span className="grow" />
          <Button variant="danger" onClick={() => setStep('disable')}>
            {m.disable}
          </Button>
        </div>
      ) : (
        <div>
          <Button variant="primary" icon="lock" onClick={() => setStep('password')}>
            {m.enable}
          </Button>
        </div>
      )}

      <Dialog
        open={step === 'password' || step === 'scan'}
        title={m.enable}
        onClose={close}
        footer={
          <>
            <Button onClick={close}>{t.common.cancel}</Button>
            {step === 'password' ? (
              <Button
                variant="primary"
                loading={busy}
                onClick={() =>
                  run(async () => {
                    setSetup(await call('POST', '/api/v2/me/totp/setup', { password }));
                    setStep('scan');
                  })
                }
              >
                {t.common.next}
              </Button>
            ) : (
              <Button
                variant="primary"
                loading={busy}
                disabled={code.length !== 6}
                onClick={() =>
                  run(async () => {
                    const r = await call<{ recovery_codes: string[] }>('POST', '/api/v2/me/totp/enable', { code });
                    setCodes(r.recovery_codes);
                    setStep('codes');
                    setPassword('');
                    setCode('');
                    await auth.refresh();
                  })
                }
              >
                {m.turnOn}
              </Button>
            )}
          </>
        }
      >
        {error && <Alert>{error}</Alert>}
        {step === 'password' ? (
          <Field label={t.account.currentPassword}>
            <Input
              type="password"
              autoComplete="current-password"
              value={password}
              onChange={(e) => setPassword(e.target.value)}
            />
          </Field>
        ) : (
          setup && (
            <>
              <div className="row gap wrap" style={{ alignItems: 'flex-start' }}>
                <QrCode text={setup.uri} />
                <div className="stack-sm grow">
                  <span className="small muted">{m.secret}</span>
                  <div className="secret small">
                    <Copy text={setup.secret} />
                  </div>
                </div>
              </div>
              <Field label={t.account.verificationCode}>
                <Input
                  className="code"
                  inputMode="numeric"
                  maxLength={6}
                  value={code}
                  onChange={(e) => setCode(e.target.value.replace(/\D/g, ''))}
                  placeholder="000000"
                />
              </Field>
            </>
          )
        )}
      </Dialog>

      <Dialog
        open={step === 'codes'}
        title={m.saveCodes}
        onClose={close}
        footer={
          <Button variant="primary" onClick={close}>
            {t.common.done}
          </Button>
        }
      >
        <div className="secret codes">
          {codes.map((c) => (
            <span key={c}>{c}</span>
          ))}
        </div>
        <div>
          <Copy text={codes.join('\n')} display={t.common.copyAll} mono={false} />
        </div>
        <Alert tone="amber">{m.shownOnce}</Alert>
      </Dialog>

      <Dialog
        open={step === 'disable'}
        title={m.disable}
        onClose={close}
        footer={
          <>
            <Button onClick={close}>{t.common.cancel}</Button>
            <Button
              variant="danger"
              loading={busy}
              onClick={() =>
                run(async () => {
                  await call('POST', '/api/v2/me/totp/disable', { password, code: code.trim() });
                  toast(m.disabled);
                  close();
                  await auth.refresh();
                })
              }
            >
              {m.turnOff}
            </Button>
          </>
        }
      >
        {error && <Alert>{error}</Alert>}
        <Field label={t.account.currentPassword}>
          <Input
            type="password"
            autoComplete="current-password"
            value={password}
            onChange={(e) => setPassword(e.target.value)}
          />
        </Field>
        <Field label={m.codeOrRecovery}>
          <Input value={code} onChange={(e) => setCode(e.target.value)} />
        </Field>
      </Dialog>
    </Card>
  );
}

function Logins() {
  const t = useT();
  const m = t.account.logins;
  const { confirm, toast } = useOverlay();
  const { data, error, reload } = useApi<{ sessions: AuthSession[] }>('/api/v2/auth/sessions');
  return (
    <Card title={m.title} pad={false}>
      {error && <Alert>{error}</Alert>}
      {!data ? (
        <Loading />
      ) : data.sessions.length === 0 ? (
        <Empty icon="lock" title={m.none} />
      ) : (
        <div className="list">
          {[...data.sessions]
            .sort((a, b) => Number(b.current) - Number(a.current) || b.last_used_at - a.last_used_at)
            .map((s) => (
              <div className="list-item" key={s.device_id}>
                <div className="grow">
                  <div className="row gap-sm">
                    <span className="ellipsis">{s.device_alias}</span>
                    {s.current && <Badge tone="blue">{t.common.thisBrowser}</Badge>}
                  </div>
                  <div className="small muted">
                    {m.line(platformText(s.platform), dateTime(s.created_at), ago(s.last_used_at))}
                  </div>
                </div>
                {!s.current && (
                  <Button
                    size="sm"
                    variant="ghost"
                    onClick={async () => {
                      if (
                        await confirm({
                          title: m.revokeTitle,
                          message: m.revokeMessage(s.device_alias),
                          confirm: m.revoke,
                          danger: true,
                        })
                      ) {
                        try {
                          await call('DELETE', `/api/v2/auth/sessions/${s.device_id}`);
                          toast(m.revoked);
                          await reload();
                        } catch (err) {
                          toast(errorText(err), 'red');
                        }
                      }
                    }}
                  >
                    {m.revoke}
                  </Button>
                )}
              </div>
            ))}
        </div>
      )}
    </Card>
  );
}

export default function Account() {
  const t = useT();
  return (
    <div className="stack">
      <PageHeader title={t.nav.account} />
      <div className="grid grid-2">
        <Profile />
        <Password />
      </div>
      <TwoStep />
      <Logins />
    </div>
  );
}
