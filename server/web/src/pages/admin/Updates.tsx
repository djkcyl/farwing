// Client update announcements (what `GET /api/v2/updates/{platform}`
// offers to clients and the download links on "Connect to this server").
import { useState, type FormEvent } from 'react';
import { useT } from '../../i18n/react';
import { call, errorText } from '../../lib/api';
import { dateTime, platformText } from '../../lib/format';
import type { Announcement } from '../../lib/types';
import { useApi } from '../../lib/useApi';
import {
  Alert,
  Badge,
  Button,
  Card,
  Checkbox,
  Empty,
  Field,
  IconButton,
  Input,
  Loading,
  PageHeader,
  Select,
} from '../../ui/kit';
import { Dialog, useOverlay } from '../../ui/overlay';

const BLANK: Announcement = {
  id: '',
  platform: 'windows',
  channel: 'stable',
  version: '',
  notes: '',
  url: '',
  published_at: 0,
  prerelease: false,
};

function Editor({ value, onClose, onSaved }: { value: Announcement | undefined; onClose(): void; onSaved(): void }) {
  const t = useT();
  const m = t.admin.updates;
  const [form, setForm] = useState<Announcement>(BLANK);
  const [error, setError] = useState<string>();
  const [busy, setBusy] = useState(false);
  const [shown, setShown] = useState<Announcement>();
  if (value !== shown) {
    setShown(value);
    setForm(value ?? BLANK);
    setError(undefined);
  }
  const set = (patch: Partial<Announcement>) => setForm((f) => ({ ...f, ...patch }));

  async function submit(e: FormEvent) {
    e.preventDefault();
    setBusy(true);
    setError(undefined);
    try {
      const body = { ...form, version: form.version.trim(), url: form.url.trim() };
      if (form.id) await call('PUT', `/api/v2/admin/updates/${form.id}`, body);
      else await call('POST', '/api/v2/admin/updates', body);
      onSaved();
    } catch (err) {
      setError(errorText(err));
    } finally {
      setBusy(false);
    }
  }

  return (
    <Dialog
      open={!!value}
      title={form.id ? m.edit : m.publish}
      onClose={onClose}
      wide
      footer={
        <>
          <Button onClick={onClose}>{t.common.cancel}</Button>
          <Button variant="primary" type="submit" form="update-form" loading={busy}>
            {form.id ? t.common.save : m.publishSubmit}
          </Button>
        </>
      }
    >
      <form id="update-form" className="stack" onSubmit={submit}>
        {error && <Alert>{error}</Alert>}
        <div className="grid grid-2">
          <Field label={m.platform}>
            <Select
              value={form.platform}
              onChange={(platform) => set({ platform })}
              options={['windows', 'macos', 'linux', 'android', 'ios'].map((p) => ({
                value: p,
                label: platformText(p),
              }))}
            />
          </Field>
          <Field label={m.channel}>
            <Select
              value={form.channel}
              onChange={(channel) => set({ channel })}
              options={[
                { value: 'stable', label: m.stableOption },
                { value: 'beta', label: m.betaOption },
              ]}
            />
          </Field>
        </div>
        <Field label={m.version}>
          <Input
            value={form.version}
            onChange={(e) => set({ version: e.target.value })}
            required
            maxLength={64}
            placeholder="0.2.0"
          />
        </Field>
        <Field label={m.url}>
          <Input value={form.url} onChange={(e) => set({ url: e.target.value })} required placeholder="https://" />
        </Field>
        <Field label={m.notes}>
          <textarea
            name="notes"
            className="input"
            value={form.notes}
            onChange={(e) => set({ notes: e.target.value })}
            maxLength={16384}
          />
        </Field>
        <Checkbox checked={form.prerelease} onChange={(v) => set({ prerelease: v })} label={m.prerelease} />
      </form>
    </Dialog>
  );
}

export default function Updates() {
  const t = useT();
  const m = t.admin.updates;
  const { confirm, toast } = useOverlay();
  const { data, error, reload } = useApi<Announcement[]>('/api/v2/admin/updates');
  const [editing, setEditing] = useState<Announcement>();

  return (
    <div className="stack">
      <PageHeader
        title={t.nav.updates}
        actions={
          <Button variant="primary" icon="plus" onClick={() => setEditing({ ...BLANK })}>
            {m.publish}
          </Button>
        }
      />
      {error && <Alert>{error}</Alert>}
      <Card pad={false}>
        {!data ? (
          <Loading />
        ) : data.length === 0 ? (
          <Empty icon="download" title={m.empty} />
        ) : (
          <div className="table-wrap">
            <table className="table">
              <thead>
                <tr>
                  <th>{m.versionCol}</th>
                  <th>{m.platform}</th>
                  <th>{m.channel}</th>
                  <th>{m.url}</th>
                  <th>{m.published}</th>
                  <th />
                </tr>
              </thead>
              <tbody>
                {data.map((a) => (
                  <tr key={a.id}>
                    <td>
                      <div className="cell-main row gap-sm">
                        {a.version}
                        {a.prerelease && <Badge tone="amber">{m.prereleaseBadge}</Badge>}
                      </div>
                      {a.notes && (
                        <div className="cell-sub ellipsis" style={{ maxWidth: 280 }}>
                          {a.notes}
                        </div>
                      )}
                    </td>
                    <td>{platformText(a.platform)}</td>
                    <td>{a.channel === 'stable' ? m.stable : m.beta}</td>
                    <td className="small ellipsis" style={{ maxWidth: 260 }}>
                      <a href={a.url} target="_blank" rel="noreferrer">
                        {a.url}
                      </a>
                    </td>
                    <td className="nowrap muted">{dateTime(a.published_at)}</td>
                    <td className="actions">
                      <IconButton icon="edit" label={t.common.edit} onClick={() => setEditing(a)} />
                      <IconButton
                        icon="trash"
                        label={t.common.delete}
                        variant="danger"
                        onClick={async () => {
                          if (
                            await confirm({
                              title: m.deleteTitle,
                              message: m.deleteMessage(platformText(a.platform), a.version),
                              confirm: t.common.delete,
                              danger: true,
                            })
                          ) {
                            try {
                              await call('DELETE', `/api/v2/admin/updates/${a.id}`);
                              toast(t.common.deleted);
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
      <Editor
        value={editing}
        onClose={() => setEditing(undefined)}
        onSaved={() => {
          setEditing(undefined);
          toast(t.common.saved);
          void reload();
        }}
      />
    </div>
  );
}
