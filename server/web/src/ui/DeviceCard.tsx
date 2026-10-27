// A device as a card: wallpaper, name, state and "watch in the browser".
import { useEffect, useState } from 'react';
import { useT } from '../i18n/react';
import { fetchBlob } from '../lib/api';
import { ago, deviceIdText, displayName, platformText, shareSummary } from '../lib/format';
import { Link, navigate } from '../lib/router';
import type { Device } from '../lib/types';
import { viewBlocker } from '../lib/devices';
import { Icon } from './icons';
import { Button, Online } from './kit';

/// A device's desktop (its wallpaper), or its system's mark: a browser
/// shows a globe, a computer without a wallpaper a screen.
export function DeviceShot({
  device,
  className = 'device-shot',
  iconSize = 34,
}: {
  device?: Pick<Device, 'wallpaper_url' | 'platform'>;
  className?: string;
  iconSize?: number;
}) {
  const [url, setUrl] = useState<string>();
  const wallpaper = device?.wallpaper_url;
  useEffect(() => {
    if (!wallpaper) return;
    let alive = true;
    let objectUrl: string | undefined;
    fetchBlob(wallpaper)
      .then((blob) => {
        objectUrl = URL.createObjectURL(blob);
        if (alive) setUrl(objectUrl);
        else URL.revokeObjectURL(objectUrl);
      })
      .catch(() => undefined);
    return () => {
      alive = false;
      if (objectUrl) URL.revokeObjectURL(objectUrl);
    };
  }, [wallpaper]);
  return (
    <div className={className} style={url ? { backgroundImage: `url(${url})` } : undefined}>
      {!url && <Icon name={device?.platform === 'web' ? 'globe' : 'monitor'} size={iconSize} />}
    </div>
  );
}

export function DeviceCard({ device: d, shared }: { device: Device; shared?: boolean }) {
  const t = useT();
  const blocker = viewBlocker(d);
  const watching = d.participants.length;
  return (
    <article className="device-card">
      <Link to={`/devices/${d.device_id}`} className="device-link" aria-label={d.alias}>
        <DeviceShot device={d} />
      </Link>
      <div className="device-info">
        <div className="row gap-sm">
          <Link to={`/devices/${d.device_id}`} className="device-name ellipsis">
            {d.alias}
          </Link>
          <span className="grow" />
          <Online online={d.status === 'online'} text={d.status === 'online' ? t.common.online : ago(d.last_seen_at)} />
        </div>
        <div className="small muted ellipsis">
          {platformText(d.platform)} {d.version} · <span className="mono">{deviceIdText(d.device_id)}</span>
        </div>
        {shared && d.owner && (
          <div className="small muted ellipsis">
            {t.devices.sharedBy(displayName(d.owner))} · {shareSummary(d.granted)}
          </div>
        )}
        {watching > 0 && (
          <div className="small amber">
            <Icon name="eye" size={12} /> {t.devices.watching(watching)}
          </div>
        )}
        <div className="row gap-sm device-actions">
          <Button
            size="sm"
            variant="primary"
            icon="play"
            disabled={!!blocker}
            title={blocker}
            onClick={() => navigate(`/view/${d.device_id}`)}
          >
            {t.devices.watch}
          </Button>
          <Button size="sm" variant="ghost" onClick={() => navigate(`/devices/${d.device_id}`)}>
            {t.common.details}
          </Button>
        </div>
      </div>
    </article>
  );
}
