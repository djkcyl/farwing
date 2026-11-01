// Pieces every chat view uses: avatars with status, rich message text
// (the Markdown subset, emoji images, mentions), times, external links.
import { Fragment, useEffect, useState, type ReactNode } from 'react';
import { locale, t } from '../../i18n';
import { useT } from '../../i18n/react';
import { chat } from '../../lib/chat/store';
import { onlyEmoji, pieces } from '../../lib/chat/emoji';
import { emojiUrl } from '../../lib/chat/emojiAssets';
import { parse, type Inline } from '../../lib/chat/markdown';
import { userName, type ChatUser, type GroupState, type Status } from '../../lib/chat/types';
import { Checkbox } from '../../ui/kit';
import { Dialog } from '../../ui/overlay';
import { Button } from '../../ui/kit';

export function Avatar({ user, size = 36, status }: { user?: ChatUser; size?: number; status?: Status }) {
  const url = user?.avatar ? chat.media(`/media/avatars/${user.user_id}?v=${user.avatar}`) : undefined;
  const name = user?.gone ? '?' : userName(user) || '?';
  return (
    <span className="chat-avatar" style={{ width: size, height: size, fontSize: Math.round(size * 0.42) }}>
      {url ? <img src={url} alt="" draggable={false} /> : <span>{Array.from(name)[0]?.toUpperCase()}</span>}
      {status && status !== 'offline' && <span className={`chat-status status-${status}`} />}
    </span>
  );
}

export function GroupIcon({ group, size = 44 }: { group: { id: string; name: string; icon: string }; size?: number }) {
  const url = group.icon ? chat.media(`/media/icons/${group.id}?v=${group.icon}`) : undefined;
  return (
    <span className="chat-group-icon" style={{ width: size, height: size }}>
      {url ? <img src={url} alt="" draggable={false} /> : <span>{Array.from(group.name).slice(0, 2).join('')}</span>}
    </span>
  );
}

export function displayName(user: ChatUser, nickname?: string): string {
  if (user.gone && !user.username) return t().chat.goneUser;
  return nickname || userName(user);
}

/// Text with its emoji drawn as images (`big`: a message of only emoji).
export function EmojiText({ text }: { text: string }) {
  return (
    <>
      {pieces(text).map((p, i) =>
        p.emoji ? (
          <img
            key={i}
            className="emoji"
            src={emojiUrl(p.key)}
            alt={p.text}
            title={p.text}
            draggable={false}
            onError={(e) => {
              const span = document.createElement('span');
              span.textContent = p.text;
              e.currentTarget.replaceWith(span);
            }}
          />
        ) : (
          <Fragment key={i}>{p.text}</Fragment>
        ),
      )}
    </>
  );
}

const TRUST_KEY = 'farwing.chat.trusted';

function trusted(): string[] {
  try {
    return JSON.parse(localStorage.getItem(TRUST_KEY) ?? '[]') as string[];
  } catch {
    return [];
  }
}

let askLink: ((href: string) => void) | undefined;

/// The confirmation before leaving for an external link (once per page).
export function LinkGuard() {
  const tt = useT();
  const [href, setHref] = useState<string>();
  const [trust, setTrust] = useState(false);
  useEffect(() => {
    askLink = (h) => {
      setTrust(false);
      setHref(h);
    };
    return () => {
      askLink = undefined;
    };
  }, []);
  const host = href ? new URL(href).host : '';
  return (
    <Dialog
      open={!!href}
      title={tt.chat.link.title}
      onClose={() => setHref(undefined)}
      footer={
        <>
          <Button onClick={() => setHref(undefined)}>{tt.common.cancel}</Button>
          <Button
            variant="primary"
            onClick={() => {
              if (trust) localStorage.setItem(TRUST_KEY, JSON.stringify([...new Set([...trusted(), host])]));
              window.open(href, '_blank', 'noopener,noreferrer');
              setHref(undefined);
            }}
          >
            {tt.chat.link.open}
          </Button>
        </>
      }
    >
      <div className="stack-sm">
        <div className="mono chat-link-target">{href}</div>
        <Checkbox checked={trust} onChange={setTrust} label={tt.chat.link.trust} />
      </div>
    </Dialog>
  );
}

function openLink(href: string) {
  let host = '';
  try {
    host = new URL(href).host;
  } catch {
    return;
  }
  if (trusted().includes(host)) window.open(href, '_blank', 'noopener,noreferrer');
  else askLink?.(href);
}

function Spoiler({ children }: { children: ReactNode }) {
  const [shown, setShown] = useState(false);
  return (
    <span
      className={`md-spoiler ${shown ? 'shown' : ''}`}
      title={shown ? undefined : t().chat.message.spoiler}
      onClick={() => setShown(true)}
    >
      {children}
    </span>
  );
}

function renderInline(nodes: Inline[], group: GroupState | undefined, onUser?: (id: string) => void): ReactNode[] {
  const names = chat.names(group);
  return nodes.map((n, i) => {
    switch (n.t) {
      case 'text':
        return <EmojiText key={i} text={n.v} />;
      case 'br':
        return <br key={i} />;
      case 'code':
        return (
          <code key={i} className="md-code">
            {n.v}
          </code>
        );
      case 'b':
        return <strong key={i}>{renderInline(n.c, group, onUser)}</strong>;
      case 'i':
        return <em key={i}>{renderInline(n.c, group, onUser)}</em>;
      case 's':
        return <s key={i}>{renderInline(n.c, group, onUser)}</s>;
      case 'spoiler':
        return <Spoiler key={i}>{renderInline(n.c, group, onUser)}</Spoiler>;
      case 'link':
        return (
          <a
            key={i}
            href={n.href}
            title={n.href}
            rel="noopener noreferrer"
            onClick={(e) => {
              e.preventDefault();
              openLink(n.href);
            }}
          >
            {renderInline(n.c, group, onUser)}
          </a>
        );
      case 'user':
        return (
          <span key={i} className="md-mention" onClick={() => onUser?.(n.id)}>
            @{names.user(n.id)}
          </span>
        );
      case 'role': {
        const role = group?.roles.find((r) => r.id === n.id);
        const color = role?.color ? `#${role.color.toString(16).padStart(6, '0')}` : undefined;
        return (
          <span key={i} className="md-mention" style={color ? { color } : undefined}>
            @{names.role(n.id)}
          </span>
        );
      }
      case 'channel':
        return (
          <span key={i} className="md-mention">
            #{names.channel(n.id)}
          </span>
        );
      case 'everyone':
        return (
          <span key={i} className="md-mention">
            @{t().chat.mentionAll}
          </span>
        );
      case 'here':
        return (
          <span key={i} className="md-mention">
            @{t().chat.mentionHere}
          </span>
        );
    }
    return null;
  });
}

/// A message's text: blocks of the Markdown subset.
export function Rich({
  content,
  group,
  onUser,
}: {
  content: string;
  group?: GroupState;
  onUser?: (id: string) => void;
}) {
  if (onlyEmoji(content)) {
    return (
      <div className="md md-big">
        <EmojiText text={content} />
      </div>
    );
  }
  return (
    <div className="md">
      {parse(content).map((b, i) =>
        b.t === 'pre' ? (
          <pre key={i} className="md-pre">
            {b.v}
          </pre>
        ) : b.t === 'quote' ? (
          <blockquote key={i} className="md-quote">
            {renderInline(b.c, group, onUser)}
          </blockquote>
        ) : (
          <p key={i}>{renderInline(b.c, group, onUser)}</p>
        ),
      )}
    </div>
  );
}

function sameDay(a: Date, b: Date): boolean {
  return a.getFullYear() === b.getFullYear() && a.getMonth() === b.getMonth() && a.getDate() === b.getDate();
}

/// A message's time: "14:05" today, else with the date.
export function timeText(ms: number): string {
  const d = new Date(ms);
  const time = d.toLocaleTimeString(locale(), { hour: '2-digit', minute: '2-digit' });
  const now = new Date();
  if (sameDay(d, now)) return time;
  const yesterday = new Date(now.getTime() - 86_400_000);
  if (sameDay(d, yesterday)) return `${t().chat.yesterday} ${time}`;
  return `${d.toLocaleDateString(locale())} ${time}`;
}

export function dayText(ms: number): string {
  const d = new Date(ms);
  const now = new Date();
  if (sameDay(d, now)) return t().chat.today;
  if (sameDay(d, new Date(now.getTime() - 86_400_000))) return t().chat.yesterday;
  return d.toLocaleDateString(locale(), { year: 'numeric', month: 'long', day: 'numeric', weekday: 'short' });
}

export function fullTime(ms: number): string {
  return new Date(ms).toLocaleString(locale());
}

export function roleColor(group: GroupState | undefined, roles: string[] | undefined): string | undefined {
  if (!group || !roles?.length) return undefined;
  const colored = group.roles.filter((r) => roles.includes(r.id) && r.color).sort((a, b) => b.position - a.position)[0];
  return colored ? `#${colored.color.toString(16).padStart(6, '0')}` : undefined;
}
