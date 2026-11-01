// Every chat group on the server (`docs/protocol/chat-design.md` §13):
// freeze, dissolve and restore, hand to another owner, set the attachment
// space, read the audit log. Never the messages.
import { useMemo, useState } from 'react';
import { useT } from '../../i18n/react';
import { call, errorText } from '../../lib/api';
import type { ChatUser } from '../../lib/chat/types';
import { bytes, date } from '../../lib/format';
import type { AccountSummary } from '../../lib/types';
import { useApi } from '../../lib/useApi';
import { Alert, Badge, Button, Card, Empty, Field, Input, Loading, PageHeader, SearchBox, Select } from '../../ui/kit';
import { Dialog, useOverlay } from '../../ui/overlay';
import { displayName, GroupIcon } from '../chat/parts';
import { AuditList } from '../chat/GroupSettings';

interface AdminGroup {
  id: string;
  name: string;
  icon: string;
  owner: ChatUser;
  public: boolean;
  members: number;
  channels: number;
  storage: number;
  quota_bytes: number | null;
  created_at: number;
  frozen: boolean;
  dissolved_at: number | null;
}

const GB = 1024 ** 3;

export default function AdminGroups() {
  const t = useT();
  const m = t.chat.admin;
  const { confirm, toast } = useOverlay();
  const { data, error, reload } = useApi<AdminGroup[]>('/api/v2/admin/chat/groups', 15000);
  const users = useApi<AccountSummary[]>('/api/v2/admin/users');
  const [query, setQuery] = useState('');
  const [quotaOf, setQuotaOf] = useState<AdminGroup>();
  const [quota, setQuota] = useState('');
  const [transferOf, setTransferOf] = useState<AdminGroup>();
  const [owner, setOwner] = useState('');
  const [auditOf, setAuditOf] = useState<AdminGroup>();

  const rows = useMemo(() => {
    const q = query.trim().toLowerCase();
    return (data ?? []).filter(
      (g) => !q || [g.name, g.owner.username, g.owner.nickname].some((v) => v.toLowerCase().includes(q)),
    );
  }, [data, query]);

  async function act(f: () => Promise<unknown>) {
    try {
      await f();
      await reload();
      return true;
    } catch (e) {
      toast(errorText(e), 'red');
      return false;
    }
  }
  const patch = (g: AdminGroup, body: object) => act(() => call('PATCH', `/api/v2/admin/chat/groups/${g.id}`, body));

  return (
    <div className="stack">
      <PageHeader
        title={t.nav.groups}
        actions={<SearchBox value={query} onChange={setQuery} placeholder={m.search} />}
      />
      {error && <Alert>{error}</Alert>}
      <Card pad={false}>
        {!data ? (
          <Loading />
        ) : rows.length === 0 ? (
          <Empty icon="message" title={m.none} />
        ) : (
          <div className="table-wrap">
            <table className="table">
              <thead>
                <tr>
                  <th>{m.name}</th>
                  <th>{m.owner}</th>
                  <th>{m.members}</th>
                  <th>{m.channels}</th>
                  <th>{m.storage}</th>
                  <th>{m.quota}</th>
                  <th>{m.created}</th>
                  <th>{m.state}</th>
                  <th />
                </tr>
              </thead>
              <tbody>
                {rows.map((g) => (
                  <tr key={g.id}>
                    <td>
                      <span className="row gap-sm">
                        <GroupIcon group={g} size={28} />
                        <span className="ellipsis">{g.name}</span>
                      </span>
                    </td>
                    <td className="ellipsis">{displayName(g.owner)}</td>
                    <td>{g.members}</td>
                    <td>{g.channels}</td>
                    <td className="nowrap">{bytes(g.storage)}</td>
                    <td className="nowrap">{g.quota_bytes === null ? m.defaultQuota : bytes(g.quota_bytes)}</td>
                    <td className="nowrap">{date(g.created_at)}</td>
                    <td>
                      {g.dissolved_at ? (
                        <Badge tone="red">{m.dissolved}</Badge>
                      ) : g.frozen ? (
                        <Badge tone="amber">{m.frozen}</Badge>
                      ) : (
                        <Badge tone="green">{m.active}</Badge>
                      )}
                    </td>
                    <td>
                      <span className="row gap-sm nowrap">
                        {g.dissolved_at ? (
                          <Button
                            size="sm"
                            onClick={() => void act(() => call('POST', `/api/v2/admin/chat/groups/${g.id}/restore`))}
                          >
                            {m.restore}
                          </Button>
                        ) : (
                          <>
                            <Button
                              size="sm"
                              onClick={async () => {
                                if (
                                  g.frozen ||
                                  (await confirm({ title: m.freezeQuestion(g.name), confirm: m.freeze }))
                                ) {
                                  await patch(g, { frozen: !g.frozen });
                                }
                              }}
                            >
                              {g.frozen ? m.unfreeze : m.freeze}
                            </Button>
                            <Button size="sm" onClick={() => (setTransferOf(g), setOwner(''))}>
                              {m.transfer}
                            </Button>
                            <Button
                              size="sm"
                              onClick={() => {
                                setQuotaOf(g);
                                setQuota(
                                  g.quota_bytes === null ? '' : String(Math.round((g.quota_bytes / GB) * 100) / 100),
                                );
                              }}
                            >
                              {m.setQuota}
                            </Button>
                            <Button
                              size="sm"
                              variant="danger"
                              onClick={async () => {
                                if (
                                  await confirm({
                                    title: m.dissolveQuestion(g.name),
                                    danger: true,
                                    confirm: m.dissolve,
                                  })
                                ) {
                                  await act(() => call('DELETE', `/api/v2/admin/chat/groups/${g.id}`));
                                }
                              }}
                            >
                              {m.dissolve}
                            </Button>
                          </>
                        )}
                        <Button size="sm" onClick={() => setAuditOf(g)}>
                          {m.audit}
                        </Button>
                      </span>
                    </td>
                  </tr>
                ))}
              </tbody>
            </table>
          </div>
        )}
      </Card>
      <Dialog
        open={!!quotaOf}
        title={quotaOf ? `${m.setQuota} · ${quotaOf.name}` : ''}
        onClose={() => setQuotaOf(undefined)}
        footer={
          <>
            <Button onClick={() => setQuotaOf(undefined)}>{t.common.cancel}</Button>
            <Button
              variant="primary"
              disabled={quota.trim() !== '' && !(Number(quota) > 0)}
              onClick={async () => {
                const value = quota.trim() ? Math.round(Number(quota) * GB) : null;
                if (quotaOf && (await patch(quotaOf, { quota_bytes: value }))) setQuotaOf(undefined);
              }}
            >
              {t.common.save}
            </Button>
          </>
        }
      >
        <Field label={m.quotaGb}>
          <Input value={quota} inputMode="decimal" onChange={(e) => setQuota(e.target.value)} />
        </Field>
      </Dialog>
      <Dialog
        open={!!transferOf}
        title={transferOf ? m.transferTitle(transferOf.name) : ''}
        onClose={() => setTransferOf(undefined)}
        footer={
          <>
            <Button onClick={() => setTransferOf(undefined)}>{t.common.cancel}</Button>
            <Button
              variant="primary"
              disabled={!owner}
              onClick={async () => {
                if (transferOf && (await patch(transferOf, { owner }))) setTransferOf(undefined);
              }}
            >
              {m.transfer}
            </Button>
          </>
        }
      >
        <Field label={m.newOwner}>
          <Select
            value={owner}
            onChange={setOwner}
            options={[
              { value: '', label: '—' },
              ...(users.data ?? []).map((u) => ({
                value: u.id,
                label: u.nickname ? `${u.nickname} (${u.username})` : u.username,
              })),
            ]}
          />
        </Field>
      </Dialog>
      <Dialog
        open={!!auditOf}
        title={auditOf ? `${m.audit} · ${auditOf.name}` : ''}
        onClose={() => setAuditOf(undefined)}
        wide
      >
        {auditOf && <AuditList path={`/api/v2/admin/chat/groups/${auditOf.id}/audit`} />}
      </Dialog>
    </div>
  );
}
