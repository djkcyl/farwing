// The user centre's first page: online devices, who is connected to mine,
// open friend requests, recent logins.
import { useT } from '../../i18n/react';
import { ago, platformText, purposeText, relationText } from '../../lib/format';
import { Link } from '../../lib/router';
import type { AuthSession, DeviceList, FriendRequests } from '../../lib/types';
import { useApi } from '../../lib/useApi';
import { DeviceCard } from '../../ui/DeviceCard';
import { Badge, Card, Empty, Loading, PageHeader, PageState, Stat } from '../../ui/kit';

export default function Home() {
  const t = useT();
  const m = t.account.home;
  const devices = useApi<DeviceList>('/api/v2/devices', 10000);
  const requests = useApi<FriendRequests>('/api/v2/friends/requests', 30000);
  const logins = useApi<{ sessions: AuthSession[] }>('/api/v2/auth/sessions');
  const d = devices.data;
  if (!d) return <PageState title={t.nav.home} error={devices.error} />;

  const hosts = d.own.filter((x) => x.platform !== 'web' && x.capabilities.includes('host'));
  const online = [...hosts, ...d.shared].filter((x) => x.status === 'online');
  const connections = hosts.flatMap((h) => h.participants.map((p) => ({ host: h.alias, p })));
  const incoming = requests.data?.incoming.length ?? 0;

  return (
    <div className="stack">
      <PageHeader title={t.nav.home} />
      <div className="grid grid-stats">
        <Stat
          label={m.myComputersOnline}
          value={`${hosts.filter((x) => x.status === 'online').length} / ${hosts.length}`}
          tone={hosts.some((x) => x.status === 'online') ? 'green' : undefined}
        />
        <Stat
          label={m.sharedByFriends}
          value={d.shared.length}
          hint={m.onlineCount(d.shared.filter((x) => x.status === 'online').length)}
        />
        <Stat label={m.connectedToMine} value={connections.length} tone={connections.length ? 'amber' : undefined} />
        <Stat
          label={m.friendRequests}
          value={incoming}
          hint={incoming ? <Link to="/friends">{m.viewRequests}</Link> : undefined}
        />
      </div>

      <div>
        <div className="section-title">
          <h2>{m.onlineDevices}</h2>
          <Link to="/devices">{m.allDevices}</Link>
        </div>
        {online.length === 0 ? (
          <Card>
            <Empty icon="monitor" title={m.noOnlineDevices} />
          </Card>
        ) : (
          <div className="device-grid">
            {online.slice(0, 6).map((x) => (
              <DeviceCard key={x.device_id} device={x} shared={!!x.owner} />
            ))}
          </div>
        )}
      </div>

      <div className="grid grid-2">
        <Card title={m.connectedToMine} pad={false}>
          {connections.length === 0 ? (
            <Empty icon="activity" title={t.account.noConnections} />
          ) : (
            <div className="list">
              {connections.map(({ host, p }) => (
                <div className="list-item" key={p.participant_id}>
                  <div className="grow">
                    <div>
                      {p.nickname || t.common.guest} <span className="muted">→ {host}</span>
                    </div>
                    <div className="small muted">
                      {p.device_alias} · {relationText(p.relation)} · {purposeText(p.purpose)}
                    </div>
                  </div>
                  <span className="small faint">{ago(p.started_at)}</span>
                </div>
              ))}
            </div>
          )}
        </Card>
        <Card title={m.recentLogins} actions={<Link to="/account">{t.common.manage}</Link>} pad={false}>
          {!logins.data ? (
            <Loading />
          ) : (
            <div className="list">
              {[...logins.data.sessions]
                .sort((a, b) => b.last_used_at - a.last_used_at)
                .slice(0, 5)
                .map((s) => (
                  <div className="list-item" key={s.device_id}>
                    <div className="grow">
                      <div className="row gap-sm">
                        <span className="ellipsis">{s.device_alias}</span>
                        {s.current && <Badge tone="blue">{t.common.thisBrowser}</Badge>}
                      </div>
                      <div className="small muted">{platformText(s.platform)}</div>
                    </div>
                    <span className="small faint">{ago(s.last_used_at)}</span>
                  </div>
                ))}
            </div>
          )}
        </Card>
      </div>
    </div>
  );
}
