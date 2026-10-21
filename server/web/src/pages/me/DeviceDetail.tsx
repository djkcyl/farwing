// One device: facts, hardware, who is connected; rename or remove own ones.
import { useState } from 'react';
import { call, errorText } from '../../lib/api';
import {
  ago,
  bytes,
  connectIdText,
  dateTime,
  deviceIdText,
  displayName,
  platformText,
  PURPOSES,
  RELATIONS,
  shareSummary,
} from '../../lib/format';
import { Link, navigate } from '../../lib/router';
import type { DeviceDetail as Detail, DeviceList } from '../../lib/types';
import { useApi } from '../../lib/useApi';
import { viewBlocker } from '../../lib/devices';
import { Icon } from '../../ui/icons';
import { Alert, Button, Card, Copy, Empty, Field, Input, Loading, Online, PageHeader, Row, Rows } from '../../ui/kit';
import { Dialog, useOverlay } from '../../ui/overlay';

export default function DeviceDetail({ params }: { params: Record<string, string> }) {
  const { confirm, toast } = useOverlay();
  const detail = useApi<Detail>(`/api/v2/devices/${params.id}/detail`, 10000);
  const list = useApi<DeviceList>('/api/v2/devices', 10000);
  const [renaming, setRenaming] = useState(false);
  const [alias, setAlias] = useState('');

  const d = detail.data;
  if (!d) return detail.error ? <Alert>{detail.error}</Alert> : <Loading />;
  const device = [...(list.data?.own ?? []), ...(list.data?.shared ?? [])].find((x) => x.device_id === d.device_id);
  const own = !d.owner;
  const blocker = device ? viewBlocker(device) : '正在读取';

  return (
    <div className="stack">
      <PageHeader
        back={
          <Link to="/devices" className="back">
            <Icon name="back" size={14} /> 设备
          </Link>
        }
        title={d.alias}
        subtitle={
          <span className="row gap-sm">
            <Online online={d.online} text={d.online ? '在线' : `离线 · ${ago(d.last_seen_at)}`} />
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
              在浏览器中观看
            </Button>
            {own && (
              <Button
                icon="edit"
                onClick={() => {
                  setAlias(d.alias);
                  setRenaming(true);
                }}
              >
                重命名
              </Button>
            )}
            {own && (
              <Button
                icon="unlink"
                variant="danger"
                onClick={async () => {
                  if (
                    await confirm({
                      title: '从账号移除',
                      message: `${d.alias} 将退出这个账号。`,
                      confirm: '移除',
                      danger: true,
                    })
                  ) {
                    try {
                      await call('DELETE', `/api/v2/devices/${d.device_id}`);
                      toast('已移除');
                      navigate('/devices');
                    } catch (err) {
                      toast(errorText(err), 'red');
                    }
                  }
                }}
              >
                移除
              </Button>
            )}
          </>
        }
      />
      <div className="grid grid-2">
        <Card title="设备">
          <Rows>
            <Row label="设备 ID">
              <Copy text={d.device_id} display={deviceIdText(d.device_id)} />
            </Row>
            {d.connect_id && (
              <Row label="短号">
                <Copy text={d.connect_id} display={connectIdText(d.connect_id)} />
              </Row>
            )}
            <Row label="系统">{d.os || '—'}</Row>
            <Row label="版本">{d.version || '—'}</Row>
            <Row label="注册时间">{dateTime(d.registered_at)}</Row>
            <Row label="最近在线">{d.online ? '现在' : ago(d.last_seen_at)}</Row>
            {d.owner && <Row label="所属">{displayName(d.owner)}</Row>}
            {device && !own && <Row label="我的权限">{shareSummary(device.granted)}</Row>}
          </Rows>
        </Card>
        {d.hardware ? (
          <Card title="硬件">
            <Rows>
              <Row label="处理器">{d.hardware.cpu || '—'}</Row>
              <Row label="显卡">{d.hardware.video.join('、') || '—'}</Row>
              <Row label="内存">{d.hardware.memory ? bytes(d.hardware.memory) : '—'}</Row>
              <Row label="主板">{d.hardware.base_board || '—'}</Row>
              <Row label="屏幕">{d.hardware.screen || '—'}</Row>
            </Rows>
          </Card>
        ) : (
          <Card title="硬件">
            <Empty icon="info" title={own ? '暂无硬件信息' : '仅设备主人可见'} />
          </Card>
        )}
      </div>
      <Card title="正在连接" pad={false}>
        {!device || device.participants.length === 0 ? (
          <Empty icon="users" title="没有人在连接" />
        ) : (
          <div className="list">
            {device.participants.map((p) => (
              <div className="list-item" key={p.participant_id}>
                <div className="grow">
                  <div>{p.nickname || '访客'}</div>
                  <div className="small muted">
                    {p.device_alias} · {RELATIONS[p.relation] ?? p.relation} · {PURPOSES[p.purpose] ?? p.purpose}
                  </div>
                </div>
                <span className="small faint">{ago(p.started_at)}开始</span>
              </div>
            ))}
          </div>
        )}
      </Card>
      <Dialog
        open={renaming}
        title="重命名设备"
        onClose={() => setRenaming(false)}
        footer={
          <>
            <Button onClick={() => setRenaming(false)}>取消</Button>
            <Button
              variant="primary"
              onClick={async () => {
                try {
                  await call('PATCH', `/api/v2/devices/${d.device_id}`, { alias: alias.trim() });
                  setRenaming(false);
                  toast('已重命名');
                  await detail.reload();
                } catch (err) {
                  toast(errorText(err), 'red');
                }
              }}
            >
              保存
            </Button>
          </>
        }
      >
        <Field label="名称">
          <Input value={alias} onChange={(e) => setAlias(e.target.value)} maxLength={64} />
        </Field>
      </Dialog>
    </div>
  );
}
