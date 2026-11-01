// A group's members beside the messages: by role, online first; and the
// card a click on a name opens.
import { useEffect, useMemo, type ReactNode } from 'react';
import { useT } from '../../i18n/react';
import { call, errorText } from '../../lib/api';
import { navigate } from '../../lib/router';
import { chat, useChat } from '../../lib/chat/store';
import { type Dm, type GroupState, type Member } from '../../lib/chat/types';
import { Button } from '../../ui/kit';
import { useOverlay } from '../../ui/overlay';
import { useDismiss } from '../../ui/dismiss';
import { Avatar, displayName, roleColor } from './parts';

export function MemberList({ group, onUser }: { group: GroupState; onUser(id: string, anchor: HTMLElement): void }) {
  const t = useT();
  const state = useChat();
  const members = state.members[group.group.id];
  useEffect(() => {
    if (!state.members[group.group.id]) void chat.loadMembers(group.group.id);
  }, [group.group.id, state.members]);

  const sections = useMemo(() => {
    if (!members) return [];
    const online = (m: Member) =>
      (state.presences[m.user.user_id] ?? 'offline') !== 'offline' && state.presences[m.user.user_id] !== 'invisible';
    const roles = [...group.roles].filter((r) => r.id !== group.group.id).sort((a, b) => b.position - a.position);
    const placed = new Set<string>();
    const out: { title: string; list: Member[] }[] = [];
    for (const r of roles) {
      const list = members.filter((m) => online(m) && m.roles.includes(r.id) && !placed.has(m.user.user_id));
      list.forEach((m) => placed.add(m.user.user_id));
      if (list.length) out.push({ title: r.name, list });
    }
    const rest = members.filter((m) => online(m) && !placed.has(m.user.user_id));
    if (rest.length) out.push({ title: t.chat.members.online, list: rest });
    const offline = members.filter((m) => !online(m));
    if (offline.length) out.push({ title: t.chat.members.offline, list: offline });
    const name = (m: Member) => (m.nickname || displayName(m.user)).toLowerCase();
    for (const s of out) s.list.sort((a, b) => name(a).localeCompare(name(b)));
    return out;
  }, [members, state.presences, group, t]);

  return (
    <aside className="chat-members">
      {sections.map((s) => (
        <div key={s.title} className="chat-members-section">
          <div className="chat-members-title">
            {s.title} — {s.list.length}
          </div>
          {s.list.map((m) => {
            const status = state.presences[m.user.user_id];
            const color = roleColor(group, m.roles);
            return (
              <button
                key={m.user.user_id}
                type="button"
                className={`chat-member ${status && status !== 'offline' ? '' : 'away'}`}
                onClick={(e) => onUser(m.user.user_id, e.currentTarget)}
              >
                <Avatar user={m.user} size={30} status={status} />
                <span className="ellipsis" style={color ? { color } : undefined}>
                  {m.nickname || displayName(m.user)}
                </span>
                {group.group.owner_id === m.user.user_id && (
                  <span className="chat-owner" title={t.chat.members.owner}>
                    ♛
                  </span>
                )}
              </button>
            );
          })}
        </div>
      ))}
    </aside>
  );
}

/// The card of a user: name, status, roles, and a direct message.
export function UserCard({
  userId,
  anchor,
  group,
  onClose,
  children,
}: {
  userId: string;
  anchor: HTMLElement;
  group?: GroupState;
  onClose(): void;
  /// More for this person (their voice controls).
  children?: ReactNode;
}) {
  const t = useT();
  const state = useChat();
  const { toast } = useOverlay();
  const ref = useDismiss(true, onClose);
  const member = group ? state.members[group.group.id]?.find((m) => m.user.user_id === userId) : undefined;
  const dm: Dm | undefined = state.snap?.dms.find((d) => d.user.user_id === userId);
  const user = member?.user ?? dm?.user ?? (state.snap?.me.user_id === userId ? state.snap.me : undefined);
  const r = anchor.getBoundingClientRect();
  const left = Math.min(r.right + 8, window.innerWidth - 300);
  const top = Math.min(r.top, window.innerHeight - 260);
  const status = state.presences[userId] ?? 'offline';
  const me = state.snap?.me.user_id === userId;
  return (
    <div className="chat-user-card" ref={ref} style={{ left, top }}>
      <div className="row gap">
        <Avatar user={user} size={56} status={status} />
        <div className="grow">
          <div className="chat-user-name ellipsis">
            {user ? displayName(user, member?.nickname) : t.chat.unknownUser}
          </div>
          <div className="small muted ellipsis">{user?.username}</div>
          <div className="small muted">{t.chat.status[status]}</div>
        </div>
      </div>
      {member && member.roles.length > 0 && (
        <div className="chat-user-roles">
          {group!.roles
            .filter((r) => member.roles.includes(r.id))
            .sort((a, b) => b.position - a.position)
            .map((r) => (
              <span key={r.id} className="chat-role-chip">
                <span
                  className="dot"
                  style={{ background: r.color ? `#${r.color.toString(16).padStart(6, '0')}` : 'var(--faint)' }}
                />
                {r.name}
              </span>
            ))}
        </div>
      )}
      {!me && user && !user.gone && (
        <Button
          size="sm"
          icon="message"
          onClick={async () => {
            try {
              const opened = await call<Dm>('POST', '/api/v2/chat/dm', { user: userId });
              chat.upsertDm(opened);
              onClose();
              navigate(`/chat/dm/${opened.channel.id}`);
            } catch (e) {
              toast(errorText(e), 'red');
            }
          }}
        >
          {t.chat.members.sendDm}
        </Button>
      )}
      {children}
    </div>
  );
}
