// Invitation codes: create (shown once), revoke, see who registered.
import { useState, type FormEvent } from 'react';
import type { Messages } from '../../i18n';
import { useT } from '../../i18n/react';
import { call, errorText } from '../../lib/api';
import { sitePath } from '../../lib/base';
import { dateTime } from '../../lib/format';
import { Link } from '../../lib/router';
import type { AccountSummary, Invite, InviteUse, SettingsView } from '../../lib/types';
import { useApi } from '../../lib/useApi';
import {
  Alert,
  Badge,
  Button,
  Card,
  Checkbox,
  Copy,
  Empty,
  Field,
  IconButton,
  Input,
  Loading,
  PageHeader,
  Select,
  type Tone,
} from '../../ui/kit';
import { Dialog, useOverlay } from '../../ui/overlay';
import { OnceDialog } from './Users';

type InviteMessages = Messages['admin']['invites'];

function state(i: Invite, m: InviteMessages): [string, Tone] {
  if (i.revoked_at) return [m.revoked, 'muted'];
  if (i.expires_at && i.expires_at * 1000 <= Date.now()) return [m.expired, 'muted'];
  if (i.max_uses !== null && i.uses >= i.max_uses) return [m.usedUp, 'muted'];
  return [m.valid, 'green'];
}

/// Validity choices in hours (0: no expiry).
const VALIDITY = [24, 24 * 7, 24 * 30, 0];

function CreateInvite({ open, onClose, onCreated }: { open: boolean; onClose(): void; onCreated(code: string): void }) {
  const t = useT();
  const m = t.admin.invites;
  const [note, setNote] = useState('');
  const [uses, setUses] = useState('1');
  const [hours, setHours] = useState(24 * 7);
  const [autoFriend, setAutoFriend] = useState(true);
  const [error, setError] = useState<string>();
  const [busy, setBusy] = useState(false);

  async function submit(e: FormEvent) {
    e.preventDefault();
    setBusy(true);
    setError(undefined);
    try {
      const r = await call<{ code: string }>('POST', '/api/v2/admin/invites', {
        note: note.trim(),
        max_uses: uses === '' ? null : Number(uses),
        valid_hours: hours || null,
        auto_friend: autoFriend,
      });
      setNote('');
      onCreated(r.code);
    } catch (err) {
      setError(errorText(err));
    } finally {
      setBusy(false);
    }
  }

  return (
    <Dialog
      open={open}
      title={m.create}
      onClose={onClose}
      footer={
        <>
          <Button onClick={onClose}>{t.common.cancel}</Button>
          <Button variant="primary" type="submit" form="create-invite" loading={busy}>
            {m.createSubmit}
          </Button>
        </>
      }
    >
      <form id="create-invite" className="stack" onSubmit={submit}>
        {error && <Alert>{error}</Alert>}
        <Field label={m.note}>
          <Input value={note} onChange={(e) => setNote(e.target.value)} maxLength={128} />
        </Field>
        <Field label={m.maxUses}>
          <Input
            type="number"
            min={1}
            max={1000}
            value={uses}
            onChange={(e) => setUses(e.target.value)}
            placeholder={t.common.unlimited}
          />
        </Field>
        <Field label={m.validity}>
          <Select
            value={hours}
            onChange={setHours}
            options={VALIDITY.map((h) => ({ value: h, label: h ? m.days(h / 24) : m.noExpiry }))}
          />
        </Field>
        <Checkbox checked={autoFriend} onChange={setAutoFriend} label={m.autoFriend} />
      </form>
    </Dialog>
  );
}

function Uses({ invite, onClose }: { invite: Invite | undefined; onClose(): void }) {
  const t = useT();
  const m = t.admin.invites;
  const { data, error } = useApi<InviteUse[]>(invite ? `/api/v2/admin/invites/${invite.id}/uses` : null);
  return (
    <Dialog open={!!invite} title={m.usesTitle(invite?.note || m.invite)} onClose={onClose} wide>
      {error && <Alert>{error}</Alert>}
      {!data ? (
        <Loading />
      ) : data.length === 0 ? (
        <Empty icon="ticket" title={m.noUses} />
      ) : (
        <table className="table">
          <thead>
            <tr>
              <th>{m.registeredAccount}</th>
              <th>{t.admin.col.time}</th>
              <th>IP</th>
            </tr>
          </thead>
          <tbody>
            {data.map((u) => (
              <tr key={u.user_id + u.used_at}>
                <td>
                  {u.username ? (
                    <Link to={`/admin/users/${u.user_id}`}>{u.username}</Link>
                  ) : (
                    <span className="muted">{t.common.deleted}</span>
                  )}
                </td>
                <td className="nowrap">{dateTime(u.used_at)}</td>
                <td className="mono small">{u.ip}</td>
              </tr>
            ))}
          </tbody>
        </table>
      )}
    </Dialog>
  );
}

export default function Invites() {
  const t = useT();
  const m = t.admin.invites;
  const { confirm, toast } = useOverlay();
  const { data, error, reload } = useApi<Invite[]>('/api/v2/admin/invites');
  const users = useApi<AccountSummary[]>('/api/v2/admin/users');
  const settings = useApi<SettingsView>('/api/v2/admin/settings');
  const [creating, setCreating] = useState(false);
  const [code, setCode] = useState<string>();
  const [viewing, setViewing] = useState<Invite>();
  const names = new Map((users.data ?? []).map((u) => [u.id, u.username]));
  // In the fragment: it never reaches a server or a proxy log.
  const link = code ? `${location.origin}${sitePath(`/register#code=${encodeURIComponent(code)}`)}` : '';

  return (
    <div className="stack">
      <PageHeader
        title={t.nav.invites}
        actions={
          <Button variant="primary" icon="plus" onClick={() => setCreating(true)}>
            {m.create}
          </Button>
        }
      />
      {settings.data && !settings.data.settings.registration && (
        <Alert tone="amber">
          <Link to="/admin/settings">{m.registrationOff[0]}</Link>
          {m.registrationOff[1]}
        </Alert>
      )}
      {error && <Alert>{error}</Alert>}
      <Card pad={false}>
        {!data ? (
          <Loading />
        ) : data.length === 0 ? (
          <Empty icon="ticket" title={m.empty} />
        ) : (
          <div className="table-wrap">
            <table className="table">
              <thead>
                <tr>
                  <th>{m.note}</th>
                  <th>{t.admin.col.status}</th>
                  <th>{m.used}</th>
                  <th>{m.expires}</th>
                  <th>{m.autoFriendCol}</th>
                  <th>{t.admin.col.created}</th>
                  <th />
                </tr>
              </thead>
              <tbody>
                {data.map((i) => {
                  const [text, tone] = state(i, m);
                  return (
                    <tr key={i.id}>
                      <td>
                        <div className="cell-main">{i.note || <span className="muted">{t.common.noNote}</span>}</div>
                        <div className="cell-sub">{m.createdBy(names.get(i.created_by) ?? m.deletedAccount)}</div>
                      </td>
                      <td>
                        <Badge tone={tone}>{text}</Badge>
                      </td>
                      <td>
                        {i.uses} / {i.max_uses ?? t.common.unlimited}
                      </td>
                      <td className="nowrap">{i.expires_at ? dateTime(i.expires_at) : m.never}</td>
                      <td>{i.auto_friend ? t.common.yes : t.common.no}</td>
                      <td className="nowrap muted">{dateTime(i.created_at)}</td>
                      <td className="actions">
                        <IconButton icon="log" label={m.uses} onClick={() => setViewing(i)} />
                        {!i.revoked_at && (
                          <IconButton
                            icon="trash"
                            label={m.revoke}
                            variant="danger"
                            onClick={async () => {
                              if (
                                await confirm({
                                  title: m.revokeTitle,
                                  message: m.revokeMessage,
                                  confirm: m.revoke,
                                  danger: true,
                                })
                              ) {
                                try {
                                  await call('DELETE', `/api/v2/admin/invites/${i.id}`);
                                  toast(m.revokedToast);
                                  await reload();
                                } catch (err) {
                                  toast(errorText(err), 'red');
                                }
                              }
                            }}
                          />
                        )}
                      </td>
                    </tr>
                  );
                })}
              </tbody>
            </table>
          </div>
        )}
      </Card>
      <CreateInvite
        open={creating}
        onClose={() => setCreating(false)}
        onCreated={(c) => {
          setCreating(false);
          setCode(c);
          void reload();
        }}
      />
      <OnceDialog title={m.createdTitle} open={!!code} onClose={() => setCode(undefined)}>
        <Field label={m.invite}>
          <div className="secret">
            <Copy text={code ?? ''} />
          </div>
        </Field>
        <Field label={m.link}>
          <div className="secret small">
            <Copy text={link} />
          </div>
        </Field>
      </OnceDialog>
      <Uses invite={viewing} onClose={() => setViewing(undefined)} />
    </div>
  );
}
