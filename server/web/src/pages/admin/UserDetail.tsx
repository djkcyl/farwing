// One account: role, status, password, two-step verification, devices,
// logins, friends and what they share.
import { useState } from 'react';
import { call, errorText } from '../../lib/api';
import { byName } from '../../lib/devices';
import { useAuth } from '../../lib/auth';
import { ago, capText, connectIdText, dateTime, deviceIdText, platformText } from '../../lib/format';
import { Link, navigate } from '../../lib/router';
import type { AdminFriend, Grants, UserDetail as Detail } from '../../lib/types';
import { useApi } from '../../lib/useApi';
import { Icon } from '../../ui/icons';
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
  Loading,
  Online,
  PageHeader,
  Row,
  Rows,
} from '../../ui/kit';
import { Dialog, useOverlay } from '../../ui/overlay';
import { OnceDialog } from './Users';

function grantsText(g: Grants, devices: Map<string, string>): string {
  const parts: string[] = [];
  parts.push(g.default.length ? `默认：${g.default.map(capText).join('、')}` : '默认不共享');
  const specific = Object.entries(g.devices);
  if (specific.length) {
    parts.push(
      ...specific.map(
        ([id, caps]) =>
          `${devices.get(id) ?? deviceIdText(id)}：${caps.length ? caps.map(capText).join('、') : '不共享'}`,
      ),
    );
  }
  return parts.join('；');
}

function FriendRow({ f, devices }: { f: AdminFriend; devices: Map<string, string> }) {
  return (
    <tr>
      <td>
        <div className="cell-main">{f.nickname || f.username}</div>
        <div className="cell-sub">{f.username}</div>
      </td>
      <td className="small">{grantsText(f.grants_given, devices)}</td>
      <td className="small">{grantsText(f.grants_received, new Map())}</td>
      <td className="nowrap muted">{dateTime(f.since)}</td>
    </tr>
  );
}

export default function UserDetail({ params }: { params: Record<string, string> }) {
  const { me } = useAuth();
  const { confirm, toast } = useOverlay();
  const { data, error, reload } = useApi<Detail>(`/api/v2/admin/users/${params.id}`);
  const [password, setPassword] = useState<string>();
  const [renaming, setRenaming] = useState(false);
  const [nickname, setNickname] = useState('');

  if (!data) return error ? <Alert>{error}</Alert> : <Loading />;
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
            <Icon name="back" size={14} /> 账号
          </Link>
        }
        title={
          <span className="row gap-sm">
            {a.nickname || a.username}
            {a.role === 'admin' && <Badge tone="blue">管理员</Badge>}
            {a.disabled && <Badge tone="red">已禁用</Badge>}
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
              修改昵称
            </Button>
            <Button
              icon="key"
              onClick={async () => {
                if (
                  await confirm({
                    title: '重置密码',
                    message: `${a.username} 的所有登录将被注销${self ? '，包括当前登录' : ''}。`,
                    confirm: '重置',
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
              重置密码
            </Button>
            {!self && (
              <Button
                onClick={async () => {
                  const make = a.role !== 'admin';
                  if (
                    await confirm({
                      title: make ? '设为管理员' : '取消管理员',
                      message: make ? `${a.username} 将成为管理员。` : `${a.username} 将不再是管理员。`,
                      confirm: make ? '设为管理员' : '取消管理员',
                    })
                  )
                    await act(() => patch({ admin: make }), make ? '已设为管理员' : '已取消管理员');
                }}
              >
                {a.role === 'admin' ? '取消管理员' : '设为管理员'}
              </Button>
            )}
            {!self && (
              <Button
                variant={a.disabled ? 'default' : 'danger'}
                onClick={async () => {
                  if (a.disabled) {
                    await act(() => patch({ disabled: false }), '已启用');
                  } else if (
                    await confirm({
                      title: '禁用账号',
                      message: `${a.username} 的登录与连接将立即结束。`,
                      confirm: '禁用',
                      danger: true,
                    })
                  ) {
                    await act(() => patch({ disabled: true }), '已禁用');
                  }
                }}
              >
                {a.disabled ? '启用' : '禁用'}
              </Button>
            )}
          </>
        }
      />

      <div className="grid grid-2">
        <Card title="账号">
          <Rows>
            <Row label="用户名">{a.username}</Row>
            <Row label="昵称">{a.nickname || <span className="muted">未设置</span>}</Row>
            <Row label="用户 ID">
              <Copy text={a.id} display={<span className="small">{a.id}</span>} />
            </Row>
            <Row label="创建时间">{dateTime(a.created_at)}</Row>
            <Row label="最近在线">{a.last_seen_at ? ago(a.last_seen_at) : '从未'}</Row>
            <Row label="二次验证">
              {a.totp ? (
                <span className="row gap-sm">
                  <Badge tone="green">已开启</Badge>
                  <Button
                    size="sm"
                    onClick={async () => {
                      if (
                        await confirm({
                          title: '重置二次验证',
                          message: `${a.username} 的二次验证将被关闭。`,
                          confirm: '重置',
                          danger: true,
                        })
                      )
                        await act(() => call('POST', `/api/v2/admin/users/${a.id}/totp/reset`, {}), '已重置二次验证');
                    }}
                  >
                    重置
                  </Button>
                </span>
              ) : (
                <span className="muted">未开启</span>
              )}
            </Row>
          </Rows>
        </Card>
        <Card title={`登录（${data.logins.length}）`} pad={false}>
          {data.logins.length === 0 ? (
            <Empty icon="lock" title="暂无登录" />
          ) : (
            <div className="list">
              {data.logins.map((l) => (
                <div className="list-item" key={l.device_id}>
                  <div className="grow">
                    <div className="ellipsis">{l.alias}</div>
                    <div className="small muted">
                      {platformText(l.platform)} · 登录于 {dateTime(l.created_at)}
                    </div>
                  </div>
                  <span className="small faint nowrap">{ago(l.last_used_at)}使用</span>
                </div>
              ))}
            </div>
          )}
        </Card>
      </div>

      <Card title={`设备（${data.devices.length}）`} pad={false}>
        {data.devices.length === 0 ? (
          <Empty icon="monitor" title="暂无设备" />
        ) : (
          <div className="table-wrap">
            <table className="table">
              <thead>
                <tr>
                  <th>设备</th>
                  <th>平台</th>
                  <th>短号</th>
                  <th>状态</th>
                  <th>最近在线</th>
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
                        label="解绑并注销"
                        variant="danger"
                        onClick={async () => {
                          if (
                            await confirm({
                              title: '解绑设备',
                              message: `${d.alias} 将从 ${a.username} 的账号移除并退出登录。`,
                              confirm: '解绑',
                              danger: true,
                            })
                          )
                            await act(() => call('DELETE', `/api/v2/admin/devices/${d.device_id}`), '已解绑');
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

      <Card title={`好友与授权（${data.friends.length}）`} pad={false}>
        {data.friends.length === 0 ? (
          <Empty icon="users" title="暂无好友" />
        ) : (
          <div className="table-wrap">
            <table className="table">
              <thead>
                <tr>
                  <th>好友</th>
                  <th>共享给对方</th>
                  <th>对方共享</th>
                  <th>成为好友</th>
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

      {!self && (
        <Card title="删除账号">
          <div className="row gap wrap">
            <Button
              variant="danger"
              icon="trash"
              onClick={async () => {
                if (
                  await confirm({
                    title: '删除账号',
                    message: `将永久删除 ${a.username}。`,
                    confirm: '删除',
                    danger: true,
                    typeToConfirm: a.username,
                  })
                ) {
                  try {
                    await call('DELETE', `/api/v2/admin/users/${a.id}`);
                    toast(`已删除 ${a.username}`);
                    navigate('/admin/users');
                  } catch (err) {
                    toast(errorText(err), 'red');
                  }
                }
              }}
            >
              删除账号
            </Button>
          </div>
        </Card>
      )}

      <OnceDialog title="密码已重置" open={!!password} onClose={() => setPassword(undefined)}>
        <p>{a.username} 的新密码：</p>
        <div className="secret">
          <Copy text={password ?? ''} />
        </div>
      </OnceDialog>
      <Dialog
        open={renaming}
        title="修改昵称"
        onClose={() => setRenaming(false)}
        footer={
          <>
            <Button onClick={() => setRenaming(false)}>取消</Button>
            <Button
              variant="primary"
              onClick={async () => {
                setRenaming(false);
                await act(() => patch({ nickname }), '已修改昵称');
              }}
            >
              保存
            </Button>
          </>
        }
      >
        <Field label="昵称">
          <Input value={nickname} onChange={(e) => setNickname(e.target.value)} maxLength={64} />
        </Field>
      </Dialog>
    </div>
  );
}
