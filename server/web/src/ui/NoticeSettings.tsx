// Which notifications the account receives: a switch per kind, saved at
// once.
import { useState } from 'react';
import { useT } from '../i18n/react';
import { call, errorText } from '../lib/api';
import { NOTICE_GROUPS } from '../lib/notices';
import type { NoticeSettings as Settings } from '../lib/types';
import { useApi } from '../lib/useApi';
import { Alert, Card, Loading, Switch } from './kit';

export function NoticeSettings() {
  const t = useT();
  const m = t.notices;
  const { data, error } = useApi<Settings>('/api/v2/me/notice-settings');
  const [saved, setSaved] = useState<Settings>();
  const [failed, setFailed] = useState<string>();
  const current = saved ?? data;
  const enabled = new Map(current?.settings.map((s) => [s.kind, s.enabled]));

  async function set(kind: string, on: boolean) {
    try {
      setSaved(await call<Settings>('PUT', '/api/v2/me/notice-settings', { settings: [{ kind, enabled: on }] }));
      setFailed(undefined);
    } catch (err) {
      setFailed(errorText(err));
    }
  }

  return (
    <Card title={m.settings}>
      {error && <Alert>{errorText(error)}</Alert>}
      {failed && <Alert>{failed}</Alert>}
      {!current ? (
        <Loading />
      ) : (
        <div className="stack">
          {NOTICE_GROUPS.map(([group, kinds]) => {
            const shown = kinds.filter((k) => enabled.has(k));
            if (!shown.length) return null;
            return (
              <div className="stack-sm" key={group}>
                <div className="small muted">{m.groups[group]}</div>
                {shown.map((kind) => (
                  <Switch
                    key={kind}
                    checked={enabled.get(kind) ?? false}
                    onChange={(on) => void set(kind, on)}
                    label={m.kinds[kind] ?? kind}
                  />
                ))}
              </div>
            );
          })}
        </div>
      )}
    </Card>
  );
}
