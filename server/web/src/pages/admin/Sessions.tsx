// Sessions now on the server; end one.
import { useT } from '../../i18n/react';
import { call, errorText } from '../../lib/api';
import { ago, bitrate, displayName, platformText, purposeText, relationText } from '../../lib/format';
import type { LiveSession } from '../../lib/types';
import { useApi } from '../../lib/useApi';
import { Alert, Badge, Card, Empty, IconButton, Loading, PageHeader } from '../../ui/kit';
import { useOverlay } from '../../ui/overlay';

export default function Sessions() {
  const t = useT();
  const m = t.admin.sessions;
  const { confirm, toast } = useOverlay();
  const { data, error, reload } = useApi<LiveSession[]>('/api/v2/admin/sessions', 3000);

  return (
    <div className="stack">
      <PageHeader title={t.nav.sessions} />
      {error && <Alert>{error}</Alert>}
      <Card pad={false}>
        {!data ? (
          <Loading />
        ) : data.length === 0 ? (
          <Empty icon="activity" title={m.empty} />
        ) : (
          <div className="table-wrap">
            <table className="table">
              <thead>
                <tr>
                  <th>{m.host}</th>
                  <th>{m.controller}</th>
                  <th>{m.relation}</th>
                  <th>{m.purpose}</th>
                  <th>{m.relay}</th>
                  <th>{m.forwarding}</th>
                  <th>{m.egress}</th>
                  <th>{t.admin.col.status}</th>
                  <th>{m.started}</th>
                  <th />
                </tr>
              </thead>
              <tbody>
                {[...data]
                  .sort((a, b) => b.created_at - a.created_at)
                  .map((s) => (
                    <tr key={s.id}>
                      <td>
                        <div className="cell-main">{s.host_alias || s.host_device_id}</div>
                        <div className="cell-sub">{s.host_owner ?? t.admin.noAccount}</div>
                      </td>
                      <td>
                        <div className="cell-main">
                          {s.controller_alias || s.controller.alias || s.controller_device_id}
                        </div>
                        <div className="cell-sub">
                          {displayName(s.controller) || t.common.guest} · {platformText(s.controller.platform)}
                        </div>
                      </td>
                      <td>{relationText(s.relation)}</td>
                      <td>{purposeText(s.purpose)}</td>
                      <td className="small">
                        {[...(s.local_relay ? [m.thisServer] : []), ...s.relay_nodes].join(t.format.listSeparator) || (
                          <span className="muted">—</span>
                        )}
                      </td>
                      <td className="small">
                        {s.forwarded_by === null ? <span className="muted">—</span> : s.forwarded_by || m.thisServer}
                      </td>
                      <td className="small nowrap">
                        {s.egress_bps > 0 ? bitrate(s.egress_bps) : <span className="muted">—</span>}
                      </td>
                      <td className="nowrap">
                        {s.connected ? (
                          <Badge tone="green">{m.connected}</Badge>
                        ) : (
                          <Badge tone="amber">{m.connecting}</Badge>
                        )}
                      </td>
                      <td className="nowrap">{ago(s.created_at)}</td>
                      <td className="actions">
                        <IconButton
                          icon="close"
                          label={m.end}
                          variant="danger"
                          onClick={async () => {
                            if (
                              await confirm({
                                title: m.endTitle,
                                message: m.endMessage,
                                confirm: m.endConfirm,
                                danger: true,
                              })
                            ) {
                              try {
                                await call('DELETE', `/api/v2/admin/sessions/${s.id}`);
                                toast(m.ended);
                                await reload();
                              } catch (err) {
                                toast(errorText(err), 'red');
                              }
                            }
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
    </div>
  );
}
