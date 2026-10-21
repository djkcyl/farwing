// This account's devices and the devices friends share with it.
import { useState } from 'react';
import { byName } from '../../lib/devices';
import type { DeviceList } from '../../lib/types';
import { useApi } from '../../lib/useApi';
import { DeviceCard } from '../../ui/DeviceCard';
import { Alert, Empty, Loading, PageHeader, Tabs } from '../../ui/kit';

type Tab = 'own' | 'shared';

export default function Devices() {
  const { data, error } = useApi<DeviceList>('/api/v2/devices', 10000);
  const [tab, setTab] = useState<Tab>('own');
  if (!data) return error ? <Alert>{error}</Alert> : <Loading />;
  const own = data.own.filter((d) => d.platform !== 'web');
  const browsers = data.own.length - own.length;
  const list = tab === 'own' ? own : data.shared;
  const order = [...list].sort(byName);
  return (
    <div className="stack">
      <PageHeader title="设备" />
      {error && <Alert tone="amber">刷新失败：{error}</Alert>}
      <Tabs<Tab>
        value={tab}
        onChange={setTab}
        items={[
          { id: 'own', label: `我的设备 ${own.length}` },
          { id: 'shared', label: `好友共享 ${data.shared.length}` },
        ]}
      />
      {order.length === 0 ? (
        <Empty icon="monitor" title={tab === 'own' ? '暂无设备' : '暂无共享设备'} />
      ) : (
        <div className="device-grid">
          {order.map((d) => (
            <DeviceCard key={d.device_id} device={d} shared={tab === 'shared'} />
          ))}
        </div>
      )}
      {tab === 'own' && browsers > 0 && <p className="small muted">另有 {browsers} 个网页登录</p>}
    </div>
  );
}
