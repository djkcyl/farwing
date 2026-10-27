// Friends: add, answer requests, and what each friend may do on this
// account's devices (the same editor as the client's).
import { useEffect, useState, type FormEvent } from 'react';
import { useT } from '../../i18n/react';
import { call, errorText } from '../../lib/api';
import { byName } from '../../lib/devices';
import {
  capGroups,
  capPresets,
  capText,
  dateTime,
  DEFAULT_FRIEND,
  deviceIdText,
  displayName,
  normalizeCaps,
  sameCaps,
  shareSummary,
} from '../../lib/format';
import type { Device, DeviceList, Friend, FriendRequests, Grants, UserSummary } from '../../lib/types';
import { useApi } from '../../lib/useApi';
import { Alert, Button, Card, Checkbox, Empty, Input, Loading, PageHeader, Select } from '../../ui/kit';
import { Dialog, useOverlay } from '../../ui/overlay';

function Avatar({ user }: { user: UserSummary }) {
  const colors = ['#4b88eb', '#66cf9c', '#eebe6f', '#be84f0', '#f17d84', '#5cc4d6'];
  let hash = 0;
  for (const ch of user.user_id) hash = (hash * 31 + ch.charCodeAt(0)) >>> 0;
  return (
    <span className="avatar" style={{ background: colors[hash % colors.length], color: '#10141a' }}>
      {(displayName(user) || '?').slice(0, 1).toUpperCase()}
    </span>
  );
}

function Person({
  user,
  children,
  actions,
}: {
  user: UserSummary;
  children?: React.ReactNode;
  actions?: React.ReactNode;
}) {
  return (
    <div className="list-item">
      <Avatar user={user} />
      <div className="grow">
        <div className="row gap-sm">
          <b>{displayName(user)}</b>
          {user.nickname && user.nickname !== user.username && <span className="small muted">@{user.username}</span>}
        </div>
        {children && <div className="small muted">{children}</div>}
      </div>
      {actions && <div className="row gap-sm">{actions}</div>}
    </div>
  );
}

function CapsEditor({ value, onChange }: { value: string[]; onChange(v: string[]): void }) {
  const t = useT();
  return (
    <div className="stack-sm caps-editor">
      <div className="row gap-sm wrap">
        <span className="small muted caps-group">{t.account.friends.presets}</span>
        {capPresets().map(([label, preset]) => (
          <button
            type="button"
            key={label}
            className={`chip ${sameCaps(value, preset) ? 'on' : ''}`}
            onClick={() => onChange(normalizeCaps(preset))}
          >
            {label}
          </button>
        ))}
      </div>
      {capGroups().map(([group, members]) => (
        <div className="row gap wrap" key={group}>
          <span className="small muted caps-group">{group}</span>
          {members.map((cap) => (
            <Checkbox
              key={cap}
              checked={value.includes(cap)}
              label={capText(cap)}
              onChange={(on) => onChange(normalizeCaps(on ? [...value, cap] : value.filter((c) => c !== cap)))}
            />
          ))}
        </div>
      ))}
    </div>
  );
}

type Mode = 'default' | 'custom' | 'hidden';

function modeOf(g: Grants, id: string): Mode {
  const caps = g.devices[id];
  if (caps === undefined) return 'default';
  return caps.length ? 'custom' : 'hidden';
}

function GrantsEditor({
  friend,
  devices,
  onClose,
}: {
  friend: UserSummary | undefined;
  devices: Device[];
  onClose(): void;
}) {
  const t = useT();
  const m = t.account.friends;
  const { toast } = useOverlay();
  const [grants, setGrants] = useState<Grants>();
  const [original, setOriginal] = useState<string>();
  const [error, setError] = useState<string>();
  const [busy, setBusy] = useState(false);

  useEffect(() => {
    if (!friend) return;
    let alive = true;
    call<Grants>('GET', `/api/v2/grants/${friend.user_id}`)
      .then((g) => {
        if (!alive) return;
        setGrants(g);
        setOriginal(JSON.stringify(g));
        setError(undefined);
      })
      .catch((e) => alive && setError(errorText(e)));
    return () => {
      alive = false;
      setGrants(undefined);
    };
  }, [friend]);

  const sharing = !!grants && grants.default.length > 0;
  const setDevice = (id: string, caps: string[] | undefined) =>
    setGrants((g) => {
      if (!g) return g;
      const devices = { ...g.devices };
      if (caps === undefined) delete devices[id];
      else devices[id] = caps;
      return { ...g, devices };
    });

  async function save() {
    if (!friend || !grants) return;
    setBusy(true);
    try {
      await call('PUT', `/api/v2/grants/${friend.user_id}`, grants);
      toast(t.common.saved);
      onClose();
    } catch (err) {
      setError(errorText(err));
    } finally {
      setBusy(false);
    }
  }

  return (
    <Dialog
      open={!!friend}
      title={m.grantsTitle(friend ? displayName(friend) : '')}
      onClose={onClose}
      wide
      footer={
        <>
          <Button onClick={onClose}>{t.common.cancel}</Button>
          <Button
            variant="primary"
            onClick={save}
            loading={busy}
            disabled={!grants || JSON.stringify(grants) === original}
          >
            {t.common.save}
          </Button>
        </>
      }
    >
      {error && <Alert>{error}</Alert>}
      {!grants ? (
        !error && <Loading text={m.loadingGrants} />
      ) : (
        <>
          <div className="stack-sm">
            <h3>{m.allDevices}</h3>
            <div className="row gap-sm">
              <button
                type="button"
                className={`chip ${sharing ? 'on' : ''}`}
                onClick={() => !sharing && setGrants({ ...grants, default: [...DEFAULT_FRIEND] })}
              >
                {m.shared}
              </button>
              <button
                type="button"
                className={`chip ${!sharing ? 'on' : ''}`}
                onClick={() => sharing && setGrants({ ...grants, default: [] })}
              >
                {m.notShared}
              </button>
            </div>
            {sharing && <CapsEditor value={grants.default} onChange={(v) => setGrants({ ...grants, default: v })} />}
          </div>
          <div className="stack-sm">
            <h3>{m.perDevice}</h3>
            {devices.length === 0 && <span className="small muted">{t.account.noDevices}</span>}
            {devices.map((d) => {
              const mode = modeOf(grants, d.device_id);
              const effective = grants.devices[d.device_id] ?? grants.default;
              return (
                <div key={d.device_id} className="grant-device">
                  <div className="row gap">
                    <div className="grow">
                      <div className="ellipsis">{d.alias}</div>
                      <div className="small muted">
                        ID <span className="mono">{deviceIdText(d.device_id)}</span> · {shareSummary(effective)}
                      </div>
                    </div>
                    <Select<Mode>
                      value={mode}
                      width={120}
                      onChange={(next) => {
                        if (next === 'default') setDevice(d.device_id, undefined);
                        else if (next === 'hidden') setDevice(d.device_id, []);
                        else setDevice(d.device_id, grants.default.length ? [...grants.default] : [...DEFAULT_FRIEND]);
                      }}
                      options={[
                        { value: 'default', label: m.followDefault },
                        { value: 'custom', label: m.custom },
                        { value: 'hidden', label: m.notShared },
                      ]}
                    />
                  </div>
                  {mode === 'custom' && (
                    <CapsEditor value={grants.devices[d.device_id]} onChange={(v) => setDevice(d.device_id, v)} />
                  )}
                </div>
              );
            })}
          </div>
        </>
      )}
    </Dialog>
  );
}

export default function Friends() {
  const t = useT();
  const m = t.account.friends;
  const { confirm, toast } = useOverlay();
  const friends = useApi<{ friends: Friend[] }>('/api/v2/friends', 15000);
  const requests = useApi<FriendRequests>('/api/v2/friends/requests', 15000);
  const devices = useApi<DeviceList>('/api/v2/devices');
  const [username, setUsername] = useState('');
  const [busy, setBusy] = useState(false);
  const [editing, setEditing] = useState<UserSummary>();
  // Accepting a request: what the new friend may do on this account's devices.
  const [accepting, setAccepting] = useState<{ request: FriendRequests['incoming'][number]; caps: string[] }>();
  const hosts = (devices.data?.own ?? [])
    .filter((d) => d.capabilities.includes('host') && d.platform !== 'web')
    .sort(byName);

  const reload = async () => {
    await Promise.all([friends.reload(), requests.reload()]);
  };

  async function act(fn: () => Promise<unknown>, done: string) {
    try {
      await fn();
      toast(done);
      await reload();
    } catch (err) {
      toast(errorText(err), 'red');
    }
  }

  async function add(e: FormEvent) {
    e.preventDefault();
    if (!username.trim()) return;
    setBusy(true);
    try {
      const r = await call<{ sent?: unknown; accepted?: unknown }>('POST', '/api/v2/friends/requests', {
        username: username.trim(),
      });
      toast(r.accepted ? m.added : m.requestSent);
      setUsername('');
      await reload();
    } catch (err) {
      toast(errorText(err), 'red');
    } finally {
      setBusy(false);
    }
  }

  const incoming = requests.data?.incoming ?? [];
  const outgoing = requests.data?.outgoing ?? [];

  return (
    <div className="stack">
      <PageHeader
        title={t.nav.friends}
        actions={
          <form className="row gap-sm" onSubmit={add}>
            <Input
              value={username}
              onChange={(e) => setUsername(e.target.value)}
              placeholder={t.account.username}
              style={{ width: 200 }}
            />
            <Button type="submit" variant="primary" icon="userPlus" loading={busy}>
              {m.add}
            </Button>
          </form>
        }
      />
      {(friends.error || requests.error) && <Alert>{friends.error ?? requests.error}</Alert>}
      {incoming.length > 0 && (
        <Card title={m.requests} count={incoming.length} pad={false}>
          <div className="list">
            {incoming.map((r) => (
              <Person
                key={r.id}
                user={r.user}
                actions={
                  <>
                    <Button
                      size="sm"
                      variant="primary"
                      onClick={() => setAccepting({ request: r, caps: [...DEFAULT_FRIEND] })}
                    >
                      {m.accept}
                    </Button>
                    <Button
                      size="sm"
                      onClick={() => act(() => call('POST', `/api/v2/friends/requests/${r.id}/reject`, {}), m.declined)}
                    >
                      {m.decline}
                    </Button>
                  </>
                }
              >
                {m.requestedAt(dateTime(r.created_at))}
              </Person>
            ))}
          </div>
        </Card>
      )}
      <Card title={m.friends} count={friends.data?.friends.length ?? 0} pad={false}>
        {!friends.data ? (
          <Loading />
        ) : friends.data.friends.length === 0 ? (
          <Empty icon="users" title={m.noFriends} />
        ) : (
          <div className="list">
            {friends.data.friends.map((f) => (
              <Person
                key={f.user.user_id}
                user={f.user}
                actions={
                  <>
                    <Button size="sm" icon="share" onClick={() => setEditing(f.user)}>
                      {m.permissions}
                    </Button>
                    <Button
                      size="sm"
                      variant="ghost"
                      onClick={async () => {
                        if (
                          await confirm({
                            title: m.removeTitle,
                            message: m.removeMessage,
                            confirm: m.remove,
                            danger: true,
                          })
                        )
                          await act(() => call('DELETE', `/api/v2/friends/${f.user.user_id}`), m.removed);
                      }}
                    >
                      {m.remove}
                    </Button>
                  </>
                }
              >
                {m.since(dateTime(f.since))}
              </Person>
            ))}
          </div>
        )}
      </Card>
      {outgoing.length > 0 && (
        <Card title={m.sentRequests} count={outgoing.length} pad={false}>
          <div className="list">
            {outgoing.map((r) => (
              <Person
                key={r.id}
                user={r.user}
                actions={
                  <Button
                    size="sm"
                    variant="ghost"
                    onClick={() => act(() => call('DELETE', `/api/v2/friends/requests/${r.id}`), m.withdrawn)}
                  >
                    {m.withdraw}
                  </Button>
                }
              >
                {m.sentAt(dateTime(r.created_at))}
              </Person>
            ))}
          </div>
        </Card>
      )}
      <GrantsEditor friend={editing} devices={hosts} onClose={() => setEditing(undefined)} />
      <Dialog
        open={!!accepting}
        title={accepting ? m.acceptTitle(displayName(accepting.request.user)) : ''}
        onClose={() => setAccepting(undefined)}
        wide
        footer={
          <>
            <Button onClick={() => setAccepting(undefined)}>{t.common.cancel}</Button>
            <Button
              variant="primary"
              onClick={() => {
                if (!accepting) return;
                const { request, caps } = accepting;
                setAccepting(undefined);
                void act(() => call('POST', `/api/v2/friends/requests/${request.id}/accept`, { caps }), m.added);
              }}
            >
              {m.accept}
            </Button>
          </>
        }
      >
        {accepting && (
          <div className="stack-sm">
            <h3>{m.theyMay}</h3>
            <CapsEditor value={accepting.caps} onChange={(caps) => setAccepting({ ...accepting, caps })} />
          </div>
        )}
      </Dialog>
    </div>
  );
}
