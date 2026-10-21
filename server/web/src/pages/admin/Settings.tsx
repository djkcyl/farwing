// Settings changeable online, and the configuration file (read-only).
import { useState, type ReactNode } from 'react';
import { call, errorText } from '../../lib/api';
import { useAuth } from '../../lib/auth';
import { CAPS, WEB_CAPS } from '../../lib/format';
import type { RuntimeSettings, SettingsView } from '../../lib/types';
import { useApi } from '../../lib/useApi';
import { Alert, Button, Card, Checkbox, Field, Input, Loading, PageHeader, Row, Rows, Switch } from '../../ui/kit';
import { useOverlay } from '../../ui/overlay';

function value(v: unknown): ReactNode {
  if (v === null || v === undefined || v === '') return <span className="muted">—</span>;
  if (typeof v === 'boolean') return v ? '是' : '否';
  if (Array.isArray(v)) return v.length ? v.map((x) => String(x)).join('、') : <span className="muted">—</span>;
  return <span className="mono small">{String(v)}</span>;
}

const SECTIONS: [string, string][] = [
  ['', '常规'],
  ['tls', 'TLS'],
  ['turn', '中继（TURN）'],
  ['web', '网页端'],
  ['limits', '限流'],
];

function Form({ initial, onSaved }: { initial: RuntimeSettings; onSaved(): void }) {
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
      toast('设置已保存');
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
      title="在线设置"
      actions={
        <Button variant="primary" onClick={save} loading={busy} disabled={!changed}>
          保存
        </Button>
      }
    >
      <div className="stack">
        {error && <Alert>{error}</Alert>}
        <Field label="实例名称">
          <Input
            value={form.instance_name}
            onChange={(e) => setForm({ ...form, instance_name: e.target.value })}
            maxLength={64}
          />
        </Field>
        <Field label="码率上限（Mbit/s）">
          <Input
            type="number"
            min={1}
            max={10000}
            value={bitrate}
            onChange={(e) => setBitrate(e.target.value)}
            placeholder="不限"
          />
        </Field>
        <Switch checked={form.guests} onChange={(v) => setForm({ ...form, guests: v })} label="游客功能" />
        <Switch
          checked={form.registration}
          onChange={(v) => setForm({ ...form, registration: v })}
          label="邀请码注册"
        />
        <div className="field" role="group" aria-labelledby="web-caps-label">
          <span className="field-label" id="web-caps-label">
            网页端可以使用的功能
          </span>
          <div className="row gap wrap">
            {WEB_CAPS.map((id) => (
              <Checkbox
                key={id}
                label={CAPS.find((c) => c.id === id)?.label ?? id}
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
  const { data, error, reload } = useApi<SettingsView>('/api/v2/admin/settings');
  if (!data) return error ? <Alert>{error}</Alert> : <Loading />;
  const config = data.config;
  return (
    <div className="stack">
      <PageHeader title="服务器设置" />
      <Form key={JSON.stringify(data.runtime)} initial={data.runtime} onSaved={() => void reload()} />
      <Card title="配置文件（只读）">
        <div className="stack">
          {SECTIONS.map(([key, title]) => {
            const section = (key ? config[key] : config) as Record<string, unknown> | undefined;
            if (!section) return null;
            const rows = Object.entries(section).filter(
              ([, v]) => key !== '' || typeof v !== 'object' || Array.isArray(v) || v === null,
            );
            return (
              <div key={key} className="stack-sm">
                <h3>{title}</h3>
                <Rows>
                  {rows.map(([k, v]) => (
                    <Row key={k} label={<span className="mono small">{k}</span>}>
                      {value(v)}
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
