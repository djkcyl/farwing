// Settings changeable online, and the configuration file (read-only).
import { useState, type ReactNode } from 'react';
import type { Messages } from '../../i18n';
import { useT } from '../../i18n/react';
import { call, errorText } from '../../lib/api';
import { useAuth } from '../../lib/auth';
import { capText, listText, WEB_CAPS } from '../../lib/format';
import type { RuntimeSettings, SettingsView } from '../../lib/types';
import { useApi } from '../../lib/useApi';
import { Alert, Button, Card, Checkbox, Field, Input, PageHeader, PageState, Row, Rows, Switch } from '../../ui/kit';
import { useOverlay } from '../../ui/overlay';

function value(v: unknown, t: Messages): ReactNode {
  if (v === null || v === undefined || v === '') return <span className="muted">—</span>;
  if (typeof v === 'boolean') return v ? t.common.yes : t.common.no;
  if (Array.isArray(v)) return v.length ? listText(v.map((x) => String(x))) : <span className="muted">—</span>;
  return <span className="mono small">{String(v)}</span>;
}

/// Sections of the configuration file ('': top level), with their titles.
const SECTIONS: [string, keyof Messages['admin']['settings']['sections']][] = [
  ['', 'general'],
  ['tls', 'tls'],
  ['turn', 'turn'],
  ['web', 'web'],
  ['limits', 'limits'],
];

function Form({ initial, onSaved }: { initial: RuntimeSettings; onSaved(): void }) {
  const t = useT();
  const m = t.admin.settings;
  const { toast } = useOverlay();
  const auth = useAuth();
  const [form, setForm] = useState(initial);
  const [bitrate, setBitrate] = useState(initial.bitrate_limit_mbps ? String(initial.bitrate_limit_mbps) : '');
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string>();
  const changed =
    JSON.stringify({ ...form, bitrate_limit_mbps: bitrate ? Number(bitrate) : null }) !== JSON.stringify(initial);

  async function save() {
    setBusy(true);
    setError(undefined);
    try {
      await call('PUT', '/api/v2/admin/settings', {
        ...form,
        instance_name: form.instance_name.trim(),
        bitrate_limit_mbps: bitrate ? Number(bitrate) : null,
      });
      toast(m.saved);
      onSaved();
      void auth.refresh();
    } catch (err) {
      setError(errorText(err));
    } finally {
      setBusy(false);
    }
  }

  return (
    <Card
      title={m.runtime}
      actions={
        <Button variant="primary" onClick={save} loading={busy} disabled={!changed}>
          {t.common.save}
        </Button>
      }
    >
      <div className="stack">
        {error && <Alert>{error}</Alert>}
        <Field label={m.instanceName}>
          <Input
            value={form.instance_name}
            onChange={(e) => setForm({ ...form, instance_name: e.target.value })}
            maxLength={64}
          />
        </Field>
        <Field label={m.bitrateLimit}>
          <Input
            type="number"
            min={1}
            max={10000}
            value={bitrate}
            onChange={(e) => setBitrate(e.target.value)}
            placeholder={t.common.unlimited}
          />
        </Field>
        <Switch checked={form.guests} onChange={(v) => setForm({ ...form, guests: v })} label={t.admin.guests} />
        <Switch
          checked={form.registration}
          onChange={(v) => setForm({ ...form, registration: v })}
          label={t.admin.registration}
        />
        <div className="field" role="group" aria-labelledby="web-caps-label">
          <span className="field-label" id="web-caps-label">
            {m.webCaps}
          </span>
          <div className="row gap wrap">
            {WEB_CAPS.map((id) => (
              <Checkbox
                key={id}
                label={capText(id)}
                checked={form.web_caps.includes(id)}
                disabled={id === 'view'}
                onChange={(v) =>
                  setForm({
                    ...form,
                    web_caps: WEB_CAPS.filter((c) => (c === id ? v : form.web_caps.includes(c))),
                  })
                }
              />
            ))}
          </div>
        </div>
      </div>
    </Card>
  );
}

export default function Settings() {
  const t = useT();
  const { data, error, reload } = useApi<SettingsView>('/api/v2/admin/settings');
  if (!data) return <PageState title={t.nav.settings} error={error} />;
  const config = data.config;
  return (
    <div className="stack">
      <PageHeader title={t.nav.settings} />
      <Form key={JSON.stringify(data.runtime)} initial={data.runtime} onSaved={() => void reload()} />
      <Card title={t.admin.settings.config}>
        <div className="stack">
          {SECTIONS.map(([key, title]) => {
            const section = (key ? config[key] : config) as Record<string, unknown> | undefined;
            if (!section) return null;
            const rows = Object.entries(section).filter(
              ([, v]) => key !== '' || typeof v !== 'object' || Array.isArray(v) || v === null,
            );
            return (
              <div key={key} className="stack-sm">
                <h3>{t.admin.settings.sections[title]}</h3>
                <Rows>
                  {rows.map(([k, v]) => (
                    <Row key={k} label={<span className="mono small">{k}</span>}>
                      {value(v, t)}
                    </Row>
                  ))}
                </Rows>
              </div>
            );
          })}
        </div>
      </Card>
    </div>
  );
}
