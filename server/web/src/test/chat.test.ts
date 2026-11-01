import { describe, expect, it } from 'vitest';
import { emojiKey, onlyEmoji, pieces, searchEmoji, withTone, type EmojiEntry } from '../lib/chat/emoji';
import { parse, parseInline } from '../lib/chat/markdown';
import { idAfter, idCmp } from '../lib/chat/types';

describe('chat markdown', () => {
  it('reads the inline subset', () => {
    expect(parseInline('**bold** and *it* _it_ ~~gone~~ `x*y` ||secret||')).toEqual([
      { t: 'b', c: [{ t: 'text', v: 'bold' }] },
      { t: 'text', v: ' and ' },
      { t: 'i', c: [{ t: 'text', v: 'it' }] },
      { t: 'text', v: ' ' },
      { t: 'i', c: [{ t: 'text', v: 'it' }] },
      { t: 'text', v: ' ' },
      { t: 's', c: [{ t: 'text', v: 'gone' }] },
      { t: 'text', v: ' ' },
      { t: 'code', v: 'x*y' },
      { t: 'text', v: ' ' },
      { t: 'spoiler', c: [{ t: 'text', v: 'secret' }] },
    ]);
  });

  it('nests and escapes', () => {
    expect(parseInline('**a *b* c**')).toEqual([
      {
        t: 'b',
        c: [
          { t: 'text', v: 'a ' },
          { t: 'i', c: [{ t: 'text', v: 'b' }] },
          { t: 'text', v: ' c' },
        ],
      },
    ]);
    expect(parseInline('\\*not\\* 2*3 snake_case_name')).toEqual([{ t: 'text', v: '*not* 2*3 snake_case_name' }]);
    expect(parseInline('** **')).toEqual([{ t: 'text', v: '** **' }]);
  });

  it('links only to http and https', () => {
    expect(parseInline('[site](https://example.com/a) [bad](javascript:alert(1))')).toEqual([
      { t: 'link', href: 'https://example.com/a', c: [{ t: 'text', v: 'site' }] },
      { t: 'text', v: ' [bad](javascript:alert(1))' },
    ]);
    expect(parseInline('see https://example.com/x_(y). ok')).toEqual([
      { t: 'text', v: 'see ' },
      { t: 'link', href: 'https://example.com/x_(y)', c: [{ t: 'text', v: 'https://example.com/x_(y)' }] },
      { t: 'text', v: '. ok' },
    ]);
  });

  it('reads mentions', () => {
    const u = '0f8fad5b-d9cb-469f-a165-70867728950e';
    expect(parseInline(`<@${u}> <@&42> <@everyone> <@here> <#7> <@nobody>`)).toEqual([
      { t: 'user', id: u },
      { t: 'text', v: ' ' },
      { t: 'role', id: '42' },
      { t: 'text', v: ' ' },
      { t: 'everyone' },
      { t: 'text', v: ' ' },
      { t: 'here' },
      { t: 'text', v: ' ' },
      { t: 'channel', id: '7' },
      { t: 'text', v: ' <@nobody>' },
    ]);
  });

  it('cuts blocks: code, quotes, paragraphs', () => {
    expect(parse('hi\n```rust\nfn main() {}\n**x**\n```\n> quoted *a*\n> more\nafter')).toEqual([
      { t: 'p', c: [{ t: 'text', v: 'hi' }] },
      { t: 'pre', lang: 'rust', v: 'fn main() {}\n**x**' },
      {
        t: 'quote',
        c: [{ t: 'text', v: 'quoted ' }, { t: 'i', c: [{ t: 'text', v: 'a' }] }, { t: 'br' }, { t: 'text', v: 'more' }],
      },
      { t: 'p', c: [{ t: 'text', v: 'after' }] },
    ]);
    expect(parse('```one line```')).toEqual([{ t: 'pre', lang: '', v: 'one line' }]);
    expect(parse('```not closed\ntext')).toEqual([
      { t: 'p', c: [{ t: 'text', v: '```not closed' }, { t: 'br' }, { t: 'text', v: 'text' }] },
    ]);
  });
});

describe('chat emoji', () => {
  it('cuts text into emoji and the rest', () => {
    const family = '👨‍👩‍👧';
    const p = pieces(`hi 👍🏽 ${family}!🇨🇳`);
    expect(p.map((x) => (x.emoji ? x.key : x.text))).toEqual([
      'hi ',
      '1f44d-1f3fd',
      ' ',
      '1f468-200d-1f469-200d-1f467',
      '!',
      '1f1e8-1f1f3',
    ]);
    expect(emojiKey('❤️')).toBe('2764');
    expect(emojiKey('#️⃣')).toBe('23-20e3');
  });

  it('knows messages of only emoji', () => {
    expect(onlyEmoji('😀 😀')).toBe(true);
    expect(onlyEmoji('😀 a')).toBe(false);
    expect(onlyEmoji('   ')).toBe(false);
    expect(onlyEmoji('😀'.repeat(28))).toBe(false);
  });

  it('finds emoji by either language and adds skin tones', () => {
    const entries: EmojiEntry[] = [
      {
        e: '😀',
        c: '1f600',
        g: 'Smileys & Emotion',
        n: 'grinning face',
        k: ['grin'],
        zn: '嘿嘿',
        zk: ['笑脸'],
        t: false,
      },
      { e: '👍', c: '1f44d', g: 'People & Body', n: 'thumbs up', k: ['+1'], zn: '拇指向上', zk: ['赞'], t: true },
    ];
    expect(searchEmoji(entries, 'thumb').map((e) => e.c)).toEqual(['1f44d']);
    expect(searchEmoji(entries, '笑').map((e) => e.c)).toEqual(['1f600']);
    expect(withTone(entries[1], 3)).toBe('👍🏽');
    expect(withTone(entries[0], 3)).toBe('😀');
  });
});

describe('chat ids', () => {
  it('compare as numbers', () => {
    expect(idCmp('99', '100')).toBeLessThan(0);
    expect(idAfter('1000', '999')).toBe(true);
    expect(idAfter(null, '1')).toBe(false);
    expect(idAfter('1', null)).toBe(true);
  });
});
