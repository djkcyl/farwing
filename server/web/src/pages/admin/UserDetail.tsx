// One account: role, status, password, two-step verification, devices,
// logins, friends and what they share.
import { useState } from 'react';
import type { Messages } from '../../i18n';
import { useT } from '../../i18n/react';
import { call, errorText } from '../../lib/api';
import { byName } from '../../lib/devices';
import { useAuth } from '../../lib/auth';
import {
  ago,
  bitrate,
  bytes,
  capText,
  connectIdText,
  dateTime,
  deviceIdText,
  listText,
  platformText,
} from '../../lib/format';
import { Link, navigate } from '../../lib/router';
import type { AdminFriend, Grants, UserDetail as Detail, UserPolicy } from '../../lib/types';
import { useApi } from '../../lib/useApi';
import { Icon } from '../../ui/icons';
import {
  Badge,
  Button,
  Card,
  Copy,
  Empty,
  Field,
  IconButton,
  Input,
  Online,
  PageHeader,
  PageState,
  Row,
  Rows,
  Select,
} from '../../ui/kit';
import { Dialog, useOverlay } from '../../ui/overlay';
import { RecordList } from '../../ui/Records';
import { OnceDialog } from './Users';

function grantsText(g: Grants, devices: Map<string, string>, m: Messages['admin']['userDetail']): string {
  const parts: string[] = [];
  parts.push(g.default.length ? m.defaultCaps(listText(g.default.map(capText))) : m.defaultNone);
  const specific = Object.entries(g.devices);
  if (specific.length) {
    parts.push(
      ...specific.map(([id, caps]) =>
        m.deviceCaps(devices.get(id) ?? deviceIdText(id), caps.length ? listText(caps.map(capText)) : m.notShared),
      ),
    );
  }
  return parts.join(m.grantsSeparator);
}

function FriendRow({ f, devices }: { f: AdminFriend; devices: Map<string, string> }) {
  const m = useT().admin.userDetail;
  return (
    <tr>
      <td>
        <div className="cell-main">{f.nickname || f.username}</div>
        <div className="cell-sub">{f.username}</div>
      </td>
      <td className="small">{grantsText(f.grants_given, devices, m)}</td>
      <td className="small">{grantsText(f.grants_received, new Map(), m)}</td>
      <td className="nowrap muted">{dateTime(f.since)}</td>
    </tr>
  );
}

type Limit = 'default' | 'unlimited' | 'custom';

/// The account's traffic this month and its own egress policy
/// (`docs/protocol/egress-policy.md` §3.1).
function Traffic({ data, onSave }: { data: Detail; onSave(policy: UserPolicy): Promise<boolean> }) {
  const t = useT();
  const m = t.admin.userDetail;
  const [draft, setDraft] = useState(data.policy);
  const [busy, setBusy] = useState(false);
  const d = data.policy_default;
  const set = (p: Partial<UserPolicy>) => setDraft({ ...draft, ...p });
  const allowedText = (v: boolean) => (v ? m.allowed : m.denied);
  const toggle = (value: boolean | null, fallback: boolean, onChange: (v: boolean | null) => void) => (
    <Select<string>
      width={200}
      value={value === null ? 'default' : value ? 'on' : 'off'}
      onChange={(v) => onChange(v === 'default' ? null : v === 'on')}
      options={[
        { value: 'default', label: m.byDefault(allowedText(fallback)) },
        { value: 'on', label: m.allowed },
        { value: 'off', label: m.denied },
      ]}
    />
  );
  const limit = (
    value: number | null,
    fallback: number | null,
    unit: (n: number) => string,
    unitName: string,
    onChange: (v: number | null) => void,
  ) => {
    const choice: Limit = value === null ? 'default' : value === 0 ? 'unlimited' : 'custom';
    return (
      <div className="row gap-sm">
        <Select<Limit>
          width={200}
          value={choice}
          onChange={(c) => onChange(c === 'default' ? null : c === 'unlimited' ? 0 : value || fallback || 10)}
          options={[
            { value: 'default', label: m.byDefault(fallback === null ? t.common.unlimited : unit(fallback)) },
            { value: 'unlimited', label: t.common.unlimited },
            { value: 'custom', label: m.custom },
          ]}
        />
        {choice === 'custom' && (
          <>
            <Input
              className="limit-input"
              type="number"
              min={1}
              value={value ?? ''}
              onChange={(e) => onChange(Math.max(1, Math.round(Number(e.target.value) || 1)))}
            />
            <span className="muted">{unitName}</span>
          </>
        )}
      </div>
    );
  };
  const admin = data.account.role === 'admin';
  const changed = JSON.stringify(draft) !== JSON.stringify(data.policy);
  return (
    <Card
      title={m.traffic}
      actions={
        !admin && (
          <Button
            variant="primary"
            disabled={!changed}
            loading={busy}
            onClick={async () => {
              setBusy(true);
              await onSave(draft);
              setBusy(false);
            }}
          >
            {t.common.save}
          </Button>
        )
      }
    >
      <Rows>
        <Row label={t.admin.users.monthTraffic}>
          {bytes(data.egress.month_bytes)}
          {data.egress.bps > 0 && <span className="muted"> · {bitrate(data.egress.bps)}</span>}
        </Row>
        {admin ? (
          <Row label={m.relay}>
            <span className="muted">{m.adminUnlimited}</span>
          </Row>
        ) : (
          <>
            <Row label={m.relay}>{toggle(draft.relay, d.relay, (relay) => set({ relay }))}</Row>
            <Row label={m.forward}>{toggle(draft.forward, d.forward, (forward) => set({ forward }))}</Row>
            <Row label={m.rate}>
              {limit(draft.rate_mbps, d.rate_mbps, m.mbps, 'Mbit/s', (rate_mbps) => set({ rate_mbps }))}
            </Row>
            <Row label={m.quota}>{limit(draft.quota_gb, d.quota_gb, m.gb, 'GB', (quota_gb) => set({ quota_gb }))}</Row>
          </>
        )}
      </Rows>
    </Card>
  );
}

export default function UserDetail({ params }: { params: Record<string, string> }) {
  const t = useT();
  const m = t.admin.userDetail;
  const { me } = useAuth();
  const { confirm, toast } = useOverlay();
  const { data, error, reload } = useApi<Detail>(`/api/v2/admin/users/${params.id}`);
  const [password, setPassword] = useState<string>();
  const [renaming, setRenaming] = useState(false);
  const [nickname, setNickname] = useState('');

  if (!data) {
    return (
      <PageState
        title={t.nav.users}
        back={
          <Link to="/admin/users" className="back">
            <Icon name="back" size={14} /> {t.nav.users}
          </Link>
        }
        error={error}
      />
    );
  }
  const a = data.account;
  const self = a.id === me?.user_id;
  const deviceNames = new Map(data.devices.map((d) => [d.device_id, d.alias]));

  async function act(fn: () => Promise<unknown>, done: string) {
    try {
      await fn();
      toast(done);
      await reload();
    } catch (err) {
      toast(errorText(err), 'red');
    }
  }

  const patch = (body: object) => call('PATCH', `/api/v2/admin/users/${a.id}`, body);

  return (
    <div className="stack">
      <PageHeader
        back={
          <Link to="/admin/users" className="back">
            <Icon name="back" size={14} /> {t.nav.users}
          </Link>
        }
        title={
          <span className="row gap-sm">
            {a.nickname || a.username}
            {a.role === 'admin' && <Badge tone="blue">{t.common.admin}</Badge>}
            {a.disabled && <Badge tone="red">{m.disabled}</Badge>}
          </span>
        }
        subtitle={a.username}
        actions={
          <>
            <Button
              icon="edit"
              onClick={() => {
                setNickname(a.nickname);
                setRenaming(true);
              }}
            >
              {m.editNickname}
            </Button>
            <Button
              icon="key"
              onClick={async () => {
                if (
                  await confirm({
                    title: m.resetPasswordTitle,
                    message: m.resetPasswordMessage(a.username, self),
                    confirm: m.reset,
                    danger: true,
                  })
                ) {
                  try {
                    const r = await call<{ password: string | null }>(
                      'POST',
                      `/api/v2/admin/users/${a.id}/password`,
                      {},
                    );
                    setPassword(r.password ?? undefined);
                    await reload();
                  } catch (err) {
                    toast(errorText(err), 'red');
                  }
                }
              }}
            >
              {m.resetPassword}
            </Button>
            {!self && (
              <Button
                onClick={async () => {
                  const make = a.role !== 'admin';
                  if (
                    await confirm({
                      title: make ? m.makeAdminTitle : m.removeAdminTitle,
                      message: make ? m.makeAdminMessage(a.username) : m.removeAdminMessage(a.username),
                      confirm: make ? m.makeAdmin : m.removeAdmin,
                    })
                  )
                    await act(() => patch({ admin: make }), make ? m.madeAdmin : m.removedAdmin);
                }}
              >
                {a.role === 'admin' ? m.removeAdmin : m.makeAdmin}
              </Button>
            )}
            {!self && (
              <Button
                variant={a.disabled ? 'default' : 'danger'}
                onClick={async () => {
                  if (a.disabled) {
                    await act(() => patch({ disabled: false }), m.enabled);
                  } else if (
                    await confirm({
                      title: m.disableTitle,
                      message: m.disableMessage(a.username),
                      confirm: m.disable,
                      danger: true,
                    })
                  ) {
                    await act(() => patch({ disabled: true }), m.disabled);
                  }
                }}
              >
                {a.disabled ? m.enable : m.disable}
              </Button>
            )}
          </>
        }
      />

      <div className="grid grid-2">
        <Card title={m.account}>
          <Rows>
            <Row label={m.username}>{a.username}</Row>
            <Row label={m.nickname}>{a.nickname || <span className="muted">{t.common.notSet}</span>}</Row>
            <Row label={m.userId}>
              <Copy text={a.id} display={<span className="small">{a.id}</span>} />
            </Row>
            <Row label={m.createdAt}>{dateTime(a.created_at)}</Row>
            <Row label={t.admin.col.lastSeen}>{a.last_seen_at ? ago(a.last_seen_at) : t.format.never}</Row>
            <Row label={m.totp}>
              {a.totp ? (
                <span className="row gap-sm">
                  <Badge tone="green">{t.common.enabled}</Badge>
                  <Button
                    size="sm"
                    onClick={async () => {
                      if (
                        await confirm({
                          title: m.resetTotpTitle,
                          message: m.resetTotpMessage(a.username),
                          confirm: m.reset,
                          danger: true,
                        })
                      )
                        await act(() => call('POST', `/api/v2/admin/users/${a.id}/totp/reset`, {}), m.totpReset);
                    }}
                  >
                    {m.reset}
                  </Button>
                </span>
              ) : (
                <span className="muted">{t.common.notEnabled}</span>
              )}
            </Row>
          </Rows>
        </Card>
        <Card title={m.logins} count={data.logins.length} pad={false}>
          {data.logins.length === 0 ? (
            <Empty icon="lock" title={m.noLogins} />
          ) : (
            <div className="list">
              {data.logins.map((l) => (
                <div className="list-item" key={l.device_id}>
                  <div className="grow">
                    <div className="ellipsis">{l.alias}</div>
                    <div className="small muted">
                      {platformText(l.platform)} · {m.signedInAt(dateTime(l.created_at))}
                    </div>
                  </div>
                  <span className="small faint nowrap">{m.lastUsed(ago(l.last_used_at))}</span>
                </div>
              ))}
            </div>
          )}
        </Card>
      </div>

      <Traffic
        key={JSON.stringify(data.policy)}
        data={data}
        onSave={async (policy) => {
          try {
            await patch({ egress: policy });
            toast(m.policySaved);
            await reload();
            return true;
          } catch (err) {
            toast(errorText(err), 'red');
            return false;
          }
        }}
      />
      <Card title={m.devices} count={data.devices.length} pad={false}>
        {data.devices.length === 0 ? (
          <Empty icon="monitor" title={m.noDevices} />
        ) : (
          <div className="table-wrap">
            <table className="table">
              <thead>
                <tr>
                  <th>{t.admin.col.device}</th>
                  <th>{t.admin.col.platform}</th>
                  <th>{t.admin.col.connectId}</th>
                  <th>{t.admin.col.status}</th>
                  <th>{t.admin.col.lastSeen}</th>
                  <th />
                </tr>
              </thead>
              <tbody>
                {[...data.devices].sort(byName).map((d) => (
                  <tr key={d.device_id}>
                    <td>
                      <div className="cell-main">{d.alias}</div>
                      <div className="cell-sub mono">{deviceIdText(d.device_id)}</div>
                    </td>
                    <td>
                      {platformText(d.platform)} <span className="muted small">{d.version}</span>
                    </td>
                    <td className="mono">{connectIdText(d.connect_id) || <span className="muted">—</span>}</td>
                    <td>
                      <Online online={d.online} />
                    </td>
                    <td className="nowrap">{ago(d.last_seen_at)}</td>
                    <td className="actions">
                      <IconButton
                        icon="unlink"
                        label={t.admin.unbind.label}
                        variant="danger"
                        onClick={async () => {
                          if (
                            await confirm({
                              title: t.admin.unbind.title,
                              message: t.admin.unbind.message(d.alias, a.username),
                              confirm: t.admin.unbind.confirm,
                              danger: true,
                            })
                          )
                            await act(
                              () => call('DELETE', `/api/v2/admin/devices/${d.device_id}`),
                              t.admin.unbind.done,
                            );
                        }}
                      />
                    </td>
                  </tr>
                ))}
              </tbody>
            </table>
          </div>
        )}
      </Card>

      <Card title={m.friends} count={data.friends.length} pad={false}>
        {data.friends.length === 0 ? (
          <Empty icon="users" title={m.noFriends} />
        ) : (
          <div className="table-wrap">
            <table className="table">
              <thead>
                <tr>
                  <th>{m.friend}</th>
                  <th>{m.sharedWith}</th>
                  <th>{m.sharedBy}</th>
                  <th>{m.since}</th>
                </tr>
              </thead>
              <tbody>
                {data.friends.map((f) => (
                  <FriendRow key={f.user_id} f={f} devices={deviceNames} />
                ))}
              </tbody>
            </table>
          </div>
        )}
      </Card>

      <Card title={t.nav.allRecords} pad={false}>
        <RecordList path="/api/v2/admin/records" user={params.id} />
      </Card>

      {!self && (
        <Card title={m.deleteAccount}>
          <div className="row gap wrap">
            <Button
              variant="danger"
              icon="trash"
              onClick={async () => {
                if (
                  await confirm({
                    title: m.deleteTitle,
                    message: m.deleteMessage(a.username),
                    confirm: t.common.delete,
                    danger: true,
                    typeToConfirm: a.username,
                  })
                ) {
                  try {
                    await call('DELETE', `/api/v2/admin/users/${a.id}`);
                    toast(m.deleted(a.username));
                    navigate('/admin/users');
                  } catch (err) {
                    toast(errorText(err), 'red');
                  }
                }
              }}
            >
              {m.deleteAccount}
            </Button>
          </div>
        </Card>
      )}

      <OnceDialog title={m.passwordReset} open={!!password} onClose={() => setPassword(undefined)}>
        <p>{m.newPasswordOf(a.username)}</p>
        <div className="secret">
          <Copy text={password ?? ''} />
        </div>
      </OnceDialog>
      <Dialog
        open={renaming}
        title={m.editNickname}
        onClose={() => setRenaming(false)}
        footer={
          <>
            <Button onClick={() => setRenaming(false)}>{t.common.cancel}</Button>
            <Button
              variant="primary"
              onClick={async () => {
                setRenaming(false);
                await act(() => patch({ nickname }), m.nicknameChanged);
              }}
            >
              {t.common.save}
            </Button>
          </>
        }
      >
        <Field label={m.nickname}>
          <Input value={nickname} onChange={(e) => setNickname(e.target.value)} maxLength={64} />
        </Field>
      </Dialog>
    </div>
  );
}
