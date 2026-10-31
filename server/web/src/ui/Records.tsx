// Connection records and account events (`docs/connection-records.md`):
// paged lists, newest first; a record opens to its details.
import { useEffect, useState } from 'react';
import { auditDetail } from '../i18n/audit';
import { useLang, useT } from '../i18n/react';
import { call, errorText } from '../lib/api';
import { auditKindText, bytes, dateTime, duration, num, purposeText, relationText } from '../lib/format';
import type { AccountEvent, ConnectionRecord, RecordParty } from '../lib/types';
import { Icon } from './icons';
import { Alert, Badge, Button, Empty, Loading, Row, Rows } from './kit';

const PAGE = 50;

/// Pages of `path` (`?before=<id>`), newest first.
function usePages<T extends { id: number }>(path: string, query: Record<string, string | undefined>) {
  const [rows, setRows] = useState<T[]>();
  const [more, setMore] = useState(false);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string>();
  const key = JSON.stringify(query);

  async function load(before?: number) {
    setBusy(true);
    try {
      const q = new URLSearchParams({ limit: String(PAGE) });
      for (const [k, v] of Object.entries(query)) if (v) q.set(k, v);
      if (before) q.set('before', String(before));
      const page = await call<T[]>('GET', `${path}?${q}`);
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
  }, [path, key]);

  return { rows, more, busy, error, next: () => rows?.length && load(rows[rows.length - 1].id) };
}

function party(p: RecordParty, guest: string): string {
  const account = p.nickname || p.username;
  return [account || guest, p.name].filter(Boolean).join(' · ');
}

function RecordItem({ r }: { r: ConnectionRecord }) {
  const t = useT();
  const m = t.records;
  const [open, setOpen] = useState(false);
  const refused = m.outcomes[r.outcome];
  const end = r.ended_at ?? 0;
  const route = r.viewer?.route ? (m.routes[r.viewer.route] ?? r.viewer.route) : '';
  const relay = r.viewer?.relay ? ` · ${r.viewer.relay.toUpperCase()}` : '';
  const how = [
    relationText(r.relation),
    m.admissions[r.admission],
    r.direct ? m.direct : route,
    r.controller.platform === 'web' ? m.web : undefined,
  ]
    .filter(Boolean)
    .join(' · ');
  const v = r.viewer;
  const h = r.host_report;
  return (
    <div className="record">
      <div
        className="record-head"
        role="button"
        tabIndex={0}
        onClick={() => setOpen(!open)}
        onKeyDown={(e) => (e.key === 'Enter' || e.key === ' ') && setOpen(!open)}
      >
        <div className="grow">
          <div>
            {party(r.controller, t.common.guest)} <span className="faint">→</span> {r.host.name || '—'}
          </div>
          <div className="small muted">
            {dateTime(r.started_at)} · {purposeText(r.purpose)} · {how}
          </div>
        </div>
        {r.outcome === 'open' ? (
          <Badge tone="green">{m.open}</Badge>
        ) : refused ? (
          <Badge tone={r.outcome === 'busy' ? 'amber' : 'red'}>{refused}</Badge>
        ) : (
          <span className="small muted nowrap">{duration(end - r.started_at)}</span>
        )}
        <Icon name="chevron" size={16} className={`record-chevron ${open ? 'open' : ''}`} />
      </div>
      {open && (
        <div className="record-detail">
          <Rows>
            <Row label={m.time}>
              {dateTime(r.started_at)}
              {end > r.started_at && ` – ${dateTime(end)}`}
            </Row>
            {r.outcome === 'ended' && m.endReasons[r.end_reason] && (
              <Row label={m.ended}>{m.endReasons[r.end_reason]}</Row>
            )}
            {(r.ip || !r.direct) && (
              <Row label={m.source}>
                {[r.ip, r.controller.platform === 'web' ? m.web : m.app].filter(Boolean).join(' · ')}
              </Row>
            )}
            {(route || r.direct) && (
              <Row label={m.route}>
                {r.direct ? m.direct : route}
                {relay}
              </Row>
            )}
            {v && v.seconds > 0 && (
              <Row label={m.picture}>
                {[
                  v.width ? `${v.width}×${v.height}` : '',
                  m.fps(num(v.fps)),
                  v.rtt_ms ? m.rtt(v.rtt_ms) : '',
                  m.stalls(v.stalls),
                  Math.max(v.reconnects, r.reconnects) ? m.reconnects(Math.max(v.reconnects, r.reconnects)) : '',
                ]
                  .filter(Boolean)
                  .join(' · ')}
              </Row>
            )}
            {h && r.purpose === 'desktop' && <Row label={m.control}>{h.controlled ? m.controlled : m.viewOnly}</Row>}
            {h?.camera && r.purpose === 'desktop' && <Row label={m.camera}>{m.cameraViewed}</Row>}
            {h && h.files.length > 0 && (
              <Row label={m.files}>
                <div className="stack-xs">
                  {h.files.map((f, i) => (
                    <div key={i}>
                      {f.upload ? m.upload : m.download} · {f.name}
                      {f.count > 1 && ` · ${m.fileCount(f.count)}`} · {bytes(f.bytes)} ·{' '}
                      <span className={f.outcome === 'done' ? 'muted' : 'amber'}>{m.fileOutcomes[f.outcome]}</span>
                    </div>
                  ))}
                  {h.more_files > 0 && <div className="muted">{m.moreFiles(h.more_files)}</div>}
                </div>
              </Row>
            )}
            {h && h.ports.length > 0 && (
              <Row label={m.ports}>
                <span className="mono small">{h.ports.join(', ')}</span>
              </Row>
            )}
          </Rows>
        </div>
      )}
    </div>
  );
}

/// Connection records of `path`, optionally of one `device` or `user`.
export function RecordList({ path, device, user }: { path: string; device?: string; user?: string }) {
  const t = useT();
  const pages = usePages<ConnectionRecord>(path, { device, user });
  if (pages.error && !pages.rows) return <Alert>{pages.error}</Alert>;
  if (!pages.rows) return <Loading />;
  if (pages.rows.length === 0) return <Empty icon="history" title={t.records.empty} />;
  return (
    <>
      <div className="list">
        {pages.rows.map((r) => (
          <RecordItem key={r.id} r={r} />
        ))}
      </div>
      {pages.more && (
        <div className="pager">
          <Button loading={pages.busy} onClick={() => void pages.next()}>
            {t.records.loadMore}
          </Button>
        </div>
      )}
    </>
  );
}

/// The account's events.
export function EventList() {
  const t = useT();
  const [lang] = useLang();
  const m = t.records;
  const pages = usePages<AccountEvent>('/api/v2/me/events', {});
  if (pages.error && !pages.rows) return <Alert>{pages.error}</Alert>;
  if (!pages.rows) return <Loading />;
  if (pages.rows.length === 0) return <Empty icon="history" title={m.empty} />;
  return (
    <>
      <div className="table-wrap">
        <table className="table">
          <thead>
            <tr>
              <th>{m.time}</th>
              <th>{m.event}</th>
              <th>{m.detail}</th>
              <th>{m.actor}</th>
              <th>IP</th>
            </tr>
          </thead>
          <tbody>
            {pages.rows.map((e) => (
              <tr key={e.id}>
                <td className="nowrap">{dateTime(e.at)}</td>
                <td className="nowrap">
                  {auditKindText(e.kind)} {!e.ok && <Badge tone="red">{t.common.failed}</Badge>}
                </td>
                <td className="small muted" style={{ maxWidth: 360, overflowWrap: 'anywhere' }}>
                  {e.detail && auditDetail(lang, e.kind, JSON.stringify(e.detail))}
                </td>
                <td>{e.actor}</td>
                <td className="mono small">{e.ip}</td>
              </tr>
            ))}
          </tbody>
        </table>
      </div>
      {pages.more && (
        <div className="pager">
          <Button loading={pages.busy} onClick={() => void pages.next()}>
            {m.loadMore}
          </Button>
        </div>
      )}
    </>
  );
}
