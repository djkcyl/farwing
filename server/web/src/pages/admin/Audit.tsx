// The audit log: administrator actions, logins (also failed ones),
// registrations; filter by kind and actor, page backwards.
import { useEffect, useState } from 'react';
import { call, errorText } from '../../lib/api';
import { AUDIT_KINDS, auditKindText, dateTime } from '../../lib/format';
import type { AuditEntry } from '../../lib/types';
import { Alert, Badge, Button, Card, Empty, Input, Loading, PageHeader, Select } from '../../ui/kit';

const PAGE = 100;

export default function Audit() {
  const [kind, setKind] = useState('');
  const [actor, setActor] = useState('');
  const [applied, setApplied] = useState({ kind: '', actor: '' });
  const [rows, setRows] = useState<AuditEntry[]>();
  const [more, setMore] = useState(false);
  const [error, setError] = useState<string>();
  const [busy, setBusy] = useState(false);

  async function load(before?: number) {
    setBusy(true);
    try {
      const q = new URLSearchParams({ limit: String(PAGE) });
      if (applied.kind) q.set('kind', applied.kind);
      if (applied.actor) q.set('actor', applied.actor);
      if (before) q.set('before', String(before));
      const page = await call<AuditEntry[]>('GET', `/api/v2/admin/audit?${q}`);
      setRows((r) => (before ? [...(r ?? []), ...page] : page));
      setMore(page.length === PAGE);
      setError(undefined);
    } catch (err) {
      setError(errorText(err));
    } finally {
      setBusy(false);
    }
  }

  useEffect(() => {
    // Results arrive after the request; nothing is set synchronously.
    // oxlint-disable-next-line react/set-state-in-effect
    void load();
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [applied]);

  return (
    <div className="stack">
      <PageHeader title="审计日志" />
      <form
        className="row gap-sm wrap"
        onSubmit={(e) => {
          e.preventDefault();
          setApplied({ kind, actor: actor.trim() });
        }}
      >
        <Select value={kind} onChange={(e) => setKind(e.target.value)} style={{ width: 180 }}>
          <option value="">全部类型</option>
          {Object.entries(AUDIT_KINDS).map(([k, v]) => (
            <option key={k} value={k}>
              {v}
            </option>
          ))}
        </Select>
        <Input
          value={actor}
          onChange={(e) => setActor(e.target.value)}
          placeholder="操作者用户名"
          style={{ width: 180 }}
        />
        <Button type="submit" icon="search">
          筛选
        </Button>
      </form>
      {error && <Alert>{error}</Alert>}
      <Card pad={false}>
        {!rows ? (
          <Loading />
        ) : rows.length === 0 ? (
          <Empty icon="log" title="暂无记录" />
        ) : (
          <>
            <div className="table-wrap">
              <table className="table">
                <thead>
                  <tr>
                    <th>时间</th>
                    <th>操作者</th>
                    <th>类型</th>
                    <th>对象</th>
                    <th>详情</th>
                    <th>IP</th>
                  </tr>
                </thead>
                <tbody>
                  {rows.map((e) => (
                    <tr key={e.id}>
                      <td className="nowrap">{dateTime(e.at)}</td>
                      <td>{e.actor_name || <span className="muted">匿名</span>}</td>
                      <td className="nowrap">
                        {auditKindText(e.kind)} {!e.ok && <Badge tone="red">失败</Badge>}
                      </td>
                      <td className="small" style={{ maxWidth: 260, overflowWrap: 'anywhere' }}>
                        {e.target}
                      </td>
                      <td className="small muted" style={{ maxWidth: 320, overflowWrap: 'anywhere' }}>
                        {e.detail}
                      </td>
                      <td className="mono small">{e.ip}</td>
                    </tr>
                  ))}
                </tbody>
              </table>
            </div>
            {more && (
              <div className="pager">
                <Button loading={busy} onClick={() => void load(rows[rows.length - 1].id)}>
                  加载更早的记录
                </Button>
              </div>
            )}
          </>
        )}
      </Card>
    </div>
  );
}
