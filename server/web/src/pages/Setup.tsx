// The first start (no administrator yet) and the recovery of an
// administrator, both with the one-time code from the server's console
// (`farwing-server setup-code`).
import { useState, type FormEvent } from 'react';
import { useT } from '../i18n/react';
import { call, errorText } from '../lib/api';
import { useAuth } from '../lib/auth';
import { navigate } from '../lib/router';
import type { Applied, SetupInfo, TlsMode, TurnMode } from '../lib/types';
import { useApi } from '../lib/useApi';
import {
  Alert,
  Button,
  Checkbox,
  Empty,
  Field,
  FilePick,
  Input,
  LinesInput,
  Loading,
  Segmented,
  Switch,
} from '../ui/kit';
import { AuthFrame } from './Login';

function Code({ value, onChange }: { value: string; onChange(v: string): void }) {
  const t = useT();
  return (
    <Field label={t.setup.code}>
      <Input
        className="code"
        autoComplete="one-time-code"
        spellCheck={false}
        value={value}
        onChange={(e) => onChange(e.target.value.toUpperCase())}
        autoFocus
        required
      />
    </Field>
  );
}

function Account({
  username,
  password,
  repeat,
  onChange,
}: {
  username: string;
  password: string;
  repeat: string;
  onChange(p: { username?: string; password?: string; repeat?: string }): void;
}) {
  const t = useT();
  const m = t.setup;
  return (
    <>
      <Field label={m.username}>
        <Input
          autoComplete="username"
          value={username}
          onChange={(e) => onChange({ username: e.target.value })}
          required
        />
      </Field>
      <Field label={m.password}>
        <Input
          type="password"
          autoComplete="new-password"
          value={password}
          onChange={(e) => onChange({ password: e.target.value })}
          required
        />
      </Field>
      <Field label={m.confirmPassword} error={repeat && repeat !== password ? m.mismatch : undefined}>
        <Input
          type="password"
          autoComplete="new-password"
          value={repeat}
          onChange={(e) => onChange({ repeat: e.target.value })}
          required
        />
      </Field>
    </>
  );
}

/// Where to continue: the new address when the listener changed.
function Finished({ title, url }: { title: string; url?: string }) {
  const t = useT();
  const auth = useAuth();
  const here = !url || new URL(url).origin === location.origin;
  return (
    <AuthFrame title={title}>
      {here ? (
        <Button
          variant="primary"
          className="btn-block"
          onClick={() => {
            navigate('/', true);
            void auth.refresh();
          }}
        >
          {t.setup.signIn}
        </Button>
      ) : (
        <a className="btn btn-primary btn-md btn-block" href={url}>
          {t.setup.openAddress}
        </a>
      )}
    </AuthFrame>
  );
}

function SetupForm({ info }: { info: SetupInfo }) {
  const t = useT();
  const m = t.setup;
  const s = t.admin.settings;
  const view = info.current!;
  const initial = view.settings;
  const [code, setCode] = useState('');
  const [account, setAccount] = useState({ username: 'admin', password: '', repeat: '' });
  const [instance, setInstance] = useState(initial.instance_name);
  const [publicUrl, setPublicUrl] = useState(initial.public_url);
  const [tls, setTls] = useState<TlsMode>(initial.tls.mode);
  const [hostnames, setHostnames] = useState(initial.tls.hostnames);
  const [domains, setDomains] = useState(initial.tls.acme.domains);
  const [contact, setContact] = useState(initial.tls.acme.contact);
  const [agree, setAgree] = useState(initial.tls.acme.agree_tos);
  const [cert, setCert] = useState<string>();
  const [key, setKey] = useState<string>();
  const [turn, setTurn] = useState<TurnMode>(initial.turn.mode);
  const [publicIp, setPublicIp] = useState(initial.turn.public_ip ?? view.relay.local_ipv4 ?? '');
  const [urls, setUrls] = useState(initial.turn.urls);
  const [secret, setSecret] = useState('');
  const [registration, setRegistration] = useState(initial.registration);
  const [guests, setGuests] = useState(initial.guest.enabled);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string>();
  // Set up; where the console answers now.
  const [done, setDone] = useState<{ url?: string }>();

  if (done) return <Finished title={m.finished} url={done.url} />;

  async function submit(e: FormEvent) {
    e.preventDefault();
    if (account.password !== account.repeat) {
      setError(m.mismatch);
      return;
    }
    setBusy(true);
    setError(undefined);
    const turnSettings =
      turn === 'builtin'
        ? { mode: turn, public_ip: publicIp.trim() || null }
        : { mode: turn, urls, ...(secret ? { shared_secret: secret } : {}) };
    try {
      const applied = await call<Applied>(
        'POST',
        '/api/v2/setup',
        {
          code: code.trim(),
          admin: { username: account.username.trim(), password: account.password },
          settings: {
            instance_name: instance.trim(),
            public_url: publicUrl.trim(),
            registration,
            guest: { enabled: guests },
            tls: { mode: tls, hostnames, acme: { domains, contact, agree_tos: agree } },
            turn: turnSettings,
          },
          certificate: tls === 'provided' ? { cert_pem: cert ?? '', key_pem: key ?? '' } : undefined,
        },
        { sign: false },
      );
      setDone({ url: applied.outcome.listen_url ?? undefined });
    } catch (err) {
      // The reverse proxy mode closes this connection with the listener.
      if (err instanceof TypeError && (tls === 'proxy') !== (initial.tls.mode === 'proxy')) {
        setDone({ url: publicUrl.trim() });
        return;
      }
      setError(errorText(err));
    } finally {
      setBusy(false);
    }
  }

  return (
    <AuthFrame title={m.title} wide>
      <form className="stack" onSubmit={submit}>
        <Code value={code} onChange={setCode} />
        <h3>{m.admin}</h3>
        <Account {...account} onChange={(p) => setAccount({ ...account, ...p })} />
        <h3>{m.server}</h3>
        <Field label={s.instanceName}>
          <Input value={instance} maxLength={64} onChange={(e) => setInstance(e.target.value)} />
        </Field>
        <Field label={s.publicUrl}>
          <Input className="mono-input" value={publicUrl} onChange={(e) => setPublicUrl(e.target.value)} required />
        </Field>
        <h3>{m.certificate}</h3>
        <Segmented<TlsMode>
          value={tls}
          onChange={setTls}
          options={(['self_signed', 'provided', 'acme', 'proxy'] as const).map((v) => ({
            value: v,
            label: s.tlsModes[v],
          }))}
        />
        {tls === 'self_signed' && (
          <Field label={s.hostnames}>
            <LinesInput value={hostnames} onChange={setHostnames} />
          </Field>
        )}
        {tls === 'provided' && (
          <div className="grid grid-2">
            <Field label={s.certPem} group>
              <FilePick label={s.chooseFile} accept=".pem,.crt,.cer" onText={(text) => setCert(text)} />
            </Field>
            <Field label={s.keyPem} group>
              <FilePick label={s.chooseFile} accept=".pem,.key" onText={(text) => setKey(text)} />
            </Field>
          </div>
        )}
        {tls === 'acme' && (
          <>
            <Field label={s.acmeDomains}>
              <LinesInput value={domains} rows={2} onChange={setDomains} />
            </Field>
            <Field label={s.acmeContact}>
              <Input type="email" value={contact} onChange={(e) => setContact(e.target.value.trim())} />
            </Field>
            <Checkbox checked={agree} onChange={setAgree} label={s.agreeTos} />
          </>
        )}
        <h3>{m.relay}</h3>
        <Segmented<TurnMode>
          value={turn}
          onChange={setTurn}
          options={(['builtin', 'external'] as const).map((v) => ({ value: v, label: s.turnModes[v] }))}
        />
        {turn === 'builtin' && (
          <Field label={s.publicIp}>
            <Input className="mono-input" value={publicIp} onChange={(e) => setPublicIp(e.target.value)} />
          </Field>
        )}
        {turn !== 'builtin' && (
          <Field label={s.turnUrls}>
            <LinesInput value={urls} onChange={setUrls} />
          </Field>
        )}
        {turn === 'external' && (
          <Field label={s.sharedSecret}>
            <Input type="password" autoComplete="off" value={secret} onChange={(e) => setSecret(e.target.value)} />
          </Field>
        )}
        <h3>{m.accounts}</h3>
        <div className="stack-sm">
          <Switch checked={registration} onChange={setRegistration} label={s.registration} />
          <Switch checked={guests} onChange={setGuests} label={s.guests} />
        </div>
        {error && <Alert>{error}</Alert>}
        <Button type="submit" variant="primary" className="btn-block" loading={busy}>
          {m.submit}
        </Button>
      </form>
    </AuthFrame>
  );
}

function RecoverForm() {
  const t = useT();
  const m = t.setup;
  const [code, setCode] = useState('');
  const [account, setAccount] = useState({ username: '', password: '', repeat: '' });
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string>();
  const [done, setDone] = useState(false);

  if (done) return <Finished title={m.recovered} />;

  async function submit(e: FormEvent) {
    e.preventDefault();
    if (account.password !== account.repeat) {
      setError(m.mismatch);
      return;
    }
    setBusy(true);
    setError(undefined);
    try {
      await call(
        'POST',
        '/api/v2/setup/recover',
        { code: code.trim(), username: account.username.trim(), password: account.password },
        { sign: false },
      );
      setDone(true);
    } catch (err) {
      setError(errorText(err));
    } finally {
      setBusy(false);
    }
  }

  return (
    <AuthFrame title={m.recoverTitle}>
      <form className="stack" onSubmit={submit}>
        {error && <Alert>{error}</Alert>}
        <Code value={code} onChange={setCode} />
        <Account {...account} onChange={(p) => setAccount({ ...account, ...p })} />
        <Button type="submit" variant="primary" className="btn-block" loading={busy}>
          {m.recoverSubmit}
        </Button>
      </form>
    </AuthFrame>
  );
}

export default function Setup() {
  const t = useT();
  const { data, error } = useApi<SetupInfo>('/api/v2/setup');
  if (error) {
    return (
      <AuthFrame title={t.setup.title}>
        <Empty icon="alert" title={error} />
      </AuthFrame>
    );
  }
  if (!data) return <Loading />;
  if (data.state === 'setup') return <SetupForm info={data} />;
  if (data.state === 'recovery') return <RecoverForm />;
  return <Finished title={t.setup.doneTitle} />;
}
