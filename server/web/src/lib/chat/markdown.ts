// The Markdown subset of chat messages (`docs/protocol/chat-design.md` §4.1):
// **bold**, *italic* / _italic_, ~~strike~~, `code`, ``` blocks ```, > quotes,
// ||spoilers||, links (written and bare), backslash escapes and mentions.
// No headings, tables, images or HTML. The native client parses the same
// way (`src/features/chat/markdown.rs`).

export type Inline =
  | { t: 'text'; v: string }
  | { t: 'code'; v: string }
  | { t: 'b' | 'i' | 's' | 'spoiler'; c: Inline[] }
  | { t: 'link'; href: string; c: Inline[] }
  | { t: 'user'; id: string }
  | { t: 'role'; id: string }
  | { t: 'channel'; id: string }
  | { t: 'everyone' }
  | { t: 'here' }
  | { t: 'br' };

export type Block = { t: 'p'; c: Inline[] } | { t: 'quote'; c: Inline[] } | { t: 'pre'; lang: string; v: string };

const ESCAPABLE = '\\*_~|`[]()<>#';
const UUID = /^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/i;

/// Index of the next unescaped `delim` at or after `from`; -1 if none.
function closing(src: string, delim: string, from: number): number {
  for (let i = from; i < src.length; i++) {
    if (src[i] === '\\') {
      i++;
      continue;
    }
    if (delim !== '`' && src[i] === '`') {
      const end = src.indexOf('`', i + 1);
      if (end > 0) {
        i = end;
        continue;
      }
    }
    if (src.startsWith(delim, i)) return i;
  }
  return -1;
}

function isWord(ch: string | undefined): boolean {
  return !!ch && /[\p{L}\p{N}]/u.test(ch);
}

/// A bare link's end: up to whitespace or `<`, without trailing punctuation
/// (a closing parenthesis only when it closes one in the link).
function bareLinkEnd(src: string, start: number): number {
  let end = start;
  while (end < src.length && !/\s|</.test(src[end])) end++;
  while (end > start) {
    const ch = src[end - 1];
    if ('.,;:!?\'"'.includes(ch)) {
      end--;
      continue;
    }
    if (ch === ')') {
      const text = src.slice(start, end);
      const open = (text.match(/\(/g) ?? []).length;
      const close = (text.match(/\)/g) ?? []).length;
      if (close > open) {
        end--;
        continue;
      }
    }
    break;
  }
  return end;
}

export function safeHref(href: string): string | undefined {
  const h = href.trim();
  return /^https?:\/\/[^\s]+$/i.test(h) ? h : undefined;
}

export function parseInline(src: string): Inline[] {
  const out: Inline[] = [];
  let text = '';
  const flush = () => {
    if (text) out.push({ t: 'text', v: text });
    text = '';
  };
  const wrap = (t: 'b' | 'i' | 's' | 'spoiler', delim: string, i: number): number | undefined => {
    const end = closing(src, delim, i + delim.length);
    if (end <= i + delim.length || !src.slice(i + delim.length, end).trim()) return undefined;
    flush();
    out.push({ t, c: parseInline(src.slice(i + delim.length, end)) });
    return end + delim.length;
  };
  let i = 0;
  while (i < src.length) {
    const ch = src[i];
    if (ch === '\\' && i + 1 < src.length && ESCAPABLE.includes(src[i + 1])) {
      text += src[i + 1];
      i += 2;
      continue;
    }
    if (ch === '\n') {
      flush();
      out.push({ t: 'br' });
      i++;
      continue;
    }
    if (ch === '`') {
      const end = src.indexOf('`', i + 1);
      if (end > i + 1) {
        flush();
        out.push({ t: 'code', v: src.slice(i + 1, end) });
        i = end + 1;
        continue;
      }
    }
    if (src.startsWith('**', i)) {
      const next = wrap('b', '**', i);
      if (next !== undefined) {
        i = next;
        continue;
      }
    }
    if (src.startsWith('~~', i)) {
      const next = wrap('s', '~~', i);
      if (next !== undefined) {
        i = next;
        continue;
      }
    }
    if (src.startsWith('||', i)) {
      const next = wrap('spoiler', '||', i);
      if (next !== undefined) {
        i = next;
        continue;
      }
    }
    if ((ch === '*' || (ch === '_' && !isWord(src[i - 1]))) && src[i + 1] !== ch && src[i + 1] !== ' ') {
      const end = closing(src, ch, i + 1);
      if (end > i + 1 && src[end - 1] !== ' ' && (ch !== '_' || !isWord(src[end + 1]))) {
        flush();
        out.push({ t: 'i', c: parseInline(src.slice(i + 1, end)) });
        i = end + 1;
        continue;
      }
    }
    if (ch === '[') {
      const close = closing(src, ']', i + 1);
      if (close > i + 1 && src[close + 1] === '(') {
        const end = src.indexOf(')', close + 2);
        const href = end > 0 ? safeHref(src.slice(close + 2, end)) : undefined;
        if (href) {
          flush();
          out.push({ t: 'link', href, c: parseInline(src.slice(i + 1, close)) });
          i = end + 1;
          continue;
        }
      }
    }
    if (ch === '<') {
      const end = src.indexOf('>', i + 1);
      if (end > 0) {
        const inner = src.slice(i + 1, end);
        let node: Inline | undefined;
        if (inner === '@everyone') node = { t: 'everyone' };
        else if (inner === '@here') node = { t: 'here' };
        else if (/^@&\d+$/.test(inner)) node = { t: 'role', id: inner.slice(2) };
        else if (/^#\d+$/.test(inner)) node = { t: 'channel', id: inner.slice(1) };
        else if (inner.startsWith('@') && UUID.test(inner.slice(1))) node = { t: 'user', id: inner.slice(1) };
        if (node) {
          flush();
          out.push(node);
          i = end + 1;
          continue;
        }
      }
    }
    if ((src.startsWith('https://', i) || src.startsWith('http://', i)) && !isWord(src[i - 1])) {
      const end = bareLinkEnd(src, i);
      const href = safeHref(src.slice(i, end));
      if (href && end > i + 8) {
        flush();
        out.push({ t: 'link', href, c: [{ t: 'text', v: href }] });
        i = end;
        continue;
      }
    }
    text += ch;
    i++;
  }
  flush();
  return out;
}

/// Blocks: fenced code, quotes (`> ` lines) and the rest.
export function parse(src: string): Block[] {
  const blocks: Block[] = [];
  const lines = src.split('\n');
  let para: string[] = [];
  let quote: string[] = [];
  const flushPara = () => {
    if (para.length) blocks.push({ t: 'p', c: parseInline(para.join('\n')) });
    para = [];
  };
  const flushQuote = () => {
    if (quote.length) blocks.push({ t: 'quote', c: parseInline(quote.join('\n')) });
    quote = [];
  };
  for (let i = 0; i < lines.length; i++) {
    const line = lines[i];
    if (line.startsWith('```')) {
      const rest = line.slice(3);
      // ```one line```
      if (rest.length > 3 && rest.trimEnd().endsWith('```')) {
        flushPara();
        flushQuote();
        const body = rest.trimEnd();
        blocks.push({ t: 'pre', lang: '', v: body.slice(0, body.length - 3) });
        continue;
      }
      // To the next line ending in ```; the opening line's rest is the
      // language when it reads as one, else the first line.
      let end = -1;
      for (let j = i + 1; j < lines.length; j++) {
        if (lines[j].trimEnd().endsWith('```')) {
          end = j;
          break;
        }
      }
      if (end >= 0) {
        flushPara();
        flushQuote();
        const lang = /^[\w+#.-]{1,20}$/.test(rest.trim()) ? rest.trim() : '';
        const first = !lang && rest.trim() ? [rest] : [];
        const closingLine = lines[end].trimEnd();
        const last = closingLine === '```' ? [] : [closingLine.slice(0, -3)];
        blocks.push({ t: 'pre', lang, v: [...first, ...lines.slice(i + 1, end), ...last].join('\n') });
        i = end;
        continue;
      }
    }
    if (line.startsWith('> ') || line === '>') {
      flushPara();
      quote.push(line.slice(line === '>' ? 1 : 2));
      continue;
    }
    flushQuote();
    para.push(line);
  }
  flushPara();
  flushQuote();
  // Leading and trailing empty paragraphs are noise.
  return blocks.filter((b) => b.t !== 'p' || b.c.length > 0);
}

/// Plain text of inline nodes (previews and notifications).
export function plain(
  nodes: Inline[],
  names: { user(id: string): string; role(id: string): string; channel(id: string): string },
): string {
  return nodes
    .map((n) => {
      switch (n.t) {
        case 'text':
        case 'code':
          return n.v;
        case 'br':
          return ' ';
        case 'user':
          return `@${names.user(n.id)}`;
        case 'role':
          return `@${names.role(n.id)}`;
        case 'channel':
          return `#${names.channel(n.id)}`;
        case 'everyone':
          return '@everyone';
        case 'here':
          return '@here';
        case 'spoiler':
          return '▒▒▒';
        default:
          return plain(n.c, names);
      }
    })
    .join('');
}
