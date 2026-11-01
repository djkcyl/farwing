// Chat dialogs and panels: creating, joining and discovering groups,
// inviting, the user's chat settings, the image viewer, pins and search.
import { useEffect, useState } from 'react';
import { useT } from '../../i18n/react';
import { call, errorText, signedRaw } from '../../lib/api';
import { navigate } from '../../lib/router';
import { chat, useChat } from '../../lib/chat/store';
import type {
  Attachment,
  Channel,
  GroupPreview,
  GroupState,
  Invite,
  InvitePreview,
  JoinOutcome,
  Message,
  Prefs,
  Status,
} from '../../lib/chat/types';
import type { Friend } from '../../lib/types';

type FriendList = { friends: Friend[] };
import { Icon } from '../../ui/icons';
import { Button, Checkbox, Copy, Empty, Field, IconButton, Input, Loading, Select, Switch } from '../../ui/kit';
import { Dialog, useOverlay } from '../../ui/overlay';
import { Avatar, displayName, GroupIcon, Rich, timeText } from './parts';

export function CreateGroup({ open, onClose }: { open: boolean; onClose(): void }) {
  const t = useT();
  const { toast } = useOverlay();
  const [name, setName] = useState('');
  const [description, setDescription] = useState('');
  const [busy, setBusy] = useState(false);
  const submit = async () => {
    setBusy(true);
    try {
      const s = t.chat.create.starter;
      const g = await call<GroupState>('POST', '/api/v2/chat/groups', {
        name,
        description,
        admin_role: s.admin,
        category: s.category,
        text_channel: s.text,
        voice_channel: s.voice,
      });
      chat.upsertGroup(g);
      onClose();
      setName('');
      setDescription('');
      const first = g.channels.find((c) => c.kind === 'text');
      navigate(`/chat/${g.group.id}${first ? `/${first.id}` : ''}`);
    } catch (e) {
      toast(errorText(e), 'red');
    } finally {
      setBusy(false);
    }
  };
  return (
    <Dialog
      open={open}
      title={t.chat.create.title}
      onClose={onClose}
      footer={
        <>
          <Button onClick={onClose}>{t.common.cancel}</Button>
          <Button variant="primary" loading={busy} disabled={!name.trim()} onClick={() => void submit()}>
            {t.chat.create.submit}
          </Button>
        </>
      }
    >
      <div className="stack">
        <Field label={t.chat.create.name}>
          <Input value={name} maxLength={100} onChange={(e) => setName(e.target.value)} />
        </Field>
        <Field label={t.chat.create.description}>
          <Input value={description} maxLength={1024} onChange={(e) => setDescription(e.target.value)} />
        </Field>
      </div>
    </Dialog>
  );
}

/// The code in an invite link (`…/chat/invite/<code>`) or the code itself.
export function inviteCode(text: string): string {
  const v = text.trim();
  const m = v.match(/\/chat\/invite\/([A-Za-z0-9]+)/);
  return m ? m[1] : v;
}

async function joined(
  outcome: JoinOutcome,
  toast: (s: string, tone?: 'green' | 'red' | 'blue') => void,
  pending: string,
) {
  if (outcome.joined) {
    chat.upsertGroup(outcome.joined);
    const first = outcome.joined.channels.find((c) => c.kind === 'text');
    navigate(`/chat/${outcome.joined.group.id}${first ? `/${first.id}` : ''}`);
  } else if (outcome.pending) {
    toast(pending, 'blue');
  }
}

export function JoinGroup({ open, onClose, initial }: { open: boolean; onClose(): void; initial?: string }) {
  const t = useT();
  const { toast } = useOverlay();
  const [text, setText] = useState(initial ?? '');
  const [preview, setPreview] = useState<InvitePreview>();
  const [busy, setBusy] = useState(false);
  const code = inviteCode(text);
  useEffect(() => {
    if (initial) setText(initial);
  }, [initial]);
  useEffect(() => {
    setPreview(undefined);
    if (!/^[A-Za-z0-9]{10}$/.test(code)) return;
    let stale = false;
    call<InvitePreview>('GET', `/api/v2/chat/invites/${code}`)
      .then((p) => !stale && setPreview(p))
      .catch(() => undefined);
    return () => {
      stale = true;
    };
  }, [code]);
  const submit = async () => {
    setBusy(true);
    try {
      await joined(await call<JoinOutcome>('POST', `/api/v2/chat/invites/${code}/join`), toast, t.chat.join.pending);
      onClose();
      setText('');
    } catch (e) {
      toast(errorText(e), 'red');
    } finally {
      setBusy(false);
    }
  };
  return (
    <Dialog
      open={open}
      title={t.chat.join.title}
      onClose={onClose}
      footer={
        <>
          <Button onClick={onClose}>{t.common.cancel}</Button>
          <Button
            variant="primary"
            loading={busy}
            disabled={!preview || preview.expired || preview.member}
            onClick={() => void submit()}
          >
            {t.chat.join.submit}
          </Button>
        </>
      }
    >
      <div className="stack">
        <Field label={t.chat.join.code}>
          <Input value={text} onChange={(e) => setText(e.target.value)} />
        </Field>
        {preview && (
          <PreviewCard
            g={preview.group}
            note={preview.expired ? t.chat.join.expired : preview.member ? t.chat.join.already : undefined}
          />
        )}
      </div>
    </Dialog>
  );
}

function PreviewCard({ g, note, action }: { g: GroupPreview; note?: string; action?: React.ReactNode }) {
  const t = useT();
  return (
    <div className="chat-preview">
      <GroupIcon group={g} size={48} />
      <div className="grow">
        <div className="ellipsis">
          <b>{g.name}</b>
        </div>
        <div className="small muted">
          {t.chat.members.count(g.member_count)}
          {g.join_approval ? ` · ${t.chat.join.needsApproval}` : ''}
        </div>
        {g.description && <div className="small muted ellipsis">{g.description}</div>}
        {note && <div className="small amber">{note}</div>}
      </div>
      {action}
    </div>
  );
}

export function Discover({ open, onClose }: { open: boolean; onClose(): void }) {
  const t = useT();
  const { toast } = useOverlay();
  const state = useChat();
  const [q, setQ] = useState('');
  const [list, setList] = useState<GroupPreview[]>();
  useEffect(() => {
    if (!open) return;
    let stale = false;
    const timer = window.setTimeout(() => {
      call<{ groups: GroupPreview[] }>('GET', `/api/v2/chat/discover?q=${encodeURIComponent(q)}`)
        .then((r) => !stale && setList(r.groups))
        .catch(() => !stale && setList([]));
    }, 250);
    return () => {
      stale = true;
      window.clearTimeout(timer);
    };
  }, [q, open]);
  const mine = new Set(state.snap?.groups.map((g) => g.group.id));
  return (
    <Dialog open={open} title={t.chat.discover.title} onClose={onClose} wide>
      <div className="stack">
        <Input value={q} placeholder={t.chat.discover.search} onChange={(e) => setQ(e.target.value)} />
        {!list ? (
          <Loading />
        ) : list.length === 0 ? (
          <Empty icon="compass" title={t.chat.discover.none} />
        ) : (
          <div className="stack-sm">
            {list.map((g) => (
              <PreviewCard
                key={g.id}
                g={g}
                action={
                  mine.has(g.id) ? (
                    <Button size="sm" onClick={() => (navigate(`/chat/${g.id}`), onClose())}>
                      {t.common.open}
                    </Button>
                  ) : (
                    <Button
                      size="sm"
                      variant="primary"
                      onClick={async () => {
                        try {
                          await joined(
                            await call<JoinOutcome>('POST', `/api/v2/chat/groups/${g.id}/join`),
                            toast,
                            t.chat.join.pending,
                          );
                          onClose();
                        } catch (e) {
                          toast(errorText(e), 'red');
                        }
                      }}
                    >
                      {t.chat.join.submit}
                    </Button>
                  )
                }
              />
            ))}
          </div>
        )}
      </div>
    </Dialog>
  );
}

const DURATIONS: [string, number | null][] = [
  ['m30', 1800],
  ['h1', 3600],
  ['h6', 6 * 3600],
  ['d1', 86400],
  ['d7', 7 * 86400],
  ['d30', 30 * 86400],
  ['never', null],
];

export function inviteLink(code: string): string {
  return `${location.origin}${new URL(document.baseURI).pathname.replace(/\/$/, '')}/chat/invite/${code}`;
}

export function InviteDialog({ group, open, onClose }: { group: GroupState; open: boolean; onClose(): void }) {
  const t = useT();
  const { toast } = useOverlay();
  const [uses, setUses] = useState<number>(0);
  const [expires, setExpires] = useState<string>('d7');
  const [made, setMade] = useState<Invite>();
  const [friends, setFriends] = useState<FriendList>();
  const [sent, setSent] = useState<Set<string>>(new Set());
  const members = useChat().members[group.group.id];
  useEffect(() => {
    if (!open) return;
    setMade(undefined);
    call<FriendList>('GET', '/api/v2/friends')
      .then(setFriends)
      .catch(() => undefined);
    if (!members) void chat.loadMembers(group.group.id);
  }, [open, group.group.id, members]);
  const create = async () => {
    try {
      const seconds = DURATIONS.find(([k]) => k === expires)?.[1] ?? null;
      setMade(
        await call<Invite>('POST', `/api/v2/chat/groups/${group.group.id}/invites`, {
          max_uses: uses || null,
          expires_s: seconds,
        }),
      );
    } catch (e) {
      toast(errorText(e), 'red');
    }
  };
  const inGroup = new Set(members?.map((m) => m.user.user_id));
  return (
    <Dialog open={open} title={`${t.chat.invite.title} · ${group.group.name}`} onClose={onClose}>
      <div className="stack">
        <div className="row gap wrap">
          <Field label={t.chat.invite.maxUses}>
            <Select
              value={uses}
              onChange={setUses}
              options={[0, 1, 5, 10, 25, 50, 100].map((n) => ({
                value: n,
                label: n ? String(n) : t.chat.invite.unlimited,
              }))}
            />
          </Field>
          <Field label={t.chat.invite.expires}>
            <Select
              value={expires}
              onChange={setExpires}
              options={DURATIONS.map(([k]) => ({
                value: k,
                label:
                  k === 'never'
                    ? t.chat.invite.never
                    : t.chat.invite.durations[k as keyof typeof t.chat.invite.durations],
              }))}
            />
          </Field>
        </div>
        <Button variant="primary" icon="link" onClick={() => void create()}>
          {t.chat.invite.create}
        </Button>
        {made && <Copy text={inviteLink(made.code)} />}
        {friends && friends.friends.length > 0 && (
          <>
            <div className="field-label">{t.chat.invite.friends}</div>
            <div className="stack-sm chat-invite-friends">
              {friends.friends
                .filter((f) => !inGroup.has(f.user.user_id))
                .map((f) => (
                  <div key={f.user.user_id} className="row gap-sm">
                    <span className="grow ellipsis">{displayName({ ...f.user, avatar: '', gone: false })}</span>
                    <Button
                      size="sm"
                      disabled={sent.has(f.user.user_id)}
                      onClick={async () => {
                        try {
                          await call('POST', `/api/v2/chat/groups/${group.group.id}/add`, { user: f.user.user_id });
                          setSent(new Set([...sent, f.user.user_id]));
                        } catch (e) {
                          toast(errorText(e), 'red');
                        }
                      }}
                    >
                      {sent.has(f.user.user_id) ? t.chat.invite.sent : t.chat.invite.title}
                    </Button>
                  </div>
                ))}
            </div>
          </>
        )}
      </div>
    </Dialog>
  );
}

export function ChatSettings({ open, onClose }: { open: boolean; onClose(): void }) {
  const t = useT();
  const state = useChat();
  const { toast } = useOverlay();
  const [prefs, setPrefs] = useState<Prefs>();
  const [permission, setPermission] = useState(
    typeof Notification === 'undefined' ? 'denied' : Notification.permission,
  );
  useEffect(() => {
    if (open && state.snap) setPrefs(state.snap.prefs);
  }, [open, state.snap]);
  if (!state.snap || !prefs) return null;
  const save = async (next: Prefs) => {
    setPrefs(next);
    try {
      await chat.savePrefs(next);
    } catch (e) {
      toast(errorText(e), 'red');
    }
  };
  const status = state.snap.status;
  const me = state.snap.me;
  return (
    <Dialog open={open} title={t.chat.me.title} onClose={onClose}>
      <div className="stack">
        <div className="row gap">
          <Avatar user={me} size={56} />
          <div className="stack-sm">
            <label className="btn btn-default btn-sm file-pick">
              <input
                type="file"
                accept="image/png,image/jpeg,image/gif,image/webp"
                onChange={async (e) => {
                  const file = e.target.files?.[0];
                  e.target.value = '';
                  if (!file) return;
                  try {
                    await uploadSigned('/api/v2/me/avatar', file);
                  } catch (err) {
                    toast(errorText(err), 'red');
                  }
                }}
              />
              <Icon name="upload" size={14} />
              <span>{t.chat.me.changeAvatar}</span>
            </label>
            {me.avatar && (
              <Button
                size="sm"
                onClick={() => void call('DELETE', '/api/v2/me/avatar').catch((e) => toast(errorText(e), 'red'))}
              >
                {t.chat.me.removeAvatar}
              </Button>
            )}
          </div>
        </div>
        <Field label={t.chat.me.status}>
          <Select<Status>
            value={status}
            onChange={(s) => void chat.setStatus(s).catch((e) => toast(errorText(e), 'red'))}
            options={(['online', 'idle', 'dnd', 'invisible'] as Status[]).map((s) => ({
              value: s,
              label: t.chat.status[s],
            }))}
          />
        </Field>
        <Field label={t.chat.me.notifications} group>
          {permission === 'granted' ? (
            <span className="muted">{t.chat.me.notificationsOn}</span>
          ) : permission === 'denied' ? (
            <span className="muted">{t.chat.me.notificationsBlocked}</span>
          ) : (
            <Button size="sm" onClick={async () => setPermission(await Notification.requestPermission())}>
              {t.chat.me.allowNotifications}
            </Button>
          )}
        </Field>
        <Field label={t.chat.me.quietHours} group>
          <div className="row gap-sm">
            <Input
              type="time"
              className="chat-time-input"
              value={prefs.quiet_start}
              onChange={(e) => void save({ ...prefs, quiet_start: e.target.value })}
            />
            <span className="muted">{t.chat.me.to}</span>
            <Input
              type="time"
              className="chat-time-input"
              value={prefs.quiet_end}
              onChange={(e) => void save({ ...prefs, quiet_end: e.target.value })}
            />
          </div>
        </Field>
        <Switch
          label={t.chat.me.hideContent}
          checked={prefs.hide_content}
          onChange={(v) => void save({ ...prefs, hide_content: v })}
        />
        <Switch
          label={t.chat.me.readReceipts}
          checked={prefs.read_receipts}
          onChange={(v) => void save({ ...prefs, read_receipts: v })}
        />
      </div>
    </Dialog>
  );
}

/// A signed upload of raw bytes (an avatar or a group icon).
export async function uploadSigned(path: string, file: File) {
  await signedRaw('PUT', path, new Uint8Array(await file.arrayBuffer()));
}

export function ImageViewer({ a, onClose }: { a?: Attachment; onClose(): void }) {
  const t = useT();
  const [zoom, setZoom] = useState(1);
  useEffect(() => {
    setZoom(1);
    if (!a) return;
    const key = (e: KeyboardEvent) => e.key === 'Escape' && onClose();
    window.addEventListener('keydown', key);
    return () => window.removeEventListener('keydown', key);
  }, [a, onClose]);
  if (!a) return null;
  const full = chat.media(`/media/attachments/${a.id}/${encodeURIComponent(a.name)}`);
  const download = chat.media(`/media/attachments/${a.id}/${encodeURIComponent(a.name)}?download=1`);
  return (
    <div className="image-viewer" onMouseDown={(e) => e.target === e.currentTarget && onClose()}>
      <div className="image-viewer-bar">
        <span className="ellipsis grow">{a.name}</span>
        {download && (
          <a className="icon-btn btn-ghost" href={download} download={a.name} title={t.chat.message.download}>
            <Icon name="download" size={16} />
          </a>
        )}
        <IconButton icon="close" label={t.common.close} onClick={onClose} />
      </div>
      <div
        className="image-viewer-stage"
        onMouseDown={(e) => e.target === e.currentTarget && onClose()}
        onWheel={(e) => setZoom((z) => Math.min(8, Math.max(0.2, z * (e.deltaY < 0 ? 1.15 : 1 / 1.15))))}
      >
        {full && <img src={full} alt={a.name} style={{ transform: `scale(${zoom})` }} draggable={false} />}
      </div>
    </div>
  );
}

export function PinsPanel({
  channel,
  group,
  onJump,
  onClose,
}: {
  channel: Channel;
  group?: GroupState;
  onJump(id: string): void;
  onClose(): void;
}) {
  const t = useT();
  const [list, setList] = useState<Message[]>();
  useEffect(() => {
    let stale = false;
    const load = () =>
      call<{ messages: Message[] }>('GET', `/api/v2/chat/channels/${channel.id}/pins`)
        .then((r) => !stale && setList(r.messages))
        .catch(() => !stale && setList([]));
    void load();
    const changed = (e: Event) => (e as CustomEvent).detail === channel.id && void load();
    window.addEventListener('farwing-chat-pins', changed);
    return () => {
      stale = true;
      window.removeEventListener('farwing-chat-pins', changed);
    };
  }, [channel.id]);
  return (
    <aside className="chat-panel">
      <div className="chat-panel-head">
        <Icon name="pin" size={16} />
        <span className="grow">{t.chat.header.pins}</span>
        <IconButton icon="close" label={t.common.close} onClick={onClose} />
      </div>
      <div className="chat-panel-body">
        {!list ? (
          <Loading />
        ) : list.length === 0 ? (
          <Empty icon="pin" title={t.chat.empty.noPins} />
        ) : (
          list.map((m) => <ResultCard key={m.id} m={m} group={group} onJump={() => onJump(m.id)} />)
        )}
      </div>
    </aside>
  );
}

function ResultCard({ m, group, where, onJump }: { m: Message; group?: GroupState; where?: string; onJump(): void }) {
  const t = useT();
  return (
    <div className="chat-result">
      <div className="row gap-sm">
        <Avatar user={m.author} size={22} />
        <b className="ellipsis">{displayName(m.author)}</b>
        <span className="small muted">{timeText(m.created_at)}</span>
        {where && <span className="small muted ellipsis">{where}</span>}
        <span className="grow" />
        <button type="button" className="link-btn small" onClick={onJump}>
          {t.chat.search.jump}
        </button>
      </div>
      {m.content && <Rich content={m.content} group={group} />}
      {m.attachments.length > 0 && <div className="small muted">{t.chat.attachmentCount(m.attachments.length)}</div>}
    </div>
  );
}

export function SearchPanel({
  channel,
  group,
  onJump,
  onClose,
}: {
  channel: Channel;
  group?: GroupState;
  onJump(channel: string, id: string): void;
  onClose(): void;
}) {
  const t = useT();
  const { toast } = useOverlay();
  const [q, setQ] = useState('');
  const [files, setFiles] = useState(false);
  const [scope, setScope] = useState<'channel' | 'group' | 'all'>('channel');
  const [results, setResults] = useState<{ messages: Message[]; more: boolean }>();
  const [busy, setBusy] = useState(false);
  const run = async (before?: string) => {
    if (!q.trim() && !files) return;
    setBusy(true);
    const params = new URLSearchParams();
    if (q.trim()) params.set('q', q.trim());
    if (files) params.set('has', 'attachment');
    if (scope === 'channel') params.set('channel', channel.id);
    if (scope === 'group' && group) params.set('group', group.group.id);
    if (before) params.set('before', before);
    try {
      const r = await call<{ messages: Message[]; more: boolean }>('GET', `/api/v2/chat/search?${params}`);
      setResults(before && results ? { messages: [...results.messages, ...r.messages], more: r.more } : r);
    } catch (e) {
      toast(errorText(e), 'red');
    } finally {
      setBusy(false);
    }
  };
  return (
    <aside className="chat-panel">
      <div className="chat-panel-head">
        <Icon name="search" size={16} />
        <span className="grow">{t.chat.header.search}</span>
        <IconButton icon="close" label={t.common.close} onClick={onClose} />
      </div>
      <div className="chat-panel-tools">
        <Input
          autoFocus
          value={q}
          placeholder={t.chat.search.placeholder}
          onChange={(e) => setQ(e.target.value)}
          onKeyDown={(e) => e.key === 'Enter' && void run()}
        />
        <div className="row gap-sm wrap">
          <Select
            value={scope}
            onChange={(v) => setScope(v)}
            options={(group ? (['channel', 'group', 'all'] as const) : (['channel', 'all'] as const)).map((s) => ({
              value: s,
              label: t.chat.search.scope[s],
            }))}
          />
          <Checkbox checked={files} onChange={setFiles} label={t.chat.search.attachments} />
          <Button size="sm" loading={busy} onClick={() => void run()}>
            {t.common.search}
          </Button>
        </div>
      </div>
      <div className="chat-panel-body">
        {results &&
          (results.messages.length === 0 ? (
            <Empty icon="search" title={t.chat.empty.noResults} />
          ) : (
            <>
              {results.messages.map((m) => {
                const w = chat.channel(m.channel_id);
                const where = w?.dm ? displayName(w.dm.user) : w ? `#${w.channel.name}` : '';
                return (
                  <ResultCard
                    key={m.id}
                    m={m}
                    group={w?.group}
                    where={where}
                    onJump={() => onJump(m.channel_id, m.id)}
                  />
                );
              })}
              {results.more && (
                <Button size="sm" onClick={() => void run(results.messages[results.messages.length - 1].id)}>
                  {t.chat.search.more}
                </Button>
              )}
            </>
          ))}
      </div>
    </aside>
  );
}
