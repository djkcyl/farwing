// Edge relay nodes: add (join command), edit, disable, rejoin, remove.
import { useState, type FormEvent } from 'react';
import { useT } from '../../i18n/react';
import { call, errorText } from '../../lib/api';
import { ago, bitrate, dateTime } from '../../lib/format';
import type { EdgeNode, JoinCommand } from '../../lib/types';
import { useApi } from '../../lib/useApi';
import {
  Alert,
  Badge,
  Button,
  Card,
  Copy,
  Empty,
  Field,
  IconButton,
  Input,
  Loading,
  PageHeader,
  Switch,
  type Tone,
} from '../../ui/kit';
import { Dialog, useOverlay } from '../../ui/overlay';
import { OnceDialog } from './Users';

type M = ReturnType<typeof useT>['admin']['nodes'];

function state(node: EdgeNode, m: M): [string, Tone] {
  if (!node.public_key) return [m.waiting, 'amber'];
  if (!node.enabled) return [m.disabled, 'muted'];
  if (node.usable) return [m.online, 'green'];
  if (node.online) return [m.faulty, 'red'];
  return [m.offline, 'muted'];
}

function AddNode({ open, onClose, onCreated }: { open: boolean; onClose(): void; onCreated(j: JoinCommand): void }) {
  const t = useT();
  const m = t.admin.nodes;
  const [name, setName] = useState('');
  const [region, setRegion] = useState('');
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string>();
  async function submit(e: FormEvent) {
    e.preventDefault();
    setBusy(true);
    setError(undefined);
    try {
      const join = await call<JoinCommand>('POST', '/api/v2/admin/nodes', { name: name.trim(), region: region.trim() });
      setName('');
      setRegion('');
      onCreated(join);
    } catch (err) {
      setError(errorText(err));
    } finally {
      setBusy(false);
    }
  }
  return (
    <Dialog
      open={open}
      title={m.add}
      onClose={onClose}
      footer={
        <>
          <Button onClick={onClose}>{t.common.cancel}</Button>
          <Button variant="primary" type="submit" form="node-form" loading={busy} disabled={!name.trim()}>
            {t.common.create}
          </Button>
        </>
      }
    >
      <form id="node-form" className="stack" onSubmit={submit}>
        {error && <Alert>{error}</Alert>}
        <Field label={m.name}>
          <Input value={name} maxLength={64} onChange={(e) => setName(e.target.value)} required />
        </Field>
        <Field label={m.region}>
          <Input value={region} maxLength={64} onChange={(e) => setRegion(e.target.value)} />
        </Field>
      </form>
    </Dialog>
  );
}

function EditNode({ node, onClose, onSaved }: { node?: EdgeNode; onClose(): void; onSaved(): void }) {
  const t = useT();
  const m = t.admin.nodes;
  const s = t.admin.settings;
  const [draft, setDraft] = useState(() =>
    node
      ? {
          name: node.name,
          region: node.region,
          enabled: node.enabled,
          public_ip: node.settings.public_ip ?? '',
          bandwidth: node.settings.bandwidth_limit_mbps ? String(node.settings.bandwidth_limit_mbps) : '',
          listen_udp: node.settings.listen_udp ?? '',
          listen_tcp: node.settings.listen_tcp ?? '',
          listen_tls: node.settings.listen_tls ?? '',
          relay_port_min: node.settings.relay_port_min ? String(node.settings.relay_port_min) : '',
          relay_port_max: node.settings.relay_port_max ? String(node.settings.relay_port_max) : '',
          sfu_port: node.settings.sfu_port ? String(node.settings.sfu_port) : '',
        }
      : undefined,
  );
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string>();
  if (!node || !draft) return null;
  const set = (p: Partial<typeof draft>) => setDraft({ ...draft, ...p });
  const address = (v: string) => v.trim() || null;
  const number = (v: string) => (v.trim() ? Number(v) : null);
  async function save(e: FormEvent) {
    e.preventDefault();
    if (!node || !draft) return;
    setBusy(true);
    setError(undefined);
    try {
      await call('PATCH', `/api/v2/admin/nodes/${node.id}`, {
        name: draft.name.trim(),
        region: draft.region.trim(),
        enabled: draft.enabled,
        settings: {
          public_ip: address(draft.public_ip),
          bandwidth_limit_mbps: number(draft.bandwidth),
          listen_udp: address(draft.listen_udp),
          listen_tcp: address(draft.listen_tcp),
          listen_tls: address(draft.listen_tls),
          relay_port_min: number(draft.relay_port_min),
          relay_port_max: number(draft.relay_port_max),
          sfu_port: number(draft.sfu_port),
        },
      });
      onSaved();
    } catch (err) {
      setError(errorText(err));
    } finally {
      setBusy(false);
    }
  }
  return (
    <Dialog
      open
      wide
      title={node.name}
      onClose={onClose}
      footer={
        <>
          <Button onClick={onClose}>{t.common.cancel}</Button>
          <Button variant="primary" type="submit" form="node-edit" loading={busy}>
            {t.common.save}
          </Button>
        </>
      }
    >
      <form id="node-edit" className="stack" onSubmit={save}>
        {error && <Alert>{error}</Alert>}
        <Switch checked={draft.enabled} onChange={(enabled) => set({ enabled })} label={m.enabled} />
        <div className="grid grid-3">
          <Field label={m.name}>
            <Input value={draft.name} maxLength={64} onChange={(e) => set({ name: e.target.value })} required />
          </Field>
          <Field label={m.region}>
            <Input value={draft.region} maxLength={64} onChange={(e) => set({ region: e.target.value })} />
          </Field>
          <Field label={s.publicIp}>
            <Input
              className="mono-input"
              value={draft.public_ip}
              placeholder={node.address ?? undefined}
              onChange={(e) => set({ public_ip: e.target.value })}
            />
          </Field>
          <Field label={m.bandwidthLimit}>
            <Input
              type="number"
              min={1}
              value={draft.bandwidth}
              placeholder={t.common.unlimited}
              onChange={(e) => set({ bandwidth: e.target.value })}
            />
          </Field>
          <Field label={s.listenUdp}>
            <Input
              className="mono-input"
              value={draft.listen_udp}
              onChange={(e) => set({ listen_udp: e.target.value })}
            />
          </Field>
          <Field label={s.listenTcp}>
            <Input
              className="mono-input"
              value={draft.listen_tcp}
              onChange={(e) => set({ listen_tcp: e.target.value })}
            />
          </Field>
          <Field label={s.listenTls}>
            <Input
              className="mono-input"
              value={draft.listen_tls}
              onChange={(e) => set({ listen_tls: e.target.value })}
            />
          </Field>
          <Field label={s.relayPorts} group>
            <div className="row gap-sm">
              <Input
                type="number"
                min={1024}
                max={65535}
                aria-label={s.relayPorts}
                value={draft.relay_port_min}
                onChange={(e) => set({ relay_port_min: e.target.value })}
              />
              <span className="muted">–</span>
              <Input
                type="number"
                min={1024}
                max={65535}
                aria-label={s.relayPorts}
                value={draft.relay_port_max}
                onChange={(e) => set({ relay_port_max: e.target.value })}
              />
            </div>
          </Field>
          <Field label={s.sfuPort}>
            <Input
              type="number"
              min={1}
              max={65535}
              value={draft.sfu_port}
              onChange={(e) => set({ sfu_port: e.target.value })}
            />
          </Field>
        </div>
      </form>
    </Dialog>
  );
}

export default function Nodes() {
  const t = useT();
  const m = t.admin.nodes;
  const { confirm, toast } = useOverlay();
  const { data, error, reload } = useApi<EdgeNode[]>('/api/v2/admin/nodes', 5000);
  const [adding, setAdding] = useState(false);
  const [editing, setEditing] = useState<EdgeNode>();
  const [join, setJoin] = useState<JoinCommand>();

  async function rejoin(node: EdgeNode) {
    if (!(await confirm({ title: m.rejoinTitle, message: m.rejoinMessage(node.name), confirm: m.rejoin }))) return;
    try {
      setJoin(await call<JoinCommand>('POST', `/api/v2/admin/nodes/${node.id}/rejoin`));
    } catch (err) {
      toast(errorText(err), 'red');
    }
  }

  async function remove(node: EdgeNode) {
    if (
      !(await confirm({
        title: m.removeTitle,
        message: m.removeMessage(node.name),
        confirm: m.remove,
        danger: true,
      }))
    )
      return;
    try {
      await call('DELETE', `/api/v2/admin/nodes/${node.id}`);
      toast(m.removed);
      await reload();
    } catch (err) {
      toast(errorText(err), 'red');
    }
  }

  return (
    <div className="stack">
      <PageHeader
        title={t.nav.nodes}
        actions={
          <Button variant="primary" icon="plus" onClick={() => setAdding(true)}>
            {m.add}
          </Button>
        }
      />
      {error && <Alert>{error}</Alert>}
      <Card pad={false}>
        {!data ? (
          <Loading />
        ) : data.length === 0 ? (
          <Empty icon="globe" title={m.empty} />
        ) : (
          <div className="table-wrap">
            <table className="table">
              <thead>
                <tr>
                  <th>{m.name}</th>
                  <th>{t.admin.col.status}</th>
                  <th>{m.address}</th>
                  <th>{m.allocations}</th>
                  <th>{m.bandwidth}</th>
                  <th>{m.forwarding}</th>
                  <th>{t.admin.col.lastSeen}</th>
                  <th />
                </tr>
              </thead>
              <tbody>
                {data.map((node) => {
                  const [text, tone] = state(node, m);
                  const health = node.health;
                  return (
                    <tr key={node.id}>
                      <td>
                        <div className="cell-main">{node.name}</div>
                        <div className="cell-sub">{node.region || m.noRegion}</div>
                      </td>
                      <td>
                        <Badge tone={tone}>{text}</Badge>
                        {health?.error && <div className="cell-sub red">{health.error}</div>}
                      </td>
                      <td className="mono small">
                        {node.settings.public_ip ?? node.address ?? <span className="muted">—</span>}
                      </td>
                      <td>{health ? health.allocations : <span className="muted">—</span>}</td>
                      <td className="nowrap">
                        {health ? bitrate(health.egress_bps) : <span className="muted">—</span>}
                      </td>
                      <td className="nowrap">
                        {health?.sfu_port ? (
                          <>
                            <span className="mono small">{health.sfu_port}</span>
                            {health.rooms.length > 0 && (
                              <span className="muted small"> · {m.rooms(health.rooms.length)}</span>
                            )}
                          </>
                        ) : (
                          <span className="muted">—</span>
                        )}
                      </td>
                      <td className="nowrap muted">
                        {node.online ? m.now : node.last_seen_at ? ago(node.last_seen_at) : m.never}
                      </td>
                      <td className="actions">
                        <IconButton icon="edit" label={t.common.edit} onClick={() => setEditing(node)} />
                        <IconButton icon="refresh" label={m.rejoin} onClick={() => void rejoin(node)} />
                        <IconButton icon="trash" label={m.remove} variant="danger" onClick={() => void remove(node)} />
                      </td>
                    </tr>
                  );
                })}
              </tbody>
            </table>
          </div>
        )}
      </Card>
      <AddNode
        open={adding}
        onClose={() => setAdding(false)}
        onCreated={(j) => {
          setAdding(false);
          setJoin(j);
          void reload();
        }}
      />
      <EditNode
        key={editing?.id}
        node={editing}
        onClose={() => setEditing(undefined)}
        onSaved={() => {
          setEditing(undefined);
          toast(t.admin.settings.saved);
          void reload();
        }}
      />
      <OnceDialog title={m.joinTitle} open={!!join} onClose={() => setJoin(undefined)}>
        <div className="stack">
          <Field label={m.command} group>
            <div className="secret small">
              <Copy text={join?.command ?? ''} />
            </div>
          </Field>
          <span className="small muted">{m.validUntil(dateTime(join?.expires_at))}</span>
        </div>
      </OnceDialog>
    </div>
  );
}
