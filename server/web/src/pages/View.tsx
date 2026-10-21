// Watch an own or shared device (account session).
import { useCallback } from 'react';
import { useT } from '../i18n/react';
import { displayName, platformText } from '../lib/format';
import { navigate } from '../lib/router';
import type { DeviceList } from '../lib/types';
import { useApi } from '../lib/useApi';
import { accountAdmission } from '../lib/viewer';
import { ViewerScreen } from '../ui/ViewerScreen';

export default function View({ params }: { params: Record<string, string> }) {
  const t = useT();
  const target = params.id;
  const list = useApi<DeviceList>('/api/v2/devices');
  const device = [...(list.data?.own ?? []), ...(list.data?.shared ?? [])].find((d) => d.device_id === target);
  const admit = useCallback(() => accountAdmission(target), [target]);
  const title = device?.alias ?? target;
  const subtitle = device
    ? `${platformText(device.platform)}${device.owner ? ` · ${t.devices.sharedBy(displayName(device.owner))}` : ''}`
    : undefined;
  return (
    <ViewerScreen
      key={target}
      title={title}
      subtitle={subtitle}
      admit={admit}
      onLeave={() => (history.length > 1 ? history.back() : navigate('/devices'))}
    />
  );
}
