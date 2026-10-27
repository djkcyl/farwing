// One device: facts, hardware, who is connected; rename or remove own ones.
import { useState } from 'react';
import { useT } from '../../i18n/react';
import { call, errorText } from '../../lib/api';
import {
  ago,
  bytes,
  connectIdText,
  dateTime,
  deviceIdText,
  displayName,
  listText,
  platformText,
  purposeText,
  relationText,
  shareSummary,
} from '../../lib/format';
import { Link, navigate } from '../../lib/router';
import type { DeviceDetail as Detail, DeviceList } from '../../lib/types';
import { useApi } from '../../lib/useApi';
import { viewBlocker } from '../../lib/devices';
import { DeviceShot } from '../../ui/DeviceCard';
import { Icon } from '../../ui/icons';
import { Button, Card, Copy, Empty, Field, Input, Online, PageHeader, PageState, Row, Rows } from '../../ui/kit';
import { Dialog, useOverlay } from '../../ui/overlay';
import { RecordList } from '../../ui/Records';

export default function DeviceDetail({ params }: { params: Record<string, string> }) {
  const t = useT();
  const m = t.account.device;
  const { confirm, toast } = useOverlay();
  const detail = useApi<Detail>(`/api/v2/devices/${params.id}/detail`, 10000);
  const list = useApi<DeviceList>('/api/v2/devices', 10000);
  const [renaming, setRenaming] = useState(false);
  const [alias, setAlias] = useState('');

  const d = detail.data;
  if (!d) {
    return (
      <PageState
        title={t.nav.devices}
        back={
          <Link to="/devices" className="back">
            <Icon name="back" size={14} /> {t.nav.devices}
          </Link>
        }
        error={detail.error}
      />
    );
  }
  const device = [...(list.data?.own ?? []), ...(list.data?.shared ?? [])].find((x) => x.device_id === d.device_id);
  const own = !d.owner;
  const blocker = device ? viewBlocker(device) : t.common.loading;

  return (
    <div className="stack">
      <PageHeader
        back={
          <Link to="/devices" className="back">
            <Icon name="back" size={14} /> {t.nav.devices}
          </Link>
        }
        title={d.alias}
        subtitle={
          <span className="row gap-sm">
            <Online online={d.online} text={d.online ? t.common.online : m.offlineSince(ago(d.last_seen_at))} />
            <span>·</span>
            <span>
              {platformText(d.platform)} {d.version}
            </span>
          </span>
        }
        actions={
          <>
            <Button
              variant="primary"
              icon="play"
              disabled={!!blocker}
              title={blocker}
              onClick={() => navigate(`/view/${d.device_id}`)}
            >
              {m.watchInBrowser}
            </Button>
            {own && (
              <Button
                icon="edit"
                onClick={() => {
                  setAlias(d.alias);
                  setRenaming(true);
                }}
              >
                {t.common.rename}
              </Button>
            )}
            {own && (
              <Button
                icon="unlink"
                variant="danger"
                onClick={async () => {
                  if (
                    await confirm({
                      title: m.removeTitle,
                      message: m.removeMessage(d.alias),
                      confirm: t.common.remove,
                      danger: true,
                    })
                  ) {
                    try {
                      await call('DELETE', `/api/v2/devices/${d.device_id}`);
                      toast(m.removed);
                      navigate('/devices');
                    } catch (err) {
                      toast(errorText(err), 'red');
                    }
                  }
                }}
              >
                {t.common.remove}
              </Button>
            )}
          </>
        }
      />
      <div className="grid grid-2">
        <Card title={m.info}>
          {device && <DeviceShot device={device} className="device-shot detail-shot" />}
          <Rows>
            <Row label={t.account.deviceId}>
              <Copy text={d.device_id} display={deviceIdText(d.device_id)} />
            </Row>
            {d.connect_id && (
              <Row label={t.account.connectId}>
                <Copy text={d.connect_id} display={connectIdText(d.connect_id)} />
              </Row>
            )}
            <Row label={m.os}>{d.os || '—'}</Row>
            <Row label={t.account.version}>{d.version || '—'}</Row>
            <Row label={m.registered}>{dateTime(d.registered_at)}</Row>
            <Row label={m.lastSeen}>{d.online ? m.now : ago(d.last_seen_at)}</Row>
            {d.owner && <Row label={m.owner}>{displayName(d.owner)}</Row>}
            {device && !own && <Row label={m.myPermissions}>{shareSummary(device.granted)}</Row>}
          </Rows>
        </Card>
        {d.hardware ? (
          <Card title={m.hardware}>
            <Rows>
              <Row label={m.cpu}>{d.hardware.cpu || '—'}</Row>
              <Row label={m.gpu}>{listText(d.hardware.video) || '—'}</Row>
              <Row label={m.memory}>{d.hardware.memory ? bytes(d.hardware.memory) : '—'}</Row>
              <Row label={m.board}>{d.hardware.base_board || '—'}</Row>
              <Row label={m.screen}>{d.hardware.screen || '—'}</Row>
            </Rows>
          </Card>
        ) : (
          <Card title={m.hardware}>
            <Empty icon="info" title={own ? m.noHardware : m.ownerOnly} />
          </Card>
        )}
      </div>
      <Card title={m.connections} pad={false}>
        {!device || device.participants.length === 0 ? (
          <Empty icon="users" title={t.account.noConnections} />
        ) : (
          <div className="list">
            {device.participants.map((p) => (
              <div className="list-item" key={p.participant_id}>
                <div className="grow">
                  <div>{p.nickname || t.common.guest}</div>
                  <div className="small muted">
                    {p.device_alias} · {relationText(p.relation)} · {purposeText(p.purpose)}
                  </div>
                </div>
                <span className="small faint">{m.startedAgo(ago(p.started_at))}</span>
              </div>
            ))}
          </div>
        )}
      </Card>
      <Card title={t.nav.allRecords} pad={false}>
        <RecordList path="/api/v2/records" device={params.id} />
      </Card>
      <Dialog
        open={renaming}
        title={m.renameTitle}
        onClose={() => setRenaming(false)}
        footer={
          <>
            <Button onClick={() => setRenaming(false)}>{t.common.cancel}</Button>
            <Button
              variant="primary"
              onClick={async () => {
                try {
                  await call('PATCH', `/api/v2/devices/${d.device_id}`, { alias: alias.trim() });
                  setRenaming(false);
                  toast(m.renamed);
                  await detail.reload();
                } catch (err) {
                  toast(errorText(err), 'red');
                }
              }}
            >
              {t.common.save}
            </Button>
          </>
        }
      >
        <Field label={t.account.name}>
          <Input value={alias} onChange={(e) => setAlias(e.target.value)} maxLength={64} />
        </Field>
      </Dialog>
    </div>
  );
}
