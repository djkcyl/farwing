// Saved short codes (favourites, recent connections), shared with the
// client. Access codes stay on each client; the web never stores them.
import { useState, type FormEvent } from 'react';
import { call, errorText } from '../../lib/api';
import { ago, connectIdText, dateTime } from '../../lib/format';
import { navigate } from '../../lib/router';
import type { SavedDevice } from '../../lib/types';
import { useApi } from '../../lib/useApi';
import { Alert, Button, Card, Empty, Field, IconButton, Input, Loading, PageHeader, Tabs } from '../../ui/kit';
import { Dialog, useOverlay } from '../../ui/overlay';

type Tab = 'favorites' | 'recent';

const toItem = (s: SavedDevice) => ({
  connect_id: s.connect_id,
  remark: s.remark,
  favorited_at: s.favorited_at ?? Math.floor(Date.now() / 1000),
});

export default function Favorites() {
  const { confirm, toast } = useOverlay();
  const [tab, setTab] = useState<Tab>('favorites');
  const favorites = useApi<{ saved: SavedDevice[] }>('/api/v2/assist/saved/favorites');
  const recent = useApi<{ saved: SavedDevice[] }>('/api/v2/assist/saved/recent');
  const [adding, setAdding] = useState(false);
  const [editing, setEditing] = useState<SavedDevice>();
  const [code, setCode] = useState('');
  const [remark, setRemark] = useState('');
  const [error, setError] = useState<string>();

  const list = (tab === 'favorites' ? favorites.data?.saved : recent.data?.saved) ?? [];
  const current = favorites.data?.saved ?? [];

  async function put(items: ReturnType<typeof toItem>[]) {
    await call('PUT', '/api/v2/assist/saved/favorites', { favorites: items });
    await Promise.all([favorites.reload(), recent.reload()]);
  }

  async function submit(e: FormEvent) {
    e.preventDefault();
    const id = code.replace(/\s/g, '');
    setError(undefined);
    try {
      if (editing) {
        await put(
          current.map((s) =>
            s.connect_id === editing.connect_id ? { ...toItem(s), remark: remark.trim() } : toItem(s),
          ),
        );
        toast('已保存');
      } else {
        if (!/^\d{9}$/.test(id)) {
          setError('短号是 9 位数字');
          return;
        }
        if (current.some((s) => s.connect_id === id)) {
          setError('已经收藏过这个短号');
          return;
        }
        await put([
          ...current.map(toItem),
          { connect_id: id, remark: remark.trim(), favorited_at: Math.floor(Date.now() / 1000) },
        ]);
        toast('已收藏');
      }
      setAdding(false);
      setEditing(undefined);
    } catch (err) {
      setError(errorText(err));
    }
  }

  return (
    <div className="stack">
      <PageHeader
        title="收藏"
        actions={
          <Button
            variant="primary"
            icon="plus"
            onClick={() => {
              setCode('');
              setRemark('');
              setError(undefined);
              setAdding(true);
            }}
          >
            收藏短号
          </Button>
        }
      />
      <Tabs<Tab>
        value={tab}
        onChange={setTab}
        items={[
          { id: 'favorites', label: `收藏 ${favorites.data?.saved.length ?? ''}` },
          { id: 'recent', label: `最近连接 ${recent.data?.saved.length ?? ''}` },
        ]}
      />
      {(favorites.error || recent.error) && <Alert>{favorites.error ?? recent.error}</Alert>}
      <Card pad={false}>
        {!(tab === 'favorites' ? favorites.data : recent.data) ? (
          <Loading />
        ) : list.length === 0 ? (
          <Empty
            icon={tab === 'favorites' ? 'star' : 'activity'}
            title={tab === 'favorites' ? '暂无收藏' : '暂无最近连接'}
          />
        ) : (
          <div className="table-wrap">
            <table className="table">
              <thead>
                <tr>
                  <th>设备</th>
                  <th>短号</th>
                  <th>{tab === 'favorites' ? '收藏于' : '最近连接'}</th>
                  <th />
                </tr>
              </thead>
              <tbody>
                {list.map((s) => (
                  <tr key={s.connect_id + s.publisher_device_id}>
                    <td className="cell-main">{s.remark || <span className="muted">（无备注）</span>}</td>
                    <td className="mono">{connectIdText(s.connect_id)}</td>
                    <td className="nowrap muted">
                      {tab === 'favorites' ? dateTime(s.favorited_at) : ago(s.last_connected_at)}
                    </td>
                    <td className="actions">
                      <IconButton icon="play" label="观看" onClick={() => navigate(`/visit?code=${s.connect_id}`)} />
                      {tab === 'favorites' && (
                        <IconButton
                          icon="edit"
                          label="修改备注"
                          onClick={() => {
                            setEditing(s);
                            setCode(s.connect_id);
                            setRemark(s.remark);
                            setError(undefined);
                          }}
                        />
                      )}
                      <IconButton
                        icon="trash"
                        label="删除"
                        variant="danger"
                        onClick={async () => {
                          if (
                            await confirm({
                              title: '删除',
                              message: `删除 ${connectIdText(s.connect_id)}？`,
                              confirm: '删除',
                              danger: true,
                            })
                          ) {
                            try {
                              if (tab === 'favorites')
                                await put(current.filter((x) => x.connect_id !== s.connect_id).map(toItem));
                              else {
                                await call('DELETE', `/api/v2/assist/saved/recent/${s.publisher_device_id}`);
                                await recent.reload();
                              }
                              toast('已删除');
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
      <Dialog
        open={adding || !!editing}
        title={editing ? '修改备注' : '收藏短号'}
        onClose={() => {
          setAdding(false);
          setEditing(undefined);
        }}
        footer={
          <>
            <Button onClick={() => (setAdding(false), setEditing(undefined))}>取消</Button>
            <Button variant="primary" type="submit" form="favorite-form">
              保存
            </Button>
          </>
        }
      >
        <form id="favorite-form" className="stack" onSubmit={submit}>
          {error && <Alert>{error}</Alert>}
          <Field label="短号">
            <Input
              className="mono"
              value={code}
              disabled={!!editing}
              onChange={(e) => setCode(e.target.value)}
              placeholder="000 000 000"
            />
          </Field>
          <Field label="备注">
            <Input value={remark} onChange={(e) => setRemark(e.target.value)} maxLength={64} />
          </Field>
        </form>
      </Dialog>
    </div>
  );
}
