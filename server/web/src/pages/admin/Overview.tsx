// Server overview: live counts, server facts and the latest audit events.
import { auditDetail } from '../../i18n/audit';
import { useLang, useT } from '../../i18n/react';
import { auditKindText, bytes, dateTime, duration, listText } from '../../lib/format';
import { Link } from '../../lib/router';
import type { Overview } from '../../lib/types';
import { useApi } from '../../lib/useApi';
import { Alert, Badge, Card, Copy, Empty, PageHeader, PageState, Row, Rows, Stat } from '../../ui/kit';

export default function AdminOverview() {
  const t = useT();
  const [lang] = useLang();
  const m = t.admin.overview;
  const { data, error } = useApi<Overview>('/api/v2/admin/overview', 5000);
  if (!data) return <PageState title={t.nav.adminHome} error={error} />;
  const onOff = (on: boolean) => (on ? <Badge tone="green">{t.common.on}</Badge> : <Badge>{t.common.off}</Badge>);
  return (
    <div className="stack">
      <PageHeader title={t.nav.adminHome} subtitle={`${data.instance_name} · Farwing Server ${data.version}`} />
      {error && <Alert tone="amber">{t.common.refreshFailed(error)}</Alert>}
      <div className="grid grid-stats">
        <Stat
          label={m.devicesOnline}
          value={data.devices_online}
          hint={m.devicesTotal(data.devices)}
          tone={data.devices_online ? 'green' : undefined}
        />
        <Stat
          label={m.sessions}
          value={data.sessions_connected}
          hint={
            data.sessions > data.sessions_connected
              ? m.sessionsPending(data.sessions - data.sessions_connected)
              : data.relayed_sessions > 0
                ? m.sessionsRelayed(data.relayed_sessions)
                : m.sessionsAllConnected
          }
        />
        <Stat label={m.accounts} value={data.accounts} hint={m.admins(data.admins)} />
        <Stat label={m.uptime} value={duration(data.uptime_s)} />
        <Stat label={m.database} value={bytes(data.db_bytes)} />
      </div>
      <div className="grid grid-2">
        <Card title={m.server}>
          <Rows>
            <Row label={m.publicUrl}>
              <Copy text={data.public_url} />
            </Row>
            <Row label={m.fingerprint}>
              {data.tls_fingerprint ? (
                <Copy text={data.tls_fingerprint} display={<span className="small">{data.tls_fingerprint}</span>} />
              ) : (
                <span className="muted">{m.tlsByProxy}</span>
              )}
            </Row>
            {data.acme && (
              <Row label={m.certificate}>
                {data.acme.issued ? (
                  <span>
                    {m.certIssued(listText(data.acme.domains), dateTime(data.acme.not_after))}
                    <span className="muted small"> · {m.certRenews(dateTime(data.acme.renew_at))}</span>
                  </span>
                ) : (
                  <span className="amber">{m.certPending(listText(data.acme.domains))}</span>
                )}
                {data.acme.last_error && <div className="small red">{m.certFailed(data.acme.last_error)}</div>}
              </Row>
            )}
            <Row label={m.relay}>{(m.turnModes as Record<string, string>)[data.turn_mode] ?? data.turn_mode}</Row>
            {data.nodes > 0 && (
              <Row label={m.nodes}>
                <Link to="/admin/nodes">{m.nodesUsable(data.nodes_usable, data.nodes)}</Link>
              </Row>
            )}
            <Row label={m.forwarding}>
              {data.forwarding && data.forwarding_rooms > 0
                ? m.forwardingRooms(data.forwarding_rooms, data.forwarding_viewers)
                : onOff(data.forwarding)}
            </Row>
            <Row label={m.nat}>
              {data.nat_discovery.length ? (
                listText(data.nat_discovery)
              ) : (
                <span className="muted">{t.common.notConfigured}</span>
              )}
            </Row>
            <Row label={t.admin.guests}>{onOff(data.guests)}</Row>
            <Row label={t.admin.registration}>{onOff(data.registration)}</Row>
          </Rows>
        </Card>
        <Card title={m.recentEvents} actions={<Link to="/admin/audit">{m.viewAll}</Link>} pad={false}>
          {data.recent.length === 0 ? (
            <Empty icon="log" title={m.noEvents} />
          ) : (
            <div className="list">
              {data.recent.map((e) => (
                <div className="list-item" key={e.id}>
                  <div className="grow">
                    <div className="row gap-sm">
                      <span>{auditKindText(e.kind)}</span>
                      {!e.ok && <Badge tone="red">{t.common.failed}</Badge>}
                      <span className="muted ellipsis">{e.target}</span>
                    </div>
                    <div className="small muted ellipsis">
                      {e.actor_name || t.admin.anonymous} · {e.ip}{' '}
                      {e.detail && `· ${auditDetail(lang, e.kind, e.detail)}`}
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
