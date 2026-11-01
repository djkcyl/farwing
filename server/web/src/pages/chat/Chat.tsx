// The chat page (`docs/protocol/chat-design.md` §12.2): groups on the
// left, then channels (or direct messages), the messages, and the members.
import { useCallback, useEffect, useMemo, useRef, useState, type DragEvent } from 'react';
import { useT } from '../../i18n/react';
import { call, errorText } from '../../lib/api';
import { navigate, useLocation } from '../../lib/router';
import { chat, useChat } from '../../lib/chat/store';
import {
  has,
  PERM,
  type Attachment,
  type Channel,
  type Dm,
  type GroupState,
  type JoinOutcome,
  type Message,
} from '../../lib/chat/types';
import type { DeviceList } from '../../lib/types';
import { useApi } from '../../lib/useApi';
import { Icon, type IconName } from '../../ui/icons';
import { Button, Empty, IconButton, Loading } from '../../ui/kit';
import { useOverlay } from '../../ui/overlay';
import { useDismiss } from '../../ui/dismiss';
import { Composer, type ComposerHandle } from './Composer';
import {
  ChatSettings,
  CreateGroup,
  Discover,
  ImageViewer,
  InviteDialog,
  JoinGroup,
  PinsPanel,
  SearchPanel,
} from './Dialogs';
import { GroupSettings, NotifyMenu, orderChannels } from './GroupSettings';
import { MemberList, UserCard } from './Members';
import { MessageList } from './Messages';
import { Avatar, displayName, GroupIcon, LinkGuard } from './parts';

const LAST_KEY = 'farwing.chat.last';

function lastVisited(): Record<string, string> {
  try {
    return JSON.parse(localStorage.getItem(LAST_KEY) ?? '{}') as Record<string, string>;
  } catch {
    return {};
  }
}

function remember(group: string, channel: string) {
  localStorage.setItem(LAST_KEY, JSON.stringify({ ...lastVisited(), [group]: channel, '': group }));
}

function channelIcon(c: Channel): IconName {
  return c.kind === 'voice' ? 'volume' : c.kind === 'announcement' ? 'megaphone' : 'hash';
}

export default function ChatPage({ params }: { params: Record<string, string> }) {
  const t = useT();
  const state = useChat();
  const snap = state.snap;
  const [dialog, setDialog] = useState<'create' | 'join' | 'discover' | 'me' | 'settings' | 'invite'>();
  const [panel, setPanel] = useState<'search' | 'pins'>();
  const [members, setMembers] = useState(() => localStorage.getItem('farwing.chat.members') !== '0');
  const [reply, setReply] = useState<Message>();
  const [editing, setEditing] = useState<Message>();
  const [card, setCard] = useState<{ user: string; anchor: HTMLElement }>();
  const [image, setImage] = useState<Attachment>();
  const [jumpTo, setJumpTo] = useState<{ id: string; at: number }>();
  const [notifyOpen, setNotifyOpen] = useState(false);
  const composer = useRef<ComposerHandle>(null);
  const inviteCode = params.code;
  const { path } = useLocation();
  const dmView = params.group === undefined;

  const group = !dmView ? snap?.groups.find((g) => g.group.id === params.group) : undefined;
  const dm = dmView && params.channel ? snap?.dms.find((d) => d.channel.id === params.channel) : undefined;
  const channel: Channel | undefined =
    dm?.channel ?? group?.channels.find((c) => c.id === params.channel && c.kind !== 'category');

  // Where `/chat` and `/chat/<group>` lead.
  useEffect(() => {
    if (!snap || inviteCode) return;
    if (path === '/chat') {
      const last = lastVisited()[''];
      if (last && snap.groups.some((g) => g.group.id === last)) navigate(`/chat/${last}`, true);
      return;
    }
    if (group && !params.channel && window.innerWidth > 860) {
      const last = lastVisited()[group.group.id];
      const target =
        group.channels.find((c) => c.id === last && c.kind !== 'category') ??
        orderChannels(group.channels).find((c) => c.kind === 'text') ??
        orderChannels(group.channels).find((c) => c.kind === 'announcement');
      if (target) navigate(`/chat/${group.group.id}/${target.id}`, true);
    }
  }, [snap, path, params.group, params.channel, group, inviteCode]);

  useEffect(() => {
    if (group && channel) remember(group.group.id, channel.id);
    chat.setFocus(channel?.id);
    setReply(undefined);
    setEditing(undefined);
    return () => chat.setFocus(undefined);
  }, [group, channel]);

  useEffect(() => {
    if (inviteCode) setDialog('join');
  }, [inviteCode]);

  // Jumps from replies (inside the list) and from search results.
  useEffect(() => {
    const jump = (e: Event) => setJumpTo({ id: (e as CustomEvent<string>).detail, at: Date.now() });
    window.addEventListener('farwing-chat-jump', jump);
    return () => window.removeEventListener('farwing-chat-jump', jump);
  }, []);

  const { toast } = useOverlay();
  useEffect(() => {
    const answered = (e: Event) => {
      const d = (e as CustomEvent).detail as { group: { name: string }; user?: unknown; approved: boolean };
      if (d.user) return;
      toast(
        d.approved ? t.chat.join.approved(d.group.name) : t.chat.join.rejected(d.group.name),
        d.approved ? 'green' : 'blue',
      );
    };
    window.addEventListener('farwing-chat-request', answered);
    return () => window.removeEventListener('farwing-chat-request', answered);
  }, [toast, t]);

  const onUser = useCallback((user: string, anchor: HTMLElement) => setCard({ user, anchor }), []);

  const editLast = () => {
    if (!channel) return;
    const me = snap?.me.user_id;
    const last = [...chat.messagesOf(channel.id).list]
      .reverse()
      .find((m) => m.author.user_id === me && !m.deleted && !m.sending);
    if (last) setEditing(last);
  };

  const onDrop = (e: DragEvent) => {
    if (!e.dataTransfer.files.length) return;
    e.preventDefault();
    composer.current?.addFiles(Array.from(e.dataTransfer.files));
  };

  if (!snap) {
    return (
      <div className="chat">
        <div className="chat-wait">
          {state.connection === 'elsewhere' ? (
            <Empty
              icon="message"
              title={t.chat.connection.elsewhere}
              action={<Button onClick={() => chat.takeOver()}>{t.chat.connection.useHere}</Button>}
            />
          ) : (
            <Loading text={t.chat.connection.connecting} />
          )}
        </div>
      </div>
    );
  }

  return (
    <div className={`chat ${channel ? 'chat-has-channel' : ''}`}>
      <Rail current={dmView ? 'dm' : params.group} onDialog={setDialog} />
      <aside className="chat-side">
        {dmView ? (
          <DmSide current={params.channel} />
        ) : group ? (
          <GroupSide
            group={group}
            current={params.channel}
            onSettings={() => setDialog('settings')}
            onInvite={() => setDialog('invite')}
          />
        ) : (
          <Empty title={t.common.notFound} />
        )}
        <UserBar onSettings={() => setDialog('me')} />
      </aside>
      <main className="chat-main" onDragOver={(e) => e.preventDefault()} onDrop={onDrop}>
        {state.connection !== 'ready' && (
          <div className="chat-banner">
            {state.connection === 'elsewhere' ? (
              <>
                {t.chat.connection.elsewhere}
                <button type="button" className="link-btn" onClick={() => chat.takeOver()}>
                  {t.chat.connection.useHere}
                </button>
              </>
            ) : (
              t.chat.connection.offline
            )}
          </div>
        )}
        {channel ? (
          <>
            <header className="chat-head">
              <button
                type="button"
                className="chat-back icon-btn btn-ghost"
                onClick={() => navigate(dmView ? '/chat/dm' : `/chat/${params.group}/`)}
              >
                <Icon name="back" size={16} />
              </button>
              {dm ? (
                <>
                  <Avatar user={dm.user} size={26} status={state.presences[dm.user.user_id]} />
                  <b className="ellipsis">{displayName(dm.user)}</b>
                  <DmDevices dm={dm} />
                </>
              ) : (
                <>
                  <Icon name={channelIcon(channel)} size={18} className="muted" />
                  <b className="ellipsis">{channel.name}</b>
                  {channel.topic && <span className="chat-topic ellipsis">{channel.topic}</span>}
                </>
              )}
              <span className="grow" />
              <IconButton
                icon="pin"
                label={t.chat.header.pins}
                className={panel === 'pins' ? 'on' : ''}
                onClick={() => setPanel(panel === 'pins' ? undefined : 'pins')}
              />
              <div className="chat-menu-anchor">
                <IconButton icon="bell" label={t.chat.header.notify} onClick={() => setNotifyOpen(!notifyOpen)} />
                {notifyOpen && <NotifyPopover channel={channel} group={group} onClose={() => setNotifyOpen(false)} />}
              </div>
              {group && (
                <IconButton
                  icon="users"
                  label={t.chat.header.members}
                  className={members ? 'on' : ''}
                  onClick={() => {
                    localStorage.setItem('farwing.chat.members', members ? '0' : '1');
                    setMembers(!members);
                  }}
                />
              )}
              <IconButton
                icon="search"
                label={t.chat.header.search}
                className={panel === 'search' ? 'on' : ''}
                onClick={() => setPanel(panel === 'search' ? undefined : 'search')}
              />
            </header>
            <div className="chat-body">
              <div className="chat-center">
                <MessageList
                  channel={channel}
                  group={group}
                  dm={dm}
                  jumpTo={jumpTo}
                  onReply={(m) => (setEditing(undefined), setReply(m))}
                  onEdit={(m) => (setReply(undefined), setEditing(m))}
                  onUser={onUser}
                  onImage={setImage}
                />
                <Composer
                  ref={composer}
                  channel={channel}
                  group={group}
                  dm={dm}
                  reply={reply}
                  editing={editing}
                  onCancel={() => (setReply(undefined), setEditing(undefined))}
                  onEditLast={editLast}
                />
              </div>
              {panel === 'pins' && (
                <PinsPanel
                  channel={channel}
                  group={group}
                  onJump={(id) => setJumpTo({ id, at: Date.now() })}
                  onClose={() => setPanel(undefined)}
                />
              )}
              {panel === 'search' && (
                <SearchPanel
                  channel={channel}
                  group={group}
                  onClose={() => setPanel(undefined)}
                  onJump={(ch, id) => {
                    const where = chat.channel(ch);
                    if (ch !== channel.id && where)
                      navigate(where.dm ? `/chat/dm/${ch}` : `/chat/${where.group?.group.id}/${ch}`);
                    window.setTimeout(() => setJumpTo({ id, at: Date.now() }), ch !== channel.id ? 300 : 0);
                  }}
                />
              )}
              {group && members && !panel && <MemberList group={group} onUser={onUser} />}
            </div>
          </>
        ) : (
          <div className="chat-wait">
            <Empty
              icon="message"
              title={
                dmView ? (snap.dms.length ? t.chat.empty.pickChannel : t.chat.empty.noDms) : t.chat.empty.pickChannel
              }
            />
          </div>
        )}
      </main>
      <CreateGroup open={dialog === 'create'} onClose={() => setDialog(undefined)} />
      <JoinGroup
        open={dialog === 'join'}
        initial={inviteCode}
        onClose={() => {
          setDialog(undefined);
          if (inviteCode) navigate('/chat', true);
        }}
      />
      <Discover open={dialog === 'discover'} onClose={() => setDialog(undefined)} />
      <ChatSettings open={dialog === 'me'} onClose={() => setDialog(undefined)} />
      {group && <GroupSettings group={group} open={dialog === 'settings'} onClose={() => setDialog(undefined)} />}
      {group && <InviteDialog group={group} open={dialog === 'invite'} onClose={() => setDialog(undefined)} />}
      {card && <UserCard userId={card.user} anchor={card.anchor} group={group} onClose={() => setCard(undefined)} />}
      <ImageViewer a={image} onClose={() => setImage(undefined)} />
      <LinkGuard />
    </div>
  );
}

function NotifyPopover({ channel, group, onClose }: { channel: Channel; group?: GroupState; onClose(): void }) {
  const ref = useDismiss(true, onClose);
  return (
    <div ref={ref}>
      <NotifyMenu channel={channel} group={group} onClose={onClose} />
    </div>
  );
}

function Rail({ current, onDialog }: { current?: string; onDialog(d: 'create' | 'join' | 'discover'): void }) {
  const t = useT();
  const state = useChat();
  const snap = state.snap!;
  const dmUnread =
    snap.dms.filter((d) => chat.unread(d.channel).unread && chat.notifyLevel(d.channel.id) !== 'none').length +
    snap.group_invites.length;
  return (
    <nav className="chat-rail">
      <RailItem active={current === 'dm'} title={t.chat.rail.dms} onClick={() => navigate('/chat/dm')} badge={dmUnread}>
        <span className="chat-rail-icon">
          <Icon name="message" size={22} />
        </span>
      </RailItem>
      <div className="chat-rail-sep" />
      {snap.groups.map((g) => {
        let mentions = 0;
        let unread = false;
        for (const c of g.channels) {
          const u = chat.unread(c);
          mentions += u.mentions;
          if (u.unread && c.kind !== 'category' && chat.notifyLevel(c.id, g.group.id) !== 'none') unread = true;
        }
        return (
          <RailItem
            key={g.group.id}
            active={current === g.group.id}
            title={g.group.name}
            onClick={() => navigate(`/chat/${g.group.id}`)}
            badge={mentions}
            dot={unread}
          >
            <GroupIcon group={g.group} />
          </RailItem>
        );
      })}
      {snap.limits.create_groups && (
        <RailItem title={t.chat.rail.create} onClick={() => onDialog('create')}>
          <span className="chat-rail-icon add">
            <Icon name="plus" size={22} />
          </span>
        </RailItem>
      )}
      <RailItem title={t.chat.rail.join} onClick={() => onDialog('join')}>
        <span className="chat-rail-icon add">
          <Icon name="link" size={20} />
        </span>
      </RailItem>
      <RailItem title={t.chat.rail.discover} onClick={() => onDialog('discover')}>
        <span className="chat-rail-icon add">
          <Icon name="compass" size={22} />
        </span>
      </RailItem>
    </nav>
  );
}

function RailItem({
  active,
  title,
  onClick,
  badge,
  dot,
  children,
}: {
  active?: boolean;
  title: string;
  onClick(): void;
  badge?: number;
  dot?: boolean;
  children: React.ReactNode;
}) {
  return (
    <button
      type="button"
      className={`chat-rail-item ${active ? 'active' : ''} ${dot && !active ? 'dot' : ''}`}
      title={title}
      onClick={onClick}
    >
      {children}
      {!!badge && <span className="chat-badge">{badge > 99 ? '99+' : badge}</span>}
    </button>
  );
}

function GroupSide({
  group,
  current,
  onSettings,
  onInvite,
}: {
  group: GroupState;
  current?: string;
  onSettings(): void;
  onInvite(): void;
}) {
  const t = useT();
  useChat();
  const [folded, setFolded] = useState<Set<string>>(
    () => new Set(JSON.parse(localStorage.getItem('farwing.chat.folded') ?? '[]') as string[]),
  );
  const ordered = useMemo(() => orderChannels(group.channels), [group.channels]);
  const fold = (id: string) => {
    const next = new Set(folded);
    if (next.has(id)) next.delete(id);
    else next.add(id);
    setFolded(next);
    localStorage.setItem('farwing.chat.folded', JSON.stringify([...next]));
  };
  const p = group.group.permissions;
  const manage =
    p & (PERM.MANAGE_GROUP | PERM.MANAGE_CHANNELS | PERM.MANAGE_ROLES | PERM.KICK | PERM.BAN | PERM.MUTE | PERM.INVITE);
  return (
    <>
      <header className="chat-side-head">
        <b className="ellipsis grow">{group.group.name}</b>
        {has(p, PERM.INVITE) && <IconButton icon="userPlus" label={t.chat.header.invite} onClick={onInvite} />}
        <IconButton icon={manage ? 'settings' : 'more'} label={t.chat.header.settings} onClick={onSettings} />
      </header>
      <div className="chat-channels">
        {ordered.map((c) => {
          if (c.kind === 'category') {
            return (
              <button key={c.id} type="button" className="chat-category" onClick={() => fold(c.id)}>
                <Icon name="chevron" size={12} style={{ transform: folded.has(c.id) ? undefined : 'rotate(90deg)' }} />
                <span className="ellipsis">{c.name}</span>
              </button>
            );
          }
          const u = chat.unread(c);
          const muted = chat.notifyLevel(c.id, group.group.id) === 'none';
          if (c.parent_id && folded.has(c.parent_id) && c.id !== current && !u.mentions) return null;
          return (
            <button
              key={c.id}
              type="button"
              className={`chat-channel ${c.id === current ? 'active' : ''} ${u.unread && !muted ? 'unread' : ''} ${muted ? 'muted-channel' : ''}`}
              onClick={() => navigate(`/chat/${group.group.id}/${c.id}`)}
            >
              <Icon name={channelIcon(c)} size={16} />
              <span className="ellipsis grow">{c.name}</span>
              {u.mentions > 0 && <span className="chat-badge inline">{u.mentions}</span>}
            </button>
          );
        })}
      </div>
    </>
  );
}

function DmSide({ current }: { current?: string }) {
  const t = useT();
  const state = useChat();
  const { toast } = useOverlay();
  const snap = state.snap!;
  const [picking, setPicking] = useState(false);
  const friends = useApi<{ friends: { user: { user_id: string; username: string; nickname: string } }[] }>(
    picking ? '/api/v2/friends' : null,
  );
  const dms = [...snap.dms].sort((a, b) => {
    const x = a.channel.last_message_id ?? a.channel.id;
    const y = b.channel.last_message_id ?? b.channel.id;
    return x.length !== y.length ? y.length - x.length : y < x ? -1 : y > x ? 1 : 0;
  });
  const open = async (user: string) => {
    try {
      const dm = await call<Dm>('POST', '/api/v2/chat/dm', { user });
      chat.upsertDm(dm);
      setPicking(false);
      navigate(`/chat/dm/${dm.channel.id}`);
    } catch (e) {
      toast(errorText(e), 'red');
    }
  };
  return (
    <>
      <header className="chat-side-head">
        <b className="grow">{t.chat.rail.dms}</b>
        <IconButton icon="plus" label={t.chat.dm.start} onClick={() => setPicking(!picking)} />
      </header>
      <div className="chat-channels">
        {picking && (
          <div className="chat-pick">
            {!friends.data ? (
              <Loading />
            ) : (
              friends.data.friends.map((f) => (
                <button
                  key={f.user.user_id}
                  type="button"
                  className="chat-channel"
                  onClick={() => void open(f.user.user_id)}
                >
                  <Icon name="user" size={16} />
                  <span className="ellipsis">{f.user.nickname || f.user.username}</span>
                </button>
              ))
            )}
          </div>
        )}
        {snap.group_invites.map((inv) => (
          <div key={inv.id} className="chat-invite-card">
            <div className="small muted">{t.chat.invite.invitedBy(displayName(inv.by))}</div>
            <div className="row gap-sm">
              <GroupIcon group={inv.group} size={32} />
              <b className="ellipsis grow">{inv.group.name}</b>
            </div>
            <div className="row gap-sm">
              <Button
                size="sm"
                variant="primary"
                onClick={async () => {
                  try {
                    const r = await call<JoinOutcome>('POST', `/api/v2/chat/group-invites/${inv.id}/accept`);
                    if (r.joined) {
                      chat.upsertGroup(r.joined);
                      navigate(`/chat/${r.joined.group.id}`);
                    }
                  } catch (e) {
                    toast(errorText(e), 'red');
                  }
                }}
              >
                {t.chat.invite.accept}
              </Button>
              <Button
                size="sm"
                onClick={() =>
                  void call('POST', `/api/v2/chat/group-invites/${inv.id}/reject`).catch((e) =>
                    toast(errorText(e), 'red'),
                  )
                }
              >
                {t.chat.invite.reject}
              </Button>
            </div>
          </div>
        ))}
        {dms.map((d) => {
          const u = chat.unread(d.channel);
          return (
            <button
              key={d.channel.id}
              type="button"
              className={`chat-channel chat-dm ${d.channel.id === current ? 'active' : ''} ${u.unread ? 'unread' : ''}`}
              onClick={() => navigate(`/chat/dm/${d.channel.id}`)}
            >
              <Avatar user={d.user} size={30} status={state.presences[d.user.user_id]} />
              <span className="ellipsis grow">{displayName(d.user)}</span>
              {u.unread && <span className="chat-badge inline">{Math.max(1, u.mentions)}</span>}
            </button>
          );
        })}
      </div>
    </>
  );
}

function UserBar({ onSettings }: { onSettings(): void }) {
  const t = useT();
  const state = useChat();
  const me = state.snap!.me;
  const status = state.snap!.status;
  return (
    <div className="chat-userbar">
      <Avatar user={me} size={32} status={status === 'invisible' ? 'offline' : status} />
      <div className="grow">
        <div className="ellipsis">{displayName(me)}</div>
        <div className="small muted">{t.chat.status[status]}</div>
      </div>
      <IconButton icon="settings" label={t.chat.me.title} onClick={onSettings} />
    </div>
  );
}

function DmDevices({ dm }: { dm: Dm }) {
  const t = useT();
  const devices = useApi<DeviceList>('/api/v2/devices');
  const shared =
    devices.data?.shared.filter((d) => d.owner?.user_id === dm.user.user_id && d.granted.includes('view')) ?? [];
  if (!shared.length) return null;
  return (
    <span className="chat-dm-devices" title={t.chat.dm.devices}>
      {shared.map((d) => (
        <button
          key={d.device_id}
          type="button"
          className="chat-dm-device"
          disabled={d.status !== 'online'}
          onClick={() => navigate(`/view/${d.device_id}`)}
        >
          <Icon name="monitor" size={14} />
          <span className="ellipsis">{d.alias}</span>
        </button>
      ))}
    </span>
  );
}
