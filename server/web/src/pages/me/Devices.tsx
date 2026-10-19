// This account's devices and the devices friends share with it.
import { useState } from 'react';
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
  const order = [...list].sort(
    (a, b) => Number(b.status === 'online') - Number(a.status === 'online') || b.last_seen_at - a.last_seen_at,
  );
  return (
    <div className="stack">
      <PageHeader title="设备" subtitle="在浏览器中观看在线的设备；网页端只看不控。" />
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
        <Empty
          icon="monitor"
          title={tab === 'own' ? '账号中还没有电脑' : '还没有好友共享设备给你'}
          hint={
            tab === 'own'
              ? '在电脑上安装 Farwing 客户端并登录这个账号，它就会出现在这里。'
              : '好友在客户端或网页的“好友”中为你开启权限后，设备会出现在这里。'
          }
        />
      ) : (
        <div className="device-grid">
          {order.map((d) => (
            <DeviceCard key={d.device_id} device={d} shared={tab === 'shared'} />
          ))}
        </div>
      )}
      {tab === 'own' && browsers > 0 && (
        <p className="small muted">另有 {browsers} 个网页登录，在“账号与安全”中管理。</p>
      )}
    </div>
  );
}
