// The message box (`docs/protocol/chat-design.md` §4.1, §12.1): several
// lines, @ / : / # completion, emoji, files (picked, pasted, dropped),
// replying and editing, the length limit.
import { forwardRef, useEffect, useImperativeHandle, useMemo, useRef, useState } from 'react';
import { useT } from '../../i18n/react';
import { call, errorText } from '../../lib/api';
import { sitePath } from '../../lib/base';
import { emojiIndex, emojiUrl } from '../../lib/chat/emojiAssets';
import { searchEmoji, type EmojiEntry } from '../../lib/chat/emoji';
import { chat, useChat } from '../../lib/chat/store';
import {
  has,
  PERM,
  type Attachment,
  type AttachmentUpload,
  type Channel,
  type Dm,
  type GroupState,
  type Message,
} from '../../lib/chat/types';
import { dateTime } from '../../lib/format';
import { Icon } from '../../ui/icons';
import { IconButton } from '../../ui/kit';
import { useOverlay } from '../../ui/overlay';
import { Avatar, displayName } from './parts';
import { EmojiPicker } from './EmojiPicker';

interface Upload {
  key: number;
  file: File;
  progress: number;
  done?: Attachment;
  error?: string;
}

interface Suggestion {
  key: string;
  label: string;
  hint?: string;
  insert: string;
  user?: import('../../lib/chat/types').ChatUser;
  emoji?: EmojiEntry;
}

const drafts = new Map<string, string>();

export interface ComposerHandle {
  addFiles(files: File[]): void;
  focus(): void;
}

function uploadBytes(path: string, file: File, onProgress: (p: number) => void): Promise<Attachment> {
  return new Promise((resolve, reject) => {
    const xhr = new XMLHttpRequest();
    xhr.open('PUT', sitePath(path));
    xhr.upload.onprogress = (e) => e.lengthComputable && onProgress(e.loaded / e.total);
    xhr.onload = () => {
      if (xhr.status >= 200 && xhr.status < 300) resolve(JSON.parse(xhr.responseText) as Attachment);
      else reject(new Error(String(xhr.status)));
    };
    xhr.onerror = () => reject(new Error('network'));
    xhr.send(file);
  });
}

export const Composer = forwardRef<
  ComposerHandle,
  {
    channel: Channel;
    group?: GroupState;
    dm?: Dm;
    reply?: Message;
    editing?: Message;
    onCancel(): void;
    onEditLast(): void;
  }
>(function Composer({ channel, group, dm, reply, editing, onCancel, onEditLast }, ref) {
  const t = useT();
  const state = useChat();
  const { toast } = useOverlay();
  const limits = state.snap?.limits;
  const max = limits?.max_content_chars ?? 4000;
  const [text, setText] = useState(drafts.get(channel.id) ?? '');
  const [shownFor, setShownFor] = useState(channel.id);
  const [uploads, setUploads] = useState<Upload[]>([]);
  const [mentionReply, setMentionReply] = useState(true);
  const [picker, setPicker] = useState(false);
  const [suggest, setSuggest] = useState<{ items: Suggestion[]; start: number; at: number }>();
  const [emoji, setEmoji] = useState<EmojiEntry[]>([]);
  const area = useRef<HTMLTextAreaElement>(null);
  const files = useRef<HTMLInputElement>(null);
  const uploadKey = useRef(1);

  // Switching channels keeps each channel's draft.
  if (shownFor !== channel.id) {
    drafts.set(shownFor, text);
    setShownFor(channel.id);
    setText(drafts.get(channel.id) ?? '');
    setUploads([]);
    setSuggest(undefined);
  }

  useEffect(() => {
    void emojiIndex().then(setEmoji);
  }, []);

  useEffect(() => {
    if (editing) {
      setText(editing.content);
      area.current?.focus();
    }
  }, [editing]);

  useEffect(() => {
    if (reply) area.current?.focus();
  }, [reply]);

  // Grow with the text, up to half the window.
  useEffect(() => {
    const el = area.current;
    if (!el) return;
    el.style.height = 'auto';
    el.style.height = `${Math.min(el.scrollHeight, window.innerHeight * 0.4)}px`;
  }, [text]);

  const canSend = has(channel.permissions, PERM.SEND);
  const canAttach = has(channel.permissions, PERM.ATTACH);
  const mentionAll = has(channel.permissions, PERM.MENTION_ALL);
  const muted = group?.me.muted_until && group.me.muted_until * 1000 > Date.now() ? group.me.muted_until : null;
  const chars = Array.from(text).length;
  const uploading = uploads.some((u) => !u.done && !u.error);

  const addFiles = (list: File[]) => {
    if (!canAttach || editing) return;
    const room = (limits?.max_attachments ?? 10) - uploads.length;
    if (list.length > room) toast(t.chat.composer.tooManyFiles(limits?.max_attachments ?? 10), 'red');
    for (const file of list.slice(0, Math.max(0, room))) {
      if (limits && file.size > limits.max_attachment_bytes) {
        toast(t.chat.composer.tooLarge(file.name), 'red');
        continue;
      }
      const key = uploadKey.current++;
      setUploads((u) => [...u, { key, file, progress: 0 }]);
      void (async () => {
        try {
          const slot = await call<AttachmentUpload>('POST', '/api/v2/chat/attachments', {
            channel_id: channel.id,
            name: file.name || 'image.png',
            mime: file.type || 'application/octet-stream',
            size: file.size,
          });
          const done = await uploadBytes(slot.upload, file, (p) =>
            setUploads((u) => u.map((x) => (x.key === key ? { ...x, progress: p } : x))),
          );
          setUploads((u) => u.map((x) => (x.key === key ? { ...x, done, progress: 1 } : x)));
        } catch (e) {
          setUploads((u) => u.map((x) => (x.key === key ? { ...x, error: errorText(e) } : x)));
        }
      })();
    }
  };

  useImperativeHandle(ref, () => ({ addFiles, focus: () => area.current?.focus() }));

  const members = group ? state.members[group.group.id] : undefined;

  /// What the word before the caret completes to.
  const complete = (value: string, caret: number) => {
    const before = value.slice(0, caret);
    const m = before.match(/(^|\s)([@#:])([^\s@#:]*)$/u);
    if (!m) {
      setSuggest(undefined);
      return;
    }
    const [, , sigil, q] = m;
    const start = caret - q.length - 1;
    const query = q.toLowerCase();
    let items: Suggestion[] = [];
    if (sigil === '@') {
      const people = group
        ? (members ?? []).map((x) => ({ user: x.user, name: x.nickname || displayName(x.user) }))
        : dm
          ? [{ user: dm.user, name: displayName(dm.user) }]
          : [];
      items = people
        .filter((p) => p.name.toLowerCase().includes(query) || p.user.username.toLowerCase().includes(query))
        .slice(0, 8)
        .map((p) => ({
          key: p.user.user_id,
          label: p.name,
          hint: p.user.username,
          insert: `<@${p.user.user_id}> `,
          user: p.user,
        }));
      if (group) {
        for (const r of group.roles) {
          if (r.id === group.group.id || !(r.mentionable || mentionAll)) continue;
          if (r.name.toLowerCase().includes(query))
            items.push({ key: r.id, label: `@${r.name}`, insert: `<@&${r.id}> ` });
        }
        if (mentionAll) {
          for (const [k, label] of [
            ['everyone', t.chat.mentionAll],
            ['here', t.chat.mentionHere],
          ]) {
            if (k.startsWith(query) || label.includes(query))
              items.push({ key: k, label: `@${label}`, insert: `<@${k}> ` });
          }
        }
      }
    } else if (sigil === '#' && group) {
      items = group.channels
        .filter((c) => c.kind !== 'category' && c.name.toLowerCase().includes(query))
        .slice(0, 8)
        .map((c) => ({ key: c.id, label: `#${c.name}`, insert: `<#${c.id}> ` }));
    } else if (sigil === ':' && q.length >= 2) {
      items = searchEmoji(emoji, q, 8).map((e) => ({ key: e.c, label: e.zn || e.n, hint: e.n, insert: e.e, emoji: e }));
    }
    setSuggest(items.length ? { items, start, at: 0 } : undefined);
  };

  const apply = (s: Suggestion) => {
    if (!suggest || !area.current) return;
    const caret = area.current.selectionStart;
    const next = text.slice(0, suggest.start) + s.insert + text.slice(caret);
    setText(next);
    setSuggest(undefined);
    const pos = suggest.start + s.insert.length;
    window.setTimeout(() => {
      area.current?.focus();
      area.current?.setSelectionRange(pos, pos);
    });
  };

  const insert = (value: string) => {
    const el = area.current;
    const at = el?.selectionStart ?? text.length;
    const end = el?.selectionEnd ?? text.length;
    setText(text.slice(0, at) + value + text.slice(end));
    window.setTimeout(() => {
      el?.focus();
      el?.setSelectionRange(at + value.length, at + value.length);
    });
  };

  const submit = async () => {
    const content = text.trim() ? text : '';
    if (uploading) return;
    if (editing) {
      if (!content || chars > max) return;
      try {
        await call('PATCH', `/api/v2/chat/messages/${editing.id}`, { content });
        setText('');
        onCancel();
      } catch (e) {
        toast(errorText(e), 'red');
      }
      return;
    }
    const attachments = uploads.filter((u) => u.done).map((u) => u.done!.id);
    if ((!content && !attachments.length) || chars > max) return;
    setText('');
    drafts.delete(channel.id);
    setUploads([]);
    const replyTo = reply;
    onCancel();
    try {
      await chat.send(channel.id, content, { reply: replyTo, mentionReply, attachments });
    } catch (e) {
      toast(errorText(e), 'red');
    }
  };

  /// A text too long for a message goes as a .txt file.
  const asFile = () => {
    const file = new File([text], 'message.txt', { type: 'text/plain' });
    setText('');
    addFiles([file]);
  };

  if (!canSend) {
    const why =
      dm && !dm.friends
        ? t.chat.composer.notFriends
        : group?.group.frozen
          ? t.chat.composer.frozen
          : muted
            ? t.chat.composer.mutedUntil(dateTime(muted))
            : t.chat.composer.noPermission;
    return (
      <div className="chat-composer">
        <div className="composer-box composer-blocked">{why}</div>
      </div>
    );
  }

  const placeholder = dm ? t.chat.composer.toUser(displayName(dm.user)) : t.chat.composer.toChannel(channel.name);

  return (
    <div className="chat-composer">
      {(reply || editing) && (
        <div className="composer-bar">
          <Icon name={editing ? 'edit' : 'reply'} size={14} />
          <span className="grow ellipsis">
            {editing ? t.chat.composer.editing : t.chat.composer.replyingTo(displayName(reply!.author))}
          </span>
          {reply && (
            <button
              type="button"
              className={`composer-toggle ${mentionReply ? 'on' : ''}`}
              onClick={() => setMentionReply(!mentionReply)}
            >
              @ {t.chat.composer.mention}
            </button>
          )}
          <IconButton
            icon="close"
            label={t.common.cancel}
            onClick={() => {
              if (editing) setText('');
              onCancel();
            }}
          />
        </div>
      )}
      {uploads.length > 0 && (
        <div className="composer-files">
          {uploads.map((u) => (
            <div key={u.key} className={`composer-file ${u.error ? 'failed' : ''}`} title={u.error ?? u.file.name}>
              <Icon name={u.file.type.startsWith('image/') ? 'image' : 'file'} size={16} />
              <span className="ellipsis">{u.file.name}</span>
              {!u.done && !u.error && (
                <span className="composer-progress" style={{ width: `${Math.round(u.progress * 100)}%` }} />
              )}
              <button
                type="button"
                className="composer-file-x"
                aria-label={t.common.remove}
                onClick={() => setUploads((list) => list.filter((x) => x.key !== u.key))}
              >
                <Icon name="close" size={12} />
              </button>
            </div>
          ))}
        </div>
      )}
      <div className="composer-box">
        {canAttach && !editing && (
          <>
            <IconButton icon="paperclip" label={t.chat.composer.attach} onClick={() => files.current?.click()} />
            <input
              ref={files}
              type="file"
              multiple
              hidden
              onChange={(e) => {
                addFiles(Array.from(e.target.files ?? []));
                e.target.value = '';
              }}
            />
          </>
        )}
        <textarea
          ref={area}
          className="composer-input"
          rows={1}
          value={text}
          placeholder={placeholder}
          onChange={(e) => {
            setText(e.target.value);
            if (e.target.value) chat.typing(channel.id);
            complete(e.target.value, e.target.selectionStart);
          }}
          onPaste={(e) => {
            const pasted = Array.from(e.clipboardData.files);
            if (pasted.length) {
              e.preventDefault();
              addFiles(pasted);
            }
          }}
          onKeyDown={(e) => {
            if (suggest) {
              if (e.key === 'ArrowDown' || e.key === 'ArrowUp') {
                e.preventDefault();
                const n = suggest.items.length;
                setSuggest({ ...suggest, at: (suggest.at + (e.key === 'ArrowDown' ? 1 : n - 1)) % n });
                return;
              }
              if (e.key === 'Enter' || e.key === 'Tab') {
                e.preventDefault();
                apply(suggest.items[suggest.at]);
                return;
              }
              if (e.key === 'Escape') {
                setSuggest(undefined);
                return;
              }
            }
            if (e.key === 'Enter' && !e.shiftKey && !e.nativeEvent.isComposing) {
              e.preventDefault();
              void submit();
            } else if (e.key === 'Escape' && (reply || editing)) {
              if (editing) setText('');
              onCancel();
            } else if (e.key === 'ArrowUp' && !text && !editing) {
              onEditLast();
            }
          }}
        />
        <div className="composer-tools">
          <IconButton icon="smile" label={t.chat.composer.emoji} onClick={() => setPicker(!picker)} />
        </div>
        {picker && (
          <EmojiPicker
            onPick={(e) => {
              insert(e);
              setPicker(false);
            }}
            onClose={() => setPicker(false)}
          />
        )}
        {suggest && (
          <div className="composer-suggest">
            {suggest.items.map((s, i) => (
              <button
                key={s.key}
                type="button"
                className={`menu-item ${i === suggest.at ? 'selected' : ''}`}
                onMouseDown={(e) => {
                  e.preventDefault();
                  apply(s);
                }}
              >
                {s.user && <Avatar user={s.user} size={22} />}
                {s.emoji && <img className="emoji" src={emojiUrl(s.emoji.c)} alt={s.emoji.e} />}
                <span className="grow ellipsis">{s.label}</span>
                {s.hint && <span className="small muted">{s.hint}</span>}
              </button>
            ))}
          </div>
        )}
      </div>
      <div className="composer-foot">
        <Typing channel={channel.id} />
        {channel.slow_mode_s > 0 && !has(channel.permissions, PERM.MANAGE_MESSAGES) && (
          <span className="small muted">{t.chat.composer.slowMode(channel.slow_mode_s)}</span>
        )}
        {chars > max - 200 && (
          <span className={`small ${chars > max ? 'red' : 'muted'}`}>
            {chars}/{max}
          </span>
        )}
        {chars > max && canAttach && (
          <button type="button" className="link-btn small" onClick={asFile}>
            {t.chat.composer.toFile}
          </button>
        )}
      </div>
    </div>
  );
});

function Typing({ channel }: { channel: string }) {
  const t = useT();
  const state = useChat();
  const names = useMemo(() => {
    const users = Object.keys(state.typing[channel] ?? {}).filter((u) => u !== state.snap?.me.user_id);
    const where = chat.channel(channel);
    const n = chat.names(where?.group);
    return users.map((u) => n.user(u));
  }, [state.typing, channel, state.snap?.me.user_id]);
  const text =
    names.length === 0
      ? ''
      : names.length === 1
        ? t.chat.typing.one(names[0])
        : names.length === 2
          ? t.chat.typing.two(names[0], names[1])
          : t.chat.typing.many;
  return <span className="composer-typing small muted grow">{text}</span>;
}
