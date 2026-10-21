// This account's devices and the devices friends share with it.
import { useState } from 'react';
import { useT } from '../../i18n/react';
import { byName } from '../../lib/devices';
import type { DeviceList } from '../../lib/types';
import { useApi } from '../../lib/useApi';
import { DeviceCard } from '../../ui/DeviceCard';
import { Alert, Empty, Loading, PageHeader, Tabs } from '../../ui/kit';

type Tab = 'own' | 'shared';

export default function Devices() {
  const t = useT();
  const m = t.account.devices;
  const { data, error } = useApi<DeviceList>('/api/v2/devices', 10000);
  const [tab, setTab] = useState<Tab>('own');
  if (!data) return error ? <Alert>{error}</Alert> : <Loading />;
  const own = data.own.filter((d) => d.platform !== 'web');
  const browsers = data.own.length - own.length;
  const list = tab === 'own' ? own : data.shared;
  const order = [...list].sort(byName);
  return (
    <div className="stack">
      <PageHeader title={t.nav.devices} />
      {error && <Alert tone="amber">{t.common.refreshFailed(error)}</Alert>}
      <Tabs<Tab>
        value={tab}
        onChange={setTab}
        items={[
          { id: 'own', label: `${m.own} ${own.length}` },
          { id: 'shared', label: `${m.shared} ${data.shared.length}` },
        ]}
      />
      {order.length === 0 ? (
        <Empty icon="monitor" title={tab === 'own' ? t.account.noDevices : m.noShared} />
      ) : (
        <div className="device-grid">
          {order.map((d) => (
            <DeviceCard key={d.device_id} device={d} shared={tab === 'shared'} />
          ))}
        </div>
      )}
      {tab === 'own' && browsers > 0 && <p className="small muted">{m.webLogins(browsers)}</p>}
    </div>
  );
}
