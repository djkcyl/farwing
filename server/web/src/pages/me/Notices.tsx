// The account's notifications, newest first; opening the page reads them.
import { useEffect, useState } from 'react';
import { useT } from '../../i18n/react';
import { call, errorText } from '../../lib/api';
import { ago, dateTime } from '../../lib/format';
import { noticesChanged, noticeText } from '../../lib/notices';
import type { Notice, NoticeList } from '../../lib/types';
import { Alert, Button, Card, Empty, Loading, PageHeader } from '../../ui/kit';

const PAGE = 50;

export default function Notices() {
  const t = useT();
  const m = t.notices;
  const [rows, setRows] = useState<Notice[]>();
  const [more, setMore] = useState(false);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string>();

  async function load(before?: number) {
    setBusy(true);
    try {
      const q = new URLSearchParams({ limit: String(PAGE) });
      if (before) q.set('before', String(before));
      const page = await call<NoticeList>('GET', `/api/v2/me/notices?${q}`);
      setRows((r) => (before ? [...(r ?? []), ...page.notices] : page.notices));
      setMore(page.notices.length === PAGE);
      setError(undefined);
      if (!before && page.unread > 0) {
        await call('POST', '/api/v2/me/notices/read', { all: true });
        noticesChanged();
      }
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
  }, []);

  return (
    <div className="stack">
      <PageHeader title={t.nav.notifications} />
      {error && <Alert>{error}</Alert>}
      <Card pad={false}>
        {!rows ? (
          <Loading />
        ) : rows.length === 0 ? (
          <Empty icon="bell" title={m.empty} />
        ) : (
          <>
            <div className="list">
              {rows.map((n) => {
                const text = noticeText(n);
                return (
                  <div className={`list-item ${n.read ? '' : 'notice-unread'}`} key={n.id}>
                    <div className="grow">
                      <div>{text.title}</div>
                      {text.body && <div className="small muted">{text.body}</div>}
                    </div>
                    <span className="small faint nowrap" title={dateTime(n.created_at)}>
                      {ago(n.created_at)}
                    </span>
                  </div>
                );
              })}
            </div>
            {more && (
              <div className="pager">
                <Button loading={busy} onClick={() => void load(rows[rows.length - 1].id)}>
                  {m.loadMore}
                </Button>
              </div>
            )}
          </>
        )}
      </Card>
    </div>
  );
}
