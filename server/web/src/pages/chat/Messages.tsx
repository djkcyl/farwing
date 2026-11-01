// A channel's messages (`docs/protocol/chat-design.md` §4): history loaded
// as the list scrolls, the "new messages" line, replies, edits, pins,
// attachments, read receipts in direct messages.
import { useCallback, useEffect, useLayoutEffect, useRef, useState } from 'react';
import { useT } from '../../i18n/react';
import { call, errorText } from '../../lib/api';
import { bytes } from '../../lib/format';
import { chat, useChat, type Pending } from '../../lib/chat/store';
import {
  has,
  idAfter,
  idCmp,
  PERM,
  type Attachment,
  type Channel,
  type Dm,
  type GroupState,
  type Message,
} from '../../lib/chat/types';
import { Icon } from '../../ui/icons';
import { IconButton, Spinner } from '../../ui/kit';
import { useOverlay } from '../../ui/overlay';
import { Avatar, dayText, displayName, EmojiText, fullTime, Rich, roleColor, timeText } from './parts';
import { parse, plain } from '../../lib/chat/markdown';

/// Messages of one author within this many ms read as one block.
const GROUP_MS = 7 * 60 * 1000;

export interface MessageListProps {
  channel: Channel;
  group?: GroupState;
  dm?: Dm;
  /// A message to show (a jump from a reply, pin or search result).
  jumpTo?: { id: string; at: number };
  onReply(m: Message): void;
  onEdit(m: Message): void;
  onUser(userId: string, anchor: HTMLElement): void;
  onImage(a: Attachment): void;
}

export function MessageList({ channel, group, dm, jumpTo, onReply, onEdit, onUser, onImage }: MessageListProps) {
  const t = useT();
  const state = useChat();
  const box = chat.messagesOf(channel.id);
  const list = useRef<HTMLDivElement>(null);
  const atBottom = useRef(true);
  const [showLatest, setShowLatest] = useState(false);
  const [highlight, setHighlight] = useState<string>();
  // Where "new messages" begins: what was read when the channel opened.
  const [readAtOpen, setReadAtOpen] = useState<{ channel: string; id?: string }>({ channel: '' });
  const me = state.snap?.me.user_id;

  if (readAtOpen.channel !== channel.id) {
    const read = state.reads[channel.id]?.last_read_id;
    setReadAtOpen({
      channel: channel.id,
      id: idAfter(channel.last_message_id, read ?? '0') ? (read ?? '0') : undefined,
    });
  }

  useEffect(() => {
    atBottom.current = true;
    if (!state.messages[channel.id] || state.messages[channel.id].newer) void chat.loadLatest(channel.id);
    // Loaded once per open of the channel; new messages come as pushes.
    // oxlint-disable-next-line react-hooks/exhaustive-deps
  }, [channel.id]);

  // Keep the view: at the bottom it follows new messages; older messages
  // added above do not move what is shown.
  const firstId = box.list[0]?.id;
  const prevFirst = useRef<{ id?: string; height: number }>({ height: 0 });
  useLayoutEffect(() => {
    const el = list.current;
    if (!el) return;
    if (prevFirst.current.id && firstId !== prevFirst.current.id && !atBottom.current) {
      el.scrollTop += el.scrollHeight - prevFirst.current.height;
    } else if (atBottom.current && !jumpTo) {
      el.scrollTop = el.scrollHeight;
    }
    prevFirst.current = { id: firstId, height: el.scrollHeight };
  });

  const markRead = useCallback(() => {
    if (atBottom.current && !document.hidden && !chat.messagesOf(channel.id).newer) void chat.markRead(channel.id);
  }, [channel.id]);

  useEffect(() => {
    const timer = window.setTimeout(markRead, 1000);
    return () => window.clearTimeout(timer);
  }, [markRead, channel.last_message_id, box.list.length]);

  useEffect(() => {
    const visible = () => window.setTimeout(markRead, 1000);
    document.addEventListener('visibilitychange', visible);
    window.addEventListener('focus', visible);
    return () => {
      document.removeEventListener('visibilitychange', visible);
      window.removeEventListener('focus', visible);
    };
  }, [markRead]);

  // A jump: load around the message, then bring it into view.
  useEffect(() => {
    if (!jumpTo) return;
    void (async () => {
      await chat.loadAround(channel.id, jumpTo.id);
      atBottom.current = false;
      window.setTimeout(() => {
        document.getElementById(`msg-${jumpTo.id}`)?.scrollIntoView({ block: 'center' });
        setHighlight(jumpTo.id);
        window.setTimeout(() => setHighlight(undefined), 2000);
      }, 50);
    })();
  }, [jumpTo, channel.id]);

  const onScroll = () => {
    const el = list.current;
    if (!el) return;
    const bottom = el.scrollHeight - el.scrollTop - el.clientHeight < 40;
    atBottom.current = bottom && !chat.messagesOf(channel.id).newer;
    setShowLatest(!bottom || chat.messagesOf(channel.id).newer);
    if (el.scrollTop < 300) void chat.loadOlder(channel.id);
    if (el.scrollHeight - el.scrollTop - el.clientHeight < 300) void chat.loadNewer(channel.id);
    if (bottom) markRead();
  };

  const jumpLatest = async () => {
    if (box.newer) await chat.loadLatest(channel.id);
    atBottom.current = true;
    const el = list.current;
    if (el) el.scrollTop = el.scrollHeight;
    setShowLatest(false);
  };

  // My last message the other side has read (direct messages).
  const readMark =
    dm?.peer_read_id &&
    [...box.list].reverse().find((m) => m.author.user_id === me && !m.sending && idCmp(m.id, dm.peer_read_id!) <= 0)
      ?.id;

  const unreadAt = readAtOpen.id;
  let unreadShown = false;
  const memberOf = (id: string) =>
    group ? state.members[group.group.id]?.find((m) => m.user.user_id === id) : undefined;

  return (
    <div className="chat-messages-wrap">
      <div className="chat-messages" ref={list} onScroll={onScroll}>
        {!box.older && (
          <div className="chat-start">
            <div className="chat-start-icon">
              <Icon name={dm ? 'message' : 'hash'} size={28} />
            </div>
            <h2>{t.chat.message.start(dm ? displayName(dm.user) : `#${channel.name}`)}</h2>
            {channel.topic && <div className="muted">{channel.topic}</div>}
          </div>
        )}
        {box.loading && box.older && (
          <div className="chat-loading">
            <Spinner size={16} />
          </div>
        )}
        {box.list.map((m, i) => {
          const prev = box.list[i - 1];
          const newDay = !prev || new Date(prev.created_at).toDateString() !== new Date(m.created_at).toDateString();
          const continued =
            !!prev &&
            !newDay &&
            !prev.deleted &&
            prev.author.user_id === m.author.user_id &&
            m.created_at - prev.created_at < GROUP_MS &&
            !m.reply;
          const showUnread =
            !unreadShown && !!unreadAt && m.author.user_id !== me && idAfter(m.id, unreadAt) && !m.sending;
          if (showUnread) unreadShown = true;
          const member = memberOf(m.author.user_id);
          return (
            <div key={m.nonce && m.sending ? m.nonce : m.id}>
              {newDay && (
                <div className="chat-day">
                  <span>{dayText(m.created_at)}</span>
                </div>
              )}
              {showUnread && (
                <div className="chat-unread-line">
                  <span>{t.chat.message.newMessages}</span>
                </div>
              )}
              <MessageItem
                m={m}
                group={group}
                channel={channel}
                continued={continued && !showUnread}
                highlight={highlight === m.id}
                nickname={member?.nickname}
                color={roleColor(group, member?.roles)}
                mine={m.author.user_id === me}
                onReply={onReply}
                onEdit={onEdit}
                onUser={onUser}
                onImage={onImage}
                onJump={(id) => window.dispatchEvent(new CustomEvent('farwing-chat-jump', { detail: id }))}
              />
              {readMark === m.id && <div className="chat-read-mark">{t.chat.message.read}</div>}
            </div>
          );
        })}
        {box.loading && box.newer && (
          <div className="chat-loading">
            <Spinner size={16} />
          </div>
        )}
      </div>
      {showLatest && (
        <button type="button" className="chat-jump-latest" onClick={() => void jumpLatest()}>
          {t.chat.message.jumpLatest}
          <Icon name="up" size={14} style={{ transform: 'rotate(180deg)' }} />
        </button>
      )}
    </div>
  );
}

function MessageItem({
  m,
  group,
  channel,
  continued,
  highlight,
  nickname,
  color,
  mine,
  onReply,
  onEdit,
  onUser,
  onImage,
  onJump,
}: {
  m: Pending;
  group?: GroupState;
  channel: Channel;
  continued: boolean;
  highlight: boolean;
  nickname?: string;
  color?: string;
  mine: boolean;
  onReply(m: Message): void;
  onEdit(m: Message): void;
  onUser(userId: string, anchor: HTMLElement): void;
  onImage(a: Attachment): void;
  onJump(id: string): void;
}) {
  const t = useT();
  const { confirm, toast } = useOverlay();
  const canSend = has(channel.permissions, PERM.SEND);
  const canManage = has(channel.permissions, PERM.MANAGE_MESSAGES) || channel.kind === 'dm';
  const name = displayName(m.author, nickname);
  const pending = m.sending || !!m.failed;

  const act = async (f: () => Promise<unknown>) => {
    try {
      await f();
    } catch (e) {
      toast(errorText(e), 'red');
    }
  };

  if (m.deleted) {
    return (
      <div id={`msg-${m.id}`} className={`msg ${continued ? 'msg-continued' : ''}`}>
        <div className="msg-gutter" />
        <div className="msg-main">
          <div className="msg-deleted">{t.chat.message.deleted}</div>
        </div>
      </div>
    );
  }

  return (
    <div
      id={`msg-${m.id}`}
      className={`msg ${continued ? 'msg-continued' : ''} ${highlight ? 'msg-highlight' : ''} ${pending ? 'msg-pending' : ''}`}
    >
      {m.reply && (
        <div className="msg-reply" onClick={() => !m.reply!.deleted && onJump(m.reply!.id)}>
          <Icon name="reply" size={12} />
          {m.reply.deleted ? (
            <span className="muted">{t.chat.message.replyDeleted}</span>
          ) : (
            <>
              <span className="msg-reply-name">@{displayName(m.reply.author)}</span>
              <span className="msg-reply-text ellipsis">
                {m.reply.content
                  ? replyText(m.reply.content, group)
                  : m.reply.attachments
                    ? t.chat.attachmentCount(m.reply.attachments)
                    : ''}
              </span>
            </>
          )}
        </div>
      )}
      <div className="msg-gutter">
        {continued ? (
          <span className="msg-hover-time" title={fullTime(m.created_at)}>
            {timeText(m.created_at).split(' ').pop()}
          </span>
        ) : (
          <button type="button" className="msg-avatar" onClick={(e) => onUser(m.author.user_id, e.currentTarget)}>
            <Avatar user={m.author} size={38} />
          </button>
        )}
      </div>
      <div className="msg-main">
        {!continued && (
          <div className="msg-head">
            <button
              type="button"
              className="msg-name"
              style={color ? { color } : undefined}
              onClick={(e) => onUser(m.author.user_id, e.currentTarget)}
            >
              {name}
            </button>
            <span className="msg-time" title={fullTime(m.created_at)}>
              {timeText(m.created_at)}
            </span>
            {m.pinned && <Icon name="pin" size={12} className="msg-pinned" />}
          </div>
        )}
        {m.content && (
          <Rich
            content={m.content}
            group={group}
            onUser={(id) => onUser(id, document.getElementById(`msg-${m.id}`)!)}
          />
        )}
        {m.edited_at && <span className="msg-edited">({t.chat.message.edited})</span>}
        {m.attachments.length > 0 && (
          <div className="msg-files">
            {m.attachments.map((a) => (
              <AttachmentView key={a.id} a={a} onImage={onImage} />
            ))}
          </div>
        )}
        {m.failed && (
          <div className="msg-failed">
            <Icon name="alert" size={13} />
            <span>{m.failed || t.chat.message.failed}</span>
            <button type="button" onClick={() => void act(() => chat.resend(m.channel_id, m))}>
              {t.chat.message.resend}
            </button>
            <button type="button" onClick={() => chat.discard(m.channel_id, m)}>
              {t.chat.message.discard}
            </button>
          </div>
        )}
      </div>
      {!pending && (
        <div className="msg-actions">
          {canSend && <IconButton icon="reply" label={t.chat.message.reply} onClick={() => onReply(m)} />}
          {mine && canSend && <IconButton icon="edit" label={t.chat.message.edit} onClick={() => onEdit(m)} />}
          {canManage && (
            <IconButton
              icon="pin"
              label={m.pinned ? t.chat.message.unpin : t.chat.message.pin}
              className={m.pinned ? 'on' : ''}
              onClick={() =>
                void act(() =>
                  call(m.pinned ? 'DELETE' : 'PUT', `/api/v2/chat/messages/${m.id}/pin`, m.pinned ? undefined : {}),
                )
              }
            />
          )}
          {m.content && (
            <IconButton
              icon="copy"
              label={t.chat.message.copy}
              onClick={() => void navigator.clipboard?.writeText(m.content).catch(() => undefined)}
            />
          )}
          {(mine || has(channel.permissions, PERM.MANAGE_MESSAGES)) && (
            <IconButton
              icon="trash"
              label={t.chat.message.delete}
              onClick={async () => {
                if (
                  await confirm({ title: t.chat.message.deleteQuestion, confirm: t.chat.message.delete, danger: true })
                ) {
                  await act(() => call('DELETE', `/api/v2/chat/messages/${m.id}`));
                }
              }}
            />
          )}
        </div>
      )}
    </div>
  );
}

/// A replied message's beginning as plain text (emoji kept as images).
function replyText(content: string, group?: GroupState) {
  const names = chat.names(group);
  const text = parse(content)
    .map((b) => (b.t === 'pre' ? b.v : plain(b.c, names)))
    .join(' ');
  return <EmojiText text={text} />;
}

function AttachmentView({ a, onImage }: { a: Attachment; onImage(a: Attachment): void }) {
  const t = useT();
  const download = chat.media(`/media/attachments/${a.id}/${encodeURIComponent(a.name)}?download=1`);
  if (a.thumb) {
    const thumb = chat.media(`/media/thumbs/${a.id}`);
    const scale = Math.min(1, 400 / Math.max(a.width, 1), 300 / Math.max(a.height, 1));
    return (
      <button
        type="button"
        className="msg-image"
        style={{ width: Math.round(a.width * scale), aspectRatio: `${a.width} / ${a.height}` }}
        onClick={() => onImage(a)}
        title={a.name}
      >
        {thumb && <img src={thumb} alt={a.name} loading="lazy" draggable={false} />}
      </button>
    );
  }
  return (
    <div className="msg-file">
      <Icon name="file" size={26} />
      <div className="grow">
        <div className="ellipsis">{a.name}</div>
        <div className="small muted">{bytes(a.size)}</div>
      </div>
      {download && (
        <a className="icon-btn btn-ghost" href={download} title={t.chat.message.download} download={a.name}>
          <Icon name="download" size={16} />
        </a>
      )}
    </div>
  );
}
