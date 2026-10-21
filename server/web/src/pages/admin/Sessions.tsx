// Sessions now on the server; end one.
import { call, errorText } from '../../lib/api';
import { ago, displayName, platformText, PURPOSES, RELATIONS } from '../../lib/format';
import type { LiveSession } from '../../lib/types';
import { useApi } from '../../lib/useApi';
import { Alert, Badge, Card, Empty, IconButton, Loading, PageHeader } from '../../ui/kit';
import { useOverlay } from '../../ui/overlay';

export default function Sessions() {
  const { confirm, toast } = useOverlay();
  const { data, error, reload } = useApi<LiveSession[]>('/api/v2/admin/sessions', 3000);

  return (
    <div className="stack">
      <PageHeader title="会话" />
      {error && <Alert>{error}</Alert>}
      <Card pad={false}>
        {!data ? (
          <Loading />
        ) : data.length === 0 ? (
          <Empty icon="activity" title="暂无会话" />
        ) : (
          <div className="table-wrap">
            <table className="table">
              <thead>
                <tr>
                  <th>被控</th>
                  <th>主控</th>
                  <th>关系</th>
                  <th>用途</th>
                  <th>状态</th>
                  <th>开始</th>
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
                        <div className="cell-sub">{s.host_owner ?? '未登录账号'}</div>
                      </td>
                      <td>
                        <div className="cell-main">
                          {s.controller_alias || s.controller.alias || s.controller_device_id}
                        </div>
                        <div className="cell-sub">
                          {displayName(s.controller) || '访客'} · {platformText(s.controller.platform)}
                        </div>
                      </td>
                      <td>{RELATIONS[s.relation] ?? s.relation}</td>
                      <td>{PURPOSES[s.purpose] ?? s.purpose}</td>
                      <td>{s.connected ? <Badge tone="green">已连接</Badge> : <Badge tone="amber">正在建立</Badge>}</td>
                      <td className="nowrap">{ago(s.created_at)}</td>
                      <td className="actions">
                        <IconButton
                          icon="close"
                          label="结束会话"
                          variant="danger"
                          onClick={async () => {
                            if (
                              await confirm({
                                title: '结束会话',
                                message: '双方的连接将立即断开。',
                                confirm: '结束',
                                danger: true,
                              })
                            ) {
                              try {
                                await call('DELETE', `/api/v2/admin/sessions/${s.id}`);
                                toast('会话已结束');
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
