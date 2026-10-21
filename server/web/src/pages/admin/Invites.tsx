// Invitation codes: create (shown once), revoke, see who registered.
import { useState, type FormEvent } from 'react';
import { call, errorText } from '../../lib/api';
import { sitePath } from '../../lib/base';
import { dateTime } from '../../lib/format';
import { Link } from '../../lib/router';
import type { AccountSummary, Invite, InviteUse, RuntimeSettings } from '../../lib/types';
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
  IconButton,
  Input,
  Loading,
  PageHeader,
  Select,
  type Tone,
} from '../../ui/kit';
import { Dialog, useOverlay } from '../../ui/overlay';
import { OnceDialog } from './Users';

function state(i: Invite): [string, Tone] {
  if (i.revoked_at) return ['已作废', 'muted'];
  if (i.expires_at && i.expires_at * 1000 <= Date.now()) return ['已过期', 'muted'];
  if (i.max_uses !== null && i.uses >= i.max_uses) return ['已用完', 'muted'];
  return ['可用', 'green'];
}

const VALIDITY = [
  { hours: 24, label: '1 天' },
  { hours: 24 * 7, label: '7 天' },
  { hours: 24 * 30, label: '30 天' },
  { hours: 0, label: '永久有效' },
];

function CreateInvite({ open, onClose, onCreated }: { open: boolean; onClose(): void; onCreated(code: string): void }) {
  const [note, setNote] = useState('');
  const [uses, setUses] = useState('1');
  const [hours, setHours] = useState(24 * 7);
  const [autoFriend, setAutoFriend] = useState(true);
  const [error, setError] = useState<string>();
  const [busy, setBusy] = useState(false);

  async function submit(e: FormEvent) {
    e.preventDefault();
    setBusy(true);
    setError(undefined);
    try {
      const r = await call<{ code: string }>('POST', '/api/v2/admin/invites', {
        note: note.trim(),
        max_uses: uses === '' ? null : Number(uses),
        valid_hours: hours || null,
        auto_friend: autoFriend,
      });
      setNote('');
      onCreated(r.code);
    } catch (err) {
      setError(errorText(err));
    } finally {
      setBusy(false);
    }
  }

  return (
    <Dialog
      open={open}
      title="生成邀请码"
      onClose={onClose}
      footer={
        <>
          <Button onClick={onClose}>取消</Button>
          <Button variant="primary" type="submit" form="create-invite" loading={busy}>
            生成
          </Button>
        </>
      }
    >
      <form id="create-invite" className="stack" onSubmit={submit}>
        {error && <Alert>{error}</Alert>}
        <Field label="备注">
          <Input value={note} onChange={(e) => setNote(e.target.value)} maxLength={128} />
        </Field>
        <Field label="可用次数">
          <Input
            type="number"
            min={1}
            max={1000}
            value={uses}
            onChange={(e) => setUses(e.target.value)}
            placeholder="不限"
          />
        </Field>
        <Field label="有效期">
          <Select value={hours} onChange={(e) => setHours(Number(e.target.value))}>
            {VALIDITY.map((v) => (
              <option key={v.hours} value={v.hours}>
                {v.label}
              </option>
            ))}
          </Select>
        </Field>
        <Checkbox checked={autoFriend} onChange={setAutoFriend} label="自动加我为好友" />
      </form>
    </Dialog>
  );
}

function Uses({ invite, onClose }: { invite: Invite | undefined; onClose(): void }) {
  const { data, error } = useApi<InviteUse[]>(invite ? `/api/v2/admin/invites/${invite.id}/uses` : null);
  return (
    <Dialog open={!!invite} title={`使用记录 · ${invite?.note || '邀请码'}`} onClose={onClose} wide>
      {error && <Alert>{error}</Alert>}
      {!data ? (
        <Loading />
      ) : data.length === 0 ? (
        <Empty icon="ticket" title="暂无使用记录" />
      ) : (
        <table className="table">
          <thead>
            <tr>
              <th>注册的账号</th>
              <th>时间</th>
              <th>IP</th>
            </tr>
          </thead>
          <tbody>
            {data.map((u) => (
              <tr key={u.user_id + u.used_at}>
                <td>
                  {u.username ? (
                    <Link to={`/admin/users/${u.user_id}`}>{u.username}</Link>
                  ) : (
                    <span className="muted">已删除</span>
                  )}
                </td>
                <td className="nowrap">{dateTime(u.used_at)}</td>
                <td className="mono small">{u.ip}</td>
              </tr>
            ))}
          </tbody>
        </table>
      )}
    </Dialog>
  );
}

export default function Invites() {
  const { confirm, toast } = useOverlay();
  const { data, error, reload } = useApi<Invite[]>('/api/v2/admin/invites');
  const users = useApi<AccountSummary[]>('/api/v2/admin/users');
  const settings = useApi<{ runtime: RuntimeSettings }>('/api/v2/admin/settings');
  const [creating, setCreating] = useState(false);
  const [code, setCode] = useState<string>();
  const [viewing, setViewing] = useState<Invite>();
  const names = new Map((users.data ?? []).map((u) => [u.id, u.username]));
  const link = code ? `${location.origin}${sitePath(`/register?code=${encodeURIComponent(code)}`)}` : '';

  return (
    <div className="stack">
      <PageHeader
        title="邀请码"
        actions={
          <Button variant="primary" icon="plus" onClick={() => setCreating(true)}>
            生成邀请码
          </Button>
        }
      />
      {settings.data && !settings.data.runtime.registration && (
        <Alert tone="amber">
          <Link to="/admin/settings">邀请码注册</Link>已关闭
        </Alert>
      )}
      {error && <Alert>{error}</Alert>}
      <Card pad={false}>
        {!data ? (
          <Loading />
        ) : data.length === 0 ? (
          <Empty icon="ticket" title="暂无邀请码" />
        ) : (
          <div className="table-wrap">
            <table className="table">
              <thead>
                <tr>
                  <th>备注</th>
                  <th>状态</th>
                  <th>已用</th>
                  <th>有效期至</th>
                  <th>自动加好友</th>
                  <th>创建</th>
                  <th />
                </tr>
              </thead>
              <tbody>
                {data.map((i) => {
                  const [text, tone] = state(i);
                  return (
                    <tr key={i.id}>
                      <td>
                        <div className="cell-main">{i.note || <span className="muted">（无备注）</span>}</div>
                        <div className="cell-sub">由 {names.get(i.created_by) ?? '已删除的账号'} 生成</div>
                      </td>
                      <td>
                        <Badge tone={tone}>{text}</Badge>
                      </td>
                      <td>
                        {i.uses} / {i.max_uses ?? '不限'}
                      </td>
                      <td className="nowrap">{i.expires_at ? dateTime(i.expires_at) : '永久'}</td>
                      <td>{i.auto_friend ? '是' : '否'}</td>
                      <td className="nowrap muted">{dateTime(i.created_at)}</td>
                      <td className="actions">
                        <IconButton icon="log" label="使用记录" onClick={() => setViewing(i)} />
                        {!i.revoked_at && (
                          <IconButton
                            icon="trash"
                            label="作废"
                            variant="danger"
                            onClick={async () => {
                              if (
                                await confirm({
                                  title: '作废邀请码',
                                  message: '作废后不能再用于注册。',
                                  confirm: '作废',
                                  danger: true,
                                })
                              ) {
                                try {
                                  await call('DELETE', `/api/v2/admin/invites/${i.id}`);
                                  toast('已作废');
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
      <CreateInvite
        open={creating}
        onClose={() => setCreating(false)}
        onCreated={(c) => {
          setCreating(false);
          setCode(c);
          void reload();
        }}
      />
      <OnceDialog title="邀请码已生成" open={!!code} onClose={() => setCode(undefined)}>
        <Field label="邀请码">
          <div className="secret">
            <Copy text={code ?? ''} />
          </div>
        </Field>
        <Field label="注册链接">
          <div className="secret small">
            <Copy text={link} />
          </div>
        </Field>
      </OnceDialog>
      <Uses invite={viewing} onClose={() => setViewing(undefined)} />
    </div>
  );
}
