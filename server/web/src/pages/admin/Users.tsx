// Accounts: search, create (with a typed or generated password).
import { useMemo, useState, type FormEvent, type ReactNode } from 'react';
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
  return (
    <Dialog
      open={open}
      title={title}
      onClose={onClose}
      footer={
        <Button variant="primary" onClick={onClose}>
          完成
        </Button>
      }
    >
      {children}
      <Alert tone="amber">关闭后不会再显示，请现在复制并安全地交给对方。</Alert>
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
      title="创建账号"
      onClose={onClose}
      footer={
        <>
          <Button onClick={onClose}>取消</Button>
          <Button variant="primary" type="submit" form="create-user" loading={busy}>
            创建
          </Button>
        </>
      }
    >
      <form id="create-user" className="stack" onSubmit={submit}>
        {error && <Alert>{error}</Alert>}
        <Field label="用户名" hint="1–64 位字母、数字或 . _ - @">
          <Input value={username} onChange={(e) => setUsername(e.target.value)} required />
        </Field>
        <Field label="昵称">
          <Input value={nickname} onChange={(e) => setNickname(e.target.value)} maxLength={64} />
        </Field>
        <Field label="初始密码" hint="留空则生成随机密码">
          <Input
            type="password"
            autoComplete="new-password"
            value={password}
            onChange={(e) => setPassword(e.target.value)}
          />
        </Field>
        <Checkbox checked={admin} onChange={setAdmin} label="设为管理员" />
      </form>
    </Dialog>
  );
}

export default function Users() {
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
        title="账号"
        subtitle="账号可在客户端与网页端登录。"
        actions={
          <>
            <SearchBox value={query} onChange={setQuery} placeholder="搜索用户名或昵称" />
            <Button variant="primary" icon="plus" onClick={() => setCreating(true)}>
              创建账号
            </Button>
          </>
        }
      />
      {error && <Alert>{error}</Alert>}
      <Card pad={false}>
        {!data ? (
          <Loading />
        ) : rows.length === 0 ? (
          <Empty icon="user" title={query ? '没有匹配的账号' : '还没有账号'} />
        ) : (
          <div className="table-wrap">
            <table className="table">
              <thead>
                <tr>
                  <th>账号</th>
                  <th>角色</th>
                  <th>状态</th>
                  <th>设备</th>
                  <th>最近在线</th>
                  <th>创建</th>
                </tr>
              </thead>
              <tbody>
                {rows.map((u) => (
                  <tr key={u.id} className="clickable" onClick={() => navigate(`/admin/users/${u.id}`)}>
                    <td>
                      <div className="cell-main">
                        {u.nickname || u.username}
                        {u.id === me?.user_id && <span className="muted small">（我）</span>}
                      </div>
                      <div className="cell-sub">{u.username}</div>
                    </td>
                    <td>
                      {u.role === 'admin' ? <Badge tone="blue">管理员</Badge> : <span className="muted">用户</span>}
                      {u.totp && <span className="muted small"> · 二次验证</span>}
                    </td>
                    <td>{u.disabled ? <Badge tone="red">已禁用</Badge> : <Badge tone="green">正常</Badge>}</td>
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
          else toast(`已创建 ${username}`);
        }}
      />
      <OnceDialog title="账号已创建" open={!!created} onClose={() => setCreated(undefined)}>
        <p>
          账号 <b>{created?.username}</b> 的初始密码：
        </p>
        <div className="secret">
          <Copy text={created?.password ?? ''} />
        </div>
      </OnceDialog>
    </div>
  );
}
