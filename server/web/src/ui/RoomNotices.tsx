/// What happens in the session, shown over the picture to everyone in it:
/// who joined or left, who took control, how the picture travels.
import { useEffect, useRef, useState } from 'react';
import type { Messages } from '../i18n';
import type { Participant } from '../lib/fpp';

/// A notice stays this long; at most `MOST` at once.
const SHOWN_MS = 4000;
const MOST = 3;

interface Notice {
  id: number;
  text: string;
}

export function RoomNotices({
  people,
  forwarding,
  t,
}: {
  people?: { participants: Participant[]; controlling: string };
  forwarding: string;
  t: Messages;
}) {
  const [shown, setShown] = useState<Notice[]>([]);
  const next = useRef(0);
  const known = useRef<Map<string, string> | undefined>(undefined);
  const controlling = useRef('');
  const forwarded = useRef<string | undefined>(undefined);

  useEffect(() => {
    const push = (texts: string[]) => {
      if (!texts.length) return;
      const added = texts.map((text) => ({ id: next.current++, text }));
      setShown((list) => [...list, ...added].slice(-MOST));
      for (const notice of added) {
        window.setTimeout(() => setShown((list) => list.filter((n) => n.id !== notice.id)), SHOWN_MS);
      }
    };
    const s = t.viewer.screen;
    const news: string[] = [];
    // The first list is not announced; an empty one is a gap between lists.
    if (people && people.participants.length) {
      const others = new Map(people.participants.filter((p) => !p.you).map((p) => [p.id, p.nickname]));
      const holder = people.participants.find((p) => p.id === people.controlling && !p.you);
      if (known.current) {
        for (const [id, name] of others) if (!known.current.has(id)) news.push(s.roomJoined(name));
        for (const [id, name] of known.current) if (!others.has(id)) news.push(s.roomLeft(name));
        if (holder && holder.id !== controlling.current) news.push(s.roomControlTaken(holder.nickname));
      }
      known.current = others;
      controlling.current = holder?.id ?? '';
    }
    if (forwarded.current !== undefined && forwarded.current !== forwarding) {
      news.push(forwarding === 'e2ee' ? s.forwardedE2ee : forwarding ? s.forwardedCompatible : s.roomForwardingEnded);
    }
    forwarded.current = forwarding;
    push(news);
  }, [people, forwarding, t]);

  if (!shown.length) return null;
  return (
    <div className="room-notices">
      {shown.map((notice) => (
        <div className="room-notice" key={notice.id}>
          {notice.text}
        </div>
      ))}
    </div>
  );
}
