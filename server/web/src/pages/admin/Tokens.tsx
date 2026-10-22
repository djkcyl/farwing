// Administration API tokens: for scripts and curl (`docs/server-admin-api.md`).
import { useState, type FormEvent } from 'react';
import { useT } from '../../i18n/react';
import { call, errorText } from '../../lib/api';
import { sitePath } from '../../lib/base';
import { ago, dateTime, listText } from '../../lib/format';
import type { AdminToken } from '../../lib/types';
import { useApi } from '../../lib/useApi';
import {
  Alert,
  Badge,
  Button,
  Card,
  Copy,
  Empty,
  Field,
  IconButton,
  Input,
  LinesInput,
  Loading,
  PageHeader,
  Segmented,
  Select,
  type Tone,
} from '../../ui/kit';
import { Dialog, useOverlay } from '../../ui/overlay';
import { OnceDialog } from './Users';

const VALIDITY = [30, 90, 365, 0];

function CreateToken({ open, onClose, onCreated }: { open: boolean; onClose(): void; onCreated(token: string): void }) {
  const t = useT();
  const m = t.admin.tokens;
  const [name, setName] = useState('');
  const [scope, setScope] = useState<'read' | 'admin'>('read');
  const [days, setDays] = useState(90);
  const [allow, setAllow] = useState<string[]>([]);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string>();

  async function submit(e: FormEvent) {
    e.preventDefault();
    setBusy(true);
    setError(undefined);
    try {
      const r = await call<{ token: string }>('POST', '/api/v2/admin/tokens', {
        name: name.trim(),
        scope,
        expires_days: days || null,
        allow,
      });
      setName('');
      setAllow([]);
      onCreated(r.token);
    } catch (err) {
      setError(errorText(err));
    } finally {
      setBusy(false);
    }
  }

  return (
    <Dialog
      open={open}
      title={m.create}
      onClose={onClose}
      footer={
        <>
          <Button onClick={onClose}>{t.common.cancel}</Button>
          <Button variant="primary" type="submit" form="token-form" loading={busy} disabled={!name.trim()}>
            {t.common.create}
          </Button>
        </>
      }
    >
      <form id="token-form" className="stack" onSubmit={submit}>
        {error && <Alert>{error}</Alert>}
        <Field label={m.name}>
          <Input value={name} maxLength={64} onChange={(e) => setName(e.target.value)} required />
        </Field>
        <Field label={m.scope} group>
          <Segmented
            value={scope}
            onChange={setScope}
            options={[
              { value: 'read', label: m.scopes.read },
              { value: 'admin', label: m.scopes.admin },
            ]}
          />
        </Field>
        <Field label={m.validity} group>
          <Select
            value={days}
            onChange={setDays}
            options={VALIDITY.map((d) => ({ value: d, label: d ? m.days(d) : m.noExpiry }))}
          />
        </Field>
        <Field label={m.allow}>
          <LinesInput key={String(open)} value={allow} rows={2} placeholder={m.anyNetwork} onChange={setAllow} />
        </Field>
      </form>
    </Dialog>
  );
}

function state(token: AdminToken, m: ReturnType<typeof useT>['admin']['tokens']): [string, Tone] {
  if (token.revoked_at) return [m.revoked, 'muted'];
  if (token.expires_at && token.expires_at * 1000 <= Date.now()) return [m.expired, 'amber'];
  return [m.active, 'green'];
}

export default function Tokens() {
  const t = useT();
  const m = t.admin.tokens;
  const { confirm, toast } = useOverlay();
  const { data, error, reload } = useApi<AdminToken[]>('/api/v2/admin/tokens');
  const [creating, setCreating] = useState(false);
  const [token, setToken] = useState<string>();
  const example = `curl -H "Authorization: Bearer ${token ?? ''}" -H "X-Farwing-Protocol: 2" ${location.origin}${sitePath('/api/v2/admin/settings')}`;

  return (
    <div className="stack">
      <PageHeader
        title={t.nav.tokens}
        actions={
          <Button variant="primary" icon="plus" onClick={() => setCreating(true)}>
            {m.create}
          </Button>
        }
      />
      {error && <Alert>{error}</Alert>}
      <Card pad={false}>
        {!data ? (
          <Loading />
        ) : data.length === 0 ? (
          <Empty icon="key" title={m.empty} />
        ) : (
          <div className="table-wrap">
            <table className="table">
              <thead>
                <tr>
                  <th>{m.name}</th>
                  <th>{t.admin.col.status}</th>
                  <th>{m.scope}</th>
                  <th>{m.allow}</th>
                  <th>{m.expires}</th>
                  <th>{m.lastUsed}</th>
                  <th />
                </tr>
              </thead>
              <tbody>
                {data.map((k) => {
                  const [text, tone] = state(k, m);
                  return (
                    <tr key={k.id}>
                      <td>
                        <div className="cell-main">{k.name}</div>
                        <div className="cell-sub">
                          {k.created_by_name} · {dateTime(k.created_at)}
                        </div>
                      </td>
                      <td>
                        <Badge tone={tone}>{text}</Badge>
                      </td>
                      <td>{m.scopes[k.scope]}</td>
                      <td className="mono small">{k.allow.length ? listText(k.allow) : m.anyNetwork}</td>
                      <td className="nowrap">{k.expires_at ? dateTime(k.expires_at) : m.noExpiry}</td>
                      <td className="nowrap">
                        {k.last_used_at ? (
                          <>
                            <div>{ago(k.last_used_at)}</div>
                            <div className="cell-sub mono">{k.last_used_ip}</div>
                          </>
                        ) : (
                          <span className="muted">{m.neverUsed}</span>
                        )}
                      </td>
                      <td className="actions">
                        {!k.revoked_at && (
                          <IconButton
                            icon="trash"
                            label={m.revoke}
                            variant="danger"
                            onClick={async () => {
                              if (
                                await confirm({
                                  title: m.revokeTitle,
                                  message: m.revokeMessage(k.name),
                                  confirm: m.revoke,
                                  danger: true,
                                })
                              ) {
                                try {
                                  await call('DELETE', `/api/v2/admin/tokens/${k.id}`);
                                  toast(m.revokedToast);
                                  await reload();
                                } catch (err) {
                                  toast(errorText(err), 'red');
                                }
                              }
                            }}
                          />
                        )}
                      </td>
                    </tr>
                  );
                })}
              </tbody>
            </table>
          </div>
        )}
      </Card>
      <CreateToken
        open={creating}
        onClose={() => setCreating(false)}
        onCreated={(value) => {
          setCreating(false);
          setToken(value);
          void reload();
        }}
      />
      <OnceDialog title={m.createdTitle} open={!!token} onClose={() => setToken(undefined)}>
        <div className="stack">
          <div className="secret">
            <Copy text={token ?? ''} />
          </div>
          <Field label={m.example} group>
            <div className="secret small">
              <Copy text={example} />
            </div>
          </Field>
        </div>
      </OnceDialog>
    </div>
  );
}
