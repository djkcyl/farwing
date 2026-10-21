// Every device on the server: owner, online state, where it registered;
// sign out (unbind) or drop its connections.
import { useMemo, useState } from 'react';
import { call, errorText } from '../../lib/api';
import { byName } from '../../lib/devices';
import { ago, connectIdText, dateTime, deviceIdText, platformText } from '../../lib/format';
import { Link } from '../../lib/router';
import type { AdminDevice } from '../../lib/types';
import { useApi } from '../../lib/useApi';
import { Alert, Badge, Card, Empty, IconButton, Loading, Online, PageHeader, SearchBox, Tabs } from '../../ui/kit';
import { useOverlay } from '../../ui/overlay';

type Filter = 'all' | 'online' | 'hosts' | 'guests';

export default function AdminDevices() {
  const { confirm, toast } = useOverlay();
  const { data, error, reload } = useApi<AdminDevice[]>('/api/v2/admin/devices', 10000);
  const [query, setQuery] = useState('');
  const [filter, setFilter] = useState<Filter>('all');

  const rows = useMemo(() => {
    const q = query.trim().toLowerCase().replace(/\s/g, '');
    return (data ?? [])
      .filter((d) => {
        if (filter === 'online' && !d.online) return false;
        if (filter === 'hosts' && !d.host) return false;
        if (filter === 'guests' && d.owner_id) return false;
        if (!q) return true;
        return [d.alias, d.name, d.device_id, d.connect_id ?? '', d.owner ?? '', d.created_ip].some((v) =>
          v.toLowerCase().replace(/\s/g, '').includes(q),
        );
      })
      .sort(byName);
  }, [data, query, filter]);

  async function act(fn: () => Promise<unknown>, done: string) {
    try {
      await fn();
      toast(done);
      await reload();
    } catch (err) {
      toast(errorText(err), 'red');
    }
  }

  const count = (f: Filter) =>
    (data ?? []).filter((d) =>
      f === 'online' ? d.online : f === 'hosts' ? d.host : f === 'guests' ? !d.owner_id : true,
    ).length;

  return (
    <div className="stack">
      <PageHeader
        title="全部设备"
        subtitle="在本服务器注册过的所有设备。"
        actions={<SearchBox value={query} onChange={setQuery} placeholder="名称、设备 ID、短号、账号、IP" />}
      />
      {error && <Alert>{error}</Alert>}
      <Tabs<Filter>
        value={filter}
        onChange={setFilter}
        items={[
          { id: 'all', label: `全部 ${count('all')}` },
          { id: 'online', label: `在线 ${count('online')}` },
          { id: 'hosts', label: `可被控 ${count('hosts')}` },
          { id: 'guests', label: `未登录账号 ${count('guests')}` },
        ]}
      />
      <Card pad={false}>
        {!data ? (
          <Loading />
        ) : rows.length === 0 ? (
          <Empty icon="monitor" title="没有匹配的设备" />
        ) : (
          <div className="table-wrap">
            <table className="table">
              <thead>
                <tr>
                  <th>设备</th>
                  <th>平台</th>
                  <th>账号</th>
                  <th>短号</th>
                  <th>状态</th>
                  <th>注册</th>
                  <th />
                </tr>
              </thead>
              <tbody>
                {rows.map((d) => (
                  <tr key={d.device_id}>
                    <td>
                      <div className="cell-main row gap-sm">
                        <span className="ellipsis">{d.alias}</span>
                        {d.host && <Badge>可被控</Badge>}
                      </div>
                      <div className="cell-sub mono">{deviceIdText(d.device_id)}</div>
                    </td>
                    <td>
                      <div>{platformText(d.platform)}</div>
                      <div className="cell-sub ellipsis" style={{ maxWidth: 240 }} title={d.os}>
                        {d.version} {d.os && `· ${d.os}`}
                      </div>
                    </td>
                    <td>
                      {d.owner_id ? (
                        <Link to={`/admin/users/${d.owner_id}`}>{d.owner}</Link>
                      ) : (
                        <span className="muted">未登录</span>
                      )}
                    </td>
                    <td className="mono nowrap">{connectIdText(d.connect_id) || <span className="muted">—</span>}</td>
                    <td className="nowrap">
                      <Online online={d.online} />
                      {!d.online && <div className="cell-sub">{ago(d.last_seen_at)}</div>}
                    </td>
                    <td>
                      <div className="nowrap">{dateTime(d.created_at)}</div>
                      <div className="cell-sub mono">{d.created_ip || '—'}</div>
                    </td>
                    <td className="actions">
                      {d.online && (
                        <IconButton
                          icon="power"
                          label="断开连接"
                          onClick={async () => {
                            if (
                              await confirm({
                                title: '断开连接',
                                message: `断开 ${d.alias} 与服务器的所有连接（含进行中的会话）。登录仍然有效，设备会自动重连。`,
                                confirm: '断开',
                              })
                            )
                              await act(
                                () => call('POST', `/api/v2/admin/devices/${d.device_id}/disconnect`, {}),
                                '已断开',
                              );
                          }}
                        />
                      )}
                      {d.owner_id && (
                        <IconButton
                          icon="unlink"
                          label="解绑并注销"
                          variant="danger"
                          onClick={async () => {
                            if (
                              await confirm({
                                title: '解绑设备',
                                message: `${d.alias} 将从 ${d.owner} 的账号移除并退出登录，需要重新登录才能使用账号功能。`,
                                confirm: '解绑',
                                danger: true,
                              })
                            )
                              await act(() => call('DELETE', `/api/v2/admin/devices/${d.device_id}`), '已解绑');
                          }}
                        />
                      )}
                    </td>
                  </tr>
                ))}
              </tbody>
            </table>
          </div>
        )}
      </Card>
    </div>
  );
}
