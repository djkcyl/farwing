// Server overview: live counts, server facts and the latest audit events.
import { auditKindText, bytes, dateTime, duration } from '../../lib/format';
import { Link } from '../../lib/router';
import type { Overview } from '../../lib/types';
import { useApi } from '../../lib/useApi';
import { Alert, Badge, Card, Copy, Empty, Loading, PageHeader, Row, Rows, Stat } from '../../ui/kit';

const TURN_MODES: Record<string, string> = {
  builtin: '内置 TURN',
  external: '外部 TURN',
  none: '只有 STUN（不中继）',
};

export default function AdminOverview() {
  const { data, error } = useApi<Overview>('/api/v2/admin/overview', 5000);
  if (!data) return error ? <Alert>{error}</Alert> : <Loading />;
  return (
    <div className="stack">
      <PageHeader title="服务器概览" subtitle={`${data.instance_name} · Farwing Server ${data.version}`} />
      {error && <Alert tone="amber">刷新失败：{error}</Alert>}
      <div className="grid grid-stats">
        <Stat
          label="在线设备"
          value={data.devices_online}
          hint={`共 ${data.devices} 台`}
          tone={data.devices_online ? 'green' : undefined}
        />
        <Stat
          label="进行中的会话"
          value={data.sessions_connected}
          hint={
            data.sessions > data.sessions_connected
              ? `另有 ${data.sessions - data.sessions_connected} 个正在建立`
              : '全部已连接'
          }
        />
        <Stat label="账号" value={data.accounts} hint={`${data.admins} 个管理员`} />
        <Stat label="运行时长" value={duration(data.uptime_s)} />
        <Stat label="数据库" value={bytes(data.db_bytes)} />
      </div>
      <div className="grid grid-2">
        <Card title="服务器">
          <Rows>
            <Row label="公开地址">
              <Copy text={data.public_url} />
            </Row>
            <Row label="证书指纹">
              {data.tls_fingerprint ? (
                <Copy text={data.tls_fingerprint} display={<span className="small">{data.tls_fingerprint}</span>} />
              ) : (
                <span className="muted">由反向代理提供 TLS</span>
              )}
            </Row>
            {data.acme && (
              <Row label="证书">
                {data.acme.issued ? (
                  <span>
                    自动签发 · {data.acme.domains.join('、')} · {dateTime(data.acme.not_after)} 到期
                    <span className="muted small"> · {dateTime(data.acme.renew_at)} 起自动续期</span>
                  </span>
                ) : (
                  <span className="amber">正在申请（{data.acme.domains.join('、')}），暂用临时证书</span>
                )}
                {data.acme.last_error && <div className="small red">上次申请失败：{data.acme.last_error}</div>}
              </Row>
            )}
            <Row label="中继">{TURN_MODES[data.turn_mode] ?? data.turn_mode}</Row>
            <Row label="NAT 探测">
              {data.nat_discovery.length ? data.nat_discovery.join('、') : <span className="muted">未配置</span>}
            </Row>
            <Row label="游客功能">{data.guests ? <Badge tone="green">开启</Badge> : <Badge>关闭</Badge>}</Row>
            <Row label="邀请码注册">{data.registration ? <Badge tone="green">开启</Badge> : <Badge>关闭</Badge>}</Row>
          </Rows>
        </Card>
        <Card title="最近的事件" actions={<Link to="/admin/audit">全部</Link>} pad={false}>
          {data.recent.length === 0 ? (
            <Empty icon="log" title="还没有事件" />
          ) : (
            <div className="list">
              {data.recent.map((e) => (
                <div className="list-item" key={e.id}>
                  <div className="grow">
                    <div className="row gap-sm">
                      <span>{auditKindText(e.kind)}</span>
                      {!e.ok && <Badge tone="red">失败</Badge>}
                      <span className="muted ellipsis">{e.target}</span>
                    </div>
                    <div className="small muted ellipsis">
                      {e.actor_name || '匿名'} · {e.ip} {e.detail && `· ${e.detail}`}
                    </div>
                  </div>
                  <span className="small faint nowrap">{dateTime(e.at)}</span>
                </div>
              ))}
            </div>
          )}
        </Card>
      </div>
    </div>
  );
}
