// Server settings, applied while the server runs (`docs/server-admin-api.md`):
// one tab per group, each saved on its own.
import { useEffect, useState, type ReactNode } from 'react';
import type { Messages } from '../../i18n';
import { useT } from '../../i18n/react';
import { ApiError, call, errorText, now } from '../../lib/api';
import { useAuth } from '../../lib/auth';
import { capText, dateTime, listText, WEB_CAPS } from '../../lib/format';
import type {
  Applied,
  LimitsSettings,
  ListenPending,
  ServerSettings,
  SettingsView,
  StunEndpoint,
  TlsMode,
  TurnMode,
} from '../../lib/types';
import { useApi } from '../../lib/useApi';
import {
  Alert,
  Badge,
  Button,
  Card,
  Checkbox,
  Copy,
  Field,
  FilePick,
  IconButton,
  Input,
  LinesInput,
  PageHeader,
  PageState,
  Row,
  Rows,
  Segmented,
  Select,
  Switch,
  Tabs,
} from '../../ui/kit';
import { useOverlay } from '../../ui/overlay';

type Tab = 'basic' | 'certificate' | 'relay' | 'access' | 'limits' | 'backup';
type M = Messages['admin']['settings'];

/// The label of a setting the server refused (`details.field`).
function fieldLabel(m: M, field: string): string {
  const labels: Record<string, string> = {
    listen: m.listen,
    public_url: m.publicUrl,
    instance_name: m.instanceName,
    trusted_proxies: m.trustedProxies,
    bitrate_limit_mbps: m.bitrateLimit,
    log: m.log,
    'tls.hostnames': m.hostnames,
    'tls.certificate': m.certPem,
    'tls.acme.domains': m.acmeDomains,
    'tls.acme.contact': m.acmeContact,
    'tls.acme.directory': m.acmeDirectory,
    'tls.acme.challenge': m.challenge,
    'tls.acme.http_listen': m.httpListen,
    'tls.acme.agree_tos': m.agreeTos,
    'turn.urls': m.turnUrls,
    'turn.shared_secret': m.sharedSecret,
    'turn.public_ip': m.publicIp,
    'turn.listen_udp': m.listenUdp,
    'turn.listen_tcp': m.listenTcp,
    'turn.relay_port_min': m.relayPorts,
    'turn.relay_port_max': m.relayPorts,
    'turn.relay_bind': m.relayBind,
    'turn.relay_ip': m.relayIp,
    'turn.credential_ttl': m.credentialTtl,
    'turn.stun': m.stun,
    'web.caps': m.webCaps,
    'web.admin_allow': m.adminAllow,
    'web.session_idle_minutes': m.sessionIdle,
    'web.session_max_hours': m.sessionMax,
    'web.audit_days': m.auditDays,
  };
  const limit = field.startsWith('limits.') ? (m.limits as Record<string, string>)[field.slice(7)] : undefined;
  return labels[field] ?? limit ?? field;
}

/// Why saving failed: the setting and its problem when the server names them.
function saveError(m: M, error: unknown): string {
  if (error instanceof ApiError) {
    const d = error.details as { reason?: string; field?: string; problem?: string } | undefined;
    if (d?.reason === 'setting_invalid' && d.field && d.field !== 'settings') {
      const problems: Record<string, string> = m.problems;
      return m.fieldError(fieldLabel(m, d.field), problems[d.problem ?? ''] ?? problems.invalid);
    }
  }
  return errorText(error);
}

/// Save a partial settings document; the toast says what restarted.
function useSave(onApplied: (view: Applied) => void) {
  const t = useT();
  const m = t.admin.settings;
  const { toast } = useOverlay();
  const auth = useAuth();
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string>();
  /// `undefined` when saved, else the failure.
  async function save(patch: unknown, path = '/api/v2/admin/settings', method = 'PUT'): Promise<unknown> {
    setBusy(true);
    setError(undefined);
    try {
      const applied = await call<Applied>(method, path, patch);
      const o = applied.outcome;
      // A moved listener shows its own bar instead.
      if (!o.listen_url) {
        toast(o.relay_restarted ? m.relayRestarted : o.fingerprint_changed ? m.certificateChanged : m.saved);
      }
      onApplied(applied);
      void auth.refresh();
      return undefined;
    } catch (err) {
      setError(saveError(m, err));
      return err ?? new Error();
    } finally {
      setBusy(false);
    }
  }
  return { save, busy, error };
}

function same(a: unknown, b: unknown): boolean {
  return JSON.stringify(a) === JSON.stringify(b);
}

function SaveButton({ busy, changed, onClick }: { busy: boolean; changed: boolean; onClick(): void }) {
  const t = useT();
  return (
    <Button variant="primary" onClick={onClick} loading={busy} disabled={!changed}>
      {t.common.save}
    </Button>
  );
}

/// A number field kept as text while typed; `null` when empty.
function NumberInput({
  value,
  onChange,
  min,
  max,
  placeholder,
}: {
  value: number | null;
  onChange(v: number | null): void;
  min?: number;
  max?: number;
  placeholder?: string;
}) {
  return (
    <Input
      type="number"
      min={min}
      max={max}
      value={value ?? ''}
      placeholder={placeholder}
      onChange={(e) => onChange(e.target.value === '' ? null : Number(e.target.value))}
    />
  );
}

/// An address field; `null` when empty.
function AddressInput({ value, onChange }: { value: string | null; onChange(v: string | null): void }) {
  return <Input className="mono-input" value={value ?? ''} onChange={(e) => onChange(e.target.value.trim() || null)} />;
}

// --------------------------------------------------------------- basic

const LOG_LEVELS = ['error', 'warn', 'info', 'debug', 'trace'];

function Basic({ view, onApplied }: { view: SettingsView; onApplied(v: Applied): void }) {
  const t = useT();
  const m = t.admin.settings;
  const s = view.settings;
  const initial = {
    instance_name: s.instance_name,
    public_url: s.public_url,
    trusted_proxies: s.trusted_proxies,
    bitrate_limit_mbps: s.bitrate_limit_mbps,
    log: s.log,
  };
  const [draft, setDraft] = useState(initial);
  const { save, busy, error } = useSave(onApplied);
  const set = (p: Partial<typeof initial>) => setDraft({ ...draft, ...p });
  const levels = LOG_LEVELS.includes(draft.log) ? LOG_LEVELS : [...LOG_LEVELS, draft.log];
  return (
    <div className="stack">
      <Card
        title={m.tabs.basic}
        actions={
          <SaveButton
            busy={busy}
            changed={!same(draft, initial)}
            onClick={() => void save({ ...draft, instance_name: draft.instance_name.trim() })}
          />
        }
      >
        <div className="stack">
          {error && <Alert>{error}</Alert>}
          <div className="grid grid-2">
            <Field label={m.instanceName}>
              <Input
                value={draft.instance_name}
                maxLength={64}
                onChange={(e) => set({ instance_name: e.target.value })}
              />
            </Field>
            <Field label={m.publicUrl}>
              <Input
                className="mono-input"
                value={draft.public_url}
                onChange={(e) => set({ public_url: e.target.value.trim() })}
              />
            </Field>
            <Field label={m.bitrateLimit}>
              <NumberInput
                value={draft.bitrate_limit_mbps}
                min={1}
                max={10000}
                placeholder={t.common.unlimited}
                onChange={(v) => set({ bitrate_limit_mbps: v })}
              />
            </Field>
            <Field label={m.log} group>
              <Select
                value={draft.log}
                onChange={(log) => set({ log })}
                options={levels.map((l) => ({ value: l, label: l }))}
              />
            </Field>
          </div>
          <Field label={m.trustedProxies}>
            <LinesInput value={draft.trusted_proxies} onChange={(trusted_proxies) => set({ trusted_proxies })} />
          </Field>
        </div>
      </Card>
      <Listen view={view} onApplied={onApplied} />
    </div>
  );
}

function Listen({ view, onApplied }: { view: SettingsView; onApplied(v: Applied): void }) {
  const t = useT();
  const m = t.admin.settings;
  const [listen, setListen] = useState(view.settings.listen);
  const { save, busy, error } = useSave(onApplied);
  return (
    <Card title={m.listen}>
      <div className="stack">
        {error && <Alert>{error}</Alert>}
        <div className="row gap">
          <div className="grow">
            <Input
              className="mono-input"
              aria-label={m.listen}
              value={listen}
              disabled={view.listen_locked || !!view.listen_pending}
              onChange={(e) => setListen(e.target.value.trim())}
            />
          </div>
          {view.listen_locked ? (
            <Badge>{m.listenLocked}</Badge>
          ) : (
            <Button
              onClick={() => void save({ listen })}
              loading={busy}
              disabled={listen === view.settings.listen || !!view.listen_pending}
            >
              {m.changeListen}
            </Button>
          )}
        </div>
        <Rows>
          <Row label={m.dataDir}>
            <span className="mono small">{view.data_dir}</span>
          </Row>
        </Rows>
      </div>
    </Card>
  );
}

/// A moved listener waits for confirmation; it goes back by itself.
function PendingListen({ pending, onDone }: { pending: ListenPending; onDone(view?: SettingsView): void }) {
  const t = useT();
  const m = t.admin.settings;
  const { toast } = useOverlay();
  const left = () => Math.max(0, Math.round(pending.confirm_by - now() / 1000));
  const [remaining, setRemaining] = useState(left);
  const [busy, setBusy] = useState(false);
  useEffect(() => {
    const timer = window.setInterval(() => {
      const r = left();
      setRemaining(r);
      if (r === 0) {
        window.clearInterval(timer);
        window.setTimeout(() => onDone(), 1500);
      }
    }, 1000);
    return () => window.clearInterval(timer);
    // Restarts only for another pending change.
    // oxlint-disable-next-line react-hooks/exhaustive-deps
  }, [pending.confirm_by]);
  async function finish(action: 'confirm' | 'revert') {
    setBusy(true);
    try {
      const view = await call<SettingsView>('POST', `/api/v2/admin/settings/listen/${action}`);
      toast(action === 'confirm' ? m.listenKept : m.listenReverted);
      onDone(view);
    } catch (err) {
      toast(errorText(err), 'red');
      onDone();
    } finally {
      setBusy(false);
    }
  }
  return (
    <Alert tone="amber">
      <div className="row gap wrap">
        <div className="grow stack-sm">
          <b>{m.listenPending}</b>
          <span>
            {m.listenNew}{' '}
            <a href={pending.url} target="_blank" rel="noreferrer" className="mono">
              {pending.url}
            </a>
          </span>
          <span className="small muted">{m.listenRemaining(remaining)}</span>
        </div>
        <div className="row gap-sm">
          <Button onClick={() => void finish('revert')} disabled={busy}>
            {m.revert}
          </Button>
          <Button variant="primary" onClick={() => void finish('confirm')} loading={busy}>
            {m.keep}
          </Button>
        </div>
      </div>
    </Alert>
  );
}

// --------------------------------------------------------- certificate

const DIRECTORIES = ['letsencrypt', 'letsencrypt-staging'];

function Certificate({ view, onApplied }: { view: SettingsView; onApplied(v: Applied): void }) {
  const t = useT();
  const m = t.admin.settings;
  const o = t.admin.overview;
  const initial = view.settings.tls;
  const [draft, setDraft] = useState(initial);
  const [cert, setCert] = useState<string>();
  const [key, setKey] = useState<string>();
  const { save, busy, error } = useSave(onApplied);
  const upload = useSave(onApplied);
  // The reverse proxy mode closed this page's connection with the listener.
  const [moved, setMoved] = useState<string>();
  const acme = draft.acme;
  const setAcme = (p: Partial<typeof acme>) => setDraft({ ...draft, acme: { ...acme, ...p } });
  const directory = DIRECTORIES.includes(acme.directory) ? acme.directory : 'custom';
  const status = view.tls.acme;
  // The reverse proxy mode changes the listener: it is saved on its own.
  const proxyChanged = (draft.mode === 'proxy') !== (initial.mode === 'proxy');
  return (
    <div className="stack">
      <Card
        title={m.tlsMode}
        actions={
          <SaveButton
            busy={busy}
            changed={!same(draft, initial)}
            onClick={async () => {
              const failure = await save({ tls: proxyChanged ? { mode: draft.mode } : draft });
              if (failure instanceof TypeError && proxyChanged) {
                setMoved(`${draft.mode === 'proxy' ? 'http' : 'https'}://${location.host}${location.pathname}`);
              }
            }}
          />
        }
      >
        <div className="stack">
          {moved ? (
            <Alert tone="amber">
              {m.listenPending} ·{' '}
              <a href={moved} className="mono">
                {moved}
              </a>
            </Alert>
          ) : (
            error && <Alert>{error}</Alert>
          )}
          <Segmented<TlsMode>
            value={draft.mode}
            onChange={(mode) => setDraft({ ...draft, mode })}
            options={(['self_signed', 'provided', 'acme', 'proxy'] as const).map((v) => ({
              value: v,
              label: m.tlsModes[v],
            }))}
          />
          {draft.mode === 'self_signed' && (
            <Field label={m.hostnames}>
              <LinesInput value={draft.hostnames} onChange={(hostnames) => setDraft({ ...draft, hostnames })} />
            </Field>
          )}
          {draft.mode === 'acme' && (
            <>
              <div className="grid grid-2">
                <Field label={m.acmeDomains}>
                  <LinesInput value={acme.domains} rows={2} onChange={(domains) => setAcme({ domains })} />
                </Field>
                <Field label={m.acmeContact}>
                  <Input
                    type="email"
                    value={acme.contact}
                    onChange={(e) => setAcme({ contact: e.target.value.trim() })}
                  />
                </Field>
                <Field label={m.acmeDirectory} group>
                  <Select
                    value={directory}
                    onChange={(d) => setAcme({ directory: d === 'custom' ? '' : d })}
                    options={[
                      { value: 'letsencrypt', label: m.directories.letsencrypt },
                      { value: 'letsencrypt-staging', label: m.directories.staging },
                      { value: 'custom', label: m.directories.custom },
                    ]}
                  />
                </Field>
                {directory === 'custom' && (
                  <Field label={m.directoryUrl}>
                    <Input
                      className="mono-input"
                      value={acme.directory}
                      onChange={(e) => setAcme({ directory: e.target.value.trim() })}
                    />
                  </Field>
                )}
                <Field label={m.challenge} group>
                  <Segmented
                    value={acme.challenge}
                    onChange={(challenge) => setAcme({ challenge })}
                    options={(['auto', 'tls-alpn-01', 'http-01'] as const).map((v) => ({
                      value: v,
                      label: m.challenges[v],
                    }))}
                  />
                </Field>
                <Field label={m.httpListen}>
                  <AddressInput value={acme.http_listen} onChange={(http_listen) => setAcme({ http_listen })} />
                </Field>
                <Field label={m.caRoot}>
                  <AddressInput value={acme.ca_root} onChange={(ca_root) => setAcme({ ca_root })} />
                </Field>
              </div>
              <Checkbox checked={acme.agree_tos} onChange={(agree_tos) => setAcme({ agree_tos })} label={m.agreeTos} />
            </>
          )}
        </div>
      </Card>
      {draft.mode === 'provided' && (
        <Card
          title={m.tlsModes.provided}
          actions={
            <Button
              variant="primary"
              disabled={!cert || !key}
              loading={upload.busy}
              onClick={() => void upload.save({ cert_pem: cert, key_pem: key }, '/api/v2/admin/settings/certificate')}
            >
              {m.useCertificate}
            </Button>
          }
        >
          <div className="stack">
            {upload.error && <Alert>{upload.error}</Alert>}
            <div className="grid grid-2">
              <Field label={m.certPem} group>
                <FilePick label={m.chooseFile} accept=".pem,.crt,.cer" onText={(text) => setCert(text)} />
              </Field>
              <Field label={m.keyPem} group>
                <FilePick label={m.chooseFile} accept=".pem,.key" onText={(text) => setKey(text)} />
              </Field>
            </div>
          </div>
        </Card>
      )}
      <Card title={m.currentCertificate}>
        <Rows>
          <Row label={m.fingerprint}>
            {view.tls.fingerprint ? (
              <Copy text={view.tls.fingerprint} display={<span className="small">{view.tls.fingerprint}</span>} />
            ) : (
              <span className="muted">{o.tlsByProxy}</span>
            )}
          </Row>
          <Row label={m.tlsModes.provided}>{view.tls.provided ? m.providedState : m.notProvided}</Row>
          {status && (
            <Row label={m.acmeState}>
              {status.issued ? (
                <span>
                  {o.certIssued(listText(status.domains), dateTime(status.not_after))}
                  <span className="muted small"> · {o.certRenews(dateTime(status.renew_at))}</span>
                </span>
              ) : (
                <span className="amber">{o.certPending(listText(status.domains))}</span>
              )}
              {status.last_error && <div className="small red">{o.certFailed(status.last_error)}</div>}
            </Row>
          )}
        </Rows>
      </Card>
    </div>
  );
}

// --------------------------------------------------------------- relay

function Relay({ view, onApplied }: { view: SettingsView; onApplied(v: Applied): void }) {
  const t = useT();
  const m = t.admin.settings;
  const { confirm } = useOverlay();
  const initial = view.settings.turn;
  const [draft, setDraft] = useState(initial);
  const { save, busy, error } = useSave(onApplied);
  const rotate = useSave(onApplied);
  const set = (p: Partial<typeof initial>) => setDraft({ ...draft, ...p });
  const setStun = (i: number, p: Partial<StunEndpoint>) =>
    set({ stun: draft.stun.map((e, j) => (j === i ? { ...e, ...p } : e)) });
  const local = view.relay.local_ipv4;
  const relay = view.relay;
  return (
    <div className="stack">
      <Card
        title={m.turnMode}
        actions={<SaveButton busy={busy} changed={!same(draft, initial)} onClick={() => void save({ turn: draft })} />}
      >
        <div className="stack">
          {error && <Alert>{error}</Alert>}
          <Segmented<TurnMode>
            value={draft.mode}
            onChange={(mode) => set({ mode })}
            options={(['builtin', 'external', 'none'] as const).map((v) => ({ value: v, label: m.turnModes[v] }))}
          />
          {draft.mode === 'builtin' && (
            <div className="grid grid-2">
              <Field label={m.publicIp} group>
                <div className="row gap-sm">
                  <div className="grow">
                    <AddressInput value={draft.public_ip} onChange={(public_ip) => set({ public_ip })} />
                  </div>
                  {local && local !== draft.public_ip && (
                    <Button onClick={() => set({ public_ip: local })}>{m.useAddress(local)}</Button>
                  )}
                </div>
              </Field>
              <Field label={m.relayPorts} group>
                <div className="row gap-sm">
                  <NumberInput
                    value={draft.relay_port_min}
                    min={1024}
                    max={65535}
                    onChange={(relay_port_min) => set({ relay_port_min })}
                  />
                  <span className="muted">–</span>
                  <NumberInput
                    value={draft.relay_port_max}
                    min={1024}
                    max={65535}
                    onChange={(relay_port_max) => set({ relay_port_max })}
                  />
                </div>
              </Field>
              <Field label={m.listenUdp}>
                <AddressInput value={draft.listen_udp} onChange={(listen_udp) => set({ listen_udp })} />
              </Field>
              <Field label={m.listenTcp}>
                <AddressInput value={draft.listen_tcp} onChange={(listen_tcp) => set({ listen_tcp })} />
              </Field>
              <Field label={m.relayBind}>
                <Input
                  className="mono-input"
                  value={draft.relay_bind}
                  onChange={(e) => set({ relay_bind: e.target.value.trim() })}
                />
              </Field>
              <Field label={m.relayIp}>
                <AddressInput value={draft.relay_ip} onChange={(relay_ip) => set({ relay_ip })} />
              </Field>
              <Field label={m.realm}>
                <Input value={draft.realm} onChange={(e) => set({ realm: e.target.value.trim() })} />
              </Field>
              <Field label={m.credentialTtl}>
                <NumberInput
                  value={Math.round(draft.credential_ttl / 60)}
                  min={1}
                  onChange={(v) => set({ credential_ttl: (v ?? 0) * 60 })}
                />
              </Field>
            </div>
          )}
          <Field label={m.turnUrls}>
            <LinesInput
              value={draft.urls}
              placeholder={draft.mode === 'builtin' ? m.auto : undefined}
              onChange={(urls) => set({ urls })}
            />
          </Field>
          {draft.mode === 'external' && (
            <div className="grid grid-2">
              <Field label={m.sharedSecret}>
                <Input
                  type="password"
                  autoComplete="off"
                  value={draft.shared_secret}
                  placeholder={view.turn_secret_set ? m.secretSet : m.secretUnset}
                  onChange={(e) => set({ shared_secret: e.target.value })}
                />
              </Field>
              <Field label={m.credentialTtl}>
                <NumberInput
                  value={Math.round(draft.credential_ttl / 60)}
                  min={1}
                  onChange={(v) => set({ credential_ttl: (v ?? 0) * 60 })}
                />
              </Field>
            </div>
          )}
          {draft.mode !== 'none' && (
            <div className="stack-sm">
              <Switch
                checked={draft.force_relay}
                onChange={(force_relay) => set({ force_relay })}
                label={m.forceRelay}
              />
              <div className="switch-row">
                <span className="switch-text">
                  <span>{m.sharedSecret}</span>
                </span>
                <Button
                  size="sm"
                  loading={rotate.busy}
                  onClick={async () => {
                    if (await confirm({ title: m.rotateTitle, message: m.rotateMessage, confirm: m.rotateSecret })) {
                      void rotate.save(undefined, '/api/v2/admin/settings/relay-secret', 'POST');
                    }
                  }}
                >
                  {m.rotateSecret}
                </Button>
              </div>
              {rotate.error && <Alert>{rotate.error}</Alert>}
            </div>
          )}
        </div>
      </Card>
      <Card
        title={m.stun}
        actions={
          <Button
            size="sm"
            icon="plus"
            onClick={() => set({ stun: [...draft.stun, { listen: '0.0.0.0:3479', public: null }] })}
          >
            {m.addEndpoint}
          </Button>
        }
      >
        {draft.stun.length === 0 ? (
          <span className="muted">—</span>
        ) : (
          <div className="stack-sm">
            <div className="row gap small muted">
              <span className="grow">{m.stunListen}</span>
              <span className="grow">{m.stunPublic}</span>
              <span className="icon-spacer" />
            </div>
            {draft.stun.map((e, i) => (
              <div key={i} className="row gap">
                <div className="grow">
                  <Input
                    className="mono-input"
                    aria-label={m.stunListen}
                    value={e.listen}
                    onChange={(ev) => setStun(i, { listen: ev.target.value.trim() })}
                  />
                </div>
                <div className="grow">
                  <Input
                    className="mono-input"
                    aria-label={m.stunPublic}
                    value={e.public ?? ''}
                    onChange={(ev) => setStun(i, { public: ev.target.value.trim() || null })}
                  />
                </div>
                <IconButton
                  icon="trash"
                  variant="danger"
                  label={t.common.delete}
                  onClick={() => set({ stun: draft.stun.filter((_, j) => j !== i) })}
                />
              </div>
            ))}
          </div>
        )}
      </Card>
      <Card title={m.running}>
        <Rows>
          <Row label={m.listenUdp}>
            {relay.turn_udp ? (
              <span className="mono small">{relay.turn_udp}</span>
            ) : (
              <span className="muted">{m.stopped}</span>
            )}
          </Row>
          <Row label={m.listenTcp}>
            {relay.turn_tcp ? (
              <span className="mono small">{relay.turn_tcp}</span>
            ) : (
              <span className="muted">{m.stopped}</span>
            )}
          </Row>
          <Row label={m.natDiscovery}>
            {relay.nat_discovery.length ? (
              <span className="mono small">{relay.nat_discovery.join('  ')}</span>
            ) : (
              <span className="muted">—</span>
            )}
          </Row>
        </Rows>
      </Card>
    </div>
  );
}

// ------------------------------------------------------ accounts and web

function Access({ view, onApplied }: { view: SettingsView; onApplied(v: Applied): void }) {
  const t = useT();
  const m = t.admin.settings;
  const { confirm } = useOverlay();
  const s = view.settings;
  const initial = { registration: s.registration, guest: s.guest, web: s.web };
  const [draft, setDraft] = useState(initial);
  const { save, busy, error } = useSave(onApplied);
  const web = draft.web;
  const setWeb = (p: Partial<typeof web>) => setDraft({ ...draft, web: { ...web, ...p } });
  return (
    <div className="stack">
      <Card
        title={m.tabs.access}
        actions={<SaveButton busy={busy} changed={!same(draft, initial)} onClick={() => void save(draft)} />}
      >
        <div className="stack">
          {error && <Alert>{error}</Alert>}
          <div className="stack-sm">
            <Switch
              checked={draft.registration}
              onChange={(registration) => setDraft({ ...draft, registration })}
              label={m.registration}
            />
            <Switch
              checked={draft.guest.enabled}
              onChange={(enabled) => setDraft({ ...draft, guest: { enabled } })}
              label={m.guests}
            />
            <Switch
              checked={web.enabled}
              onChange={async (enabled) => {
                if (!enabled && !(await confirm({ title: m.webOffTitle, message: m.webOffMessage, danger: true })))
                  return;
                setWeb({ enabled });
              }}
              label={m.webEnabled}
            />
          </div>
          <Field label={m.webCaps} group>
            <div className="chips">
              {WEB_CAPS.map((id) => (
                <Checkbox
                  key={id}
                  label={capText(id)}
                  checked={web.caps.includes(id)}
                  disabled={id === 'view'}
                  onChange={(v) => setWeb({ caps: WEB_CAPS.filter((c) => (c === id ? v : web.caps.includes(c))) })}
                />
              ))}
            </div>
          </Field>
          <div className="grid grid-3">
            <Field label={m.sessionIdle}>
              <NumberInput
                value={web.session_idle_minutes}
                min={1}
                onChange={(v) => setWeb({ session_idle_minutes: v ?? 0 })}
              />
            </Field>
            <Field label={m.sessionMax}>
              <NumberInput
                value={web.session_max_hours}
                min={1}
                onChange={(v) => setWeb({ session_max_hours: v ?? 0 })}
              />
            </Field>
            <Field label={m.auditDays}>
              <NumberInput value={web.audit_days} min={1} onChange={(v) => setWeb({ audit_days: v ?? 0 })} />
            </Field>
          </div>
          <Field label={m.adminAllow}>
            <LinesInput value={web.admin_allow} onChange={(admin_allow) => setWeb({ admin_allow })} />
          </Field>
        </div>
      </Card>
    </div>
  );
}

// -------------------------------------------------------------- limits

function Limits({ view, onApplied }: { view: SettingsView; onApplied(v: Applied): void }) {
  const t = useT();
  const m = t.admin.settings;
  const initial = view.settings.limits;
  const [draft, setDraft] = useState(initial);
  const { save, busy, error } = useSave(onApplied);
  const keys = Object.keys(m.limits) as (keyof LimitsSettings)[];
  return (
    <Card
      title={m.tabs.limits}
      actions={<SaveButton busy={busy} changed={!same(draft, initial)} onClick={() => void save({ limits: draft })} />}
    >
      <div className="stack">
        {error && <Alert>{error}</Alert>}
        <div className="grid grid-3">
          {keys.map((k) => (
            <Field key={k} label={m.limits[k]}>
              <NumberInput value={draft[k]} min={0} onChange={(v) => setDraft({ ...draft, [k]: v ?? 0 })} />
            </Field>
          ))}
        </div>
      </div>
    </Card>
  );
}

// -------------------------------------------------------------- backup

function Backup({ onApplied }: { onApplied(v: Applied): void }) {
  const t = useT();
  const m = t.admin.settings;
  const { confirm, toast } = useOverlay();
  const [secrets, setSecrets] = useState(false);
  const [busy, setBusy] = useState(false);
  const { save, busy: importing, error } = useSave(onApplied);
  async function exportSettings() {
    setBusy(true);
    try {
      const doc = await call<ServerSettings>('GET', `/api/v2/admin/settings/export${secrets ? '?secrets=true' : ''}`);
      const blob = new Blob([JSON.stringify(doc, null, 2)], { type: 'application/json' });
      const a = document.createElement('a');
      a.href = URL.createObjectURL(blob);
      a.download = `farwing-settings-${new Date().toISOString().slice(0, 10)}.json`;
      a.click();
      URL.revokeObjectURL(a.href);
    } catch (err) {
      toast(errorText(err), 'red');
    } finally {
      setBusy(false);
    }
  }
  return (
    <Card title={m.tabs.backup}>
      <div className="stack-sm">
        <div className="switch-row">
          <span className="switch-text">
            <span>{m.export}</span>
          </span>
          <div className="row gap">
            <Checkbox checked={secrets} onChange={setSecrets} label={m.includeSecrets} />
            <Button icon="download" loading={busy} onClick={() => void exportSettings()}>
              {m.export}
            </Button>
          </div>
        </div>
        <div className="switch-row">
          <span className="switch-text">
            <span>{m.import}</span>
          </span>
          <FilePick
            label={m.chooseFile}
            accept=".json,application/json"
            onText={async (text) => {
              let doc: unknown;
              try {
                doc = JSON.parse(text);
              } catch {
                toast(m.badFile, 'red');
                return;
              }
              if (await confirm({ title: m.importTitle, message: m.importMessage, confirm: m.import })) {
                await save(doc, '/api/v2/admin/settings/import', 'POST');
              }
            }}
          />
        </div>
        {importing && <span className="muted small">{t.common.loading}</span>}
        {error && <Alert>{error}</Alert>}
      </div>
    </Card>
  );
}

// ---------------------------------------------------------------- page

export default function Settings() {
  const t = useT();
  const m = t.admin.settings;
  const { data, error, reload } = useApi<SettingsView>('/api/v2/admin/settings');
  const [view, setView] = useState<SettingsView>();
  const [tab, setTab] = useState<Tab>('basic');
  const [generation, setGeneration] = useState(0);
  const current = view ?? data;
  if (!current) return <PageState title={t.nav.settings} error={error} />;
  // A saved tab starts again from what the server keeps.
  const applied = (v: SettingsView) => {
    setView(v);
    setGeneration((g) => g + 1);
  };
  const tabs: [Tab, ReactNode][] = [
    ['basic', <Basic key={generation} view={current} onApplied={applied} />],
    ['certificate', <Certificate key={generation} view={current} onApplied={applied} />],
    ['relay', <Relay key={generation} view={current} onApplied={applied} />],
    ['access', <Access key={generation} view={current} onApplied={applied} />],
    ['limits', <Limits key={generation} view={current} onApplied={applied} />],
    ['backup', <Backup key={generation} onApplied={applied} />],
  ];
  return (
    <div className="stack">
      <PageHeader title={t.nav.settings} />
      {current.listen_pending && (
        <PendingListen
          pending={current.listen_pending}
          onDone={(v) => {
            if (v) applied(v);
            else {
              setView(undefined);
              void reload().then(() => setGeneration((g) => g + 1));
            }
          }}
        />
      )}
      <div>
        <Tabs<Tab> items={tabs.map(([id]) => ({ id, label: m.tabs[id] }))} value={tab} onChange={setTab} />
        {tabs.find(([id]) => id === tab)?.[1]}
      </div>
    </div>
  );
}
