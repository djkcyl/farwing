// The user centre's first page: online devices, who is connected to mine,
// open friend requests, recent logins.
import { useAuth } from '../../lib/auth';
import { ago, displayName, platformText, PURPOSES, RELATIONS } from '../../lib/format';
import { Link } from '../../lib/router';
import type { AuthSession, DeviceList, FriendRequests } from '../../lib/types';
import { useApi } from '../../lib/useApi';
import { DeviceCard } from '../../ui/DeviceCard';
import { Alert, Badge, Card, Empty, Loading, PageHeader, Stat } from '../../ui/kit';

export default function Home() {
  const auth = useAuth();
  const devices = useApi<DeviceList>('/api/v2/devices', 10000);
  const requests = useApi<FriendRequests>('/api/v2/friends/requests', 30000);
  const logins = useApi<{ sessions: AuthSession[] }>('/api/v2/auth/sessions');
  const d = devices.data;
  if (!d) return devices.error ? <Alert>{devices.error}</Alert> : <Loading />;

  const hosts = d.own.filter((x) => x.platform !== 'web' && x.capabilities.includes('host'));
  const online = [...hosts, ...d.shared].filter((x) => x.status === 'online');
  const connections = hosts.flatMap((h) => h.participants.map((p) => ({ host: h.alias, p })));
  const incoming = requests.data?.incoming.length ?? 0;

  return (
    <div className="stack">
      <PageHeader title={`你好，${displayName(auth.me)}`} subtitle={auth.info?.instance_name} />
      <div className="grid grid-stats">
        <Stat
          label="我的电脑在线"
          value={`${hosts.filter((x) => x.status === 'online').length} / ${hosts.length}`}
          tone={hosts.some((x) => x.status === 'online') ? 'green' : undefined}
        />
        <Stat
          label="好友共享"
          value={d.shared.length}
          hint={`${d.shared.filter((x) => x.status === 'online').length} 台在线`}
        />
        <Stat label="正在连接我的电脑" value={connections.length} tone={connections.length ? 'amber' : undefined} />
        <Stat label="好友申请" value={incoming} hint={incoming ? <Link to="/friends">去处理</Link> : undefined} />
      </div>

      <div>
        <div className="section-title">
          <h2>在线的设备</h2>
          <Link to="/devices">全部设备</Link>
        </div>
        {online.length === 0 ? (
          <Card>
            <Empty icon="monitor" title="暂无在线设备" />
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
        <Card title="正在连接我的电脑" pad={false}>
          {connections.length === 0 ? (
            <Empty icon="activity" title="没有人在连接" />
          ) : (
            <div className="list">
              {connections.map(({ host, p }) => (
                <div className="list-item" key={p.participant_id}>
                  <div className="grow">
                    <div>
                      {p.nickname || '访客'} <span className="muted">→ {host}</span>
                    </div>
                    <div className="small muted">
                      {p.device_alias} · {RELATIONS[p.relation] ?? p.relation} · {PURPOSES[p.purpose] ?? p.purpose}
                    </div>
                  </div>
                  <span className="small faint">{ago(p.started_at)}</span>
                </div>
              ))}
            </div>
          )}
        </Card>
        <Card title="最近登录" actions={<Link to="/account">管理</Link>} pad={false}>
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
                        {s.current && <Badge tone="blue">这个浏览器</Badge>}
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
