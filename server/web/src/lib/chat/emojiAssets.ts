// Emoji images and the picker's index, served by the server (`/emoji/...`).
import { sitePath } from '../base';
import type { EmojiEntry } from './emoji';

export function emojiUrl(key: string): string {
  return sitePath(`/emoji/${key}.png`);
}

let index: Promise<EmojiEntry[]> | undefined;

export function emojiIndex(): Promise<EmojiEntry[]> {
  index ??= fetch(sitePath('/emoji/index.json'))
    .then((r) => (r.ok ? (r.json() as Promise<EmojiEntry[]>) : []))
    .catch(() => {
      index = undefined;
      return [];
    });
  return index;
}
