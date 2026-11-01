// Emoji in chat text (`docs/protocol/chat-design.md` §4.1, §12.3): split
// text into graphemes, find the emoji among them and name their images
// (`/emoji/<code points>.png`, `fe0f` left out); searching the picker's
// index. Loading the images and the index is in `emojiAssets.ts`.

const EMOJI = /^(?:\p{Extended_Pictographic}|\p{Regional_Indicator}{2}|[#*0-9]\uFE0F?\u20E3)/u;
const segmenter =
  typeof Intl !== 'undefined' && 'Segmenter' in Intl ? new Intl.Segmenter('en', { granularity: 'grapheme' }) : null;

export function graphemes(text: string): string[] {
  if (segmenter) return Array.from(segmenter.segment(text), (s) => s.segment);
  return Array.from(text);
}

export function isEmoji(grapheme: string): boolean {
  return EMOJI.test(grapheme);
}

/// The image name of an emoji grapheme: code points in hex, `-` joined.
export function emojiKey(grapheme: string): string {
  return Array.from(grapheme)
    .map((c) => c.codePointAt(0)!)
    .filter((cp) => cp !== 0xfe0f)
    .map((cp) => cp.toString(16))
    .join('-');
}

export type Piece = { emoji: false; text: string } | { emoji: true; text: string; key: string };

/// Text cut into runs of plain text and single emoji.
export function pieces(text: string): Piece[] {
  const out: Piece[] = [];
  let run = '';
  for (const g of graphemes(text)) {
    if (isEmoji(g)) {
      if (run) out.push({ emoji: false, text: run });
      run = '';
      out.push({ emoji: true, text: g, key: emojiKey(g) });
    } else {
      run += g;
    }
  }
  if (run) out.push({ emoji: false, text: run });
  return out;
}

/// A message of only emoji (whitespace aside), at most 27: shown large.
export function onlyEmoji(text: string): boolean {
  let count = 0;
  for (const g of graphemes(text)) {
    if (/^\s+$/.test(g)) continue;
    if (!isEmoji(g)) return false;
    count++;
    if (count > 27) return false;
  }
  return count > 0;
}

export interface EmojiEntry {
  /// The emoji itself.
  e: string;
  /// Its image name.
  c: string;
  /// Group (Unicode's).
  g: string;
  /// English name and keywords.
  n: string;
  k: string[];
  /// Chinese name and keywords.
  zn: string;
  zk: string[];
  /// Has skin tones.
  t: boolean;
}

/// Entries whose names or keywords contain `query` (either language).
export function searchEmoji(entries: EmojiEntry[], query: string, limit = 40): EmojiEntry[] {
  const q = query.trim().toLowerCase();
  if (!q) return entries.slice(0, limit);
  const scored: [number, EmojiEntry][] = [];
  for (const e of entries) {
    const names = [e.n, e.zn].map((n) => n.toLowerCase());
    const words = [...e.k, ...e.zk].map((w) => w.toLowerCase());
    const score = names.some((n) => n === q)
      ? 0
      : names.some((n) => n.startsWith(q))
        ? 1
        : words.some((w) => w.startsWith(q))
          ? 2
          : names.some((n) => n.includes(q)) || words.some((w) => w.includes(q))
            ? 3
            : -1;
    if (score >= 0) scored.push([score, e]);
  }
  scored.sort((a, b) => a[0] - b[0]);
  return scored.slice(0, limit).map(([, e]) => e);
}

/// Skin tone modifiers, lightest first (index 1–5; 0 is none).
export const TONES = ['', '\u{1F3FB}', '\u{1F3FC}', '\u{1F3FD}', '\u{1F3FE}', '\u{1F3FF}'];

/// An emoji with a skin tone (the first person in it).
export function withTone(e: EmojiEntry, tone: number): string {
  if (!e.t || tone <= 0) return e.e;
  const chars = Array.from(e.e.replace(/\uFE0F/g, ''));
  return chars[0] + TONES[tone] + chars.slice(1).join('');
}
