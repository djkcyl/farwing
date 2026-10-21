// Saved short codes (favourites, recent connections), shared with the
// client. Access codes stay on each client; the web never stores them.
import { useState, type FormEvent } from 'react';
import { useT } from '../../i18n/react';
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
  const t = useT();
  const m = t.account.favorites;
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
        toast(t.common.saved);
      } else {
        if (!/^\d{9}$/.test(id)) {
          setError(m.badConnectId);
          return;
        }
        if (current.some((s) => s.connect_id === id)) {
          setError(m.duplicate);
          return;
        }
        await put([
          ...current.map(toItem),
          { connect_id: id, remark: remark.trim(), favorited_at: Math.floor(Date.now() / 1000) },
        ]);
        toast(m.added);
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
        title={t.nav.favorites}
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
            {m.add}
          </Button>
        }
      />
      <Tabs<Tab>
        value={tab}
        onChange={setTab}
        items={[
          { id: 'favorites', label: `${m.favorites} ${favorites.data?.saved.length ?? ''}` },
          { id: 'recent', label: `${m.recent} ${recent.data?.saved.length ?? ''}` },
        ]}
      />
      {(favorites.error || recent.error) && <Alert>{favorites.error ?? recent.error}</Alert>}
      <Card pad={false}>
        {!(tab === 'favorites' ? favorites.data : recent.data) ? (
          <Loading />
        ) : list.length === 0 ? (
          <Empty
            icon={tab === 'favorites' ? 'star' : 'activity'}
            title={tab === 'favorites' ? m.noFavorites : m.noRecent}
          />
        ) : (
          <div className="table-wrap">
            <table className="table">
              <thead>
                <tr>
                  <th>{m.device}</th>
                  <th>{t.account.connectId}</th>
                  <th>{tab === 'favorites' ? m.favoritedAt : m.lastConnected}</th>
                  <th />
                </tr>
              </thead>
              <tbody>
                {list.map((s) => (
                  <tr key={s.connect_id + s.publisher_device_id}>
                    <td className="cell-main">{s.remark || <span className="muted">{t.common.noNote}</span>}</td>
                    <td className="mono">{connectIdText(s.connect_id)}</td>
                    <td className="nowrap muted">
                      {tab === 'favorites' ? dateTime(s.favorited_at) : ago(s.last_connected_at)}
                    </td>
                    <td className="actions">
                      <IconButton
                        icon="play"
                        label={t.devices.watch}
                        onClick={() => navigate(`/visit?code=${s.connect_id}`)}
                      />
                      {tab === 'favorites' && (
                        <IconButton
                          icon="edit"
                          label={m.editNote}
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
                        label={t.common.delete}
                        variant="danger"
                        onClick={async () => {
                          if (
                            await confirm({
                              title: m.deleteTitle(connectIdText(s.connect_id)),
                              confirm: t.common.delete,
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
                              toast(t.common.deleted);
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
        title={editing ? m.editNote : m.add}
        onClose={() => {
          setAdding(false);
          setEditing(undefined);
        }}
        footer={
          <>
            <Button onClick={() => (setAdding(false), setEditing(undefined))}>{t.common.cancel}</Button>
            <Button variant="primary" type="submit" form="favorite-form">
              {t.common.save}
            </Button>
          </>
        }
      >
        <form id="favorite-form" className="stack" onSubmit={submit}>
          {error && <Alert>{error}</Alert>}
          <Field label={t.account.connectId}>
            <Input
              className="mono"
              value={code}
              disabled={!!editing}
              onChange={(e) => setCode(e.target.value)}
              placeholder="000 000 000"
            />
          </Field>
          <Field label={t.account.note}>
            <Input value={remark} onChange={(e) => setRemark(e.target.value)} maxLength={64} />
          </Field>
        </form>
      </Dialog>
    </div>
  );
}
