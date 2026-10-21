// Accounts: search, create (with a typed or generated password).
import { useMemo, useState, type FormEvent, type ReactNode } from 'react';
import { useT } from '../../i18n/react';
import { call, errorText } from '../../lib/api';
import { useAuth } from '../../lib/auth';
import { ago, date } from '../../lib/format';
import { navigate } from '../../lib/router';
import type { AccountSummary } from '../../lib/types';
import { useApi } from '../../lib/useApi';
import {
  Alert,
  Badge,
  Button,
  Card,
  Checkbox,
  Copy,
  Empty,
  Field,
  Input,
  Loading,
  PageHeader,
  SearchBox,
} from '../../ui/kit';
import { Dialog, useOverlay } from '../../ui/overlay';

/// A secret shown once (generated password, invitation code).
export function OnceDialog({
  title,
  open,
  onClose,
  children,
}: {
  title: string;
  open: boolean;
  onClose(): void;
  children: ReactNode;
}) {
  const t = useT();
  return (
    <Dialog
      open={open}
      title={title}
      onClose={onClose}
      footer={
        <Button variant="primary" onClick={onClose}>
          {t.common.done}
        </Button>
      }
    >
      {children}
      <Alert tone="amber">{t.admin.users.shownOnce}</Alert>
    </Dialog>
  );
}

function CreateUser({
  open,
  onClose,
  onCreated,
}: {
  open: boolean;
  onClose(): void;
  onCreated(username: string, password: string | null): void;
}) {
  const t = useT();
  const m = t.admin.users;
  const [username, setUsername] = useState('');
  const [nickname, setNickname] = useState('');
  const [password, setPassword] = useState('');
  const [admin, setAdmin] = useState(false);
  const [error, setError] = useState<string>();
  const [busy, setBusy] = useState(false);

  async function submit(e: FormEvent) {
    e.preventDefault();
    setBusy(true);
    setError(undefined);
    try {
      const r = await call<{ id: string; password: string | null }>('POST', '/api/v2/admin/users', {
        username: username.trim(),
        nickname: nickname.trim(),
        password,
        admin,
      });
      onCreated(username.trim(), r.password);
      setUsername('');
      setNickname('');
      setPassword('');
      setAdmin(false);
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
          <Button variant="primary" type="submit" form="create-user" loading={busy}>
            {t.common.create}
          </Button>
        </>
      }
    >
      <form id="create-user" className="stack" onSubmit={submit}>
        {error && <Alert>{error}</Alert>}
        <Field label={m.username} hint={t.auth.register.usernameHint}>
          <Input value={username} onChange={(e) => setUsername(e.target.value)} required />
        </Field>
        <Field label={m.nickname}>
          <Input value={nickname} onChange={(e) => setNickname(e.target.value)} maxLength={64} />
        </Field>
        <Field label={m.initialPassword}>
          <Input
            type="password"
            autoComplete="new-password"
            value={password}
            onChange={(e) => setPassword(e.target.value)}
            placeholder={m.generated}
          />
        </Field>
        <Checkbox checked={admin} onChange={setAdmin} label={m.makeAdmin} />
      </form>
    </Dialog>
  );
}

export default function Users() {
  const t = useT();
  const m = t.admin.users;
  const { me } = useAuth();
  const { toast } = useOverlay();
  const { data, error, reload } = useApi<AccountSummary[]>('/api/v2/admin/users');
  const [query, setQuery] = useState('');
  const [creating, setCreating] = useState(false);
  const [created, setCreated] = useState<{ username: string; password: string }>();

  const rows = useMemo(() => {
    const q = query.trim().toLowerCase();
    return (data ?? []).filter(
      (u) => !q || u.username.toLowerCase().includes(q) || u.nickname.toLowerCase().includes(q),
    );
  }, [data, query]);

  return (
    <div className="stack">
      <PageHeader
        title={t.nav.users}
        actions={
          <>
            <SearchBox value={query} onChange={setQuery} placeholder={m.searchPlaceholder} />
            <Button variant="primary" icon="plus" onClick={() => setCreating(true)}>
              {m.create}
            </Button>
          </>
        }
      />
      {error && <Alert>{error}</Alert>}
      <Card pad={false}>
        {!data ? (
          <Loading />
        ) : rows.length === 0 ? (
          <Empty icon="user" title={query ? m.noMatch : m.empty} />
        ) : (
          <div className="table-wrap">
            <table className="table">
              <thead>
                <tr>
                  <th>{t.admin.col.account}</th>
                  <th>{m.role}</th>
                  <th>{t.admin.col.status}</th>
                  <th>{m.devices}</th>
                  <th>{t.admin.col.lastSeen}</th>
                  <th>{t.admin.col.created}</th>
                </tr>
              </thead>
              <tbody>
                {rows.map((u) => (
                  <tr key={u.id} className="clickable" onClick={() => navigate(`/admin/users/${u.id}`)}>
                    <td>
                      <div className="cell-main">
                        {u.nickname || u.username}
                        {u.id === me?.user_id && <span className="muted small">{m.you}</span>}
                      </div>
                      <div className="cell-sub">{u.username}</div>
                    </td>
                    <td>
                      {u.role === 'admin' ? (
                        <Badge tone="blue">{t.common.admin}</Badge>
                      ) : (
                        <span className="muted">{t.common.user}</span>
                      )}
                      {u.totp && <span className="muted small"> · {m.totp}</span>}
                    </td>
                    <td>
                      {u.disabled ? <Badge tone="red">{m.disabled}</Badge> : <Badge tone="green">{m.active}</Badge>}
                    </td>
                    <td>{u.devices}</td>
                    <td className="nowrap">
                      {u.last_seen_at ? ago(u.last_seen_at) : <span className="muted">—</span>}
                    </td>
                    <td className="nowrap muted">{date(u.created_at)}</td>
                  </tr>
                ))}
              </tbody>
            </table>
          </div>
        )}
      </Card>
      <CreateUser
        open={creating}
        onClose={() => setCreating(false)}
        onCreated={(username, password) => {
          setCreating(false);
          void reload();
          if (password) setCreated({ username, password });
          else toast(m.created(username));
        }}
      />
      <OnceDialog title={m.createdTitle} open={!!created} onClose={() => setCreated(undefined)}>
        <p>
          {m.initialPasswordOf[0]}
          <b>{created?.username}</b>
          {m.initialPasswordOf[1]}
        </p>
        <div className="secret">
          <Copy text={created?.password ?? ''} />
        </div>
      </OnceDialog>
    </div>
  );
}
