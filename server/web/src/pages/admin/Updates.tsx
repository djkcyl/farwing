// Client update announcements (what `GET /api/v2/updates/{platform}`
// offers to clients and the download links on "连接到本服务器").
import { useState, type FormEvent } from 'react';
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
      title={form.id ? '编辑更新公告' : '发布更新'}
      onClose={onClose}
      wide
      footer={
        <>
          <Button onClick={onClose}>取消</Button>
          <Button variant="primary" type="submit" form="update-form" loading={busy}>
            {form.id ? '保存' : '发布'}
          </Button>
        </>
      }
    >
      <form id="update-form" className="stack" onSubmit={submit}>
        {error && <Alert>{error}</Alert>}
        <div className="grid grid-2">
          <Field label="平台">
            <Select value={form.platform} onChange={(e) => set({ platform: e.target.value })}>
              {['windows', 'macos', 'linux', 'android', 'ios'].map((p) => (
                <option key={p} value={p}>
                  {platformText(p)}
                </option>
              ))}
            </Select>
          </Field>
          <Field label="渠道">
            <Select value={form.channel} onChange={(e) => set({ channel: e.target.value })}>
              <option value="stable">正式版（stable）</option>
              <option value="beta">测试版（beta）</option>
            </Select>
          </Field>
        </div>
        <Field label="版本号" hint="例如 0.2.0">
          <Input value={form.version} onChange={(e) => set({ version: e.target.value })} required maxLength={64} />
        </Field>
        <Field label="下载地址" hint="安装包的 http(s) 链接；客户端从这里下载更新">
          <Input value={form.url} onChange={(e) => set({ url: e.target.value })} required placeholder="https://" />
        </Field>
        <Field label="更新说明">
          <textarea
            name="notes"
            className="input"
            value={form.notes}
            onChange={(e) => set({ notes: e.target.value })}
            maxLength={16384}
          />
        </Field>
        <Checkbox checked={form.prerelease} onChange={(v) => set({ prerelease: v })} label="标记为预发布版本" />
      </form>
    </Dialog>
  );
}

export default function Updates() {
  const { confirm, toast } = useOverlay();
  const { data, error, reload } = useApi<Announcement[]>('/api/v2/admin/updates');
  const [editing, setEditing] = useState<Announcement>();

  return (
    <div className="stack">
      <PageHeader
        title="客户端更新"
        subtitle="客户端检查更新时看到每个平台发布时间最新的一条；选择了测试版的客户端也会看到测试版渠道。"
        actions={
          <Button variant="primary" icon="plus" onClick={() => setEditing({ ...BLANK })}>
            发布更新
          </Button>
        }
      />
      {error && <Alert>{error}</Alert>}
      <Card pad={false}>
        {!data ? (
          <Loading />
        ) : data.length === 0 ? (
          <Empty icon="download" title="还没有更新公告" hint="发布后，客户端会在检查更新时看到它。" />
        ) : (
          <div className="table-wrap">
            <table className="table">
              <thead>
                <tr>
                  <th>版本</th>
                  <th>平台</th>
                  <th>渠道</th>
                  <th>下载地址</th>
                  <th>发布</th>
                  <th />
                </tr>
              </thead>
              <tbody>
                {data.map((a) => (
                  <tr key={a.id}>
                    <td>
                      <div className="cell-main row gap-sm">
                        {a.version}
                        {a.prerelease && <Badge tone="amber">预发布</Badge>}
                      </div>
                      {a.notes && (
                        <div className="cell-sub ellipsis" style={{ maxWidth: 280 }}>
                          {a.notes}
                        </div>
                      )}
                    </td>
                    <td>{platformText(a.platform)}</td>
                    <td>{a.channel === 'stable' ? '正式版' : '测试版'}</td>
                    <td className="small ellipsis" style={{ maxWidth: 260 }}>
                      <a href={a.url} target="_blank" rel="noreferrer">
                        {a.url}
                      </a>
                    </td>
                    <td className="nowrap muted">{dateTime(a.published_at)}</td>
                    <td className="actions">
                      <IconButton icon="edit" label="编辑" onClick={() => setEditing(a)} />
                      <IconButton
                        icon="trash"
                        label="删除"
                        variant="danger"
                        onClick={async () => {
                          if (
                            await confirm({
                              title: '删除更新公告',
                              message: `删除 ${platformText(a.platform)} ${a.version}？`,
                              confirm: '删除',
                              danger: true,
                            })
                          ) {
                            try {
                              await call('DELETE', `/api/v2/admin/updates/${a.id}`);
                              toast('已删除');
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
          toast('已保存');
          void reload();
        }}
      />
    </div>
  );
}
