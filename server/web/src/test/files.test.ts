// Folder transfer helpers: paths, the folder upload message and the ZIP
// fallback for folder downloads.
import { describe, expect, test } from 'vitest';
import { hex } from '../lib/encoding';
import { folderSelection, safePath, uniquePaths, uploadMessage } from '../lib/files';
import { Fields } from '../lib/pb';
import { crc32, ZipWriter } from '../lib/zip';

describe('paths', () => {
  test('segments are cleaned and duplicates renamed', () => {
    expect(safePath('a/b:c/./d. ')).toBe('a/b_c/d');
    expect(safePath('\\x\\\\y')).toBe('x/y');
    expect(uniquePaths(['a/x.txt', 'A/X.TXT', 'a/y', 'a/y'])).toEqual(['a/x.txt', 'A/X (2).TXT', 'a/y', 'a/y (2)']);
  });

  test('a folder selection groups by its top folder', () => {
    const file = (rel: string) => Object.assign(new File(['x'], rel.split('/').pop()!), { webkitRelativePath: rel });
    const groups = folderSelection([file('Top/a.txt'), file('Top/sub/b.txt'), file('Other/c')]);
    expect(groups.map((g) => [g.folder, g.entries.map((e) => e.path)])).toEqual([
      ['Top', ['a.txt', 'sub/b.txt']],
      ['Other', ['c']],
    ]);
  });

  test('folder uploads carry the folder and directory entries', () => {
    const body = uploadMessage(
      3,
      'C:\\d',
      [
        { path: 'sub', size: 0n, mtimeNs: 0n, dir: true },
        { path: 'sub/a', size: 2n, mtimeNs: 0n },
      ],
      'k',
      'Top',
    );
    const upload = new Fields(body).message(6)!;
    expect(upload.uint32(1)).toBe(3);
    expect(upload.string(3)).toBe('Top');
    const items = upload.messages(4);
    expect(items.map((i) => [i.string(1), i.bool(4), Number(i.big(2))])).toEqual([
      ['sub', true, 0],
      ['sub/a', false, 2],
    ]);
    expect(upload.int32(5)).toBe(2);
  });
});

describe('zip', () => {
  test('crc32', () => {
    expect(crc32(new TextEncoder().encode('123456789')).toString(16)).toBe('cbf43926');
    // Incremental equals one pass.
    const a = new TextEncoder().encode('12345');
    const b = new TextEncoder().encode('6789');
    expect(crc32(b, crc32(a))).toBe(0xcbf43926);
  });

  test('stored archive layout', async () => {
    const zip = new ZipWriter();
    const when = new Date(2026, 9, 17, 12, 30, 10);
    zip.begin('Top/sub', true, when);
    zip.begin('Top/sub/中.txt', false, when);
    zip.write(new TextEncoder().encode('hello '));
    zip.write(new TextEncoder().encode('world'));
    zip.end();
    const bytes = new Uint8Array(await zip.finish().arrayBuffer());
    const view = new DataView(bytes.buffer);
    // End of central directory: 2 entries, directory right after the data.
    const eocd = bytes.length - 22;
    expect(view.getUint32(eocd, true)).toBe(0x06054b50);
    expect(view.getUint16(eocd + 10, true)).toBe(2);
    const cdOffset = view.getUint32(eocd + 16, true);
    expect(view.getUint32(cdOffset, true)).toBe(0x02014b50);
    // First local header: the directory with a trailing slash, UTF-8 flag.
    expect(view.getUint32(0, true)).toBe(0x04034b50);
    expect(view.getUint16(6, true)).toBe(0x0800);
    const nameLength = view.getUint16(26, true);
    expect(new TextDecoder().decode(bytes.subarray(30, 30 + nameLength))).toBe('Top/sub/');
    // Second entry: CRC and size of "hello world".
    const second = 30 + nameLength;
    expect(view.getUint32(second, true)).toBe(0x04034b50);
    expect(hex(bytes.subarray(second + 14, second + 18))).toBe(
      hex(new Uint8Array(new Uint32Array([crc32(new TextEncoder().encode('hello world'))]).buffer)),
    );
    expect(view.getUint32(second + 22, true)).toBe(11);
  });
});
