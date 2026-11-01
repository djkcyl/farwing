// The emoji picker: recent ones, Unicode's groups, search in either
// language, a skin tone remembered in this browser.
import { useEffect, useMemo, useRef, useState } from 'react';
import { useT } from '../../i18n/react';
import { emojiKey, searchEmoji, TONES, withTone, type EmojiEntry } from '../../lib/chat/emoji';
import { emojiIndex, emojiUrl } from '../../lib/chat/emojiAssets';
import { useDismiss } from '../../ui/dismiss';

const RECENT_KEY = 'farwing.chat.emoji.recent';
const TONE_KEY = 'farwing.chat.emoji.tone';
const GROUPS: [string, string][] = [
  ['Smileys & Emotion', '😀'],
  ['People & Body', '👋'],
  ['Animals & Nature', '🐱'],
  ['Food & Drink', '🍎'],
  ['Travel & Places', '🚗'],
  ['Activities', '⚽'],
  ['Objects', '💡'],
  ['Symbols', '❤️'],
  ['Flags', '🏁'],
];

function recent(): string[] {
  try {
    return JSON.parse(localStorage.getItem(RECENT_KEY) ?? '[]') as string[];
  } catch {
    return [];
  }
}

export function EmojiPicker({ onPick, onClose }: { onPick(emoji: string): void; onClose(): void }) {
  const t = useT();
  const ref = useDismiss(true, onClose);
  const [entries, setEntries] = useState<EmojiEntry[]>([]);
  const [query, setQuery] = useState('');
  const [tone, setTone] = useState(() => Number(localStorage.getItem(TONE_KEY) ?? 0) || 0);
  const grid = useRef<HTMLDivElement>(null);
  useEffect(() => {
    void emojiIndex().then(setEntries);
  }, []);
  const byGroup = useMemo(() => {
    const map = new Map<string, EmojiEntry[]>();
    for (const e of entries) map.set(e.g, [...(map.get(e.g) ?? []), e]);
    return map;
  }, [entries]);
  const found = query.trim() ? searchEmoji(entries, query, 120) : undefined;
  const recents = recent()
    .map((e) => entries.find((x) => x.e === e || withTone(x, tone) === e))
    .filter((e): e is EmojiEntry => !!e)
    .slice(0, 24);

  const pick = (e: EmojiEntry) => {
    const value = withTone(e, tone);
    localStorage.setItem(RECENT_KEY, JSON.stringify([value, ...recent().filter((x) => x !== value)].slice(0, 32)));
    onPick(value);
  };

  const cell = (e: EmojiEntry) => {
    const value = withTone(e, tone);
    return (
      <button key={e.c} type="button" className="emoji-cell" title={e.zn || e.n} onClick={() => pick(e)}>
        <img src={emojiUrl(emojiKey(value))} alt={value} loading="lazy" draggable={false} />
      </button>
    );
  };

  return (
    <div className="emoji-picker" ref={ref}>
      <div className="emoji-picker-head">
        <input
          className="input"
          autoFocus
          value={query}
          placeholder={t.common.search}
          onChange={(e) => setQuery(e.target.value)}
        />
        <div className="emoji-tones">
          {TONES.map((_, i) => (
            <button
              key={i}
              type="button"
              className={`emoji-tone tone-${i} ${tone === i ? 'active' : ''}`}
              onClick={() => {
                setTone(i);
                localStorage.setItem(TONE_KEY, String(i));
              }}
            />
          ))}
        </div>
      </div>
      {!found && (
        <div className="emoji-groups">
          {GROUPS.map(([g, icon]) => (
            <button
              key={g}
              type="button"
              className="emoji-group-btn"
              title={g}
              onClick={() => document.getElementById(`emoji-group-${g}`)?.scrollIntoView({ block: 'start' })}
            >
              <img src={emojiUrl(emojiKey(icon))} alt={icon} draggable={false} />
            </button>
          ))}
        </div>
      )}
      <div className="emoji-grid-wrap" ref={grid}>
        {found ? (
          <div className="emoji-grid">{found.map(cell)}</div>
        ) : (
          <>
            {recents.length > 0 && <div className="emoji-grid">{recents.map(cell)}</div>}
            {GROUPS.map(([g]) => (
              <div key={g} id={`emoji-group-${g}`}>
                <div className="emoji-grid">{(byGroup.get(g) ?? []).map(cell)}</div>
              </div>
            ))}
          </>
        )}
      </div>
    </div>
  );
}
