// Every device on the server: owner, online state, where it registered;
// sign out (unbind) or drop its connections.
import { useMemo, useState } from 'react';
import { useT } from '../../i18n/react';
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
  const t = useT();
  const m = t.admin.devices;
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
        title={t.nav.allDevices}
        actions={<SearchBox value={query} onChange={setQuery} placeholder={m.searchPlaceholder} />}
      />
      {error && <Alert>{error}</Alert>}
      <Tabs<Filter>
        value={filter}
        onChange={setFilter}
        items={[
          { id: 'all', label: m.all(count('all')) },
          { id: 'online', label: m.online(count('online')) },
          { id: 'hosts', label: m.hosts(count('hosts')) },
          { id: 'guests', label: m.guests(count('guests')) },
        ]}
      />
      <Card pad={false}>
        {!data ? (
          <Loading />
        ) : rows.length === 0 ? (
          <Empty icon="monitor" title={m.noMatch} />
        ) : (
          <div className="table-wrap">
            <table className="table">
              <thead>
                <tr>
                  <th>{t.admin.col.device}</th>
                  <th>{t.admin.col.platform}</th>
                  <th>{t.admin.col.account}</th>
                  <th>{t.admin.col.connectId}</th>
                  <th>{t.admin.col.status}</th>
                  <th>{m.registered}</th>
                  <th />
                </tr>
              </thead>
              <tbody>
                {rows.map((d) => (
                  <tr key={d.device_id}>
                    <td>
                      <div className="cell-main row gap-sm">
                        <span className="ellipsis">{d.alias}</span>
                        {d.host && <Badge>{m.host}</Badge>}
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
                        <span className="muted">{t.admin.signedOut}</span>
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
                          label={m.disconnect}
                          onClick={async () => {
                            if (
                              await confirm({
                                title: m.disconnectTitle,
                                message: m.disconnectMessage(d.alias),
                                confirm: m.disconnectConfirm,
                              })
                            )
                              await act(
                                () => call('POST', `/api/v2/admin/devices/${d.device_id}/disconnect`, {}),
                                m.disconnected,
                              );
                          }}
                        />
                      )}
                      {d.owner_id && (
                        <IconButton
                          icon="unlink"
                          label={t.admin.unbind.label}
                          variant="danger"
                          onClick={async () => {
                            if (
                              await confirm({
                                title: t.admin.unbind.title,
                                message: t.admin.unbind.message(d.alias, d.owner ?? ''),
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
