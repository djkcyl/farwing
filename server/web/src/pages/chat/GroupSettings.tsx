// A group's settings (`docs/protocol/chat-design.md` §3, §5): overview,
// roles, channels with overwrites, members, invitations, bans, join
// requests and the audit log; each tab for those whose permissions allow it.
import { useEffect, useMemo, useState } from 'react';
import { useT } from '../../i18n/react';
import { call, errorText } from '../../lib/api';
import { navigate } from '../../lib/router';
import { chat, useChat } from '../../lib/chat/store';
import {
  ALL_PERMS,
  CHANNEL_PERMS,
  has,
  PERM,
  type AuditEntry,
  type Ban,
  type Channel,
  type ChannelKind,
  type GroupState,
  type Invite,
  type JoinRequest,
  type Member,
  type Notify,
  type Role,
} from '../../lib/chat/types';
import { dateTime } from '../../lib/format';
import { Icon } from '../../ui/icons';
import { Button, Checkbox, Empty, Field, Input, Loading, Segmented, Select, Switch, Tabs } from '../../ui/kit';
import { Dialog, useOverlay } from '../../ui/overlay';
import { Avatar, displayName, GroupIcon } from './parts';
import { InviteDialog, inviteLink, uploadSigned } from './Dialogs';

type Tab = 'overview' | 'roles' | 'channels' | 'members' | 'invites' | 'bans' | 'requests' | 'audit';

const COLORS = [0, 0xe67e22, 0xe74c3c, 0xe91e63, 0x9b59b6, 0x3498db, 0x1abc9c, 0x2ecc71, 0xf1c40f, 0x95a5a6];

function hex(color: number): string {
  return `#${color.toString(16).padStart(6, '0')}`;
}

export function GroupSettings({ group, open, onClose }: { group: GroupState; open: boolean; onClose(): void }) {
  const t = useT();
  const p = group.group.permissions;
  const tabs: { id: Tab; label: string }[] = [
    { id: 'overview' as Tab, label: t.chat.settings.tabs.overview },
    ...(has(p, PERM.MANAGE_ROLES) ? [{ id: 'roles' as Tab, label: t.chat.settings.tabs.roles }] : []),
    ...(has(p, PERM.MANAGE_CHANNELS) ? [{ id: 'channels' as Tab, label: t.chat.settings.tabs.channels }] : []),
    ...(p & (PERM.KICK | PERM.BAN | PERM.MUTE | PERM.MANAGE_ROLES)
      ? [{ id: 'members' as Tab, label: t.chat.settings.tabs.members }]
      : []),
    ...(has(p, PERM.INVITE) ? [{ id: 'invites' as Tab, label: t.chat.settings.tabs.invites }] : []),
    ...(has(p, PERM.BAN) ? [{ id: 'bans' as Tab, label: t.chat.settings.tabs.bans }] : []),
    ...(has(p, PERM.MANAGE_GROUP)
      ? [
          { id: 'requests' as Tab, label: t.chat.settings.tabs.requests },
          { id: 'audit' as Tab, label: t.chat.settings.tabs.audit },
        ]
      : []),
  ];
  const [tab, setTab] = useState<Tab>('overview');
  return (
    <Dialog open={open} title={`${t.chat.header.settings} · ${group.group.name}`} onClose={onClose} wide>
      <div className="stack">
        <Tabs<Tab> items={tabs} value={tab} onChange={setTab} />
        {tab === 'overview' && <Overview group={group} onClose={onClose} />}
        {tab === 'roles' && <Roles group={group} />}
        {tab === 'channels' && <Channels group={group} />}
        {tab === 'members' && <Members group={group} />}
        {tab === 'invites' && <Invites group={group} />}
        {tab === 'bans' && <Bans group={group} />}
        {tab === 'requests' && <Requests group={group} />}
        {tab === 'audit' && <Audit group={group} />}
      </div>
    </Dialog>
  );
}

function useAct() {
  const { toast } = useOverlay();
  return async (f: () => Promise<unknown>, done?: string) => {
    try {
      await f();
      if (done) toast(done);
      return true;
    } catch (e) {
      toast(errorText(e), 'red');
      return false;
    }
  };
}

function Overview({ group, onClose }: { group: GroupState; onClose(): void }) {
  const t = useT();
  const state = useChat();
  const act = useAct();
  const { confirm } = useOverlay();
  const g = group.group;
  const manage = has(g.permissions, PERM.MANAGE_GROUP);
  const owner = g.owner_id === state.snap?.me.user_id;
  const [name, setName] = useState(g.name);
  const [description, setDescription] = useState(g.description);
  const [transferTo, setTransferTo] = useState('');
  const members = state.members[g.id];
  useEffect(() => {
    if (owner && !members) void chat.loadMembers(g.id);
  }, [owner, members, g.id]);
  const notify = state.snap?.prefs.notify[`g:${g.id}`] ?? 'mentions';
  const setNotify = (level: Notify) => {
    const prefs = state.snap!.prefs;
    void act(() => chat.savePrefs({ ...prefs, notify: { ...prefs.notify, [`g:${g.id}`]: level } }));
  };
  const patch = (body: object) =>
    act(
      async () => chat.upsertGroup(await call<GroupState>('PATCH', `/api/v2/chat/groups/${g.id}`, body)),
      t.chat.settings.saved,
    );
  return (
    <div className="stack">
      <div className="row gap">
        <GroupIcon group={g} size={64} />
        {manage && (
          <div className="stack-sm">
            <label className="btn btn-default btn-sm file-pick">
              <input
                type="file"
                accept="image/png,image/jpeg,image/gif,image/webp"
                onChange={async (e) => {
                  const file = e.target.files?.[0];
                  e.target.value = '';
                  if (file) await act(() => uploadSigned(`/api/v2/chat/groups/${g.id}/icon`, file));
                }}
              />
              <Icon name="upload" size={14} />
              <span>{t.chat.settings.changeIcon}</span>
            </label>
            {g.icon && (
              <Button size="sm" onClick={() => void act(() => call('DELETE', `/api/v2/chat/groups/${g.id}/icon`))}>
                {t.chat.settings.removeIcon}
              </Button>
            )}
          </div>
        )}
      </div>
      <Field label={t.chat.settings.name}>
        <Input value={name} disabled={!manage} maxLength={100} onChange={(e) => setName(e.target.value)} />
      </Field>
      <Field label={t.chat.settings.description}>
        <Input
          value={description}
          disabled={!manage}
          maxLength={1024}
          onChange={(e) => setDescription(e.target.value)}
        />
      </Field>
      {manage && (
        <>
          <Switch label={t.chat.settings.public} checked={g.public} onChange={(v) => void patch({ public: v })} />
          <Switch
            label={t.chat.settings.approval}
            checked={g.join_approval}
            onChange={(v) => void patch({ join_approval: v })}
          />
          <div>
            <Button
              variant="primary"
              disabled={!name.trim() || (name === g.name && description === g.description)}
              onClick={() => void patch({ name, description })}
            >
              {t.chat.settings.save}
            </Button>
          </div>
        </>
      )}
      <Field label={t.chat.header.notify}>
        <Select<Notify>
          value={notify}
          onChange={setNotify}
          options={(['all', 'mentions', 'none'] as Notify[]).map((v) => ({ value: v, label: t.chat.notifyLevel[v] }))}
        />
      </Field>
      <div className="chat-danger">
        {owner ? (
          <>
            <div className="row gap-sm wrap">
              <Select
                value={transferTo}
                onChange={setTransferTo}
                options={[
                  { value: '', label: t.chat.settings.transferTo },
                  ...(members ?? [])
                    .filter((m) => m.user.user_id !== g.owner_id)
                    .map((m) => ({ value: m.user.user_id, label: m.nickname || displayName(m.user) })),
                ]}
              />
              <Button
                disabled={!transferTo}
                onClick={async () => {
                  const who = members?.find((m) => m.user.user_id === transferTo);
                  if (
                    who &&
                    (await confirm({ title: t.chat.settings.transferQuestion(who.nickname || displayName(who.user)) }))
                  ) {
                    await act(async () =>
                      chat.upsertGroup(
                        await call<GroupState>('POST', `/api/v2/chat/groups/${g.id}/transfer`, { user: transferTo }),
                      ),
                    );
                  }
                }}
              >
                {t.chat.settings.transfer}
              </Button>
            </div>
            <Button
              variant="danger"
              onClick={async () => {
                if (
                  await confirm({
                    title: t.chat.settings.dissolveQuestion(g.name),
                    danger: true,
                    confirm: t.chat.settings.dissolve,
                    typeToConfirm: g.name,
                  })
                ) {
                  if (await act(() => call('DELETE', `/api/v2/chat/groups/${g.id}`))) {
                    chat.removeGroup(g.id);
                    onClose();
                    navigate('/chat');
                  }
                }
              }}
            >
              {t.chat.settings.dissolve}
            </Button>
          </>
        ) : (
          <Button
            variant="danger"
            onClick={async () => {
              if (
                await confirm({
                  title: t.chat.settings.leaveQuestion(g.name),
                  danger: true,
                  confirm: t.chat.header.leave,
                })
              ) {
                if (await act(() => call('DELETE', `/api/v2/chat/groups/${g.id}/members/@me`))) {
                  chat.removeGroup(g.id);
                  onClose();
                  navigate('/chat');
                }
              }
            }}
          >
            {t.chat.header.leave}
          </Button>
        )}
      </div>
    </div>
  );
}

function PermList({
  value,
  onChange,
  list,
  disabled,
}: {
  value: number;
  onChange(v: number): void;
  list: string[];
  disabled?: boolean;
}) {
  const t = useT();
  return (
    <div className="chat-perm-list">
      {list.map((name) => {
        const bit = PERM[name as keyof typeof PERM];
        return (
          <Checkbox
            key={name}
            disabled={disabled}
            checked={has(value, bit)}
            onChange={(on) => onChange(on ? value | bit : value & ~bit)}
            label={t.chat.perms[name as keyof typeof t.chat.perms]}
          />
        );
      })}
    </div>
  );
}

function Roles({ group }: { group: GroupState }) {
  const t = useT();
  const act = useAct();
  const { confirm } = useOverlay();
  const g = group.group;
  const roles = useMemo(() => [...group.roles].sort((a, b) => b.position - a.position), [group.roles]);
  const [selected, setSelected] = useState(roles[0]?.id);
  const role = roles.find((r) => r.id === selected) ?? roles[0];
  const [draft, setDraft] = useState<Role | undefined>(role);
  useEffect(() => setDraft(role), [role]);
  const everyone = role?.id === g.id;
  const others = roles.filter((r) => r.id !== g.id);
  const move = (r: Role, up: boolean) => {
    // Lowest first, without @everyone.
    const order = [...others].reverse().map((x) => x.id);
    const i = order.indexOf(r.id);
    const j = up ? i + 1 : i - 1;
    if (j < 0 || j >= order.length) return;
    [order[i], order[j]] = [order[j], order[i]];
    void act(() => call('PUT', `/api/v2/chat/groups/${g.id}/roles/order`, { roles: order }));
  };
  return (
    <div className="chat-settings-split">
      <div className="chat-settings-list">
        <Button
          size="sm"
          icon="plus"
          onClick={() =>
            void act(async () => {
              const r = await call<Role>('POST', `/api/v2/chat/groups/${g.id}/roles`, {
                name: t.chat.settings.roles.newName,
                permissions: 0,
              });
              setSelected(r.id);
            })
          }
        >
          {t.chat.settings.roles.create}
        </Button>
        {roles.map((r) => (
          <button
            key={r.id}
            type="button"
            className={`chat-settings-item ${r.id === role?.id ? 'active' : ''}`}
            onClick={() => setSelected(r.id)}
          >
            <span className="dot" style={{ background: r.color ? hex(r.color) : 'var(--faint)' }} />
            <span className="grow ellipsis">{r.id === g.id ? t.chat.settings.roles.everyone : r.name}</span>
            {r.id !== g.id && (
              <>
                <span
                  className="chat-mini"
                  onClick={(e) => (e.stopPropagation(), move(r, true))}
                  title={t.chat.settings.roles.up}
                >
                  ▲
                </span>
                <span
                  className="chat-mini"
                  onClick={(e) => (e.stopPropagation(), move(r, false))}
                  title={t.chat.settings.roles.down}
                >
                  ▼
                </span>
              </>
            )}
          </button>
        ))}
      </div>
      {draft && (
        <div className="stack grow">
          {!everyone && (
            <Field label={t.chat.settings.roles.name}>
              <Input
                value={draft.name}
                maxLength={100}
                onChange={(e) => setDraft({ ...draft, name: e.target.value })}
              />
            </Field>
          )}
          {!everyone && (
            <Field label={t.chat.settings.roles.color} group>
              <div className="row gap-sm wrap">
                {COLORS.map((c) => (
                  <button
                    key={c}
                    type="button"
                    className={`chat-swatch ${draft.color === c ? 'active' : ''}`}
                    style={{ background: c ? hex(c) : 'var(--surface-2)' }}
                    onClick={() => setDraft({ ...draft, color: c })}
                  />
                ))}
              </div>
            </Field>
          )}
          {!everyone && (
            <Switch
              label={t.chat.settings.roles.mentionable}
              checked={draft.mentionable}
              onChange={(v) => setDraft({ ...draft, mentionable: v })}
            />
          )}
          <Field label={t.chat.settings.roles.permissions} group>
            <PermList
              value={draft.permissions}
              onChange={(v) => setDraft({ ...draft, permissions: v })}
              list={ALL_PERMS}
            />
          </Field>
          <div className="row gap-sm">
            <Button
              variant="primary"
              onClick={() =>
                void act(
                  () =>
                    call('PATCH', `/api/v2/chat/groups/${g.id}/roles/${draft.id}`, {
                      name: everyone ? undefined : draft.name,
                      color: draft.color,
                      mentionable: draft.mentionable,
                      permissions: draft.permissions,
                    }),
                  t.chat.settings.saved,
                )
              }
            >
              {t.chat.settings.save}
            </Button>
            {!everyone && (
              <Button
                variant="danger"
                onClick={async () => {
                  if (await confirm({ title: t.chat.settings.roles.removeQuestion(draft.name), danger: true })) {
                    await act(() => call('DELETE', `/api/v2/chat/groups/${g.id}/roles/${draft.id}`));
                  }
                }}
              >
                {t.chat.settings.roles.remove}
              </Button>
            )}
          </div>
        </div>
      )}
    </div>
  );
}

const SLOW = [0, 5, 10, 15, 30, 60, 120, 300, 600, 900, 1800, 3600, 7200, 21600];

function Channels({ group }: { group: GroupState }) {
  const t = useT();
  const act = useAct();
  const { confirm } = useOverlay();
  const g = group.group;
  const ordered = useMemo(() => orderChannels(group.channels), [group.channels]);
  const [selected, setSelected] = useState<string>();
  const ch = group.channels.find((c) => c.id === selected);
  const [draft, setDraft] = useState<Channel>();
  useEffect(() => setDraft(ch), [ch]);
  const [creating, setCreating] = useState<{ kind: ChannelKind; name: string; parent: string }>();
  const [owRole, setOwRole] = useState(g.id);
  const categories = group.channels.filter((c) => c.kind === 'category');
  const state = useChat();
  // Compatible mode: the group owner's choice, where the server allows it.
  const compatChoice = !!state.snap?.limits.voice_compat && state.snap.me.user_id === g.owner_id;

  const move = (c: Channel, up: boolean) => {
    const siblings = ordered.filter(
      (x) => (x.parent_id ?? null) === (c.parent_id ?? null) && (x.kind === 'category') === (c.kind === 'category'),
    );
    const i = siblings.findIndex((x) => x.id === c.id);
    const j = up ? i - 1 : i + 1;
    if (j < 0 || j >= siblings.length) return;
    const swapped = [...ordered];
    const a = swapped.findIndex((x) => x.id === siblings[i].id);
    const b = swapped.findIndex((x) => x.id === siblings[j].id);
    [swapped[a], swapped[b]] = [swapped[b], swapped[a]];
    void act(() =>
      call('PUT', `/api/v2/chat/groups/${g.id}/channels/order`, {
        channels: swapped.map((x) => ({ id: x.id, parent_id: x.parent_id })),
      }),
    );
  };

  const ow = draft?.overwrites.find((o) => o.role_id === owRole) ?? { role_id: owRole, allow: 0, deny: 0 };
  const setOw = (bit: number, v: 'inherit' | 'allow' | 'deny') => {
    if (!draft) return;
    const allow = v === 'allow' ? ow.allow | bit : ow.allow & ~bit;
    const deny = v === 'deny' ? ow.deny | bit : ow.deny & ~bit;
    void act(() => call('PUT', `/api/v2/chat/channels/${draft.id}/overwrites/${owRole}`, { allow, deny }));
  };

  return (
    <div className="chat-settings-split">
      <div className="chat-settings-list">
        <Button
          size="sm"
          icon="plus"
          onClick={() => setCreating({ kind: 'text', name: '', parent: categories[0]?.id ?? '' })}
        >
          {t.chat.settings.channels.create}
        </Button>
        {ordered.map((c) => (
          <button
            key={c.id}
            type="button"
            className={`chat-settings-item ${c.id === selected ? 'active' : ''} ${c.parent_id ? 'indent' : ''}`}
            onClick={() => setSelected(c.id)}
          >
            <Icon
              name={
                c.kind === 'voice'
                  ? 'volume'
                  : c.kind === 'category'
                    ? 'folder'
                    : c.kind === 'announcement'
                      ? 'megaphone'
                      : 'hash'
              }
              size={14}
            />
            <span className="grow ellipsis">{c.name}</span>
            <span
              className="chat-mini"
              onClick={(e) => (e.stopPropagation(), move(c, true))}
              title={t.chat.settings.roles.up}
            >
              ▲
            </span>
            <span
              className="chat-mini"
              onClick={(e) => (e.stopPropagation(), move(c, false))}
              title={t.chat.settings.roles.down}
            >
              ▼
            </span>
          </button>
        ))}
      </div>
      {draft ? (
        <div className="stack grow">
          <Field label={t.chat.settings.channels.name}>
            <Input value={draft.name} maxLength={100} onChange={(e) => setDraft({ ...draft, name: e.target.value })} />
          </Field>
          {draft.kind !== 'category' && (
            <>
              <Field label={t.chat.settings.channels.topic}>
                <Input
                  value={draft.topic}
                  maxLength={1024}
                  onChange={(e) => setDraft({ ...draft, topic: e.target.value })}
                />
              </Field>
              <Field label={t.chat.settings.channels.parent}>
                <Select
                  value={draft.parent_id ?? ''}
                  onChange={(v) => setDraft({ ...draft, parent_id: v || null })}
                  options={[
                    { value: '', label: t.chat.settings.channels.none },
                    ...categories.map((c) => ({ value: c.id, label: c.name })),
                  ]}
                />
              </Field>
              <Field label={t.chat.settings.channels.slowMode}>
                <Select
                  value={draft.slow_mode_s}
                  onChange={(v) => setDraft({ ...draft, slow_mode_s: v })}
                  options={SLOW.map((s) => ({
                    value: s,
                    label: s ? t.chat.settings.channels.seconds(s) : t.chat.settings.channels.off,
                  }))}
                />
              </Field>
            </>
          )}
          {draft.kind === 'voice' && (
            <div className="row gap wrap">
              <Field label={t.chat.settings.channels.bitrate}>
                <Select
                  value={draft.voice_bitrate}
                  onChange={(v) => setDraft({ ...draft, voice_bitrate: v })}
                  options={[32, 64, 96, 128].map((k) => ({ value: k, label: `${k} kbps` }))}
                />
              </Field>
              <Field label={t.chat.settings.channels.limit}>
                <Select
                  value={draft.voice_limit}
                  onChange={(v) => setDraft({ ...draft, voice_limit: v })}
                  options={[0, 2, 5, 10, 20, 50, 99].map((n) => ({
                    value: n,
                    label: n ? String(n) : t.chat.settings.channels.unlimited,
                  }))}
                />
              </Field>
            </div>
          )}
          {draft.kind === 'voice' && compatChoice && (
            <Switch
              label={t.chat.voice.compat}
              checked={draft.voice_compat}
              onChange={(voice_compat) => setDraft({ ...draft, voice_compat })}
            />
          )}
          <div className="row gap-sm">
            <Button
              variant="primary"
              onClick={() =>
                void act(
                  () =>
                    call('PATCH', `/api/v2/chat/channels/${draft.id}`, {
                      name: draft.name,
                      topic: draft.kind === 'category' ? undefined : draft.topic,
                      parent_id: draft.kind === 'category' ? undefined : draft.parent_id,
                      slow_mode_s: draft.kind === 'category' ? undefined : draft.slow_mode_s,
                      voice_bitrate: draft.kind === 'voice' ? draft.voice_bitrate : undefined,
                      voice_limit: draft.kind === 'voice' ? draft.voice_limit : undefined,
                      voice_compat: draft.kind === 'voice' && compatChoice ? draft.voice_compat : undefined,
                    }),
                  t.chat.settings.saved,
                )
              }
            >
              {t.chat.settings.save}
            </Button>
            <Button
              variant="danger"
              onClick={async () => {
                if (await confirm({ title: t.chat.settings.channels.removeQuestion(draft.name), danger: true })) {
                  if (await act(() => call('DELETE', `/api/v2/chat/channels/${draft.id}`))) setSelected(undefined);
                }
              }}
            >
              {t.chat.settings.channels.remove}
            </Button>
          </div>
          <Field label={t.chat.settings.channels.overwrites} group>
            <div className="stack-sm">
              <Select
                value={owRole}
                onChange={setOwRole}
                options={[...group.roles]
                  .sort((a, b) => b.position - a.position)
                  .map((r) => ({ value: r.id, label: r.id === g.id ? t.chat.settings.roles.everyone : r.name }))}
              />
              {CHANNEL_PERMS.map((name) => {
                const bit = PERM[name];
                const v = has(ow.allow, bit) ? 'allow' : has(ow.deny, bit) ? 'deny' : 'inherit';
                return (
                  <div key={name} className="row gap-sm chat-ow-row">
                    <span className="grow">{t.chat.perms[name]}</span>
                    <Segmented
                      value={v}
                      onChange={(x) => setOw(bit, x)}
                      options={[
                        { value: 'deny', label: t.chat.settings.channels.deny },
                        { value: 'inherit', label: t.chat.settings.channels.inherit },
                        { value: 'allow', label: t.chat.settings.channels.allow },
                      ]}
                    />
                  </div>
                );
              })}
            </div>
          </Field>
        </div>
      ) : (
        <div className="grow" />
      )}
      <Dialog
        open={!!creating}
        title={t.chat.settings.channels.create}
        onClose={() => setCreating(undefined)}
        footer={
          <>
            <Button onClick={() => setCreating(undefined)}>{t.common.cancel}</Button>
            <Button
              variant="primary"
              disabled={!creating?.name.trim()}
              onClick={async () => {
                if (!creating) return;
                const ok = await act(async () => {
                  const c = await call<Channel>('POST', '/api/v2/chat/channels', {
                    group_id: g.id,
                    kind: creating.kind,
                    name: creating.name,
                    parent_id: creating.kind === 'category' || !creating.parent ? null : creating.parent,
                  });
                  setSelected(c.id);
                });
                if (ok) setCreating(undefined);
              }}
            >
              {t.common.create}
            </Button>
          </>
        }
      >
        {creating && (
          <div className="stack">
            <Field label={t.chat.settings.channels.kind}>
              <Select<ChannelKind>
                value={creating.kind}
                onChange={(kind) => setCreating({ ...creating, kind })}
                options={(['text', 'voice', 'announcement', 'category'] as ChannelKind[]).map((k) => ({
                  value: k,
                  label: t.chat.kinds[k],
                }))}
              />
            </Field>
            <Field label={t.chat.settings.channels.name}>
              <Input
                value={creating.name}
                maxLength={100}
                onChange={(e) => setCreating({ ...creating, name: e.target.value })}
              />
            </Field>
            {creating.kind !== 'category' && (
              <Field label={t.chat.settings.channels.parent}>
                <Select
                  value={creating.parent}
                  onChange={(parent) => setCreating({ ...creating, parent })}
                  options={[
                    { value: '', label: t.chat.settings.channels.none },
                    ...categories.map((c) => ({ value: c.id, label: c.name })),
                  ]}
                />
              </Field>
            )}
          </div>
        )}
      </Dialog>
    </div>
  );
}

/// Channels in display order: outside any category first, then each
/// category followed by its channels.
export function orderChannels(channels: Channel[]): Channel[] {
  const by = (a: Channel, b: Channel) => a.position - b.position || (a.id < b.id ? -1 : 1);
  const top = channels.filter((c) => !c.parent_id && c.kind !== 'category').sort(by);
  const cats = channels.filter((c) => c.kind === 'category').sort(by);
  const out = [...top];
  for (const cat of cats) {
    out.push(cat, ...channels.filter((c) => c.parent_id === cat.id).sort(by));
  }
  // Channels whose category is not visible.
  for (const c of channels) if (!out.includes(c)) out.push(c);
  return out;
}

const MUTES: [string, number][] = [
  ['m10', 600],
  ['h1', 3600],
  ['d1', 86400],
  ['d7', 7 * 86400],
];

function Members({ group }: { group: GroupState }) {
  const t = useT();
  const state = useChat();
  const act = useAct();
  const { confirm } = useOverlay();
  const g = group.group;
  const p = g.permissions;
  const [q, setQ] = useState('');
  const [rolesOf, setRolesOf] = useState<Member>();
  const [banning, setBanning] = useState<Member>();
  const [reason, setReason] = useState('');
  const [hours, setHours] = useState(0);
  const members = state.members[g.id];
  useEffect(() => {
    if (!members) void chat.loadMembers(g.id);
  }, [members, g.id]);
  if (!members) return <Loading />;
  const shown = members.filter((m) =>
    `${m.nickname} ${m.user.username} ${m.user.nickname}`.toLowerCase().includes(q.toLowerCase()),
  );
  const me = state.snap?.me.user_id;
  return (
    <div className="stack">
      <Input value={q} placeholder={t.chat.settings.members.search} onChange={(e) => setQ(e.target.value)} />
      <div className="stack-sm">
        {shown.map((m) => {
          const owner = m.user.user_id === g.owner_id;
          const self = m.user.user_id === me;
          const muted = m.muted_until && m.muted_until * 1000 > Date.now();
          return (
            <div key={m.user.user_id} className="chat-settings-member">
              <Avatar user={m.user} size={32} />
              <div className="grow">
                <div className="ellipsis">
                  {m.nickname || displayName(m.user)}{' '}
                  {owner && <span className="muted small">· {t.chat.members.owner}</span>}
                </div>
                <div className="small muted ellipsis">
                  {group.roles
                    .filter((r) => m.roles.includes(r.id))
                    .map((r) => r.name)
                    .join(' · ')}
                  {muted ? ` · ${t.chat.settings.members.mutedUntil(dateTime(m.muted_until))}` : ''}
                </div>
              </div>
              {!owner && !self && (
                <div className="row gap-sm">
                  {has(p, PERM.MANAGE_ROLES) && (
                    <Button size="sm" onClick={() => setRolesOf(m)}>
                      {t.chat.settings.members.roles}
                    </Button>
                  )}
                  {has(p, PERM.MUTE) &&
                    (muted ? (
                      <Button
                        size="sm"
                        onClick={() =>
                          void act(() =>
                            call('PATCH', `/api/v2/chat/groups/${g.id}/members/${m.user.user_id}`, {
                              muted_until: null,
                            }),
                          )
                        }
                      >
                        {t.chat.settings.members.unmute}
                      </Button>
                    ) : (
                      <Select<string>
                        value=""
                        onChange={(v) =>
                          v &&
                          void act(() =>
                            call('PATCH', `/api/v2/chat/groups/${g.id}/members/${m.user.user_id}`, {
                              muted_until: Math.floor(Date.now() / 1000) + Number(v),
                            }),
                          )
                        }
                        options={[
                          { value: '', label: t.chat.settings.members.mute },
                          ...MUTES.map(([k, s]) => ({
                            value: String(s),
                            label:
                              t.chat.settings.members.durations[k as keyof typeof t.chat.settings.members.durations],
                          })),
                        ]}
                      />
                    ))}
                  {has(p, PERM.KICK) && (
                    <Button
                      size="sm"
                      onClick={async () => {
                        if (
                          await confirm({
                            title: t.chat.settings.members.kickQuestion(m.nickname || displayName(m.user)),
                            danger: true,
                          })
                        ) {
                          await act(() => call('DELETE', `/api/v2/chat/groups/${g.id}/members/${m.user.user_id}`));
                        }
                      }}
                    >
                      {t.chat.settings.members.kick}
                    </Button>
                  )}
                  {has(p, PERM.BAN) && (
                    <Button size="sm" variant="danger" onClick={() => (setBanning(m), setReason(''), setHours(0))}>
                      {t.chat.settings.members.ban}
                    </Button>
                  )}
                </div>
              )}
            </div>
          );
        })}
      </div>
      <Dialog
        open={!!rolesOf}
        title={rolesOf ? `${t.chat.settings.members.roles} · ${rolesOf.nickname || displayName(rolesOf.user)}` : ''}
        onClose={() => setRolesOf(undefined)}
      >
        {rolesOf && (
          <div className="stack-sm">
            {[...group.roles]
              .filter((r) => r.id !== g.id)
              .sort((a, b) => b.position - a.position)
              .map((r) => {
                const on = rolesOf.roles.includes(r.id);
                return (
                  <Checkbox
                    key={r.id}
                    checked={on}
                    label={r.name}
                    onChange={async (v) => {
                      const roles = v ? [...rolesOf.roles, r.id] : rolesOf.roles.filter((x) => x !== r.id);
                      if (
                        await act(() =>
                          call('PATCH', `/api/v2/chat/groups/${g.id}/members/${rolesOf.user.user_id}`, { roles }),
                        )
                      ) {
                        setRolesOf({ ...rolesOf, roles });
                      }
                    }}
                  />
                );
              })}
          </div>
        )}
      </Dialog>
      <Dialog
        open={!!banning}
        title={banning ? t.chat.settings.members.banTitle(banning.nickname || displayName(banning.user)) : ''}
        onClose={() => setBanning(undefined)}
        footer={
          <>
            <Button onClick={() => setBanning(undefined)}>{t.common.cancel}</Button>
            <Button
              variant="danger"
              onClick={async () => {
                if (
                  banning &&
                  (await act(() =>
                    call('PUT', `/api/v2/chat/groups/${g.id}/bans/${banning.user.user_id}`, {
                      reason,
                      delete_hours: hours,
                    }),
                  ))
                ) {
                  setBanning(undefined);
                }
              }}
            >
              {t.chat.settings.members.ban}
            </Button>
          </>
        }
      >
        <div className="stack">
          <Field label={t.chat.settings.members.reason}>
            <Input value={reason} maxLength={512} onChange={(e) => setReason(e.target.value)} />
          </Field>
          <Field label={t.chat.settings.members.deleteMessages}>
            <Select
              value={hours}
              onChange={setHours}
              options={[0, 1, 24, 168].map((h) => ({
                value: h,
                label: t.chat.settings.members.deleteHours[`h${h}` as keyof typeof t.chat.settings.members.deleteHours],
              }))}
            />
          </Field>
        </div>
      </Dialog>
    </div>
  );
}

function useList<T>(path: string, key: string): [T[] | undefined, () => void] {
  const [list, setList] = useState<T[]>();
  const [n, setN] = useState(0);
  useEffect(() => {
    let stale = false;
    call<Record<string, T[]>>('GET', path)
      .then((r) => !stale && setList(r[key]))
      .catch(() => !stale && setList([]));
    return () => {
      stale = true;
    };
  }, [path, key, n]);
  return [list, () => setN((x) => x + 1)];
}

function Invites({ group }: { group: GroupState }) {
  const t = useT();
  const act = useAct();
  const [list, reload] = useList<Invite>(`/api/v2/chat/groups/${group.group.id}/invites`, 'invites');
  const [creating, setCreating] = useState(false);
  return (
    <div className="stack">
      <div>
        <Button icon="plus" onClick={() => setCreating(true)}>
          {t.chat.invite.create}
        </Button>
      </div>
      {!list ? (
        <Loading />
      ) : (
        list.map((i) => {
          const dead =
            i.revoked ||
            (i.expires_at !== null && i.expires_at * 1000 < Date.now()) ||
            (i.max_uses !== null && i.uses >= i.max_uses);
          return (
            <div key={i.code} className={`chat-settings-member ${dead ? 'away' : ''}`}>
              <div className="grow">
                <div className="mono ellipsis">{inviteLink(i.code)}</div>
                <div className="small muted">
                  {displayName(i.by)} · {t.chat.invite.uses(i.uses, i.max_uses)} ·{' '}
                  {i.revoked ? t.chat.invite.revoked : i.expires_at ? dateTime(i.expires_at) : t.chat.invite.never}
                </div>
              </div>
              {!i.revoked && (
                <Button
                  size="sm"
                  onClick={async () => (await act(() => call('DELETE', `/api/v2/chat/invites/${i.code}`))) && reload()}
                >
                  {t.chat.invite.revoke}
                </Button>
              )}
            </div>
          );
        })
      )}
      <InviteDialog
        group={group}
        open={creating}
        onClose={() => {
          setCreating(false);
          reload();
        }}
      />
    </div>
  );
}

function Bans({ group }: { group: GroupState }) {
  const t = useT();
  const act = useAct();
  const [list, reload] = useList<Ban>(`/api/v2/chat/groups/${group.group.id}/bans`, 'bans');
  if (!list) return <Loading />;
  if (!list.length) return <Empty icon="shield" title={t.chat.settings.bans.none} />;
  return (
    <div className="stack-sm">
      {list.map((b) => (
        <div key={b.user.user_id} className="chat-settings-member">
          <Avatar user={b.user} size={32} />
          <div className="grow">
            <div className="ellipsis">{displayName(b.user)}</div>
            <div className="small muted ellipsis">
              {displayName(b.by)} · {dateTime(b.at)}
              {b.reason ? ` · ${b.reason}` : ''}
            </div>
          </div>
          <Button
            size="sm"
            onClick={async () =>
              (await act(() => call('DELETE', `/api/v2/chat/groups/${group.group.id}/bans/${b.user.user_id}`))) &&
              reload()
            }
          >
            {t.chat.settings.bans.unban}
          </Button>
        </div>
      ))}
    </div>
  );
}

function Requests({ group }: { group: GroupState }) {
  const t = useT();
  const act = useAct();
  const [list, reload] = useList<JoinRequest>(`/api/v2/chat/groups/${group.group.id}/requests`, 'requests');
  useEffect(() => {
    const changed = () => reload();
    window.addEventListener('farwing-chat-request', changed);
    return () => window.removeEventListener('farwing-chat-request', changed);
  }, [reload]);
  if (!list) return <Loading />;
  if (!list.length) return <Empty icon="userPlus" title={t.chat.settings.requests.none} />;
  return (
    <div className="stack-sm">
      {list.map((r) => (
        <div key={r.user.user_id} className="chat-settings-member">
          <Avatar user={r.user} size={32} />
          <div className="grow">
            <div className="ellipsis">{displayName(r.user)}</div>
            <div className="small muted">
              {t.chat.settings.requests.via[r.via as 'invite' | 'public'] ?? r.via} · {dateTime(r.at)}
            </div>
          </div>
          {(['approve', 'reject'] as const).map((answer) => (
            <Button
              key={answer}
              size="sm"
              variant={answer === 'approve' ? 'primary' : 'default'}
              onClick={async () =>
                (await act(() =>
                  call('POST', `/api/v2/chat/groups/${group.group.id}/requests/${r.user.user_id}/${answer}`),
                )) && reload()
              }
            >
              {t.chat.settings.requests[answer]}
            </Button>
          ))}
        </div>
      ))}
    </div>
  );
}

export function AuditList({ path }: { path: string }) {
  const t = useT();
  const [entries, setEntries] = useState<AuditEntry[]>();
  useEffect(() => {
    call<{ entries: AuditEntry[] }>('GET', path)
      .then((r) => setEntries(r.entries))
      .catch(() => setEntries([]));
  }, [path]);
  if (!entries) return <Loading />;
  return (
    <div className="stack-sm">
      {entries.map((e) => (
        <div key={e.id} className="chat-audit-row">
          <span className="small muted nowrap">{dateTime(e.at)}</span>
          <span className="ellipsis">
            <b>{displayName(e.actor)}</b> {t.chat.audit[e.action] ?? e.action}
            {typeof e.details?.name === 'string' && e.details.name ? ` · ${e.details.name}` : ''}
          </span>
        </div>
      ))}
    </div>
  );
}

function Audit({ group }: { group: GroupState }) {
  return <AuditList path={`/api/v2/chat/groups/${group.group.id}/audit`} />;
}

export function NotifyMenu({ channel, group, onClose }: { channel: Channel; group?: GroupState; onClose(): void }) {
  const t = useT();
  const state = useChat();
  const act = useAct();
  const prefs = state.snap!.prefs;
  const key = `c:${channel.id}`;
  const own = prefs.notify[key] as Notify | undefined;
  const value: Notify | 'inherit' = own ?? (group ? 'inherit' : 'all');
  const set = (v: Notify | 'inherit') => {
    const notify = { ...prefs.notify };
    if (v === 'inherit') delete notify[key];
    else notify[key] = v;
    void act(() => chat.savePrefs({ ...prefs, notify }));
    onClose();
  };
  const options: (Notify | 'inherit')[] = group ? ['inherit', 'all', 'mentions', 'none'] : ['all', 'none'];
  return (
    <div className="menu chat-header-menu">
      {options.map((o) => (
        <button key={o} type="button" className={`menu-item ${value === o ? 'selected' : ''}`} onClick={() => set(o)}>
          <span className="menu-check">{value === o && <Icon name="check" size={14} />}</span>
          <span className="grow">{t.chat.notifyLevel[o]}</span>
        </button>
      ))}
    </div>
  );
}
